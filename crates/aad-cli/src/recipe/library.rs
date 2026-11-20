use super::{scope::identifier, Locator, Phase};
use aad_core::compiler::MAX_DESCRIPTOR_BYTES;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Call {
    pub call: Target,
    #[serde(default)]
    pub with: BTreeMap<String, Value>,
    #[serde(default)]
    pub save: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum Target {
    Named(String),
    File(Pin),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pin {
    file: PathBuf,
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Subrecipe {
    kind: String,
    format: String,
    name: String,
    version: String,
    #[serde(default)]
    pub parameters: Vec<String>,
    #[serde(default)]
    pub targets: BTreeMap<String, Locator>,
    pub phases: Vec<Phase>,
    #[serde(default)]
    pub results: BTreeMap<String, Value>,
}

pub(super) fn parse(text: &str) -> Result<Value, String> {
    if text.len() > MAX_DESCRIPTOR_BYTES {
        return Err("recipe exceeds 2 MiB".into());
    }
    // serde_yaml's own mapping visitor rejects duplicate keys, including
    // those nested inside a JSON document. Deserializing straight to a JSON
    // map would silently replace one argument or statement with the other.
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(text.trim_start_matches('\u{feff}')).map_err(|err| err.to_string())?;
    serde_json::to_value(yaml).map_err(|err| err.to_string())
}

#[derive(Default)]
pub(super) struct Library {
    cache: BTreeMap<PathBuf, (usize, Rc<Subrecipe>)>,
    stack: Vec<PathBuf>,
    expanded_bytes: usize,
}

impl Library {
    pub fn new(source: Option<&Path>) -> Result<Self, String> {
        let mut library = Self::default();
        if let Some(source) = source {
            library.stack.push(
                source
                    .canonicalize()
                    .map_err(|err| format!("{}: {err}", source.display()))?,
            );
        }
        Ok(library)
    }

    pub fn enter(&mut self, target: Target) -> Result<Rc<Subrecipe>, String> {
        let root = self
            .stack
            .first()
            .and_then(|p| p.parent())
            .ok_or("a subrecipe call requires a source file")?;
        let (base, pin) = match target {
            Target::Named(reference) => {
                let (name, version) = reference
                    .split_once('@')
                    .ok_or("call requires name@MAJOR.MINOR.PATCH")?;
                (
                    root,
                    Pin {
                        file: Path::new("lib").join(name).join(format!("{version}.yaml")),
                        name: name.into(),
                        version: version.into(),
                    },
                )
            }
            Target::File(pin) => (
                self.stack.last().and_then(|p| p.parent()).unwrap_or(root),
                pin,
            ),
        };
        identifier(&pin.name)?;
        if !aad_runtime::version::parse(&pin.version).is_some_and(|v| v.prerelease.is_none())
            || pin.version.contains('+')
        {
            return Err("pin an exact release version MAJOR.MINOR.PATCH".into());
        }
        if pin.file.is_absolute() {
            return Err("subrecipe file must be a relative path".into());
        }
        let path = base
            .join(&pin.file)
            .canonicalize()
            .map_err(|err| format!("{}: {err}", pin.file.display()))?;
        if !path.starts_with(root) {
            return Err(format!(
                "{} is outside the root recipe directory",
                path.display()
            ));
        }
        if self.stack.contains(&path) {
            return Err(format!("recursive subrecipe call to {}", path.display()));
        }
        if !self.cache.contains_key(&path) {
            if !path.is_file() {
                return Err(format!("{} must be a regular file", path.display()));
            }
            let mut text = String::new();
            std::fs::File::open(&path)
                .and_then(|file| {
                    file.take(MAX_DESCRIPTOR_BYTES as u64 + 1)
                        .read_to_string(&mut text)
                })
                .map_err(|err| format!("{}: {err}", path.display()))?;
            let spec: Subrecipe = parse(&text)
                .and_then(|doc| serde_json::from_value(doc).map_err(|err| err.to_string()))
                .map_err(|err| format!("{}: {err}", path.display()))?;
            self.cache.insert(path.clone(), (text.len(), Rc::new(spec)));
        }
        let (bytes, spec) = &self.cache[&path];
        self.expanded_bytes += bytes;
        if self.expanded_bytes > MAX_DESCRIPTOR_BYTES {
            return Err("expanded subrecipe sources exceed 2 MiB".into());
        }
        if spec.kind != "Subrecipe"
            || spec.format != super::program::FORMAT
            || spec.name != pin.name
            || spec.version != pin.version
        {
            return Err(format!(
                "{} must be a {} Subrecipe named {} at version {}",
                path.display(),
                super::program::FORMAT,
                pin.name,
                pin.version
            ));
        }
        self.stack.push(path);
        Ok(Rc::clone(spec))
    }

    pub fn leave(&mut self) {
        self.stack.pop();
    }
}

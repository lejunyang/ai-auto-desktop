use aad_control::steps::{self, App, Step};
use aad_control::Locator;
use clap::Args;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

mod library;
mod program;
mod scope;
#[cfg(test)]
mod tests;

#[derive(Args)]
pub struct Export {
    pub file: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    kind: String,
    name: String,
    #[serde(default)]
    targets: BTreeMap<String, Locator>,
    phases: Vec<Phase>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Phase {
    app: App,
    steps: Vec<Value>,
}

fn compile(name: &str, steps: &[Step]) -> Result<Value, String> {
    let workflow = steps::workflow(name, steps).map_err(|err| err.message)?;
    aad_core::compile_descriptor(workflow.clone(), None).map_err(|err| err.to_string())?;
    Ok(workflow)
}

pub fn load(raw: Value, text: &str, source: &Path) -> Result<Value, String> {
    if raw.get("format").is_some() {
        return program::expand(library::parse(text)?, Some(source));
    }
    expand_legacy(raw)
}

fn expand_legacy(raw: Value) -> Result<Value, String> {
    let recipe: Recipe = serde_json::from_value(raw).map_err(|err| err.to_string())?;
    if recipe.kind != "Recipe" || recipe.phases.is_empty() {
        return Err("expected a Recipe with at least one phase".into());
    }
    let mut steps = Vec::new();
    for phase in recipe.phases {
        if phase.steps.is_empty() || phase.steps.len() > 500 - steps.len() {
            return Err("each phase needs steps and the recipe can hold at most 500 steps".into());
        }
        for mut raw in phase.steps {
            if !raw.is_object() || raw.get("app").is_some() {
                return Err("each step uses the application of its phase".into());
            }
            raw["app"] = json!(phase.app);
            if raw.get("id").is_none() {
                raw["id"] = json!(format!("s{}", steps.len() + 1));
            }
            resolve_targets(&mut raw, &recipe.targets)?;
            steps.push(
                serde_json::from_value(raw)
                    .map_err(|err| format!("step {}: {err}", steps.len() + 1))?,
            );
        }
    }
    compile(&recipe.name, &steps)
}

fn resolve_targets(raw: &mut Value, targets: &BTreeMap<String, Locator>) -> Result<(), String> {
    for key in ["locator", "to"] {
        if let Some(alias) = raw[key].as_str() {
            raw[key] = json!(targets
                .get(alias)
                .ok_or_else(|| format!("unknown target {alias}"))?);
        }
    }
    Ok(())
}

fn compact(document: &Value) -> Result<String, String> {
    if document["kind"] != "Recording" {
        return Err("expected a control-steps/1 recording".into());
    }
    let name = document["metadata"]["name"]
        .as_str()
        .ok_or("missing recording name")?;
    let steps = steps::steps_of(document).map_err(|err| err.message)?;
    let original = compile(name, &steps)?;
    let mut targets = BTreeMap::new();
    let mut names = BTreeMap::new();
    let mut phases = String::new();
    let mut app = None;
    for step in &steps {
        if app != Some(&step.app) {
            phases.push_str(&format!("  - app: {}\n    steps:\n", json!(step.app)));
            app = Some(&step.app);
        }
        let mut raw = json!(step);
        raw.as_object_mut().unwrap().remove("app");
        for key in ["locator", "to"] {
            if let Some(locator) = raw.get(key).cloned() {
                let next = format!("t{}", names.len() + 1);
                let alias = names.entry(locator.to_string()).or_insert(next);
                targets.insert(alias.clone(), locator);
                raw[key] = json!(alias);
            }
        }
        phases.push_str(&format!("      - {raw}\n"));
    }
    let yaml = format!(
        "kind: Recipe\nname: {}\ntargets: {}\nphases:\n{phases}",
        json!(name),
        json!(targets)
    );
    if expand_legacy(serde_yaml::from_str(&yaml).map_err(|err| err.to_string())?)? != original {
        return Err("recipe round-trip changed the workflow".into());
    }
    Ok(yaml)
}

pub fn export(args: &Export) -> Result<Value, String> {
    let source = std::fs::read_to_string(&args.file).map_err(|err| err.to_string())?;
    let recording = serde_json::from_str::<Value>(&source)
        .ok()
        .filter(|doc| doc["kind"] == "Recording");
    let text = if let Some(recording) = recording {
        compact(&recording)?
    } else {
        let raw = library::parse(&source)?;
        let workflow = if raw.get("format").is_some() {
            program::expand(raw, Some(&args.file))?
        } else {
            expand_legacy(raw)?
        };
        aad_runtime::recordings::ensure_no_secret_input(&workflow).map_err(|err| err.message)?;
        let text = serde_json::to_string_pretty(&workflow).map_err(|err| err.to_string())? + "\n";
        if text.len() > aad_core::compiler::MAX_DESCRIPTOR_BYTES {
            return Err("compiled workflow exceeds 2 MiB".into());
        }
        text
    };
    let parent = args
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent).map_err(|err| err.to_string())?;
    output
        .write_all(text.as_bytes())
        .map_err(|err| err.to_string())?;
    output
        .persist_noclobber(&args.out)
        .map_err(|err| err.error.to_string())?;
    Ok(json!({"file":args.out,"lines":text.lines().count(),"bytes":text.len()}))
}

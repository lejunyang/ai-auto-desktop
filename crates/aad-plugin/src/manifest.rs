//! Capability manifest parsing and validation.
//!
//! The manifest is the plugin's contract: which actions exist, what effect
//! class and risk they carry, and which artifact slots they move over the side
//! channel.  Validation is structural rather than schema-driven so the host
//! carries no JSON Schema dependency on its hot path, and so the rules that
//! actually matter for safety are visible in one place.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const MANIFEST_API_VERSION: &str = "ai-auto-desktop.dev/v1alpha1";
pub const MANIFEST_KIND: &str = "CapabilityManifest";

/// One artifact slot moved over the private side channel.
#[derive(Clone, Debug)]
pub struct ArtifactSlot {
    pub pointer: String,
    pub media_types: Vec<String>,
    pub max_size_bytes: u64,
}

impl ArtifactSlot {
    /// RFC 6901 tokens of the slot's pointer.
    pub fn tokens(&self) -> Vec<String> {
        pointer_tokens(&self.pointer)
    }
}

/// The declared contract of a single action.
#[derive(Clone, Debug)]
pub struct ActionContract {
    pub name: String,
    pub contract_major: u32,
    pub effect_class: Option<String>,
    pub risk_category: Option<String>,
    pub risk_level: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub artifact_inputs: BTreeMap<String, ArtifactSlot>,
    pub artifact_outputs: BTreeMap<String, ArtifactSlot>,
    pub raw: Value,
}

impl ActionContract {
    pub fn has_artifacts(&self) -> bool {
        !self.artifact_inputs.is_empty() || !self.artifact_outputs.is_empty()
    }
}

/// A validated capability manifest.
#[derive(Clone, Debug)]
pub struct CapabilityManifest {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub platforms: Vec<String>,
    pub actions: BTreeMap<String, ActionContract>,
    pub raw: Value,
}

impl CapabilityManifest {
    /// Resolve a `provider.action@major` reference against this manifest.
    ///
    /// The provider prefix is optional so a plugin can be addressed either by
    /// its fully qualified action id or by the bare action name.
    pub fn resolve(&self, uses: &str) -> Option<&ActionContract> {
        let stripped = uses
            .strip_prefix(&format!("{}.", self.name))
            .unwrap_or(uses);
        let (action_name, major) = stripped.rsplit_once('@')?;
        let major: u32 = major.parse().ok()?;
        let contract = self.actions.get(action_name)?;
        (contract.contract_major == major).then_some(contract)
    }

    pub fn action_ids(&self) -> Vec<String> {
        self.actions
            .values()
            .map(|action| format!("{}.{}@{}", self.name, action.name, action.contract_major))
            .collect()
    }
}

/// Decode the RFC 6901 tokens of a JSON pointer.
pub fn pointer_tokens(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect()
}

fn slots(
    value: Option<&Value>,
    direction: &str,
    action: &str,
) -> std::result::Result<BTreeMap<String, ArtifactSlot>, String> {
    let mut parsed = BTreeMap::new();
    let Some(Value::Object(map)) = value else {
        return Ok(parsed);
    };
    for (name, raw) in map {
        let Value::Object(slot) = raw else {
            return Err(format!("action {action}: {direction} slot {name} must be an object"));
        };
        let pointer = slot
            .get("pointer")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("action {action}: {direction} slot {name} needs a pointer"))?;
        if !pointer.starts_with('/') {
            return Err(format!(
                "action {action}: {direction} slot {name} pointer must be an RFC 6901 pointer"
            ));
        }
        let media_types: Vec<String> = slot
            .get("media_types")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if media_types.is_empty() {
            return Err(format!(
                "action {action}: {direction} slot {name} needs at least one media type"
            ));
        }
        let max_size_bytes = slot
            .get("max_size_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                format!("action {action}: {direction} slot {name} needs max_size_bytes")
            })?;
        parsed.insert(
            name.clone(),
            ArtifactSlot {
                pointer: pointer.to_string(),
                media_types,
                max_size_bytes,
            },
        );
    }
    Ok(parsed)
}

/// Two slots in the same direction must not alias overlapping regions of the
/// document, otherwise one slot's payload could silently overwrite another's.
fn reject_overlapping_pointers(
    slots: &BTreeMap<String, ArtifactSlot>,
    direction: &str,
    action: &str,
) -> std::result::Result<(), String> {
    let declared: Vec<(&String, Vec<String>)> = slots
        .iter()
        .map(|(name, slot)| (name, slot.tokens()))
        .collect();
    for (index, (left_name, left)) in declared.iter().enumerate() {
        for (right_name, right) in &declared[index + 1..] {
            let shared = left.len().min(right.len());
            if left[..shared] == right[..shared] {
                return Err(format!(
                    "action {action}: {direction} slots {left_name} and {right_name} \
                     declare overlapping pointers"
                ));
            }
        }
    }
    Ok(())
}

/// Parse and validate a capability manifest document.
pub fn parse(raw: &Value) -> std::result::Result<CapabilityManifest, String> {
    let Value::Object(root) = raw else {
        return Err("manifest must be a JSON object".to_string());
    };
    if root.get("apiVersion").and_then(Value::as_str) != Some(MANIFEST_API_VERSION) {
        return Err("manifest has an unsupported apiVersion".to_string());
    }
    if root.get("kind").and_then(Value::as_str) != Some(MANIFEST_KIND) {
        return Err("manifest kind must be CapabilityManifest".to_string());
    }

    let Some(Value::Object(metadata)) = root.get("metadata") else {
        return Err("manifest must contain metadata".to_string());
    };
    let name = metadata
        .get("name")
        .and_then(Value::as_str)
        .ok_or("manifest must contain metadata.name")?
        .to_string();

    let Some(Value::Object(actions)) = root.get("actions") else {
        return Err("manifest must contain an actions object".to_string());
    };
    if actions.is_empty() {
        return Err("manifest must declare at least one action".to_string());
    }

    let mut parsed_actions = BTreeMap::new();
    for (action_name, raw_action) in actions {
        let Value::Object(action) = raw_action else {
            return Err(format!("action {action_name} must be an object"));
        };
        let contract_major = action
            .get("contract_major")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("action {action_name} needs a contract_major"))?
            as u32;

        let effect_class = action
            .get("effect")
            .and_then(Value::as_object)
            .and_then(|effect| effect.get("class"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let risk = action.get("risk").and_then(Value::as_object);

        let artifacts = action.get("artifacts").and_then(Value::as_object);
        let artifact_inputs = slots(
            artifacts.and_then(|value| value.get("inputs")),
            "input",
            action_name,
        )?;
        let artifact_outputs = slots(
            artifacts.and_then(|value| value.get("outputs")),
            "output",
            action_name,
        )?;

        // Slot names must be unique across directions so a token can never
        // refer to two different payloads within one invocation.
        let duplicates: Vec<&String> = artifact_inputs
            .keys()
            .filter(|name| artifact_outputs.contains_key(*name))
            .collect();
        if let Some(duplicate) = duplicates.first() {
            return Err(format!(
                "action {action_name}: artifact slot {duplicate} is declared as both \
                 an input and an output"
            ));
        }
        reject_overlapping_pointers(&artifact_inputs, "input", action_name)?;
        reject_overlapping_pointers(&artifact_outputs, "output", action_name)?;

        parsed_actions.insert(
            action_name.clone(),
            ActionContract {
                name: action_name.clone(),
                contract_major,
                effect_class,
                risk_category: risk
                    .and_then(|value| value.get("category"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                risk_level: risk
                    .and_then(|value| value.get("level"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                input_schema: action.get("input_schema").cloned(),
                output_schema: action.get("output_schema").cloned(),
                artifact_inputs,
                artifact_outputs,
                raw: raw_action.clone(),
            },
        );
    }

    Ok(CapabilityManifest {
        name,
        version: metadata
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        description: metadata
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        platforms: root
            .get("platforms")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        actions: parsed_actions,
        raw: raw.clone(),
    })
}

/// Build a manifest document, used by tests and by native in-process drivers.
pub fn document(name: &str, actions: Map<String, Value>) -> Value {
    serde_json::json!({
        "apiVersion": MANIFEST_API_VERSION,
        "kind": MANIFEST_KIND,
        "metadata": {"name": name},
        "actions": Value::Object(actions),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(actions: Value) -> Value {
        json!({
            "apiVersion": MANIFEST_API_VERSION,
            "kind": MANIFEST_KIND,
            "metadata": {"name": "fixture"},
            "actions": actions
        })
    }

    #[test]
    fn a_minimal_manifest_parses() {
        let parsed = parse(&manifest(json!({
            "ocr": {"contract_major": 1, "effect": {"class": "read_only"}}
        })))
        .expect("manifest should parse");

        assert_eq!(parsed.name, "fixture");
        assert_eq!(parsed.actions.len(), 1);
        assert_eq!(parsed.actions["ocr"].contract_major, 1);
        assert_eq!(parsed.actions["ocr"].effect_class.as_deref(), Some("read_only"));
    }

    #[test]
    fn the_api_version_and_kind_are_enforced() {
        let mut value = manifest(json!({"ocr": {"contract_major": 1}}));
        value["apiVersion"] = json!("other/v1");
        assert!(parse(&value).is_err());

        let mut value = manifest(json!({"ocr": {"contract_major": 1}}));
        value["kind"] = json!("Workflow");
        assert!(parse(&value).is_err());
    }

    #[test]
    fn a_manifest_without_actions_is_rejected() {
        assert!(parse(&manifest(json!({}))).is_err());
    }

    #[test]
    fn resolve_matches_qualified_and_bare_action_ids() {
        let parsed = parse(&manifest(json!({"ocr": {"contract_major": 2}}))).unwrap();

        assert!(parsed.resolve("fixture.ocr@2").is_some());
        assert!(parsed.resolve("ocr@2").is_some());
        // A different major is a different contract.
        assert!(parsed.resolve("fixture.ocr@1").is_none());
        assert!(parsed.resolve("fixture.missing@2").is_none());
    }

    #[test]
    fn action_ids_are_fully_qualified() {
        let parsed = parse(&manifest(json!({"ocr": {"contract_major": 1}}))).unwrap();
        assert_eq!(parsed.action_ids(), vec!["fixture.ocr@1".to_string()]);
    }

    #[test]
    fn artifact_slots_are_parsed_with_their_limits() {
        let parsed = parse(&manifest(json!({
            "capture": {
                "contract_major": 1,
                "artifacts": {
                    "outputs": {
                        "image": {
                            "pointer": "/image",
                            "media_types": ["image/png"],
                            "max_size_bytes": 1024
                        }
                    }
                }
            }
        })))
        .expect("manifest should parse");

        let slot = &parsed.actions["capture"].artifact_outputs["image"];
        assert_eq!(slot.pointer, "/image");
        assert_eq!(slot.max_size_bytes, 1024);
        assert_eq!(slot.tokens(), vec!["image".to_string()]);
    }

    #[test]
    fn a_slot_name_cannot_be_both_input_and_output() {
        let error = parse(&manifest(json!({
            "convert": {
                "contract_major": 1,
                "artifacts": {
                    "inputs": {
                        "data": {"pointer": "/in", "media_types": ["a/b"], "max_size_bytes": 1}
                    },
                    "outputs": {
                        "data": {"pointer": "/out", "media_types": ["a/b"], "max_size_bytes": 1}
                    }
                }
            }
        })))
        .unwrap_err();

        assert!(error.contains("both"), "{error}");
    }

    #[test]
    fn overlapping_slot_pointers_are_rejected() {
        let error = parse(&manifest(json!({
            "capture": {
                "contract_major": 1,
                "artifacts": {
                    "outputs": {
                        "whole": {"pointer": "/data", "media_types": ["a/b"], "max_size_bytes": 1},
                        "part": {"pointer": "/data/inner", "media_types": ["a/b"], "max_size_bytes": 1}
                    }
                }
            }
        })))
        .unwrap_err();

        assert!(error.contains("overlapping"), "{error}");
    }

    #[test]
    fn sibling_slot_pointers_are_allowed() {
        parse(&manifest(json!({
            "capture": {
                "contract_major": 1,
                "artifacts": {
                    "outputs": {
                        "left": {"pointer": "/a", "media_types": ["x/y"], "max_size_bytes": 1},
                        "right": {"pointer": "/b", "media_types": ["x/y"], "max_size_bytes": 1}
                    }
                }
            }
        })))
        .expect("sibling pointers do not overlap");
    }

    #[test]
    fn a_slot_without_media_types_is_rejected() {
        let error = parse(&manifest(json!({
            "capture": {
                "contract_major": 1,
                "artifacts": {
                    "outputs": {"image": {"pointer": "/image", "max_size_bytes": 1}}
                }
            }
        })))
        .unwrap_err();

        assert!(error.contains("media type"), "{error}");
    }

    #[test]
    fn pointer_tokens_decode_rfc6901_escapes() {
        assert_eq!(pointer_tokens("/a~1b/c~0d"), vec!["a/b".to_string(), "c~d".to_string()]);
        assert_eq!(pointer_tokens("/image"), vec!["image".to_string()]);
    }
}

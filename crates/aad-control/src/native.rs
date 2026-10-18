//! Adapters over the existing native providers; no second OS implementation.
use crate::{error, Action, Backend, BackendResource, Resource, Result};
use aad_runtime::Provider;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy)]
pub enum Flavor {
    Windows,
    Linux,
    MacOS,
}

pub struct NativeBackend {
    provider: Arc<dyn Provider>,
    flavor: Flavor,
}

impl NativeBackend {
    pub fn new(provider: Arc<dyn Provider>, flavor: Flavor) -> Self {
        Self { provider, flavor }
    }
    fn invoke(&self, action: &str, args: Value, timeout: Duration) -> Result<Value> {
        self.provider
            .invoke(&format!("{}.{action}@1", self.name()), args, Some(timeout))
    }
    fn native_action(&self, action: &Action) -> &'static str {
        match action {
            Action::Focus {} => "focus",
            Action::Invoke {} => "invoke",
            Action::Click {} => "pointer_click",
            Action::Fill { .. } => {
                if matches!(self.flavor, Flavor::Linux) {
                    "set_text"
                } else {
                    "set_value"
                }
            }
            Action::TypeText { .. } => "type_text",
            Action::Key { .. } => "key_chord",
        }
    }
}

pub fn resource_id(provider: &str, selector: &Value) -> String {
    format!(
        "{provider}:{:x}",
        Sha256::digest(serde_json::to_vec(selector).unwrap())
    )
}

impl Backend for NativeBackend {
    fn node_capabilities(&self, node: &Value) -> Vec<String> {
        let actions = [
            Action::Focus {},
            Action::Invoke {},
            Action::Click {},
            Action::Fill {
                value: String::new(),
            },
            Action::TypeText {
                text: String::new(),
            },
            Action::Key { keys: Vec::new() },
        ];
        actions
            .iter()
            .filter(|action| {
                node["actions"]
                    .as_array()
                    .is_some_and(|native| native.iter().any(|v| v == self.native_action(action)))
            })
            .map(|action| action.name().to_string())
            .collect()
    }
    fn lease_key(&self, _snapshot: &Value) -> Option<String> {
        Some("desktop-input".into())
    }
    fn name(&self) -> &str {
        &self.provider.manifest().name
    }
    fn capabilities(&self) -> Vec<String> {
        let actions = [
            Action::Focus {},
            Action::Invoke {},
            Action::Click {},
            Action::Fill {
                value: String::new(),
            },
            Action::TypeText {
                text: String::new(),
            },
            Action::Key { keys: Vec::new() },
        ];
        actions
            .iter()
            .filter(|action| {
                self.provider
                    .manifest()
                    .actions
                    .contains_key(self.native_action(action))
            })
            .map(|action| action.name().to_string())
            .collect()
    }
    fn resources(&self, timeout: Duration) -> Result<Vec<BackendResource>> {
        let (action, collection) = match self.flavor {
            Flavor::Windows => ("list_windows", "windows"),
            Flavor::Linux => ("list_applications", "applications"),
            Flavor::MacOS => ("list_apps", "apps"),
        };
        let raw = self.invoke(action, json!({}), timeout)?;
        let items = raw[collection].as_array().ok_or_else(|| {
            error(
                "CONTROL.BAD_PROVIDER_OUTPUT",
                "application discovery omitted its collection",
            )
        })?;
        items
            .iter()
            .take(512)
            .map(|metadata| {
                let selector = match self.flavor {
                    Flavor::Windows => json!({"window_id": metadata["window_id"]}),
                    Flavor::Linux => json!({"application": {"bus_name": metadata["bus_name"]}}),
                    Flavor::MacOS => json!({"app": {"process_id": metadata["process_id"]}}),
                };
                let name = metadata["title"]
                    .as_str()
                    .or_else(|| metadata["name"].as_str())
                    .unwrap_or("application")
                    .to_string();
                Ok(BackendResource {
                    resource: Resource {
                        resource_id: resource_id(self.name(), &selector),
                        provider: self.name().into(),
                        kind: "desktop".into(),
                        name,
                        capabilities: self.capabilities(),
                        metadata: metadata.clone(),
                    },
                    selector,
                })
            })
            .collect()
    }
    fn observe(&self, selector: &Value, max_nodes: usize, timeout: Duration) -> Result<Value> {
        let mut args = selector.clone();
        args["max_nodes"] = json!(max_nodes);
        self.invoke("snapshot", args, timeout)
    }
    fn act(
        &self,
        snapshot: &Value,
        node: &Value,
        action: &Action,
        context: &crate::Context,
    ) -> Result<Value> {
        let native = self.native_action(action);
        if !node["actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|v| v == native))
        {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "observed node does not support this native operation",
            ));
        }
        let mut args = json!({"target": {"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": node["node_id"]}});
        if !matches!(self.flavor, Flavor::Windows) {
            let mut locator = Map::new();
            locator.insert("role".into(), node["role"].clone());
            for field in ["name", "description"] {
                if node[field].as_str().is_some_and(|s| !s.is_empty()) {
                    locator.insert(field.into(), node[field].clone());
                }
            }
            if matches!(self.flavor, Flavor::Linux) {
                if node["attributes"]
                    .as_object()
                    .is_some_and(|v| !v.is_empty())
                {
                    locator.insert("attributes".into(), node["attributes"].clone());
                }
            } else if !node["subrole"].is_null() {
                locator.insert("subrole".into(), node["subrole"].clone());
            }
            args["locator"] = Value::Object(locator);
        }
        match action {
            Action::Fill { value } => {
                args[if matches!(self.flavor, Flavor::Linux) {
                    "text"
                } else {
                    "value"
                }] = json!(value)
            }
            Action::TypeText { text } => args["text"] = json!(text),
            Action::Key { keys } => args["keys"] = json!(keys),
            _ => {}
        }
        self.invoke(native, args, context.commit()?)
    }
}

pub fn backends() -> (Vec<Arc<dyn Backend>>, BTreeMap<String, Value>) {
    let mut backends: Vec<Arc<dyn Backend>> = Vec::new();
    let mut failures = BTreeMap::new();
    #[cfg(windows)]
    let native: Result<NativeBackend> = aad_uia::native_driver_with_store(
        aad_uia::SnapshotStore::new(32, Duration::from_secs(300)),
    )
    .map(|driver| NativeBackend::new(Arc::new(driver), Flavor::Windows))
    .map_err(|e| e.into_automation_error());
    #[cfg(target_os = "linux")]
    let native: Result<NativeBackend> = aad_atspi::native_provider()
        .map(|provider| NativeBackend::new(Arc::new(provider), Flavor::Linux));
    #[cfg(target_os = "macos")]
    let native: Result<NativeBackend> = aad_macos_ax::native_provider()
        .map(|provider| NativeBackend::new(Arc::new(provider), Flavor::MacOS));
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let native: Result<NativeBackend> = Err(error(
        "CONTROL.UNAVAILABLE",
        "no native provider for this host",
    ));
    match native {
        Ok(backend) => backends.push(Arc::new(backend)),
        Err(e) => {
            failures.insert("desktop".into(), e.to_json());
        }
    }
    (backends, failures)
}

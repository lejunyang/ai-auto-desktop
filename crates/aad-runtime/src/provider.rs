//! The boundary between the engine and whatever actually performs an action.
//!
//! The engine never talks to a process, a driver or an OS API directly.  It
//! resolves an action id to a [`Provider`] and asks it to invoke.  That keeps
//! the engine testable with in-process fakes and lets native drivers, plugin
//! subprocesses and recorded fixtures all be substituted for one another.

use aad_core::AutomationError;
use aad_plugin::{manifest::ActionContract, CapabilityManifest, PluginError, ProcessPlugin};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Something that can execute actions declared by a capability manifest.
pub trait Provider: Send + Sync {
    /// The manifest describing the actions this provider offers.
    fn manifest(&self) -> &CapabilityManifest;

    /// Execute one action and return its JSON result.
    fn invoke(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AutomationError>;

    /// Release any resources; called once when the run finishes.
    fn close(&self) {}
}

/// A provider backed by an NDJSON plugin subprocess.
pub struct PluginProvider {
    manifest: CapabilityManifest,
    plugin: Mutex<ProcessPlugin>,
}

impl PluginProvider {
    pub fn new(plugin: ProcessPlugin) -> Result<Self, PluginError> {
        let manifest = plugin
            .manifest()
            .cloned()
            .ok_or_else(|| PluginError::new("PLUGIN.INVALID_STATE", "plugin has no manifest"))?;
        Ok(Self {
            manifest,
            plugin: Mutex::new(plugin),
        })
    }
}

impl Provider for PluginProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    fn invoke(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AutomationError> {
        let mut plugin = self.plugin.lock().map_err(|_| {
            AutomationError::new("PLUGIN.HOST_POISONED", "plugin host lock was poisoned")
                .with_effect("unknown")
        })?;
        plugin
            .invoke(action, args, timeout)
            .map_err(PluginError::into_automation_error)
    }

    fn close(&self) {
        if let Ok(mut plugin) = self.plugin.lock() {
            plugin.close();
        }
    }
}

/// The set of providers available to one run, indexed by capability name.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, provider: Arc<dyn Provider>) {
        self.providers
            .insert(provider.manifest().name.clone(), provider);
    }

    pub fn names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(name)
    }

    /// Resolve `provider.action@major` to its provider and declared contract.
    ///
    /// The capability name may itself contain dots, so the split is tried at
    /// every boundary rather than assumed to be the last one.
    pub fn resolve(&self, uses: &str) -> Option<(&Arc<dyn Provider>, &ActionContract)> {
        let path = uses.split_once('@').map(|(head, _)| head).unwrap_or(uses);
        let mut boundary = path.len();
        while let Some(index) = path[..boundary].rfind('.') {
            let candidate = &path[..index];
            if let Some(provider) = self.providers.get(candidate) {
                if let Some(contract) = provider.manifest().resolve(uses) {
                    return Some((provider, contract));
                }
            }
            boundary = index;
        }
        // Fall back to a provider whose manifest claims the bare action id.
        self.providers.values().find_map(|provider| {
            provider
                .manifest()
                .resolve(uses)
                .map(|contract| (provider, contract))
        })
    }

    pub fn close_all(&self) {
        for provider in self.providers.values() {
            provider.close();
        }
    }
}

/// Start a named subprocess plugin and add it to a registry.
pub fn register_process_plugin(
    registry: &mut ProviderRegistry,
    name: &str,
    command: Vec<String>,
) -> Result<(), AutomationError> {
    let plugin = ProcessPlugin::start(aad_plugin::PluginSpec::new(command).with_name(name))
        .map_err(PluginError::into_automation_error)?;
    let provider = PluginProvider::new(plugin).map_err(PluginError::into_automation_error)?;
    if provider.manifest().name != name {
        return Err(AutomationError::new(
            "CAPABILITY.MISSING",
            format!(
                "plugin {name:?} advertised itself as {:?}",
                provider.manifest().name
            ),
        )
        .with_category("capability")
        .with_effect("not_applied"));
    }
    registry.insert(Arc::new(provider));
    Ok(())
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("providers", &self.names())
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! In-process providers used by the engine's own tests.

    use super::*;
    use aad_plugin::manifest;
    use serde_json::{json, Map};
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Handler = Box<dyn Fn(&str, Value) -> Result<Value, AutomationError> + Send + Sync>;

    /// A provider whose behaviour is supplied by a closure.
    pub struct FakeProvider {
        manifest: CapabilityManifest,
        handler: Handler,
        pub calls: AtomicUsize,
    }

    impl FakeProvider {
        /// Build a provider exposing `actions` as `name@1` read-only actions.
        pub fn new(name: &str, actions: &[&str], handler: Handler) -> Arc<Self> {
            let mut declared = Map::new();
            for action in actions {
                declared.insert(
                    (*action).to_string(),
                    json!({"contract_major": 1, "effect": {"default_class": "read_only"}}),
                );
            }
            let document = manifest::document(name, declared);
            Arc::new(Self {
                manifest: manifest::parse(&document).expect("fixture manifest is valid"),
                handler,
                calls: AtomicUsize::new(0),
            })
        }

        /// A provider that echoes its arguments back.
        pub fn echo(name: &str, actions: &[&str]) -> Arc<Self> {
            Self::new(
                name,
                actions,
                Box::new(|_action, args| Ok(json!({"echo": args}))),
            )
        }

        pub fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Provider for FakeProvider {
        fn manifest(&self) -> &CapabilityManifest {
            &self.manifest
        }

        fn invoke(
            &self,
            action: &str,
            args: Value,
            _timeout: Option<Duration>,
        ) -> Result<Value, AutomationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.handler)(action, args)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeProvider;
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_finds_a_provider_by_its_dotted_capability_name() {
        let mut registry = ProviderRegistry::new();
        registry.insert(FakeProvider::echo("desktop.windows_uia", &["snapshot"]));

        let (_, contract) = registry
            .resolve("desktop.windows_uia.snapshot@1")
            .expect("the action should resolve");
        assert_eq!(contract.name, "snapshot");
        assert_eq!(contract.contract_major, 1);
    }

    #[test]
    fn resolve_rejects_an_unknown_action_or_major() {
        let mut registry = ProviderRegistry::new();
        registry.insert(FakeProvider::echo("fixture", &["ocr"]));

        assert!(registry.resolve("fixture.ocr@1").is_some());
        assert!(registry.resolve("fixture.ocr@2").is_none());
        assert!(registry.resolve("fixture.missing@1").is_none());
        assert!(registry.resolve("other.ocr@1").is_none());
    }

    #[test]
    fn a_provider_invocation_reaches_its_handler() {
        let provider = FakeProvider::echo("fixture", &["ocr"]);
        let mut registry = ProviderRegistry::new();
        registry.insert(provider.clone());

        let (found, _) = registry.resolve("fixture.ocr@1").unwrap();
        let result = found
            .invoke("fixture.ocr@1", json!({"x": 1}), None)
            .expect("invocation should succeed");

        assert_eq!(result, json!({"echo": {"x": 1}}));
        assert_eq!(provider.call_count(), 1);
    }
}

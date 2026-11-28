//! Adapters over the existing native providers; no second OS implementation.
use crate::{
    error, Action, Backend, BackendResource, Expect, Locator, Resource, Result, View, ViewMode,
    ACTION_NAMES,
};
use aad_plugin::CapabilityManifest;
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

/// Workflows share the provider's snapshot store, but cannot bypass the
/// session's server-owned secret bindings with an arbitrary `with.text`.
struct WorkflowProvider {
    inner: Arc<dyn Provider>,
    manifest: CapabilityManifest,
}

/// Keep private, server-resolved secret input out of arbitrary workflow calls.
pub fn workflow_provider(inner: Arc<dyn Provider>) -> Arc<dyn Provider> {
    Arc::new(WorkflowProvider::new(inner))
}

impl WorkflowProvider {
    fn new(inner: Arc<dyn Provider>) -> Self {
        let mut manifest = inner.manifest().clone();
        manifest.actions.remove("secret_input");
        if let Some(actions) = manifest.raw["actions"].as_object_mut() {
            actions.remove("secret_input");
        }
        Self { inner, manifest }
    }

    fn refuse_secret(action: &str) -> Result<()> {
        let name = action
            .split('@')
            .next()
            .unwrap_or(action)
            .rsplit('.')
            .next();
        if name == Some("secret_input") {
            return Err(error(
                "CONTROL.SECRET_WORKFLOW_DENIED",
                "secret input cannot run through a workflow",
            ));
        }
        Ok(())
    }
}

impl Provider for WorkflowProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    fn invoke(&self, action: &str, args: Value, timeout: Option<Duration>) -> Result<Value> {
        Self::refuse_secret(action)?;
        self.inner.invoke(action, args, timeout)
    }
    fn invoke_cancellable(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
        cancellation: &aad_runtime::CancellationToken,
    ) -> Result<Value> {
        Self::refuse_secret(action)?;
        self.inner
            .invoke_cancellable(action, args, timeout, cancellation)
    }
    fn invoke_with_artifacts(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
        artifacts: &aad_runtime::ArtifactStore,
    ) -> Result<Value> {
        Self::refuse_secret(action)?;
        self.inner
            .invoke_with_artifacts(action, args, timeout, artifacts)
    }
    fn close(&self) {
        self.inner.close();
    }
}

impl NativeBackend {
    pub fn new(provider: Arc<dyn Provider>, flavor: Flavor) -> Self {
        Self { provider, flavor }
    }
    /// A workflow view sharing the native instance and snapshot store, with
    /// ephemeral secret input removed from discovery and dispatch.
    pub fn provider(&self) -> Arc<dyn Provider> {
        workflow_provider(self.provider.clone())
    }
    fn invoke(&self, action: &str, args: Value, timeout: Duration) -> Result<Value> {
        self.provider
            .invoke(&format!("{}.{action}@1", self.name()), args, Some(timeout))
    }
    fn invoke_controlled(
        &self,
        action: &str,
        args: Value,
        context: &crate::Context,
    ) -> Result<Value> {
        self.provider.invoke_cancellable(
            &format!("{}.{action}@1", self.name()),
            args,
            Some(context.commit()?),
            context.cancellation(),
        )
    }
    /// The native operation behind a typed capability name.
    fn native_name(&self, capability: &str) -> &'static str {
        match capability {
            "focus" => "focus",
            "invoke" => "invoke",
            "click" => "pointer_click",
            "fill" if matches!(self.flavor, Flavor::Linux) => "set_text",
            "fill" => "set_value",
            "type_text" => "type_text",
            "secret_input" => "secret_input",
            "key" => "key_chord",
            "scroll" | "hover" => "pointer_gesture",
            "drag" => "drag",
            "minimize" => "minimize",
            "maximize" => "maximize",
            "restore" => "restore",
            "close_window" => "close_window",
            "activate" => "activate",
            "paste" => "paste",
            _ => "",
        }
    }
    /// A click with a button, a count or a position is a gesture natively.
    fn native_action(&self, action: &Action) -> &'static str {
        match action {
            Action::Click { .. } if !action.plain_click() => "pointer_gesture",
            other => self.native_name(other.name()),
        }
    }
}

pub fn resource_id(provider: &str, selector: &Value) -> String {
    format!(
        "{provider}:{:x}",
        Sha256::digest(serde_json::to_vec(selector).unwrap())
    )
}

fn fraction(value: &Option<crate::Fraction>) -> Value {
    value.map_or(Value::Null, |f| json!({"fx": f.fx, "fy": f.fy}))
}

impl NativeBackend {
    /// Whether the driver publishes this native operation.
    fn offers(&self, native: &str) -> bool {
        self.provider.manifest().actions.contains_key(native)
    }
    fn target_of(snapshot: &Value, node_id: &Value) -> Value {
        json!({"snapshot_id": snapshot["snapshot_id"], "revision": snapshot["revision"], "node_id": node_id})
    }
    /// AT-SPI and AX re-resolve every target from a descriptive locator
    /// before acting or capturing; UI Automation does not need one.
    fn portable(&self) -> bool {
        !matches!(self.flavor, Flavor::Windows)
    }
    fn locator_of(&self, node: &Value) -> Value {
        let mut locator = Map::new();
        locator.insert("role".into(), node["role"].clone());
        for field in ["name", "description"] {
            if node[field].as_str().is_some_and(|s| !s.is_empty()) {
                locator.insert(field.into(), node[field].clone());
            }
        }
        if matches!(self.flavor, Flavor::Linux) {
            for field in ["bus_name", "object_path"] {
                if let Some(value) = node["provenance"][field].as_str() {
                    locator.insert(field.into(), json!(value));
                }
            }
            if node["attributes"]
                .as_object()
                .is_some_and(|v| !v.is_empty())
            {
                locator.insert("attributes".into(), node["attributes"].clone());
            }
        } else if !node["subrole"].is_null() {
            locator.insert("subrole".into(), node["subrole"].clone());
        }
        Value::Object(locator)
    }
    /// The target (and, for AT-SPI/AX, locator) of a read on one node.
    fn read_args(&self, snapshot: &Value, node: &Value) -> Value {
        let mut args = json!({"target": Self::target_of(snapshot, &node["node_id"])});
        if self.portable() {
            args["locator"] = self.locator_of(node);
        }
        args
    }
}

pub use crate::pixel_text::tesseract_language;

impl Backend for NativeBackend {
    fn read_grid(
        &self,
        snapshot: &Value,
        node: &Value,
        columns: &[String],
        budget: usize,
        context: &crate::Context,
    ) -> Result<Value> {
        if !matches!(self.flavor, Flavor::Linux) || !self.offers("read_table") {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "native table reading is unavailable",
            ));
        }
        let mut args = self.read_args(snapshot, node);
        args["columns"] = json!(columns);
        args["max_characters"] = json!(budget);
        self.provider.invoke_cancellable(
            &format!("{}.read_table@1", self.name()),
            args,
            Some(context.remaining()?),
            context.cancellation(),
        )
    }
    fn reads(&self) -> Vec<String> {
        let mut reads = Vec::new();
        if self.offers("read_table") {
            reads.push("read_table".into());
        }
        if self.offers("capture") {
            reads.push("capture".to_string());
        }
        // Windows reads text in its own driver; elsewhere Tesseract reads the
        // capture, so text is offered only where both exist on this host.
        let read_text = match self.flavor {
            Flavor::Windows => self.offers("read_text"),
            _ => self.offers("capture") && crate::pixel_text::tesseract().is_ok(),
        };
        if read_text {
            reads.push("read_text".to_string());
        }
        reads
    }
    fn capture(&self, snapshot: &Value, node: &Value, timeout: Duration) -> Result<Value> {
        if !self.offers("capture") {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "this provider cannot take screenshots",
            ));
        }
        let shot = self.invoke("capture", self.read_args(snapshot, node), timeout)?;
        let mut answer = Map::new();
        for key in ["format", "data", "width", "height", "clipped"] {
            answer.insert(key.into(), shot[key].clone());
        }
        Ok(Value::Object(answer))
    }
    fn offers_global_keys(&self) -> bool {
        self.offers("global_key")
    }
    fn global_key(&self, keys: &[String], context: &crate::Context) -> Result<Value> {
        if !self.offers_global_keys() {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "this provider cannot press global keys",
            ));
        }
        self.invoke_controlled("global_key", json!({"keys": keys}), context)
    }
    fn clipboard_read(&self, max_characters: Option<usize>, timeout: Duration) -> Result<Value> {
        if !self.offers("clipboard_read") {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "this provider cannot read the clipboard",
            ));
        }
        let mut args = json!({});
        if let Some(max) = max_characters {
            args["max_characters"] = json!(max);
        }
        self.invoke("clipboard_read", args, timeout)
    }
    fn read_text(
        &self,
        snapshot: &Value,
        node: &Value,
        language: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        if !matches!(self.flavor, Flavor::Windows) {
            if !self.offers("capture") {
                return Err(error(
                    "CONTROL.ACTION_UNSUPPORTED",
                    "this provider cannot read text from pixels",
                ));
            }
            return crate::pixel_text::read_capture(
                language,
                timeout,
                aad_ocr::TextLayout::Auto,
                |left| self.capture(snapshot, node, left),
            );
        }
        if !self.offers("read_text") {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "this provider cannot read text from pixels",
            ));
        }
        let mut args = json!({"target": Self::target_of(snapshot, &node["node_id"])});
        if let Some(language) = language {
            args["language"] = json!(language);
        }
        self.invoke("read_text", args, timeout)
    }
    fn node_capabilities(&self, node: &Value) -> Vec<String> {
        ACTION_NAMES
            .iter()
            .copied()
            .filter(|name| {
                if let Some(modes) = node["provenance"]["wayland_pointer_modes"].as_array() {
                    let mode = match *name {
                        "scroll" => Some("wheel"),
                        "hover" => Some("hover"),
                        _ => None,
                    };
                    if mode.is_some_and(|mode| !modes.iter().any(|m| m == mode)) {
                        return false;
                    }
                }
                node["actions"]
                    .as_array()
                    .is_some_and(|native| native.iter().any(|v| v == self.native_name(name)))
            })
            .map(|name| name.to_string())
            .collect()
    }
    fn check_action_support(
        &self,
        snapshot: &Value,
        action: &Action,
        cross_resource: bool,
    ) -> Result<()> {
        let helper = &snapshot["metadata"]["input_helper"];
        if action.has_click_timing() {
            #[cfg(target_os = "linux")]
            if matches!(self.flavor, Flavor::Linux) {
                return aad_linux::atspi::check_x11_timed_click_support(helper);
            }
            return Err(crate::click_timing_unsupported());
        }
        if !matches!(self.flavor, Flavor::Linux) {
            return Ok(());
        }
        let requires_geometry = match action {
            Action::TypeText { .. }
            | Action::Key { .. }
            | Action::Paste { .. }
            | Action::Activate {}
            | Action::Minimize {}
            | Action::Maximize {}
            | Action::Restore {}
            | Action::CloseWindow {} => true,
            Action::Click { modifiers, .. } => !modifiers.is_empty(),
            Action::Drag { .. } => true,
            _ => false,
        };
        let uses_x11_helper = requires_geometry
            || matches!(
                action,
                Action::Click { .. }
                    | Action::Drag { .. }
                    | Action::Scroll { .. }
                    | Action::Hover { .. }
            );
        if uses_x11_helper
            && helper["ready"] == false
            && helper["code"] == "DRIVER.HELPER_INCOMPATIBLE"
        {
            return Err(error(
                "DRIVER.HELPER_INCOMPATIBLE",
                helper["message"]
                    .as_str()
                    .unwrap_or("Linux XTest helper handshake is incompatible"),
            ));
        }
        if helper["ready"] != true {
            return Ok(());
        }
        let straight_drag =
            matches!(action, Action::Drag { path, .. } if path.is_empty() && !cross_resource);
        let features = helper["features"].as_array();
        let has_feature =
            |name: &str| features.is_some_and(|list| list.iter().any(|feature| feature == name));
        #[cfg(target_os = "linux")]
        match action {
            Action::TypeText { mode, .. } => {
                let command = if *mode == Some(crate::model::TypeMode::Keys) {
                    "type-text-keys"
                } else {
                    "type-text"
                };
                return aad_linux::atspi::require_x11_keyboard_features(command, has_feature);
            }
            Action::Key { .. } => {
                return aad_linux::atspi::require_x11_keyboard_features("key-chord", has_feature);
            }
            Action::Paste { .. } => {
                return aad_linux::atspi::require_x11_keyboard_features("paste", has_feature);
            }
            _ => {}
        }
        let missing = if requires_geometry && !has_feature("window-geometry-proof-v1") {
            "window-geometry-proof-v1"
        } else if straight_drag && !has_feature("anchored-straight-drag") {
            "anchored-straight-drag"
        } else {
            return Ok(());
        };
        let upgrade_hint = if straight_drag {
            ". Upgrade aad and its helper together."
        } else {
            ""
        };
        Err(error(
            "DRIVER.HELPER_INCOMPATIBLE",
            format!("Linux XTest helper is missing {missing}{upgrade_hint}"),
        ))
    }
    fn lease_key(&self, _snapshot: &Value) -> Option<String> {
        Some("desktop-input".into())
    }
    fn name(&self) -> &str {
        &self.provider.manifest().name
    }
    fn capabilities(&self) -> Vec<String> {
        ACTION_NAMES
            .iter()
            .filter(|name| {
                self.provider
                    .manifest()
                    .actions
                    .contains_key(self.native_name(name))
            })
            .map(|name| name.to_string())
            .collect()
    }
    fn supports_cross_resource_drag(&self) -> bool {
        self.offers("drag_between")
    }
    fn retains_resource_observations(&self) -> bool {
        self.supports_cross_resource_drag()
            || self.provider.manifest().raw["retains_resource_observations"] == true
    }
    fn drag_between(
        &self,
        source: crate::DragEndpoint<'_>,
        destination: crate::DragEndpoint<'_>,
        action: &Action,
        context: &crate::Context,
    ) -> Result<Value> {
        if !self.supports_cross_resource_drag() {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "this native provider cannot drag between resources",
            ));
        }
        let Action::Drag {
            to,
            button,
            fraction: from_at,
            to_fraction,
            modifiers,
            hold_ms,
            path,
        } = action
        else {
            return Err(error("CONTROL.INVALID_REQUEST", "expected a drag action"));
        };
        if !path.is_empty() {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "a drag path must stay on one observed surface",
            ));
        }
        if destination.node["node_id"] != to.node_id {
            return Err(error(
                "CONTROL.STALE_TARGET",
                "drag destination does not match the observed endpoint",
            ));
        }
        for endpoint in [&source, &destination] {
            if !self
                .node_capabilities(endpoint.node)
                .iter()
                .any(|name| name == "drag")
            {
                return Err(error(
                    "CONTROL.ACTION_UNSUPPORTED",
                    "both observed endpoints must support native drag",
                ));
            }
        }
        let mut args = self.read_args(source.snapshot, source.node);
        args["to"] = Self::target_of(destination.snapshot, &destination.node["node_id"]);
        if self.portable() {
            args["to_locator"] = self.locator_of(destination.node);
        }
        for (key, value) in [
            ("button", json!(button)),
            ("fraction", fraction(from_at)),
            ("to_fraction", fraction(to_fraction)),
            ("hold_ms", json!(hold_ms)),
        ] {
            if !value.is_null() {
                args[key] = value;
            }
        }
        if !modifiers.is_empty() {
            args["modifiers"] = json!(modifiers);
        }
        self.invoke_controlled("drag_between", args, context)
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
    fn observe_scope(
        &self,
        selector: &Value,
        scope: &str,
        max_nodes: usize,
        timeout: Duration,
    ) -> Result<Value> {
        if !self.offers("snapshot_scope") {
            return Err(error(
                "CONTROL.SCOPE_UNSUPPORTED",
                "native provider has no scoped capture",
            ));
        }
        let mut args = selector.clone();
        args["scope_id"] = json!(scope);
        args["max_nodes"] = json!(max_nodes);
        self.invoke("snapshot_scope", args, timeout)
    }
    fn observe_target_scope(
        &self,
        snapshot: &Value,
        node: &Value,
        max_nodes: usize,
        timeout: Duration,
    ) -> Result<Value> {
        if !self.offers("snapshot_scope") {
            return Err(error(
                "CONTROL.SCOPE_UNSUPPORTED",
                "native provider has no scoped capture",
            ));
        }
        let mut args = self.read_args(snapshot, node);
        args["max_nodes"] = json!(max_nodes);
        self.invoke("snapshot_scope", args, timeout)
    }
    fn view(&self, snapshot: &Value, view: &View, timeout: Duration) -> Result<Option<Value>> {
        // Only the Windows driver computes regions; the others use the portable outline.
        if !matches!(self.flavor, Flavor::Windows) {
            return Ok(None);
        }
        let mut args = json!({"snapshot_id": snapshot["snapshot_id"]});
        match view.mode {
            ViewMode::Nodes | ViewMode::Text => Ok(None),
            ViewMode::Overview => {
                let overview = self.invoke("overview", args, timeout)?;
                Ok(Some(json!({"regions": overview["regions"]})))
            }
            ViewMode::Outline => {
                if let Some(region) = &view.region {
                    args["region"] = json!(region);
                }
                if let Some(limit) = view.limit {
                    args["limit"] = json!(limit);
                }
                if let Some(chars) = view.max_characters {
                    args["max_characters"] = json!(chars);
                }
                let outline = self.invoke("describe", args, timeout)?;
                // The driver's compact `ref` addresses its own snapshot store and is
                // not a session target, so it does not leave this adapter.
                let nodes = outline["elements"]
                    .as_array()
                    .map(|elements| {
                        elements
                            .iter()
                            .map(|element| {
                                let mut element = element.clone();
                                if let Some(object) = element.as_object_mut() {
                                    object.remove("ref");
                                }
                                element
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let mut answer = json!({"nodes": nodes});
                for key in ["shown", "matched", "stopped_by", "region", "characters"] {
                    if let Some(value) = outline.get(key) {
                        answer[key] = value.clone();
                    }
                }
                answer["outline_truncated"] = outline["truncated"].clone();
                Ok(Some(answer))
            }
        }
    }
    fn find(
        &self,
        snapshot: &Value,
        locator: &Locator,
        expect: Expect,
        timeout: Duration,
    ) -> Result<Option<Value>> {
        // node_id is a session-level field; the driver locator does not know it.
        if locator.node_id.is_some() {
            return Ok(None);
        }
        if matches!(self.flavor, Flavor::Linux) && !locator.extended() {
            let found = self.invoke(
                "find",
                json!({
                    "snapshot_id": snapshot["snapshot_id"],
                    "revision": snapshot["revision"],
                    "locator": serde_json::to_value(locator).unwrap(),
                }),
                timeout,
            );
            return match found {
                Ok(found) => Ok(Some(
                    json!({"found": true, "node_id": found["target"]["node_id"], "match_count": 1}),
                )),
                Err(err) if expect == Expect::Optional && err.code == "DRIVER.NOT_FOUND" => {
                    Ok(Some(json!({"found": false})))
                }
                Err(err) => Err(err),
            };
        }
        if !matches!(self.flavor, Flavor::Windows) {
            return Ok(None);
        }
        let mut args = json!({
            "snapshot_id": snapshot["snapshot_id"],
            "locator": serde_json::to_value(locator).unwrap(),
        });
        if expect == Expect::Optional {
            args["expect"] = json!("optional");
        }
        let found = self.invoke("find", args, timeout)?;
        if found["found"] != true {
            return Ok(Some(json!({"found": false})));
        }
        Ok(Some(json!({
            "found": true,
            "node_id": found["target"]["node_id"],
            "match_count": found["match_count"],
        })))
    }
    fn act(
        &self,
        snapshot: &Value,
        node: &Value,
        action: &Action,
        context: &crate::Context,
    ) -> Result<Value> {
        if matches!(action, Action::SecretInput { .. }) {
            return Err(error(
                "POLICY.DENIED",
                "secret input requires a server-resolved binding",
            ));
        }
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
        let target_of = |node_id: &Value| Self::target_of(snapshot, node_id);
        let mut args = json!({"target": target_of(&node["node_id"])});
        // AT-SPI and AX re-resolve every target from a descriptive locator
        // before acting, so each end of an action carries one.
        let portable = self.portable();
        let locator_of = |node: &Value| self.locator_of(node);
        if portable {
            args["locator"] = locator_of(node);
        }
        let mut set = |key: &str, value: Value| {
            if !value.is_null() {
                args[key] = value;
            }
        };
        match action {
            Action::Fill { value } => set(
                if matches!(self.flavor, Flavor::Linux) {
                    "text"
                } else {
                    "value"
                },
                json!(value),
            ),
            Action::TypeText { text, mode } => {
                set("text", json!(text));
                // UIA and AT-SPI explicitly distinguish Unicode insertion from
                // layout keys; AX has no layout-aware key implementation yet.
                match (self.flavor, mode) {
                    (Flavor::Windows | Flavor::Linux, _) => {
                        set("mode", serde_json::to_value(mode).unwrap())
                    }
                    (_, Some(crate::model::TypeMode::Keys)) => {
                        return Err(error(
                            "CONTROL.ACTION_UNSUPPORTED",
                            "layout-aware key typing is unavailable on this native backend",
                        ));
                    }
                    _ => {}
                }
            }
            Action::Key { keys, hold_ms } => {
                set("keys", json!(keys));
                set("hold_ms", json!(hold_ms));
            }
            Action::Paste { text } => set("text", json!(text)),
            Action::Click {
                button,
                count,
                fraction: at,
                modifiers,
                hold_ms,
                input_timing,
            } if native == "pointer_gesture" => {
                set("button", serde_json::to_value(button).unwrap());
                set("count", serde_json::to_value(count).unwrap());
                set("fraction", fraction(at));
                set("hold_ms", json!(hold_ms));
                set("input_timing", json!(input_timing));
                if !modifiers.is_empty() {
                    set("modifiers", json!(modifiers));
                }
            }
            Action::Scroll {
                direction,
                notches,
                fraction: at,
            } => {
                let mut scroll = json!({"direction": direction});
                if let Some(notches) = notches {
                    scroll["notches"] = json!(notches);
                }
                set("scroll", scroll);
                set("fraction", fraction(at));
            }
            Action::Hover { ms, fraction: at } => {
                set("hover_ms", json!(ms.unwrap_or(500)));
                set("fraction", fraction(at));
            }
            Action::Drag {
                to,
                button,
                fraction: from_at,
                to_fraction,
                modifiers,
                hold_ms,
                path,
            } => {
                set("to", target_of(&json!(to.node_id)));
                if portable {
                    let destination = snapshot["nodes"]
                        .as_array()
                        .and_then(|nodes| nodes.iter().find(|n| n["node_id"] == to.node_id))
                        .ok_or_else(|| {
                            error(
                                "CONTROL.STALE_TARGET",
                                "drag destination is not in the snapshot",
                            )
                        })?;
                    set("to_locator", locator_of(destination));
                }
                set("button", serde_json::to_value(button).unwrap());
                set("fraction", fraction(from_at));
                set("to_fraction", fraction(to_fraction));
                set("hold_ms", json!(hold_ms));
                if !path.is_empty() {
                    let anchors = path
                        .iter()
                        .map(|anchor| {
                            let mut value = json!({
                                "target": target_of(&json!(anchor.target.node_id)),
                                "fraction": anchor.fraction,
                            });
                            if portable {
                                // Control validation requires the same observed
                                // surface for every intermediate anchor.
                                value["locator"] = locator_of(node);
                            }
                            value
                        })
                        .collect::<Vec<_>>();
                    set("path", json!(anchors));
                }
                if !modifiers.is_empty() {
                    set("modifiers", json!(modifiers));
                }
            }
            _ => {}
        }
        let mut result = self.invoke_controlled(native, args, context)?;
        if action.has_click_timing() {
            return Ok(result["backend_result"].take());
        }
        Ok(result)
    }

    fn secret_input(
        &self,
        snapshot: &Value,
        node: &Value,
        text: &str,
        context: &crate::Context,
    ) -> Result<Value> {
        if node["states"]["protected"] != true
            || !node["actions"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|action| action == "secret_input"))
        {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "the observed element does not support secret input",
            ));
        }
        let mut args = self.read_args(snapshot, node);
        args["text"] = json!(text);
        self.invoke_controlled("secret_input", args, context)
    }
}

pub fn backends() -> (Vec<Arc<NativeBackend>>, BTreeMap<String, Value>) {
    let mut backends = Vec::new();
    let mut failures = BTreeMap::new();
    #[cfg(windows)]
    let native: Result<NativeBackend> = aad_windows::uia::native_driver_with_store(
        aad_desktop::SnapshotStore::new(32, Duration::from_secs(300)),
    )
    .map(|driver| NativeBackend::new(Arc::new(driver), Flavor::Windows))
    .map_err(|e| e.into_automation_error());
    #[cfg(target_os = "linux")]
    let native: Result<NativeBackend> = aad_linux::atspi::native_provider()
        .map(|provider| NativeBackend::new(Arc::new(provider), Flavor::Linux));
    #[cfg(target_os = "macos")]
    let native: Result<NativeBackend> = aad_macos::ax::native_provider()
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

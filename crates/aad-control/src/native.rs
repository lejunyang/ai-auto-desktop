//! Adapters over the existing native providers; no second OS implementation.
use crate::{
    error, Action, Backend, BackendResource, Expect, Locator, Resource, Result, View, ViewMode,
    ACTION_NAMES,
};
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
    /// The provider itself, for saved workflows that must drive the same
    /// native instance and snapshot store as this session.
    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }
    fn invoke(&self, action: &str, args: Value, timeout: Duration) -> Result<Value> {
        self.provider
            .invoke(&format!("{}.{action}@1", self.name()), args, Some(timeout))
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
    /// OCR off Windows: Tesseract over the capture's pixels, through the
    /// resource-limited `aad-ocr` engine runner.
    fn read_text_with_tesseract(
        &self,
        snapshot: &Value,
        node: &Value,
        language: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        use base64::Engine as _;
        let until = std::time::Instant::now() + timeout;
        let left = || {
            until
                .checked_duration_since(std::time::Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| error("CONTROL.TIMEOUT", "request deadline elapsed"))
        };
        let engine = tesseract()?;
        let languages = match language {
            None => Vec::new(),
            Some(tag) => {
                let wanted = tesseract_language(tag);
                let available = engine.languages(left()?).map_err(ocr_unavailable)?;
                match wanted.filter(|code| available.iter().any(|have| have == code)) {
                    Some(code) => vec![code.to_string()],
                    None => {
                        return Err(error(
                            "DRIVER.OCR_UNAVAILABLE",
                            format!("no installed Tesseract language data reads {tag}"),
                        )
                        .with_detail("available_languages", json!(available)))
                    }
                }
            }
        };
        let shot = self.capture(snapshot, node, left()?)?;
        let png = shot["data"]
            .as_str()
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "capture returned no PNG"))?;
        let mut read = engine
            .recognize_png(&png, &languages, left()?)
            .map_err(ocr_unavailable)?;
        for key in ["format", "data", "width", "height", "clipped"] {
            read[key] = shot[key].clone();
        }
        Ok(read)
    }
}

fn tesseract() -> Result<aad_ocr::OcrProvider> {
    let engine = aad_ocr::OcrProvider::new().map_err(ocr_unavailable)?;
    engine.engine_ready().map_err(ocr_unavailable)?;
    Ok(engine)
}

/// An engine that is missing or cannot be sandboxed is "no OCR here", the
/// same answer Windows gives for a missing language pack.
fn ocr_unavailable(cause: aad_core::AutomationError) -> aad_core::AutomationError {
    match cause.code.as_str() {
        "OCR.ENGINE_UNAVAILABLE" | "OCR.ENGINE_ISOLATION_UNAVAILABLE" => error(
            "DRIVER.OCR_UNAVAILABLE",
            "Tesseract OCR is not available on this host",
        )
        .with_detail("reason", json!(cause.message))
        .with_detail("engine", json!("tesseract")),
        "OCR.TIMEOUT" => error("CONTROL.TIMEOUT", cause.message.clone()).with_cause(cause),
        _ => error("DRIVER.OCR_FAILED", "Tesseract could not read the image").with_cause(cause),
    }
}

/// A BCP-47 tag to the Tesseract traineddata that reads it.
pub fn tesseract_language(tag: &str) -> Option<&'static str> {
    let lower = tag.to_ascii_lowercase();
    let primary = lower.split('-').next().unwrap_or_default();
    Some(match primary {
        "en" => "eng",
        "de" => "deu",
        "fr" => "fra",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "nl" => "nld",
        "ru" => "rus",
        "uk" => "ukr",
        "pl" => "pol",
        "tr" => "tur",
        "ja" => "jpn",
        "ko" => "kor",
        "ar" => "ara",
        "he" => "heb",
        "hi" => "hin",
        "zh" if lower.contains("hant") || lower.ends_with("-tw") || lower.ends_with("-hk") => {
            "chi_tra"
        }
        "zh" => "chi_sim",
        _ => return None,
    })
}

impl Backend for NativeBackend {
    fn reads(&self) -> Vec<String> {
        let mut reads = Vec::new();
        if self.offers("capture") {
            reads.push("capture".to_string());
        }
        // Windows reads text in its own driver; elsewhere Tesseract reads the
        // capture, so text is offered only where both exist on this host.
        let read_text = match self.flavor {
            Flavor::Windows => self.offers("read_text"),
            _ => self.offers("capture") && tesseract().is_ok(),
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
        self.invoke("global_key", json!({"keys": keys}), context.commit()?)
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
            return self.read_text_with_tesseract(snapshot, node, language, timeout);
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
            .filter(|name| {
                node["actions"]
                    .as_array()
                    .is_some_and(|native| native.iter().any(|v| v == self.native_name(name)))
            })
            .map(|name| name.to_string())
            .collect()
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
        if !matches!(self.flavor, Flavor::Windows) || locator.node_id.is_some() {
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
                // Only UI Automation offers a choice; AT-SPI and AX always
                // type Unicode and refuse a `mode` argument.
                match (self.flavor, mode) {
                    (Flavor::Windows, _) => set("mode", serde_json::to_value(mode).unwrap()),
                    (_, Some(crate::model::TypeMode::Keys)) => {
                        return Err(error(
                            "CONTROL.ACTION_UNSUPPORTED",
                            "layout-aware key typing is available on Windows only",
                        ))
                    }
                    _ => {}
                }
            }
            Action::Key { keys } => set("keys", json!(keys)),
            Action::Paste { text } => set("text", json!(text)),
            Action::Click {
                button,
                count,
                fraction: at,
            } if native == "pointer_gesture" => {
                set("button", serde_json::to_value(button).unwrap());
                set("count", serde_json::to_value(count).unwrap());
                set("fraction", fraction(at));
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
            }
            _ => {}
        }
        self.invoke(native, args, context.commit()?)
    }
}

pub fn backends() -> (Vec<Arc<NativeBackend>>, BTreeMap<String, Value>) {
    let mut backends = Vec::new();
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

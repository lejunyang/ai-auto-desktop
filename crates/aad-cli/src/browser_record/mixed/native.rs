use super::{
    timeline::{Source, Timeline},
    Result,
};
use aad_control::Session;
use aad_runtime::Provider;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const PROVIDER: &str = "desktop.linux_atspi";
mod workflow;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Window {
    role: String,
    name: String,
}

// Watch identifies nodes by bus and object path. Resolve these in a current
// provider observation before retaining any text. A disappeared node is a gap.
struct Tree<'a> {
    nodes: HashMap<&'a str, (&'a Value, Window)>,
    windows: HashMap<Window, Vec<&'a str>>,
}

impl<'a> Tree<'a> {
    fn read(snapshot: &'a Value, bus: &str) -> Result<Self> {
        let mut parents = HashMap::new();
        let mut tree = Self {
            nodes: HashMap::new(),
            windows: HashMap::new(),
        };
        for node in snapshot["nodes"]
            .as_array()
            .ok_or("invalid AT-SPI snapshot")?
        {
            let role = node["role"].as_str().unwrap_or("");
            let parent = node["parent_id"].as_str().and_then(|id| parents.get(id));
            let window = match role {
                role if node["provenance"]["window"]["x11_window"].is_u64()
                    || matches!(
                        role,
                        "frame" | "window" | "dialog" | "file_chooser" | "alert"
                    ) =>
                {
                    let window = Window {
                        role: role.into(),
                        name: node["name"].as_str().unwrap_or("").into(),
                    };
                    tree.windows.entry(window.clone()).or_default().push(
                        node["provenance"]["object_path"]
                            .as_str()
                            .ok_or("window has no AT-SPI identity")?,
                    );
                    window
                }
                _ => match parent {
                    Some(window) => Window::clone(window),
                    None => continue,
                },
            };
            if node["provenance"]["bus_name"] == bus {
                if let Some(path) = node["provenance"]["object_path"].as_str() {
                    tree.nodes.insert(path, (node, window.clone()));
                }
            }
            parents.insert(
                node["node_id"].as_str().ok_or("snapshot node without id")?,
                window,
            );
        }
        Ok(tree)
    }
}

pub struct Watch {
    provider: Arc<dyn Provider>,
    capture: Option<String>,
    pub app_name: String,
    bus: String,
    main: Window,
    main_path: String,
    dialogs: Vec<String>,
}

impl Watch {
    pub fn start(
        session: &Session,
        resource: &Value,
        title: &str,
        dialogs: &[String],
    ) -> Result<Self> {
        let meta = &resource["metadata"];
        if title.trim().is_empty()
            || dialogs
                .iter()
                .any(|name| name.trim().is_empty() || name == title)
        {
            return Err("give a nonempty window name and distinct dialog names".into());
        }
        let provider = session
            .providers()
            .iter()
            .find(|p| {
                p.manifest().name == PROVIDER
                    && [
                        "watch",
                        "collect",
                        "release",
                        "snapshot",
                        "snapshot_scope",
                        "find",
                    ]
                    .iter()
                    .all(|a| p.manifest().actions.contains_key(*a))
            })
            .ok_or("Linux AT-SPI Watch is unavailable")?
            .clone();
        let mut watch = Self {
            provider,
            capture: None,
            app_name: meta["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("native application has no replayable name")?
                .into(),
            bus: meta["bus_name"]
                .as_str()
                .ok_or("missing AT-SPI bus identity")?
                .into(),
            main: Window {
                role: String::new(),
                name: title.into(),
            },
            main_path: String::new(),
            dialogs: dialogs.to_vec(),
        };
        let snapshot = watch.snapshot()?;
        if snapshot["session"]["session_type"] != "x11" {
            return Err("Linux mixed Watch requires a native X11 session".into());
        }
        let tree = Tree::read(&snapshot, &watch.bus)?;
        let mut matching = tree
            .windows
            .iter()
            .filter(|(window, _)| window.name == title);
        match (matching.next(), matching.next()) {
            (Some((window, paths)), None) if snapshot["truncated"] == false && paths.len() == 1 => {
                watch.main = window.clone();
                watch.main_path = paths[0].to_string();
            }
            _ => {
                return Err(format!(
                    "window {:?} must match exactly once in a complete observation",
                    title
                )
                .into())
            }
        }
        let raw = watch.call("watch", json!({"application":{"bus_name":watch.bus}}))?;
        watch.capture = Some(
            raw["capture_id"]
                .as_str()
                .ok_or("Watch returned no capture id")?
                .into(),
        );
        Ok(watch)
    }

    fn call(&self, action: &str, args: Value) -> Result<Value> {
        Ok(self.provider.invoke(
            &format!("{PROVIDER}.{action}@1"),
            args,
            Some(Duration::from_secs(10)),
        )?)
    }

    fn snapshot(&self) -> Result<Value> {
        self.call(
            "snapshot",
            json!({"application":{"bus_name":self.bus},"max_nodes":2000}),
        )
    }

    pub fn contract(&self, action: &str) -> &Value {
        &self.provider.manifest().raw["actions"][action]
    }

    pub fn collect(&self, timeline: &mut Timeline, offset: u64, index: usize) -> Result<()> {
        let raw = self.call("collect", json!({"capture_id":self.capture}))?;
        let events = raw["events"]
            .as_array()
            .ok_or("Watch did not return events")?;
        if raw["dropped"].as_u64().ok_or("Watch omitted drop count")? != 0 {
            timeline.gap(
                super::wall_ms(),
                json!({"source":"native","reason":"watch_overflow","dropped":raw["dropped"]}),
            )?;
        }
        if events.is_empty() {
            return Ok(());
        }
        let snapshot = self.snapshot()?;
        let tree = Tree::read(&snapshot, &self.bus)?;
        for event in events {
            let at = event["at_ms"]
                .as_u64()
                .and_then(|ms| ms.checked_add(offset))
                .ok_or("missing native timestamp")?;
            let node = &event["node"];
            let identity = &node["capture_identity"];
            let candidate = identity[1].as_str().and_then(|path| tree.nodes.get(path));
            let Some((observed, window)) = candidate.filter(|(seen, _)| {
                identity[0] == self.bus
                    && seen["role"] == node["role"]
                    && seen["name"].as_str().unwrap_or("") == node["name"].as_str().unwrap_or("")
            }) else {
                timeline.gap(at, json!({"source":"native","reason":"unresolved_watch_target","kind":event["kind"]}))?;
                continue;
            };
            let dialog = matches!(window.role.as_str(), "dialog" | "file_chooser" | "alert")
                && self.dialogs.contains(&window.name);
            if &self.main != window && !dialog {
                timeline.filter(at, "other_native_window")?;
                continue;
            }
            let paths = &tree.windows[window];
            if snapshot["truncated"] != false
                || paths.len() != 1
                || (&self.main == window && paths[0] != self.main_path)
            {
                timeline.gap(
                    at,
                    json!({"source":"native","reason":"ambiguous_or_truncated_window"}),
                )?;
                continue;
            }
            if event["kind"] == "text"
                && node["protected"] != true
                && node["value"]
                    .as_str()
                    .is_none_or(|s| s.chars().count() >= 4096)
            {
                timeline.gap(
                    at,
                    json!({"source":"native","reason":"missing_or_possibly_truncated_value"}),
                )?;
                continue;
            }
            if event["kind"] == "key" && observed["states"]["focused"] != true {
                timeline.gap(
                    at,
                    json!({"source":"native","reason":"unconfirmed_key_focus"}),
                )?;
                continue;
            }
            let required = match event["kind"].as_str() {
                Some("text") => "set_text",
                Some("click") if event["button"] == "left" => "pointer_click",
                Some("click" | "wheel") => "pointer_gesture",
                Some("key") => "key_chord",
                Some("focus_boundary") => "",
                _ => "unsupported",
            };
            if !required.is_empty()
                && (node["protected"] == true
                    || !observed["actions"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|v| v == required)))
            {
                timeline.gap(at, json!({"source":"native","reason":"unsupported_native_action","kind":event["kind"]}))?;
                continue;
            }
            timeline.push(at, Source::Native(index, window.clone()), event.clone())?;
        }
        Ok(())
    }

    pub fn release(&mut self) -> Result<()> {
        if let Some(capture) = self.capture.take() {
            self.call("release", json!({"capture_id":capture}))?;
        }
        Ok(())
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

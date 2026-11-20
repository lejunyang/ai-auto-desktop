use super::*;
use aad_control::steps::{self, App};
use std::collections::BTreeMap;

struct Window {
    capture: Capture,
    id: String,
    app: App,
    root: Value,
}

pub(super) struct Windows {
    pub(super) precision_ms: u64,
    client: Client,
    process: String,
    pid: u64,
    main: String,
    main_id: String,
    dialogs: Vec<String>,
    active: BTreeMap<String, Window>,
    pending: Rows,
}

impl Windows {
    pub(super) fn start(
        client: Client,
        meta: &Value,
        main: &str,
        dialogs: &[String],
    ) -> Result<Self> {
        if meta["title"] != main {
            return Err("selected UIA resource and exact window title disagree".into());
        }
        let mut this = Self {
            client,
            precision_ms: 0,
            process: string(meta, "process_name")?.into(),
            pid: number(meta, "process_id")?,
            main: main.into(),
            main_id: string(meta, "window_id")?.into(),
            dialogs: dialogs.into(),
            active: BTreeMap::new(),
            pending: vec![],
        };
        let windows = this.list()?;
        let id = this
            .candidate(main, &windows)?
            .ok_or("selected UIA window disappeared")?;
        if id != this.main_id {
            return Err("selected UIA window changed".into());
        }
        let at = this.attach(main, id)?;
        for title in dialogs {
            if let Some(id) = this.candidate(title, &windows)? {
                this.attach(title, id)?;
            } else {
                this.pending.push((at, Event::Gap(format!("dialog {title:?} is not open; input before its subscription cannot be attributed"))));
            }
        }
        Ok(this)
    }

    fn list(&self) -> Result<Vec<Value>> {
        let raw = self.client.call("list_windows", json!({}))?;
        raw["windows"]
            .as_array()
            .cloned()
            .ok_or_else(|| "invalid UIA window list".into())
    }

    fn candidate<'a>(&self, title: &str, windows: &'a [Value]) -> Result<Option<&'a str>> {
        let mut exact = windows
            .iter()
            .filter(|w| w["process_id"] == self.pid && w["title"] == title);
        let Some(first) = exact.next() else {
            return Ok(None);
        };
        // Control steps select the process exactly and the title by substring.
        // Check the same uniqueness at record time; also scope the locator to
        // the exact observed root, for replay after window titles change.
        let title_lower = title.to_lowercase();
        let matches = windows
            .iter()
            .filter(|w| {
                w["process_name"]
                    .as_str()
                    .is_some_and(|p| p.eq_ignore_ascii_case(&self.process))
                    && w["title"]
                        .as_str()
                        .is_some_and(|t| t.to_lowercase().contains(&title_lower))
            })
            .count();
        if exact.next().is_some()
            || matches != 1
            || !first["process_name"]
                .as_str()
                .is_some_and(|p| p.eq_ignore_ascii_case(&self.process))
        {
            return Err(format!("UIA process and window {title:?} are not unique").into());
        }
        Ok(Some(string(first, "window_id")?))
    }

    fn attach(&mut self, title: &str, id: &str) -> Result<u64> {
        let (capture, started) = self
            .client
            .subscribe(json!({"window_id":id,"max_nodes":2000}))?;
        let meta = &started["window"];
        let root = &started["root_locator"];
        if meta["window_id"] != id
            || meta["process_id"] != self.pid
            || meta["title"] != title
            || meta["process_name"] != self.process
            || started["truncated"] != false
            || root["name"] != title
            || string(root, "role").is_err()
        {
            return Err(
                format!("UIA baseline could not verify the complete window {title:?}").into(),
            );
        }
        let at = number(&started, "started_epoch_ms")?;
        self.precision_ms = self.precision_ms.max(number(&started, "precision_ms")?);
        let sources = started["sources"]
            .as_array()
            .ok_or("missing UIA capture sources")?;
        if !["uia", "win_event"]
            .iter()
            .all(|s| sources.iter().any(|v| v == s))
        {
            self.pending.push((
                at,
                Event::Gap(format!("partial UIA hook coverage for {title:?}")),
            ));
        }
        let app = App {
            provider: Some(WINDOWS.into()),
            process: Some(self.process.clone()),
            title: Some(title.into()),
            ..App::default()
        };
        self.active.insert(
            title.into(),
            Window {
                capture,
                id: id.into(),
                app,
                root: root.clone(),
            },
        );
        Ok(at)
    }

    pub(super) fn collect(&mut self, next: &mut usize) -> Result<Rows> {
        let windows = self.list()?;
        let mut rows = std::mem::take(&mut self.pending);
        let mut closed = Vec::new();
        for (title, window) in &self.active {
            let live = windows.iter().find(|w| w["window_id"] == window.id);
            let changed = live.is_some_and(|w| {
                w["title"] != *title
                    || w["process_id"] != self.pid
                    || w["process_name"] != self.process
            });
            if changed || live.is_none() {
                closed.push(title.clone());
            }
            let ambiguous = self.candidate(title, &windows).is_err();
            let mut drained = 0;
            loop {
                let raw = self.client.call(
                    "collect",
                    json!({"capture_id":window.capture.id,
                                                            "individual_edits":true,"max":256}),
                )?;
                if raw["window_id"] != window.id
                    || raw["capture_id"].as_str() != window.capture.id.as_deref()
                {
                    return Err("UIA collect returned a different subscription".into());
                }
                let dropped = number(&raw, "dropped")?;
                if dropped > 0 {
                    rows.push((
                        number(&raw, "first_epoch_ms")?,
                        Event::Gap(format!("UIA dropped {dropped} events")),
                    ));
                }
                for captured in raw["steps"].as_array().ok_or("missing UIA steps")? {
                    let at = number(captured, "epoch_ms")?;
                    let event = if changed || ambiguous {
                        Event::Gap("UIA window identity changed or became ambiguous".into())
                    } else {
                        convert(&window.app, &window.root, captured, next)
                    };
                    rows.push((at, event));
                }
                let count = number(&raw, "raw_events")?;
                if count < 256 {
                    break;
                }
                drained += count;
                if drained >= 20_000 {
                    return Err("UIA capture exceeded the mixed event budget".into());
                }
            }
        }
        for title in closed {
            if let Some(mut window) = self.active.remove(&title) {
                window.capture.release()?;
            }
        }
        // UIA hooks are per HWND. Attach newly opened allowed dialogs only
        // after matching their owner against the original PID.
        for title in std::iter::once(self.main.clone()).chain(self.dialogs.clone()) {
            if self.active.contains_key(&title) {
                continue;
            }
            if let Some(id) = self.candidate(&title, &windows)? {
                if title == self.main && id != self.main_id {
                    return Err("selected UIA resource was replaced during capture".into());
                }
                let precision = self.precision_ms;
                let at = self.attach(&title, id)?;
                if self.precision_ms > precision {
                    rows.push((
                        at,
                        Event::Gap(format!(
                            "UIA clock uncertainty for {title:?} increased to {} ms",
                            self.precision_ms
                        )),
                    ));
                }
                rows.push((
                    at,
                    Event::Gap(format!(
                        "UIA subscribed after {title:?} opened; earlier input is unknown"
                    )),
                ));
            }
        }
        rows.append(&mut self.pending);
        Ok(rows)
    }

    pub(super) fn release(&mut self) -> Result<()> {
        for window in self.active.values_mut() {
            window.capture.release()?;
        }
        self.active.clear();
        Ok(())
    }
}

pub(super) fn convert(app: &App, root: &Value, raw: &Value, next: &mut usize) -> Event {
    if raw["protected"] == true || (raw["action"] == "set_value" && !raw["argument"].is_string()) {
        return Event::Gap("protected or unavailable UIA value".into());
    }
    let id = format!("native{next}");
    *next += 1;
    let Ok(mut step) = steps::from_windows_capture(&id, app, raw) else {
        return Event::Gap(
            raw["unresolved"]
                .as_str()
                .unwrap_or("unresolved UIA action")
                .into(),
        );
    };
    // The generic importer can fall back to role/name for an unsupported
    // locator. Mixed capture must preserve every constraint or report a gap.
    let Ok(locator) = serde_json::from_value(raw["locator"].clone()) else {
        return Event::Gap("unsupported UIA locator".into());
    };
    step.locator = locator;
    if step.locator.is_empty() {
        return Event::Gap("empty UIA locator".into());
    }
    let mut locator = raw["locator"].clone();
    let mut scope = &mut locator;
    while scope.get("within").is_some() {
        scope = &mut scope["within"];
    }
    if scope["role"] != root["role"] || scope["name"] != root["name"] {
        scope["within"] = root.clone();
    }
    step.locator = serde_json::from_value(locator).expect("window scope is JSON");
    match step.validate() {
        Ok(()) => Event::Step(Box::new(step)),
        Err(e) => Event::Gap(e.message),
    }
}

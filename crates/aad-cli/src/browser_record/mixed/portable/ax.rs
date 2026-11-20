use super::*;
use aad_control::{recording::from_events, steps::App};

pub(super) struct Ax {
    pub(super) capture: Capture,
    pub(super) precision_ms: u64,
    selector: Value,
    app: App,
    pid: u64,
    main: String,
    dialogs: Vec<String>,
    uptime_ms: u64,
    epoch_ms: u64,
    pending: Rows,
}

impl Ax {
    pub(super) fn start(
        client: Client,
        meta: &Value,
        main: &str,
        dialogs: &[String],
    ) -> Result<Self> {
        let pid = number(meta, "process_id")?;
        let field = if string(meta, "bundle_id").is_ok() {
            "bundle_id"
        } else {
            "name"
        };
        let name = string(meta, field)?;
        let selector = json!({field:name});
        // Resolve the durable replay selector and prove it still names this PID.
        let observed = client.call("snapshot", json!({"app":selector,"max_nodes":2000}))?;
        if observed["app"]["process_id"] != pid || observed["truncated"] != false {
            return Err("AX replay selector did not resolve to a complete observation of the selected process".into());
        }
        let nodes = observed["nodes"]
            .as_array()
            .ok_or("AX snapshot omitted nodes")?;
        if nodes
            .iter()
            .filter(|n| n["role"] == "AXWindow" && n["name"] == main)
            .count()
            != 1
        {
            return Err("expected one AXWindow with the exact main window title".into());
        }
        let (capture, start) = client.subscribe(json!({"app":{"process_id":pid}}))?;
        let clock = &start["clock"];
        let epoch_ms = number(clock, "epoch_ms")?;
        let mut pending = vec![];
        if start["pointer_watch"] != true {
            pending.push((
                epoch_ms,
                Event::Gap("AX pointer and key tap unavailable".into()),
            ));
        }
        for notification in start["missing_notifications"]
            .as_array()
            .ok_or("missing AX observer status")?
        {
            pending.push((
                epoch_ms,
                Event::Gap(format!("AX notification unavailable: {notification}")),
            ));
        }
        Ok(Self {
            capture,
            precision_ms: number(clock, "precision_ms")?,
            selector,
            app: App {
                provider: Some(MACOS.into()),
                process: Some(name.into()),
                ..Default::default()
            },
            pid,
            main: main.into(),
            dialogs: dialogs.into(),
            uptime_ms: number(clock, "at_ms")?,
            epoch_ms,
            pending,
        })
    }

    pub(super) fn collect(&mut self, next: &mut usize) -> Result<Rows> {
        let raw = self
            .capture
            .client
            .call("collect", json!({"capture_id":self.capture.id}))?;
        let events = raw["events"]
            .as_array()
            .ok_or("AX collect omitted events")?;
        let mut rows = std::mem::take(&mut self.pending);
        for event in events {
            let at = self.timestamp(number(event, "at_ms")?)?;
            rows.push((at, self.convert(event, next)));
        }
        let dropped = number(&raw, "dropped")?;
        if dropped > 0 {
            let first = events
                .iter()
                .map(|e| number(e, "at_ms"))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .min()
                .ok_or("AX overflow without retained timestamps")?;
            rows.push((
                self.timestamp(first)?,
                Event::Gap(format!("AX dropped {dropped} events before this timestamp")),
            ));
        }
        Ok(rows)
    }

    fn timestamp(&self, uptime_ms: u64) -> Result<u64> {
        (i128::from(self.epoch_ms) + i128::from(uptime_ms) - i128::from(self.uptime_ms))
            .try_into()
            .map_err(|_| "invalid AX capture clock".into())
    }

    fn convert(&self, raw: &Value, next: &mut usize) -> Event {
        let node = &raw["node"];
        let window = &node["window"];
        if raw["kind"] == "tap_interrupted" {
            return Event::Gap("AX event tap interrupted".into());
        }
        if node["process_id"] != self.pid
            || !matches!(window["role"].as_str(), Some("AXWindow" | "AXSheet"))
            || string(window, "name").is_err()
        {
            return Event::Gap("AX event lacks process or window attribution".into());
        }
        let title = window["name"].as_str().unwrap();
        if !(window["role"] == "AXWindow" && title == self.main)
            && !self.dialogs.iter().any(|d| d == title)
        {
            return Event::Filtered("other_native_window");
        }
        if window["unique"] != true {
            return Event::Gap("ambiguous AX window".into());
        }
        if raw["kind"] == "focus_boundary" {
            return Event::Filtered("native_focus_boundary");
        }
        if node["protected"] != false || string(node, "role").is_err() {
            return Event::Gap("protected or unattributed AX element".into());
        }
        match raw["kind"].as_str() {
            Some("text") if !node["value"].is_string() || node["value_truncated"] == true => {
                return Event::Gap("incomplete AX text value".into())
            }
            Some("text" | "click" | "key" | "wheel") => (),
            // AX reports the resulting checked state, but has no anchored
            // set-checked action. Invoking it could invert the wrong state.
            _ => return Event::Gap("unsupported AX recording event".into()),
        }
        let mut batch = from_events(&self.app, std::slice::from_ref(raw), next);
        match batch.steps.pop() {
            Some(mut step) => {
                step.locator.within = Some(json!({"role":window["role"],"name":title}));
                Event::Step(Box::new(step))
            }
            None => Event::Gap("unreplayable AX input".into()),
        }
    }

    pub(super) fn actions(&self, step: &Step) -> Result<Vec<Value>> {
        let window = step
            .locator
            .within
            .as_ref()
            .ok_or("missing recorded AX window")?;
        if step.app != self.app
            || (!self.dialogs.iter().any(|d| window["name"] == *d)
                && !(window["role"] == "AXWindow" && window["name"] == self.main))
        {
            return Err("AX step does not belong to this recording".into());
        }
        let action = step.action.as_ref().ok_or("missing AX action")?;
        let output =
            |part: &str, field: &str| format!("${{{{ steps.{}_{part}.output.{field} }}}}", step.id);
        let mut args = json!({"target":output("find","target"), "locator":step.locator});
        let method = match action["type"].as_str() {
            Some("fill") => {
                args["value"] = action["value"].clone();
                "set_value"
            }
            Some("key") => {
                args["keys"] = action["keys"].clone();
                "key_chord"
            }
            Some("scroll") => {
                args["scroll"] =
                    json!({"direction":action["direction"],"notches":action["notches"]});
                "pointer_gesture"
            }
            Some("click") if action.get("button").is_none() && action.get("count").is_none() => {
                "pointer_click"
            }
            Some("click") => {
                for key in ["button", "count"] {
                    if let Some(value) = action.get(key) {
                        args[key] = value.clone();
                    }
                }
                "pointer_gesture"
            }
            _ => return Err("unsupported AX replay action".into()),
        };
        let mut out = vec![];
        for (id, name, args) in [
            (
                format!("{}_snapshot", step.id),
                "snapshot",
                json!({"app":self.selector,"max_nodes":2000}),
            ),
            (
                format!("{}_find", step.id),
                "find",
                json!({"snapshot_id":output("snapshot","snapshot_id"),
                 "revision":output("snapshot","revision"),"locator":step.locator}),
            ),
            (step.id.clone(), method, args),
        ] {
            let contract = self.capture.client.0.manifest().raw["actions"]
                .get(name)
                .ok_or("missing AX replay contract")?;
            out.push(
                json!({"id":id,"type":"action","uses":format!("{MACOS}.{name}@1"),"with":args,
                "effect":{"class":contract["effect"]["default_class"]},"risk":contract["risk"]}),
            );
        }
        Ok(out)
    }
}

//! Qt Widgets input on the target's own unique AT-SPI bus connection.
use super::*;

const PATH: &str = "/org/aiautodesktop/QtWaylandInput1";
const INTERFACE: &str = "org.aiautodesktop.QtWaylandInput1";

pub(super) enum Event<'a> {
    Click((i32, i32)),
    Key(&'a [String]),
}

impl LinuxBackend {
    pub(super) fn qualify_wayland_widgets(
        &self,
        snapshot: &mut BackendSnapshot,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        // A scoped capture includes ancestors in parent-first order. The only
        // eligible window anchor is a direct child of this application's root.
        let mut windows: Vec<&str> = Vec::with_capacity(snapshot.nodes.len());
        let mut targets = Vec::with_capacity(snapshot.nodes.len());
        let bus = snapshot
            .application
            .get("bus_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        for node in &snapshot.nodes {
            let path = node.native.object_path.as_str();
            let window = match node.parent_index {
                Some(0) if node.native.bus_name == bus => path,
                Some(parent) if node.native.bus_name == bus => {
                    windows.get(parent).copied().unwrap_or("")
                }
                _ => "",
            };
            targets.push(path);
            windows.push(window);
        }
        // No name activation. Apps without the optional plugin simply omit
        // these capabilities and retain their normal semantic AT-SPI actions.
        remaining(deadline, false)?;
        self.check_cancelled(false)?;
        let flags = bus
            .starts_with(':')
            .then(|| {
                let proxy = Proxy::new(&self.connection, bus, PATH, INTERFACE).ok()?;
                proxy
                    .call::<_, _, Vec<u8>>("Capabilities", &(targets, &windows))
                    .ok()
            })
            .flatten()
            .filter(|flags| flags.len() == windows.len());
        remaining(deadline, false)?;
        self.check_cancelled(false)?;
        let windows: Vec<_> = windows.into_iter().map(str::to_owned).collect();
        for (i, (node, window)) in snapshot.nodes.iter_mut().zip(windows).enumerate() {
            node.provenance
                .insert("coordinate_space".into(), json!("unproven_toolkit"));
            let flag = flags.as_ref().map_or(0, |flags| flags[i]);
            let state = |name: &str| node.states.get(name).copied().flatten();
            if window.is_empty()
                || flag == 0
                || flag > 3
                || state("protected") != Some(false)
                || !["enabled", "sensitive", "visible", "showing"]
                    .iter()
                    .all(|s| state(s) == Some(true))
                || !node.bounds.is_some_and(|b| b.width > 0 && b.height > 0)
            {
                continue;
            }
            if flag & 1 != 0 && state("focused") == Some(true) && state("focusable") == Some(true) {
                node.actions.push("key_chord".into());
            }
            if flag & 2 != 0 {
                node.actions.push("pointer_click".into());
            }
            node.provenance
                .insert("coordinate_space".into(), json!("qt_wayland_logical"));
            node.provenance
                .insert("wayland_input_window".into(), json!(window));
        }
        Ok(())
    }

    pub(super) fn wayland_input(
        &self,
        target: &NativeRef,
        window: Option<&WindowAnchor>,
        process_id: u32,
        event: Event<'_>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let denied = || {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "Wayland widget input binding is unavailable",
            )
        };
        let window = window.ok_or_else(denied)?;
        if !target.bus_name.starts_with(':')
            || target.bus_name != window.native.bus_name
            || self.process_id(&target.bus_name) != Some(process_id)
        {
            return Err(denied());
        }
        let proxy = Proxy::new(&self.connection, target.bus_name.as_str(), PATH, INTERFACE)
            .map_err(|_| denied())?;
        let object = target.object_path.as_str();
        let top = window.native.object_path.as_str();
        // Bound queued work by the transport's own reply timeout, even if the
        // caller supplied a longer deadline and the GUI thread stalls.
        let until = monotonic_deadline_ns(deadline.min(Instant::now() + Duration::from_secs(2)))?;
        self.check_cancelled(false)?;
        let reply: zbus::Result<i32> = match event {
            Event::Click((x, y)) => proxy.call("Click", &(object, top, x, y, until)),
            Event::Key(keys) => proxy.call("Key", &(object, top, keys, until)),
        };
        match reply {
            Ok(0) => Err(denied()),
            Ok(2) => {
                self.check_cancelled(true)?;
                remaining(deadline, true)?;
                Ok(
                    json!({"native_interface":"QtWidgets.sendEvent", "synthetic_input":true,
                    "target_window":top, "dispatched":true, "events":2}),
                )
            }
            _ => Err(driver_error(
                "DRIVER.UNKNOWN_EFFECT",
                "Wayland widget event pair was not confirmed",
            )
            .with_effect("unknown")
            .with_detail("dispatch_started", json!(true))),
        }
    }
}

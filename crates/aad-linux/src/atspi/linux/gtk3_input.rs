//! Optional GTK 3 widget input on the application's own AT-SPI connection.
use super::*;

const PATH: &str = "/org/aiautodesktop/Gtk3WidgetInput1";
const INTERFACE: &str = "org.aiautodesktop.Gtk3WidgetInput1";

fn unavailable() -> AutomationError {
    driver_error(
        "DRIVER.ACTION_UNSUPPORTED",
        "GTK widget input binding is unavailable",
    )
}

impl LinuxBackend {
    pub(super) fn gtk3_application(&self, bus: &str) -> Result<bool, AutomationError> {
        if !bus.starts_with(':') {
            return Err(unavailable());
        }
        let builder = ApplicationProxy::builder(self.connection.inner())
            .destination(bus)
            .and_then(|b| b.path(REGISTRY_PATH))
            .map_err(|_| unavailable())?;
        let app = zbus::block_on(
            builder
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )
        .map_err(|_| unavailable())?;
        Ok(zbus::block_on(app.toolkit_name()).map_err(|error| {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                format!("Wayland application toolkit: {error}"),
            )
        })? == "gtk")
    }

    pub(super) fn qualify_gtk3_input(
        &self,
        snapshot: &mut BackendSnapshot,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        let Some(bus) = snapshot
            .application
            .get("bus_name")
            .and_then(Value::as_str)
            .filter(|name| name.starts_with(':'))
        else {
            return Ok(());
        };
        remaining(deadline, false)?;
        self.check_cancelled(false)?;
        let proxy =
            Proxy::new(&self.connection, bus, PATH, INTERFACE).map_err(|_| unavailable())?;
        let focus = proxy.call::<_, _, (String, String)>("FocusedEntry", &());
        remaining(deadline, false)?;
        self.check_cancelled(false)?;
        let Ok((entry, entry_window)) = focus else {
            return Ok(());
        };
        let mut windows: Vec<Option<usize>> = Vec::with_capacity(snapshot.nodes.len());
        for i in 0..snapshot.nodes.len() {
            let node = &snapshot.nodes[i];
            let window = match node.parent_index {
                Some(0) if node.native.bus_name == bus => Some(i),
                Some(j) if node.native.bus_name == bus => windows[j],
                _ => None,
            };
            windows.push(window);
            let Some(window) = window else { continue };
            if node.states.get("protected") != Some(&Some(false))
                || !["enabled", "sensitive", "visible", "showing"]
                    .iter()
                    .all(|s| node.states.get(*s) == Some(&Some(true)))
            {
                continue;
            }
            let top = &snapshot.nodes[window].native.object_path;
            if node.native.object_path == entry
                && *top == entry_window
                && ["focused", "focusable", "editable"]
                    .iter()
                    .all(|s| node.states.get(*s) == Some(&Some(true)))
            {
                let node = &mut snapshot.nodes[i];
                node.actions.push("key_chord".into());
                node.provenance
                    .insert("wayland_input_window".into(), json!(entry_window));
                continue;
            }
            let mut modes = Vec::new();
            let mut allocation = None;
            for (mode, role_matches, method) in [
                (
                    "wheel",
                    matches!(node.role.as_str(), "slider" | "scroll_bar" | "drawing_area"),
                    "WheelBounds",
                ),
                (
                    "click",
                    matches!(
                        node.role.as_str(),
                        "push_button" | "button" | "drawing_area" | "canvas" | "unknown"
                    ),
                    "ClickBounds",
                ),
            ] {
                if !role_matches {
                    continue;
                }
                remaining(deadline, false)?;
                self.check_cancelled(false)?;
                let Ok((x, y, width, height)) = proxy
                    .call::<_, _, (i32, i32, i32, i32)>(method, &(&node.native.object_path, top))
                else {
                    continue;
                };
                if width <= 0 || height <= 0 {
                    continue;
                }
                let bounds = Bounds {
                    x,
                    y,
                    width,
                    height,
                };
                if allocation.is_some_and(|old| old != bounds) {
                    modes.clear();
                    break;
                }
                allocation = Some(bounds);
                modes.push(mode);
            }
            if !modes.is_empty() {
                let window = json!(top);
                let node = &mut snapshot.nodes[i];
                node.bounds = allocation;
                if modes.contains(&"click") {
                    node.actions.push("pointer_click".into());
                }
                node.actions.push("pointer_gesture".into());
                node.provenance
                    .insert("coordinate_space".into(), json!("gtk_window_logical"));
                node.provenance
                    .insert("wayland_pointer_modes".into(), json!(modes));
                node.provenance
                    .insert("wayland_input_window".into(), window);
            }
        }
        remaining(deadline, false)?;
        self.check_cancelled(false)
    }

    pub(super) fn gtk3_hit(
        &self,
        target: &NativeRef,
        top: &str,
        point: (i32, i32),
        deadline: Instant,
    ) -> Result<bool, AutomationError> {
        remaining(deadline, false)?;
        self.check_cancelled(false)?;
        let proxy = Proxy::new(&self.connection, target.bus_name.as_str(), PATH, INTERFACE)
            .map_err(|_| unavailable())?;
        let args = (&target.object_path, top, point.0, point.1);
        Ok(proxy.call::<_, _, bool>("WheelHit", &args) == Ok(true)
            || proxy.call::<_, _, bool>("ClickHit", &args) == Ok(true))
    }

    pub(super) fn gtk3_input(
        &self,
        target: &NativeRef,
        window: Option<&WindowAnchor>,
        event: wayland_input::Event<'_>,
        process_id: u32,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        let top = &window
            .ok_or_else(|| {
                driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "GTK input requires an observed top-level window",
                )
            })?
            .native;
        if target.bus_name != top.bus_name || self.process_id(&target.bus_name) != Some(process_id)
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "GTK target and window owner do not match the observed process",
            ));
        }
        let proxy = Proxy::new(&self.connection, target.bus_name.as_str(), PATH, INTERFACE)
            .map_err(|_| unavailable())?;
        let until = monotonic_deadline_ns(deadline.min(Instant::now() + Duration::from_secs(2)))?;
        remaining(deadline, false)?;
        self.check_cancelled(false)?;
        let (object, window) = (&target.object_path, &top.object_path);
        let (events, reply): (u8, zbus::Result<i32>) = match event {
            wayland_input::Event::Click((x, y)) => {
                (2, proxy.call("Click", &(object, window, x, y, until)))
            }
            wayland_input::Event::Key(keys) => {
                let (modifier, key) = match keys {
                    [key] => ("", key),
                    [ctrl, key] if ctrl.eq_ignore_ascii_case("ctrl") => ("ctrl", key),
                    _ => {
                        return Err(driver_error(
                            "DRIVER.ACTION_UNSUPPORTED",
                            "GTK entry does not offer this chord",
                        ))
                    }
                };
                (
                    2,
                    proxy.call(
                        "Key",
                        &(object, window, key.to_ascii_lowercase(), modifier, until),
                    ),
                )
            }
            wayland_input::Event::Gesture((x, y), Gesture::Wheel { button, notches }, []) => (
                notches,
                proxy.call(
                    "Wheel",
                    &(
                        object,
                        window,
                        x,
                        y,
                        i32::from(button),
                        i32::from(notches),
                        until,
                    ),
                ),
            ),
            _ => return Err(unavailable()),
        };
        match reply {
            Ok(0) => Err(unavailable()),
            Ok(2) => {
                self.check_cancelled(true)?;
                remaining(deadline, true)?;
                Ok(
                    json!({"native_interface":"gtk_widget_event", "events":events, "synthetic_input":true,
                    "dispatched":true, "target_window":window}),
                )
            }
            _ => Err(driver_error(
                "DRIVER.UNKNOWN_EFFECT",
                "GTK event delivery was not confirmed",
            )
            .with_effect("unknown")
            .with_detail("dispatch_started", json!(true))),
        }
    }
}

//! Bind X11 input to the exact client and frame geometry from observation.
use super::*;

pub(super) const GEOMETRY_FEATURE: &str = "window-geometry-proof-v1";

impl WindowAnchor {
    pub(super) fn input_binding(&self) -> Result<&X11WindowBinding, AutomationError> {
        self.x11.as_ref().ok_or_else(|| {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "observation has no proven X11 window",
            )
        })
    }
}

pub(super) fn append_window_args(
    args: &mut Vec<String>,
    window: &WindowAnchor,
    binding: Option<&X11WindowBinding>,
) {
    let bounds = window.bounds;
    args.extend([
        "--window-x".into(),
        bounds.x.to_string(),
        "--window-y".into(),
        bounds.y.to_string(),
        "--window-width".into(),
        bounds.width.to_string(),
        "--window-height".into(),
        bounds.height.to_string(),
    ]);
    if let Some(binding) = binding {
        args.extend([
            "--window-id".into(),
            binding.client.to_string(),
            "--window-proof".into(),
            binding.input_proof().to_string(),
        ]);
    }
}

fn matches_bounds(value: &Value, observed: Bounds) -> bool {
    super::super::parse_bounds(Some(value)).is_ok_and(|actual| {
        actual.width == observed.width
            && actual.height == observed.height
            && actual.x.abs_diff(observed.x) <= 2
            && actual.y.abs_diff(observed.y) <= 2
    })
}

fn proof_payload(proof: &Value, client: u32, process_id: u32, bounds: Bounds) -> Option<Value> {
    (proof["version"] == 1
        && proof["expected_pid"] == process_id
        && proof["client"] == client
        && proof["observed_bounds"] == bounds.to_json())
    .then(|| proof.clone())
}

fn reply_client(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok().filter(|id| *id > 0)
}

impl X11WindowBinding {
    pub(super) fn input_proof(&self) -> &Value {
        &self.proof
    }

    fn matches(&self, observed: Bounds) -> bool {
        matches_bounds(&self.proof["client_bounds"], observed)
            || self.proof["frames"].as_array().is_some_and(|frames| {
                frames
                    .iter()
                    .any(|f| matches_bounds(&f["bounds"], observed))
            })
    }
}

fn add_window_actions(node: &mut BackendNode, info: &Value, binding: &X11WindowBinding) {
    let supports = |name: &str| info["supports"][name] == true;
    let minimized = info["minimized"] == true;
    let maximized = info["maximized"] == true;
    let active = info["active"] == true;
    let actions = [
        ("minimize", !minimized && supports("minimize")),
        ("maximize", !minimized && !maximized && supports("maximize")),
        ("restore", (minimized || maximized) && supports("restore")),
        ("close_window", supports("close")),
        ("activate", !active && supports("activate")),
    ];
    node.actions.extend(
        actions
            .into_iter()
            .filter(|(_, allowed)| *allowed)
            .map(|(action, _)| action.to_string()),
    );
    node.provenance.insert(
        "window".into(),
        json!({
            "x11_window": binding.client,
            "window_binding": "geometry",
            "minimized": minimized,
            "maximized": maximized,
            "active": active,
        }),
    );
}

impl LinuxBackend {
    pub(super) fn window_bounds(
        &self,
        native: &NativeRef,
        deadline: Instant,
    ) -> Result<Bounds, AutomationError> {
        remaining(deadline, false)?;
        let (x, y, width, height) =
            zbus::block_on(self.component_proxy(native)?.get_extents(CoordType::Screen))
                .map_err(|error| native_failure("window bounds", native, error))?;
        Ok(Bounds {
            x,
            y,
            width,
            height,
        })
    }

    fn direct_window_bounds(
        &self,
        app: &NativeRef,
        nodes: &[BackendNode],
        deadline: Instant,
    ) -> Result<Vec<(NativeRef, Bounds)>, AutomationError> {
        self.direct_windows(app, deadline)?
            .into_iter()
            .map(|native| {
                let bounds = nodes
                    .iter()
                    .find(|node| node.parent_index == Some(0) && node.native == native)
                    .and_then(|node| node.bounds)
                    .map_or_else(|| self.window_bounds(&native, deadline), Ok)?;
                Ok((native, bounds))
            })
            .collect()
    }

    fn window_proof(
        &self,
        window: &WindowAnchor,
        process_id: u32,
        path: &Path,
        deadline: Instant,
    ) -> Result<(X11WindowBinding, bool), AutomationError> {
        let mut args = vec![
            "focus-info".into(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        append_window_args(&mut args, window, window.x11.as_ref());
        if window.x11.is_none() {
            args.extend(["--geometry-proof".into(), "1".into()]);
        }
        let output = self.run_helper(
            path,
            &args,
            None,
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )?;
        let response = parse_one_json_object(&output.stdout, "window geometry proof")?;
        let invalid_proof = || {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "native window identity or geometry is not proven",
            )
        };
        let client = response
            .get("window")
            .and_then(reply_client)
            .ok_or_else(invalid_proof)?;
        if !output.status.success()
            || response.get("ok") != Some(&json!(true))
            || response.get("window_bound") != Some(&json!(true))
            || response.get("expected_pid") != Some(&json!(process_id))
            || response.contains_key("dispatch_started")
        {
            return Err(invalid_proof());
        }
        let proof = response
            .get("geometry_proof")
            .and_then(|proof| proof_payload(proof, client, process_id, window.bounds))
            .ok_or_else(invalid_proof)?;
        let binding = X11WindowBinding { client, proof };
        if window
            .x11
            .as_ref()
            .is_some_and(|expected| expected != &binding)
        {
            return Err(super::super::stale("native window proof changed"));
        }
        Ok((binding, valid_focus_proof(&response, process_id)))
    }

    pub(super) fn capture_x11_window_bindings(
        &self,
        snapshot: &mut BackendSnapshot,
        deadline: Instant,
    ) -> BTreeMap<usize, X11WindowBinding> {
        let Some(app) = snapshot.nodes.first() else {
            return BTreeMap::new();
        };
        let Some(process_id) = snapshot
            .application
            .get("process_id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
        else {
            return BTreeMap::new();
        };
        let Ok(support) = self.probe_input(deadline) else {
            return BTreeMap::new();
        };
        if !support.has(GEOMETRY_FEATURE) || !support.has("window-info") {
            return BTreeMap::new();
        }
        let native_windows = if snapshot.qt_x11_windows.is_none() {
            let Ok(windows) = self.direct_window_bounds(&app.native, &snapshot.nodes, deadline)
            else {
                return BTreeMap::new();
            };
            windows
        } else {
            Vec::new()
        };
        snapshot
            .nodes
            .iter_mut()
            .enumerate()
            .filter(|(_, node)| node.parent_index == Some(0))
            .filter_map(|(index, node)| {
                let bounds = node.bounds.filter(|b| b.width > 0 && b.height > 0)?;
                let info = self.window_info(process_id, bounds, &support.path, deadline, true)?;
                let client = reply_client(&info["window"])?;
                if info["window_binding"] != "geometry" {
                    return None;
                }
                let proof = proof_payload(&info["geometry_proof"], client, process_id, bounds)?;
                let binding = X11WindowBinding { client, proof };
                let unique = if let Some(windows) = &snapshot.qt_x11_windows {
                    qt_binding_matches(windows, &node.native, bounds, &binding)
                } else {
                    unique_native_bounds(&native_windows, &node.native, &binding)
                };
                if !unique {
                    return None;
                }
                let unproven =
                    node.provenance.get("coordinate_space") == Some(&json!("unproven_toolkit"));
                if unproven && info["minimized"] != true {
                    return None;
                }
                if support.has("window-action") && info["ewmh"] == true {
                    add_window_actions(node, &info, &binding);
                }
                Some((index, binding))
            })
            .collect()
    }

    // Visibility recovery uses the raw ancestry and bounds to select the
    // matching entry from the current calibration inventory.
    fn native_window(
        &self,
        target: &NativeRef,
        deadline: Instant,
    ) -> Result<(NativeRef, WindowAnchor), AutomationError> {
        let mut current = target.clone();
        let mut visited = BTreeSet::new();
        for _ in 0..super::super::MAX_DEPTH {
            remaining(deadline, false)?;
            if current.bus_name != target.bus_name || !visited.insert(current.object_path.clone()) {
                break;
            }
            let Some(parent) = self.parent(&current, deadline)? else {
                break;
            };
            let role = zbus::block_on(self.accessible(&parent)?.get_role())
                .map_err(|error| native_failure("window role", &parent, error))?;
            if role == atspi::Role::Application && parent.bus_name == target.bus_name {
                let bounds = self.window_bounds(&current, deadline)?;
                if bounds.width > 0 && bounds.height > 0 {
                    return Ok((
                        parent,
                        WindowAnchor {
                            native: current,
                            bounds,
                            x11: None,
                        },
                    ));
                }
                break;
            }
            current = parent;
        }
        Err(driver_error(
            "DRIVER.ACTION_UNSUPPORTED",
            "target window ancestry is not proven",
        ))
    }

    pub(super) fn verify_window_with_helper<'a>(
        &self,
        target: &NativeRef,
        window: &'a WindowAnchor,
        process_id: u32,
        deadline: Instant,
        path: &Path,
        require_focus: bool,
    ) -> Result<&'a X11WindowBinding, AutomationError> {
        remaining(deadline, false)?;
        if window.native.bus_name != target.bus_name
            || self.process_id(&target.bus_name) != Some(process_id)
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "target window process differs from observed target",
            ));
        }
        let binding = window.input_binding()?;
        let raw = self.window_bounds(&window.native, deadline)?;
        if self.physical_window_bounds(&window.native, raw, deadline)? != window.bounds {
            return Err(super::super::stale("target window geometry changed"));
        }
        let (_, focused) = self.window_proof(window, process_id, path, deadline)?;
        if require_focus && !focused {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "native input focus is not proven for the observed window",
            ));
        }
        Ok(binding)
    }

    pub(super) fn verify_focused(
        &self,
        target: &NativeRef,
        window: Option<&WindowAnchor>,
        process_id: u32,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        let path = self.input_helper(GEOMETRY_FEATURE, deadline)?;
        self.verify_focused_with_helper(target, window, process_id, deadline, &path)
    }

    pub(super) fn verify_focused_with_helper(
        &self,
        target: &NativeRef,
        window: Option<&WindowAnchor>,
        process_id: u32,
        deadline: Instant,
        path: &Path,
    ) -> Result<(), AutomationError> {
        self.with_accessible_focus(target, process_id, deadline, || {
            if let Some(window) = window {
                self.verify_window_with_helper(target, window, process_id, deadline, path, true)?;
                return Ok(());
            }
            let (app, mut window) = self.native_window(target, deadline)?;
            let mut application = self.application_info(&app, deadline)?;
            if let Some(windows) = self.read_qt_x11_windows(&mut application, deadline)? {
                return self.verify_calibrated_focus(
                    &mut window,
                    process_id,
                    &windows,
                    deadline,
                    path,
                );
            }
            let (binding, focused) = self.window_proof(&window, process_id, path, deadline)?;
            let windows = self.direct_window_bounds(&app, &[], deadline)?;
            if focused && unique_native_bounds(&windows, &window.native, &binding) {
                Ok(())
            } else {
                Err(unproven_focus_window())
            }
        })
    }

    pub(super) fn verify_qt_focus(
        &self,
        target: &NativeRef,
        process_id: u32,
        windows: &[QtX11Window],
        deadline: Instant,
        path: &Path,
    ) -> Result<(), AutomationError> {
        self.with_accessible_focus(target, process_id, deadline, || {
            let (_, mut window) = self.native_window(target, deadline)?;
            self.verify_calibrated_focus(&mut window, process_id, windows, deadline, path)
        })
    }

    fn verify_calibrated_focus(
        &self,
        window: &mut WindowAnchor,
        process_id: u32,
        windows: &[QtX11Window],
        deadline: Instant,
        path: &Path,
    ) -> Result<(), AutomationError> {
        let client = windows
            .iter()
            .find(|entry| entry.native == window.native && entry.raw == window.bounds)
            .and_then(|entry| entry.client)
            .filter(|client| client.viewable)
            .ok_or_else(unproven_focus_window)?;
        window.bounds = client.bounds;
        let (binding, focused) = self.window_proof(window, process_id, path, deadline)?;
        if focused && qt_binding_matches(windows, &window.native, client.bounds, &binding) {
            Ok(())
        } else {
            Err(unproven_focus_window())
        }
    }

    fn with_accessible_focus(
        &self,
        target: &NativeRef,
        process_id: u32,
        deadline: Instant,
        verify: impl FnOnce() -> Result<(), AutomationError>,
    ) -> Result<(), AutomationError> {
        let focused = || -> Result<bool, AutomationError> {
            remaining(deadline, false)?;
            let states = zbus::block_on(self.accessible(target)?.get_state())
                .map_err(|error| native_failure("Accessible.GetState", target, error))?;
            Ok([
                State::Focused,
                State::Focusable,
                State::Enabled,
                State::Sensitive,
            ]
            .into_iter()
            .all(|state| states.contains(state)))
        };
        if !focused()? {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "accessible focus is not proven",
            ));
        }
        verify()?;
        if !focused()? || self.process_id(&target.bus_name) != Some(process_id) {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "accessible focus changed during native window verification",
            ));
        }
        Ok(())
    }
}

fn unproven_focus_window() -> AutomationError {
    driver_error(
        "DRIVER.ACTION_UNSUPPORTED",
        "native focus window is unproven",
    )
}

fn unique_native_bounds(
    windows: &[(NativeRef, Bounds)],
    target: &NativeRef,
    binding: &X11WindowBinding,
) -> bool {
    windows.iter().any(|(native, _)| native == target)
        && windows
            .iter()
            .all(|(native, bounds)| native == target || !binding.matches(*bounds))
}

fn qt_binding_matches(
    windows: &[QtX11Window],
    target: &NativeRef,
    observed: Bounds,
    binding: &X11WindowBinding,
) -> bool {
    windows.iter().any(|entry| entry.native == *target)
        && windows.iter().all(|entry| {
            let Some(client) = entry.client else {
                return false;
            };
            if entry.native == *target {
                // A minimized window keeps its raw observation for window
                // actions, which still require a strict native proof.
                client.xid == binding.client
                    && observed
                        == if client.viewable {
                            client.bounds
                        } else {
                            entry.raw
                        }
            } else {
                !binding.matches(client.bounds)
            }
        })
}

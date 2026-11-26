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
            binding.proof.to_string(),
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

impl X11WindowBinding {
    fn matches(&self, observed: Bounds) -> bool {
        matches_bounds(&self.proof["client_bounds"], observed)
            || self.proof["frames"].as_array().is_some_and(|frames| {
                frames
                    .iter()
                    .any(|f| matches_bounds(&f["bounds"], observed))
            })
    }
}

impl LinuxBackend {
    fn window_bounds(
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
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .filter(|id| *id > 0)
            .ok_or_else(invalid_proof)?;
        let proof = response.get("geometry_proof").ok_or_else(invalid_proof)?;
        if !output.status.success()
            || response.get("ok") != Some(&json!(true))
            || response.get("window_bound") != Some(&json!(true))
            || response.get("expected_pid") != Some(&json!(process_id))
            || response.contains_key("dispatch_started")
            || proof["version"] != 1
            || proof["expected_pid"] != process_id
            || proof["client"] != client
            || proof["observed_bounds"] != window.bounds.to_json()
        {
            return Err(invalid_proof());
        }
        let binding = X11WindowBinding {
            client,
            proof: proof.clone(),
        };
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
        snapshot: &BackendSnapshot,
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
        let Ok(path) = self.input_helper(GEOMETRY_FEATURE, deadline) else {
            return BTreeMap::new();
        };
        let Ok(windows) = self.direct_window_bounds(&app.native, &snapshot.nodes, deadline) else {
            return BTreeMap::new();
        };
        snapshot
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.parent_index == Some(0))
            .filter_map(|(index, node)| {
                if node.provenance.get("coordinate_space") == Some(&json!("unproven_toolkit")) {
                    return None;
                }
                let bounds = node.bounds.filter(|b| b.width > 0 && b.height > 0)?;
                if !windows.iter().any(|(native, _)| *native == node.native) {
                    return None;
                }
                let window = WindowAnchor {
                    native: node.native.clone(),
                    bounds,
                    x11: None,
                };
                let (binding, _) = self
                    .window_proof(&window, process_id, &path, deadline)
                    .ok()?;
                windows
                    .iter()
                    .all(|(native, bounds)| *native == window.native || !binding.matches(*bounds))
                    .then_some((index, binding))
            })
            .collect()
    }

    // Qt observation-time visibility recovery runs before the normalized
    // snapshot is assembled, so prove its native application ancestry here.
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
                let raw = self.window_bounds(&current, deadline)?;
                if raw.width > 0 && raw.height > 0 {
                    let bounds = self.physical_window_bounds(&current, raw, deadline)?;
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
        if let Some(window) = window {
            self.verify_window_with_helper(target, window, process_id, deadline, path, true)?;
        } else {
            let (app, window) = self.native_window(target, deadline)?;
            let (binding, native_focused) =
                self.window_proof(&window, process_id, path, deadline)?;
            let windows = self.direct_window_bounds(&app, &[], deadline)?;
            if !native_focused
                || !windows.iter().any(|(native, _)| *native == window.native)
                || windows
                    .iter()
                    .any(|(native, bounds)| *native != window.native && binding.matches(*bounds))
            {
                return Err(driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "native focus window is unproven",
                ));
            }
        }
        if !focused()? || self.process_id(&target.bus_name) != Some(process_id) {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "accessible focus changed during native window verification",
            ));
        }
        Ok(())
    }
}

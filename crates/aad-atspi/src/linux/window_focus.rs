//! Bind keyboard delivery to a freshly observed top-level window, not just PID.
use super::*;

pub(super) fn append_window_args(args: &mut Vec<String>, window: &WindowAnchor, id: Option<u64>) {
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
    if let Some(id) = id {
        args.extend(["--window-id".into(), id.to_string()]);
    }
}

impl LinuxBackend {
    // Observation-time visibility recovery has no normalized Record yet.
    // This path requires a real same-application native Parent chain.
    fn native_window(
        &self,
        target: &NativeRef,
        deadline: Instant,
    ) -> Result<WindowAnchor, AutomationError> {
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
            if role == atspi::Role::Application {
                let (x, y, width, height) = zbus::block_on(
                    self.component_proxy(&current)?
                        .get_extents(CoordType::Screen),
                )
                .map_err(|error| native_failure("window bounds", &current, error))?;
                if width > 0 && height > 0 {
                    return Ok(WindowAnchor {
                        native: current,
                        bounds: Bounds {
                            x,
                            y,
                            width,
                            height,
                        },
                    });
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

    pub(super) fn verify_window_with_helper(
        &self,
        target: &NativeRef,
        window: &WindowAnchor,
        process_id: u32,
        deadline: Instant,
        path: &Path,
    ) -> Result<u64, AutomationError> {
        remaining(deadline, false)?;
        if window.native.bus_name != target.bus_name
            || self.process_id(&target.bus_name) != Some(process_id)
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "target window process differs from observed target",
            ));
        }
        let (x, y, width, height) = zbus::block_on(
            self.component_proxy(&window.native)?
                .get_extents(CoordType::Screen),
        )
        .map_err(|error| native_failure("window bounds", &window.native, error))?;
        if (Bounds {
            x,
            y,
            width,
            height,
        }) != window.bounds
        {
            return Err(super::super::stale("target window geometry changed"));
        }
        let mut args = vec![
            "focus-info".into(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        append_window_args(&mut args, window, None);
        let output = self.run_helper(
            path,
            &args,
            None,
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )?;
        let proof = parse_one_json_object(&output.stdout, "focus info")?;
        if !output.status.success() || !valid_focus_proof(&proof, process_id) {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "native input focus is not proven for the observed window",
            ));
        }
        Ok(proof["window"].as_u64().expect("validated window proof"))
    }

    pub(super) fn verify_focused(
        &self,
        target: &NativeRef,
        window: Option<&WindowAnchor>,
        process_id: u32,
        deadline: Instant,
    ) -> Result<u64, AutomationError> {
        let path = self.input_helper("window-focus", deadline)?;
        self.verify_focused_with_helper(target, window, process_id, deadline, &path)
    }

    pub(super) fn verify_focused_with_helper(
        &self,
        target: &NativeRef,
        window: Option<&WindowAnchor>,
        process_id: u32,
        deadline: Instant,
        path: &Path,
    ) -> Result<u64, AutomationError> {
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
        let recovered;
        let window = match window {
            Some(window) => window,
            None => {
                recovered = self.native_window(target, deadline)?;
                &recovered
            }
        };
        let id = self.verify_window_with_helper(target, window, process_id, deadline, path)?;
        if !focused()? || self.process_id(&target.bus_name) != Some(process_id) {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "accessible focus changed during native window verification",
            ));
        }
        Ok(id)
    }
}

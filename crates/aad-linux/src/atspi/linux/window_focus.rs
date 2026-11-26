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

pub(super) struct AnchoredKeyboard<'a> {
    pub path: &'a Path,
    pub args: Vec<String>,
    pub payload: Option<&'a [u8]>,
    pub strokes: usize,
    pub target: &'a NativeRef,
    pub process_id: u32,
    pub deadline: Instant,
    pub phase: &'a str,
    pub minimum_events: u64,
}

const ANCHOR_POLL: Duration = Duration::from_millis(1);

fn focus_ready(states: atspi::StateSet) -> bool {
    [State::Focused, State::Enabled, State::Sensitive]
        .into_iter()
        .all(|state| states.contains(state))
}

fn keyboard_anchor_error(mut error: AutomationError) -> AutomationError {
    if error.code == "DRIVER.FOCUS_LOST" {
        if error.details.get("helper_code").and_then(Value::as_str) == Some("focus_owner_changed") {
            error.message = "keyboard focus moved outside the observed X11 window".into();
            error.details.remove("cause");
        }
        if error.effect == "unknown" {
            error.code = "DRIVER.UNKNOWN_EFFECT".into();
        }
    }
    if error.effect == "unknown" {
        error.retryable = false;
        if error.code == "DRIVER.TIMEOUT" {
            error.details.insert("phase".into(), json!("post_dispatch"));
        }
    }
    error
}

impl LinuxBackend {
    pub(super) fn prepare_anchored_keyboard(
        &self,
        feature: &str,
        target: &NativeRef,
        window: Option<&WindowAnchor>,
        process_id: u32,
        deadline: Instant,
        args: &mut Vec<String>,
    ) -> Result<(PathBuf, Value), AutomationError> {
        self.check_cancelled(false)?;
        let support = self.probe_input(deadline)?;
        support.require_anchored_keyboard(feature)?;
        if self.cancellation.is_some() {
            support.require("cooperative-cancel")?;
        }
        if args.iter().any(|arg| arg == "--hold-ms") {
            support.require("bounded-hold")?;
        }
        let window = window.ok_or_else(|| {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "input requires an observed top-level window",
            )
        })?;
        let focus = self.focus(target, Some(window), deadline)?;
        std::thread::sleep(Duration::from_millis(50).min(remaining(deadline, true)?));
        // Resolve the native window here. The first handshake proves the
        // element and can report a focus change before any stroke was sent.
        let window_id = self
            .verify_window_with_helper(target, window, process_id, deadline, &support.path)
            .map_err(after_focus)?;
        append_window_args(args, window, Some(window_id));
        Ok((support.path, focus))
    }

    pub(super) fn run_anchored_keyboard(
        &self,
        request: AnchoredKeyboard<'_>,
    ) -> Result<Value, AutomationError> {
        let AnchoredKeyboard {
            path,
            mut args,
            payload,
            strokes,
            target,
            process_id,
            deadline,
            phase,
            minimum_events,
        } = request;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        args.extend([
            "--anchor-token".into(),
            nonce.clone(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ]);
        let validate = |_| self.verify_keyboard_anchor(target, process_id, deadline);
        let output = anchored_helper::run(anchored_helper::Request {
            path,
            args: &args,
            nonce: &nonce,
            payload,
            handshakes: strokes,
            poll: ANCHOR_POLL,
            deadline,
            token: self.cancellation.as_ref(),
            validation: anchored_helper::Validation::Inline(&validate),
        })
        .map_err(keyboard_anchor_error)?;
        if !output.status.success()
            && helper_error_code(&output.stdout).as_deref() == Some("deadline_exceeded")
        {
            return Err(with_helper_progress(
                super::super::timeout_error(helper_dispatched(&output.stdout)),
                &output.stdout,
            ));
        }
        parse_input_helper(output, phase, minimum_events, process_id)
    }

    async fn input_states(&self, target: &NativeRef) -> Result<atspi::StateSet, AutomationError> {
        self.accessible_async(target)
            .await?
            .get_state()
            .await
            .map_err(|error| native_failure("Accessible.GetState", target, error))
    }

    fn verify_keyboard_anchor(
        &self,
        target: &NativeRef,
        process_id: u32,
        deadline: Instant,
    ) -> Result<(), AutomationError> {
        let proof = async {
            if self.process_id_async(&target.bus_name).await != Some(process_id) {
                return Err(super::super::stale(
                    "observed accessibility process changed",
                ));
            }
            // GetState is a method call on the observed bus and object path.
            // There is no properties cache or tree capture in this proof.
            let states = self.input_states(target).await.map_err(|_| {
                super::super::stale("observed accessibility element could not be rechecked")
            })?;
            if states.contains(State::Defunct) {
                return Err(super::super::stale(
                    "observed accessibility element was removed",
                ));
            }
            if !focus_ready(states) {
                return Err(driver_error(
                    "DRIVER.FOCUS_LOST",
                    "the observed control lost keyboard focus",
                )
                .with_detail("cause", json!("focused_element_changed")));
            }
            Ok(())
        };
        // The bus connection's method timeout may be longer than the time
        // left for this request. Keep both deadline and cancellation bounded.
        let interrupted = async {
            loop {
                self.check_cancelled(false)?;
                let left = remaining(deadline, false)?;
                let delay = if self.cancellation.is_some() {
                    left.min(ANCHOR_POLL)
                } else {
                    left
                };
                async_io::Timer::after(delay).await;
            }
        };
        zbus::block_on(futures_lite::future::race(proof, interrupted))
    }

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
                    let bounds = self.physical_window_bounds(
                        &current,
                        Bounds {
                            x,
                            y,
                            width,
                            height,
                        },
                        deadline,
                    )?;
                    return Ok(WindowAnchor {
                        native: current,
                        bounds,
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
        let current_bounds = self.physical_window_bounds(
            &window.native,
            Bounds {
                x,
                y,
                width,
                height,
            },
            deadline,
        )?;
        if current_bounds != window.bounds {
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
            let states = zbus::block_on(self.input_states(target))?;
            Ok(focus_ready(states) && states.contains(State::Focusable))
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

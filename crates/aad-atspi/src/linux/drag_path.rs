//! A helper owns held input; read-only AX validation runs outside its polling
//! loop. No ACK can be sent after cancellation or expiry, even if DBus is slow.
use super::*;
use crate::{DragPathRequest, DragPathValidation};
use std::os::fd::{AsRawFd, RawFd};

struct WorkerPermit(Arc<AtomicBool>);
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl LinuxBackend {
    pub(super) fn run_drag_path(
        &self,
        request: DragPathRequest<'_>,
        validate: DragPathValidation,
    ) -> Result<Value, AutomationError> {
        let DragPathRequest {
            target,
            window,
            points,
            button,
            modifiers,
            hold_ms,
            process_id,
            deadline,
        } = request;
        self.check_cancelled(false)?;
        if !(3..=34).contains(&points.len()) {
            return Err(super::super::invalid(
                "drag path must have 1..32 intermediate anchors",
            ));
        }
        let support = self.probe_input(deadline)?;
        for feature in [
            "drag",
            "anchored-drag-path",
            "cooperative-cancel",
            "window-focus",
        ] {
            support.require(feature)?;
        }
        if !modifiers.is_empty() {
            support.require("pointer-modifiers")?;
        }
        if hold_ms.is_some() {
            support.require("bounded-hold")?;
        }
        let window = window.ok_or_else(|| {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "drag path requires an observed top-level window",
            )
        })?;
        if window.native.bus_name != target.bus_name
            || self.process_id(&target.bus_name) != Some(process_id)
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "drag window process is not proven",
            ));
        }
        // Resolve the exact observed window without activating it or requiring
        // keyboard focus for an unmodified pointer gesture.
        let mut lookup = vec![
            "focus-info".into(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        append_window_args(&mut lookup, window, None);
        let proof = self.run_helper(
            &support.path,
            &lookup,
            None,
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )?;
        let proof_value = parse_one_json_object(&proof.stdout, "drag window")?;
        let window_id = proof_value
            .get("window")
            .and_then(Value::as_u64)
            .filter(|id| *id > 0 && *id <= u32::MAX as u64);
        if !proof.status.success()
            || proof_value.get("ok") != Some(&json!(true))
            || proof_value.get("window_bound") != Some(&json!(true))
            || proof_value.get("expected_pid") != Some(&json!(process_id))
            || window_id.is_none()
            || (!modifiers.is_empty() && proof_value.get("focused") != Some(&json!(true)))
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "drag window identity or modifier focus is not proven",
            ));
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let last = points[points.len() - 1];
        let path = points[1..points.len() - 1]
            .iter()
            .map(|(x, y)| format!("{x},{y}"))
            .collect::<Vec<_>>()
            .join(";");
        let mut args = vec![
            "drag".into(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--x".into(),
            points[0].0.to_string(),
            "--y".into(),
            points[0].1.to_string(),
            "--to-x".into(),
            last.0.to_string(),
            "--to-y".into(),
            last.1.to_string(),
            "--button".into(),
            button.to_string(),
            "--path".into(),
            path,
            "--path-token".into(),
            nonce.clone(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        append_window_args(&mut args, window, window_id);
        if !modifiers.is_empty() {
            args.extend(["--modifiers".into(), modifiers.join(",")]);
        }
        if let Some(ms) = hold_ms {
            args.extend(["--hold-ms".into(), ms.to_string()]);
        }
        let output = run(
            &support.path,
            &args,
            &nonce,
            points.len(),
            deadline,
            self.cancellation.as_ref(),
            self.path_validation_busy.clone(),
            validate,
        )?;
        let input = parse_input_helper(
            output,
            "drag_path",
            (points.len() + 2 + modifiers.len() * 2) as u64,
            process_id,
        )?;
        Ok(
            json!({"native_interface": "AT-SPI segment revalidation -> XTEST", "input": input,
            "synthetic_input": true, "path_anchors": points.len() - 2, "segments_revalidated": points.len() + 1}),
        )
    }
}

fn nonblocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>) -> Result<(), AutomationError> {
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if bytes.len() + count > XTEST_OUTPUT_LIMIT {
                    return Err(protocol("helper output exceeded its limit"));
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(protocol("helper output could not be read")),
        }
    }
}

fn protocol(message: &str) -> AutomationError {
    driver_error("DRIVER.ACTION_FAILED", message)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    path: &Path,
    args: &[String],
    nonce: &str,
    point_count: usize,
    deadline: Instant,
    token: Option<&super::super::CancellationToken>,
    busy: Arc<AtomicBool>,
    validate: DragPathValidation,
) -> Result<HelperOutput, AutomationError> {
    if let Some(token) = token {
        token.check()?;
    }
    remaining(deadline, false)?;
    // Retained by a blocked read-only worker after cancellation. A second path
    // cannot accumulate more DBus workers while that one is still returning.
    busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .map_err(|_| {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "an earlier drag validation is still returning",
            )
        })?;
    let permit = Arc::new(WorkerPermit(busy));
    let mut command = Command::new(path);
    command
        .args(args)
        .current_dir(path.parent().unwrap_or_else(|| Path::new("/")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    helper_environment(&mut command);
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .map_err(|_| driver_error("DRIVER.UNAVAILABLE", "drag helper could not be started"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    for fd in [
        child.stdin.as_ref().unwrap().as_raw_fd(),
        stdout.as_raw_fd(),
        stderr.as_raw_fd(),
    ] {
        if nonblocking(fd).is_err() {
            terminate(&mut child);
            return Err(protocol("drag helper pipes could not be made nonblocking"));
        }
    }
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let mut cursor = 0;
    let mut next = 0usize;
    let mut worker: Option<mpsc::Receiver<Result<(), AutomationError>>> = None;
    let mut ready = false;
    let result = (|| loop {
        if let Some(token) = token {
            token.check()?;
        }
        remaining(deadline, false)?;
        drain(&mut stdout, &mut output)?;
        drain(&mut stderr, &mut diagnostics)?;
        while let Some(end) = output[cursor..].iter().position(|b| *b == b'\n') {
            let end = cursor + end;
            let value: Value = serde_json::from_slice(&output[cursor..end])
                .map_err(|_| protocol("malformed drag helper frame"))?;
            cursor = end + 1;
            if value["event"] == "validate_segment" {
                if worker.is_some()
                    || ready
                    || next > point_count
                    || value["token"] != nonce
                    || value["index"].as_u64() != Some(next as u64)
                {
                    return Err(protocol("unexpected drag validation sequence"));
                }
                let (tx, rx) = mpsc::sync_channel(1);
                let validate = validate.clone();
                let permit = permit.clone();
                let index = next;
                std::thread::Builder::new()
                    .name("aad-drag-validation".into())
                    .spawn(move || {
                        let _permit = permit;
                        let _ = tx.send(validate(index));
                    })
                    .map_err(|_| protocol("drag validation worker could not be started"))?;
                worker = Some(rx);
            } else if value["event"] == "dispatch_started" {
                if next == 0 {
                    return Err(protocol("drag helper dispatched before validation"));
                }
            } else if !value["ok"].is_boolean() {
                return Err(protocol("unknown drag helper frame"));
            }
        }
        if let Some(receiver) = &worker {
            match receiver.try_recv() {
                Ok(result) => {
                    result?;
                    worker = None;
                    ready = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(protocol("drag validation worker failed"))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if ready {
            if let Some(token) = token {
                token.check()?;
            }
            remaining(deadline, false)?;
            let ack = format!("ack {nonce} {next}\n");
            match child
                .stdin
                .as_mut()
                .ok_or_else(|| protocol("drag helper input closed"))?
                .write(ack.as_bytes())
            {
                Ok(count) if count == ack.len() => {
                    ready = false;
                    next += 1;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                _ => return Err(protocol("drag validation ACK could not be written")),
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|_| protocol("drag helper state unavailable"))?
        {
            drain(&mut stdout, &mut output)?;
            drain(&mut stderr, &mut diagnostics)?;
            if status.success() && next != point_count + 1 {
                return Err(protocol("drag helper skipped validation"));
            }
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(5));
    })();
    match result {
        Ok(status) => Ok(HelperOutput {
            status,
            stdout: output,
            stderr: diagnostics,
        }),
        Err(error) => {
            let clean = terminate_with_grace(&mut child, Duration::from_millis(500));
            let _ = drain(&mut stdout, &mut output);
            let dispatched = helper_dispatched(&output);
            Err(error
                .with_effect(if dispatched || !clean {
                    "unknown"
                } else {
                    "not_applied"
                })
                .with_detail("dispatch_started", json!(dispatched))
                .with_detail("helper_exited_before_kill", json!(clean)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    struct Script(PathBuf);
    impl Script {
        fn new(body: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("aad-path-protocol-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, format!("#!/bin/sh\ntrap 'exit 0' TERM\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for Script {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn handshake() -> Script {
        Script::new(&format!(
            r#"
for index in 0 1 2 3; do
    printf '{{"event":"validate_segment","token":"{NONCE}","index":%s}}\n' "$index"
    IFS= read -r line || exit 1
    [ "$line" = "ack {NONCE} $index" ] || exit 2
    if [ "$index" = 0 ]; then printf '{{"event":"dispatch_started"}}\n'; fi
done
printf '{{"ok":true,"dispatch_started":true,"events":5}}\n'
"#
        ))
    }

    #[test]
    fn path_protocol_requires_sequential_nonce_bound_fresh_acks() {
        let script = handshake();
        let visited = Arc::new(Mutex::new(Vec::new()));
        let seen = visited.clone();
        let busy = Arc::new(AtomicBool::new(false));
        let result = run(
            &script.0,
            &[],
            NONCE,
            3,
            Instant::now() + Duration::from_secs(2),
            None,
            busy.clone(),
            Arc::new(move |index| {
                seen.lock().unwrap().push(index);
                Ok(())
            }),
        )
        .unwrap();
        assert!(result.status.success());
        assert_eq!(*visited.lock().unwrap(), vec![0, 1, 2, 3]);
        assert!(!busy.load(Ordering::SeqCst));
        for event in [
            r#"{"event":"validate_segment","token":"wrong","index":0}"#.to_string(),
            format!(r#"{{"event":"validate_segment","token":"{NONCE}","index":1}}"#),
        ] {
            let script = Script::new(&format!("printf '%s\\n' '{event}'\nread ignored"));
            let error = run(
                &script.0,
                &[],
                NONCE,
                3,
                Instant::now() + Duration::from_secs(2),
                None,
                Arc::new(AtomicBool::new(false)),
                Arc::new(|_| panic!("invalid sequence dispatched validation")),
            )
            .err()
            .unwrap();
            assert_eq!(error.effect, "not_applied");
            assert_eq!(error.code, "DRIVER.ACTION_FAILED");
        }
    }

    #[test]
    fn blocked_ax_worker_cannot_delay_cancel_or_deadline_and_cannot_send_a_late_ack() {
        for cancel in [true, false] {
            let script = handshake();
            let token = super::super::super::CancellationToken::new();
            let signal = token.clone();
            let entered = Arc::new(AtomicBool::new(false));
            let reached = entered.clone();
            let release = Arc::new(AtomicBool::new(false));
            let unblocked = release.clone();
            let busy = Arc::new(AtomicBool::new(false));
            let coordinator = std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(2);
                while !reached.load(Ordering::SeqCst) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(2));
                }
                if cancel {
                    signal.cancel();
                }
            });
            let started = Instant::now();
            let failure = run(
                &script.0,
                &[],
                NONCE,
                3,
                started
                    + if cancel {
                        Duration::from_secs(2)
                    } else {
                        Duration::from_millis(250)
                    },
                Some(&token),
                busy.clone(),
                Arc::new(move |index| {
                    if index == 1 {
                        entered.store(true, Ordering::SeqCst);
                        while !unblocked.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                    Ok(())
                }),
            )
            .err()
            .unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(failure.effect, "unknown");
            assert_eq!(
                failure.code,
                if cancel {
                    "DRIVER.CANCELLED"
                } else {
                    "DRIVER.TIMEOUT"
                }
            );
            assert_eq!(failure.details["helper_exited_before_kill"], true);
            assert!(
                busy.load(Ordering::SeqCst),
                "blocked worker must keep its admission permit"
            );
            let denied = run(
                &script.0,
                &[],
                NONCE,
                3,
                Instant::now() + Duration::from_secs(1),
                None,
                busy.clone(),
                Arc::new(|_| Ok(())),
            )
            .err()
            .unwrap();
            assert_eq!(denied.code, "DRIVER.ACTION_UNSUPPORTED");
            release.store(true, Ordering::SeqCst);
            coordinator.join().unwrap();
            let until = Instant::now() + Duration::from_secs(1);
            while busy.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(!busy.load(Ordering::SeqCst));
        }
    }
}

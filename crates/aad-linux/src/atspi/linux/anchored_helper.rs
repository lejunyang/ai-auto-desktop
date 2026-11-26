//! A helper owns held input; slow AX validation runs outside its polling loop.
use super::*;
use crate::atspi::{CancellationToken, DragPathValidation};
use std::io::{ErrorKind, Write};
use std::os::fd::{AsRawFd, RawFd};

struct WorkerPermit(Arc<AtomicBool>);

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub(super) enum Validation<'a> {
    Worker {
        busy: Arc<AtomicBool>,
        validate: DragPathValidation,
    },
    Inline(&'a dyn Fn(usize) -> Result<(), AutomationError>),
}

pub(super) struct Request<'a> {
    pub path: &'a Path,
    pub args: &'a [String],
    pub nonce: &'a str,
    pub payload: Option<&'a [u8]>,
    pub handshakes: usize,
    pub poll: Duration,
    pub deadline: Instant,
    pub token: Option<&'a CancellationToken>,
    pub validation: Validation<'a>,
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
pub(super) fn run_drag(
    path: &Path,
    args: &[String],
    nonce: &str,
    point_count: usize,
    deadline: Instant,
    token: Option<&CancellationToken>,
    busy: Arc<AtomicBool>,
    validate: DragPathValidation,
) -> Result<HelperOutput, AutomationError> {
    run(Request {
        path,
        args,
        nonce,
        payload: None,
        handshakes: point_count + 1,
        poll: Duration::from_millis(5),
        deadline,
        token,
        validation: Validation::Worker { busy, validate },
    })
}

fn check_wait(deadline: Instant, token: Option<&CancellationToken>) -> Result<(), AutomationError> {
    if let Some(token) = token {
        token.check()?;
    }
    remaining(deadline, false)?;
    Ok(())
}

fn write_pipe(
    pipe: &mut impl Write,
    bytes: &[u8],
    message: &str,
) -> Result<usize, AutomationError> {
    match pipe.write(bytes) {
        Ok(count) => Ok(count),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::Interrupted | ErrorKind::BrokenPipe
            ) =>
        {
            Ok(0)
        } // An exiting helper can still have a terminal result to read.
        Err(_) => Err(protocol(message)),
    }
}

fn collect_frames(
    frames: &mut Vec<u8>,
    output: &mut Vec<u8>,
    mut validate: impl FnMut(&Value) -> Result<(), AutomationError>,
) -> Result<(), AutomationError> {
    let mut cursor = 0;
    while let Some(end) = frames[cursor..].iter().position(|b| *b == b'\n') {
        let end = cursor + end + 1;
        let line = &frames[cursor..end];
        cursor = end;
        let value: Value =
            serde_json::from_slice(line).map_err(|_| protocol("malformed helper frame"))?;
        if value["event"] == "validate_segment" {
            validate(&value)?;
        } else {
            if output.len() + line.len() > XTEST_OUTPUT_LIMIT {
                return Err(protocol("helper output exceeded its limit"));
            }
            output.extend_from_slice(line);
            if value["event"] == "dispatch_started" {
                validate(&value)?;
            } else if value["event"] != "input_progress" && !value["ok"].is_boolean() {
                return Err(protocol("unknown helper frame"));
            }
        }
    }
    frames.drain(..cursor);
    Ok(())
}

pub(super) fn run(request: Request<'_>) -> Result<HelperOutput, AutomationError> {
    let Request {
        path,
        args,
        nonce,
        payload,
        handshakes,
        poll,
        deadline,
        token,
        validation,
    } = request;
    check_wait(deadline, token)?;
    // Retained by a blocked read-only worker after cancellation. A second path
    // cannot accumulate more DBus workers while that one is still returning.
    let permit = if let Validation::Worker { busy, .. } = &validation {
        busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| {
                driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "an earlier drag validation is still returning",
                )
            })?;
        Some(Arc::new(WorkerPermit(busy.clone())))
    } else {
        None
    };
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
        .map_err(|_| driver_error("DRIVER.UNAVAILABLE", "anchored helper could not be started"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    for fd in [
        child.stdin.as_ref().unwrap().as_raw_fd(),
        stdout.as_raw_fd(),
        stderr.as_raw_fd(),
    ] {
        if nonblocking(fd).is_err() {
            terminate(&mut child);
            return Err(protocol("helper pipes could not be made nonblocking"));
        }
    }
    let payload = payload
        .map(|bytes| {
            let mut frame = format!("text {}\n", bytes.len()).into_bytes();
            frame.extend_from_slice(bytes);
            frame
        })
        .unwrap_or_default();
    let mut pending = payload.as_slice();
    let mut frames = Vec::new();
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let mut next = 0usize;
    let mut worker: Option<mpsc::Receiver<Result<(), AutomationError>>> = None;
    let mut ready = false;
    let result = (|| loop {
        check_wait(deadline, token)?;
        let stdin = child.stdin.as_mut().expect("piped stdin");
        if !pending.is_empty() {
            let count = write_pipe(stdin, pending, "helper payload could not be written")?;
            pending = &pending[count..];
        }
        let status = child
            .try_wait()
            .map_err(|_| protocol("helper state unavailable"))?;
        drain(&mut stdout, &mut frames)?;
        drain(&mut stderr, &mut diagnostics)?;
        collect_frames(&mut frames, &mut output, |value| {
            if value["event"] == "dispatch_started" {
                if next == 0 {
                    return Err(protocol("helper dispatched before validation"));
                }
                return Ok(());
            }
            if worker.is_some()
                || ready
                || !pending.is_empty()
                || next >= handshakes
                || value["token"] != nonce
                || value["index"].as_u64() != Some(next as u64)
            {
                return Err(protocol("unexpected helper validation sequence"));
            }
            if status.is_some() {
                return Ok(());
            }
            match &validation {
                Validation::Inline(validate) => {
                    validate(next)?;
                    ready = true;
                }
                Validation::Worker { validate, .. } => {
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
                }
            }
            Ok(())
        })?;
        if let Some(status) = status {
            if status.success() && next != handshakes {
                return Err(protocol("helper skipped validation"));
            }
            return Ok(status);
        }
        if let Some(receiver) = &worker {
            match receiver.try_recv() {
                Ok(result) => {
                    result?;
                    worker = None;
                    ready = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(protocol("drag validation worker failed"));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if ready {
            check_wait(deadline, token)?;
            let ack = format!("ack {nonce} {next}\n");
            let stdin = child.stdin.as_mut().expect("piped stdin");
            match write_pipe(stdin, ack.as_bytes(), "helper ACK could not be written")? {
                count if count == ack.len() => {
                    ready = false;
                    next += 1;
                }
                0 => {}
                _ => return Err(protocol("helper ACK was truncated")),
            }
        }
        std::thread::sleep(poll);
    })();
    match result {
        Ok(status) => Ok(HelperOutput {
            status,
            stdout: output,
            stderr: diagnostics,
        }),
        Err(error) => {
            let clean = terminate_with_grace(&mut child, Duration::from_millis(500));
            let _ = drain(&mut stdout, &mut frames);
            // Include evidence written during cleanup, even if a malformed
            // frame caused the protocol loop to stop.
            output.extend_from_slice(&frames);
            let dispatched = helper_dispatched(&output);
            Err(with_helper_progress(
                error
                    .with_effect(if dispatched || !clean {
                        "unknown"
                    } else {
                        "not_applied"
                    })
                    .with_detail("dispatch_started", json!(dispatched))
                    .with_detail("helper_exited_before_kill", json!(clean)),
                &output,
            ))
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
        let result = run_drag(
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
            let error = run_drag(
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
    fn inline_handshakes_keep_payload_and_acks_separate_without_retaining_frames() {
        let script = Script::new(&format!(
            r#"
            IFS= read -r header
            [ "$header" = "text 5" ] || exit 1
            [ "$(dd bs=1 count=5 status=none)" = "$(printf 'hi\nab')" ] || exit 2
            index=0
            while [ "$index" -lt 1024 ]; do
                printf '{{"event":"validate_segment","token":"{NONCE}","index":%s}}\n' "$index"
                IFS= read -r ack
                [ "$ack" = "ack {NONCE} $index" ] || exit 3
                index=$((index + 1))
            done
            printf '{{"ok":true}}\n'
            "#,
        ));
        let result = run(Request {
            path: &script.0,
            args: &[],
            nonce: NONCE,
            payload: Some(b"hi\nab"),
            handshakes: 1024,
            poll: Duration::from_millis(1),
            deadline: Instant::now() + Duration::from_secs(10),
            token: None,
            validation: Validation::Inline(&|_| Ok(())),
        })
        .unwrap();
        assert!(result.status.success(), "{:?}", result.stdout);
        assert_eq!(result.stdout, b"{\"ok\":true}\n");
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
            let failure = run_drag(
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
            let denied = run_drag(
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

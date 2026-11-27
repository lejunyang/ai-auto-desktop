//! Exercise actual shipped CLI processes, including the persistent proxy path.
use aad_control::{Operation, Request, Response, PROTOCOL_VERSION};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const AAD: &str = env!("CARGO_BIN_EXE_aad");

fn with_remote(args: &[&str], argv: &[&str]) -> Command {
    let mut command = Command::new(AAD);
    command.args(args).arg(serde_json::to_string(argv).unwrap());
    command
}

fn call(input: &mut impl Write, output: &mut impl BufRead, request: Request) -> Response {
    writeln!(input, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    input.flush().unwrap();
    let mut text = String::new();
    output.read_line(&mut text).unwrap();
    assert!(text.ends_with('\n'), "{text:?}");
    serde_json::from_str(&text).unwrap()
}

fn session(proxy: bool) {
    let mut command = if proxy {
        with_remote(
            &["control", "connect", "--command"],
            &[AAD, "control", "serve"],
        )
    } else {
        let mut command = Command::new(AAD);
        command.args(["control", "serve"]);
        command
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut call = |request| call(&mut input, &mut output, request);
    let hello = call(Request::new(None, Operation::Capabilities {}));
    assert!(hello.error.is_none(), "{hello:?}");
    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
    assert_eq!(hello.data.unwrap()["allow_input"], false);
    let id = hello.session_id;
    let targets = call(Request::new(
        Some(id.clone()),
        Operation::Targets { provider: None },
    ));
    assert!(targets.error.is_none(), "{targets:?}");
    assert!(targets.data.unwrap()["resources"].is_array());
    let closed = call(Request::new(Some(id), Operation::Close {}));
    assert!(closed.error.is_none(), "{closed:?}");
    drop(input);
    assert!(child.wait().unwrap().success());
}
#[test]
fn native_cli_session_has_real_handshake_lifecycle_and_read_only_default() {
    session(false);
}
#[test]
fn cli_remote_proxy_keeps_one_child_session_alive_across_requests() {
    session(true);
}

#[cfg(unix)]
fn failed_startup(mut command: Command) -> String {
    let output = command.output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    stderr
}

#[cfg(unix)]
#[test]
fn cli_proxy_explains_invalid_stdout_without_echoing_the_frame() {
    let stderr = failed_startup(with_remote(
        &["control", "connect", "--command"],
        &[
            "/bin/sh",
            "-c",
            "printf '%s\\n' 'aad fixture: starting control serve'; exec \"$1\" control serve",
            "fixture",
            AAD,
        ],
    ));
    assert!(stderr.starts_with("aad control: --command:"), "{stderr}");
    assert!(stderr.contains("invalid response frame"), "{stderr}");
    assert!(
        stderr.contains("stdout must contain only control protocol NDJSON"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Send startup messages to stderr"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("aad fixture: starting control serve"),
        "{stderr}"
    );
    assert!(!stderr.contains("child exited"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn cli_proxy_and_mcp_report_the_child_exit_code() {
    for (args, prefix) in [
        (
            &["control", "connect", "--command"][..],
            "aad control: --command:",
        ),
        (&["mcp", "--connect"][..], "aad mcp: --connect:"),
    ] {
        let stderr = failed_startup(with_remote(args, &["/bin/sh", "-c", "exit 23"]));
        assert!(stderr.starts_with(prefix), "{stderr}");
        assert!(
            stderr.contains("child exited before the remote handshake (exit code 23)"),
            "{stderr}"
        );
        assert!(
            stderr.contains("Run the command separately and check its exit code and stderr"),
            "{stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn cli_proxy_reports_invalid_stdout_and_the_child_exit_code_together() {
    let stderr = failed_startup(with_remote(
        &["control", "connect", "--command"],
        &[
            "/bin/sh",
            "-c",
            "exec 0<&-; printf '%s\\n' 'private fixture banner'; exit 23",
        ],
    ));
    assert!(stderr.starts_with("aad control: --command:"), "{stderr}");
    assert!(stderr.contains("invalid response frame"), "{stderr}");
    assert!(stderr.contains("exit code 23"), "{stderr}");
    assert!(
        stderr.contains("Send startup messages to stderr"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Run the command separately and check its exit code and stderr"),
        "{stderr}"
    );
    assert!(!stderr.contains("private fixture banner"), "{stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn cli_proxy_waits_for_natural_exit_after_stdout_closes() {
    let stderr = failed_startup(with_remote(
        &["control", "connect", "--command"],
        &["/bin/sh", "-c", "exec 1>&-; sleep 0.1; exit 23"],
    ));
    assert!(
        stderr.contains("child exited before the remote handshake (exit code 23)"),
        "{stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cli_proxy_reports_closed_stdout_without_waiting_for_the_handshake_timeout() {
    use std::time::{Duration, Instant};

    let started = Instant::now();
    let stderr = failed_startup(with_remote(
        &["control", "connect", "--command"],
        &["/bin/sh", "-c", "exec 1>&-; exec sleep 30"],
    ));
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}: {stderr}");
    assert!(stderr.starts_with("aad control: --command:"), "{stderr}");
    assert!(
        stderr.contains(
            "child stdout closed before the remote handshake while the child was still running"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("stdout must contain only control protocol NDJSON"),
        "{stderr}"
    );
    assert!(stderr.contains("stdout redirection"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn cli_proxy_keeps_stderr_banner_out_of_the_session_stream() {
    let mut child = with_remote(
        &["control", "connect", "--command"],
        &[
            "/bin/sh",
            "-c",
            "printf '%s\\n' 'aad fixture: starting control serve' >&2; exec \"$1\" control serve",
            "fixture",
            AAD,
        ],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let hello = call(
        &mut input,
        &mut output,
        Request::new(None, Operation::Capabilities {}),
    );
    assert!(hello.error.is_none(), "{hello:?}");
    assert!(!hello.session_id.is_empty());
    assert_eq!(hello.data.unwrap()["session_id"], hello.session_id);
    let closed = call(
        &mut input,
        &mut output,
        Request::new(Some(hello.session_id.clone()), Operation::Close {}),
    );
    assert_eq!(closed.session_id, hello.session_id);
    assert!(closed.error.is_none(), "{closed:?}");
    assert_eq!(closed.data.unwrap()["closed"], true);
    drop(input);
    let mut extra = String::new();
    assert_eq!(output.read_line(&mut extra).unwrap(), 0, "{extra:?}");
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stderr, b"aad fixture: starting control serve\n");
}

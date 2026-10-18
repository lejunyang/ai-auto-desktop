//! Exercise actual shipped CLI processes, including the persistent proxy path.
use aad_control::{Operation, Request, Response, PROTOCOL_VERSION};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn session(proxy: bool) {
    let executable = env!("CARGO_BIN_EXE_aad");
    let mut command = Command::new(executable);
    if proxy {
        command.args([
            "control",
            "connect",
            "--command",
            &serde_json::to_string(&vec![executable, "control", "serve"]).unwrap(),
        ]);
    } else {
        command.args(["control", "serve"]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut call = |request: Request| {
        writeln!(input, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        input.flush().unwrap();
        let mut text = String::new();
        output.read_line(&mut text).unwrap();
        serde_json::from_str::<Response>(&text).unwrap()
    };
    let hello = call(Request::new(None, Operation::Capabilities {}));
    assert!(hello.error.is_none(), "{hello:?}");
    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
    assert_eq!(hello.data.unwrap()["allow_input"], false);
    let id = hello.session_id;
    let targets = call(Request::new(Some(id.clone()), Operation::Targets {}));
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

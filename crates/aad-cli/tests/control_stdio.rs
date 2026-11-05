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
    for cancelled in [false, true] {
        let mut seed = Request::new(Some(id.clone()), Operation::Capabilities {});
        let close_id = if cancelled {
            "cancelled-close"
        } else {
            "duplicate-close"
        };
        seed.request_id = close_id.into();
        if cancelled {
            seed.request_id = "cancel-seed".into();
            seed.operation = Operation::Cancel {
                request_id: close_id.into(),
            };
        }
        assert!(call(seed).error.is_none());
        let mut close = Request::new(Some(id.clone()), Operation::Close {});
        close.request_id = close_id.into();
        let rejected = call(close);
        assert_eq!(rejected.effect, "not_applied");
        assert_eq!(
            rejected.error.unwrap()["code"],
            if cancelled {
                "CONTROL.CANCELLED"
            } else {
                "CONTROL.DUPLICATE_REQUEST"
            }
        );
        let query = call(Request::new(Some(id.clone()), Operation::Capabilities {}));
        assert!(query.error.is_none(), "{query:?}");
        assert_eq!(query.session_id, id);
    }
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

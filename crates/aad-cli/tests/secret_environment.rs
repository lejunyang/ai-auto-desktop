//! Verify the shipped CLI scrubs secret sources before spawning descendants.
use aad_control::{Operation, Request, Response};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SECRET_ENV: &str = "AAD_STARTUP_SECRET_FIXTURE";
const PUBLIC_ENV: &str = "AAD_STARTUP_PUBLIC_FIXTURE";
const OUTPUT_ENV: &str = "AAD_STARTUP_PROBE_OUTPUT";

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Invoked in a separate test process by the real server's launch action.
#[test]
fn secret_environment_probe() {
    let Some(output) = std::env::var_os(OUTPUT_ENV) else {
        return;
    };
    let result = json!({
        "secret_present": std::env::var_os(SECRET_ENV).is_some(),
        "public_preserved": std::env::var(PUBLIC_ENV).ok().as_deref() == Some("public-marker")
    });
    std::fs::write(output, result.to_string()).unwrap();
}

#[test]
fn startup_extracts_aliases_then_removes_secret_environment_from_children() {
    let directory = std::env::temp_dir().join(format!("aad-secret-env-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let output_path = directory.join("probe.json");
    let probe = std::env::current_exe().unwrap();
    let mut child = Server(
        Command::new(env!("CARGO_BIN_EXE_aad"))
            .args([
                "control",
                "serve",
                "--provider",
                "environment-probe-only",
                "--allow-input",
                "--allow-secret-input",
                "--secret-env",
            ])
            .arg(format!("first={SECRET_ENV}"))
            .arg("--secret-env")
            .arg(format!("second={SECRET_ENV}"))
            .arg("--allow-launch")
            .arg(&probe)
            .env(SECRET_ENV, "synthetic-fixture-secret")
            .env(PUBLIC_ENV, "public-marker")
            .env(OUTPUT_ENV, &output_path)
            // This process launches only the probe above; it never drives a GUI.
            // Its private temporary directory keeps the input lease test isolated.
            .env("TMPDIR", &directory)
            .env("TMP", &directory)
            .env("TEMP", &directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut call = |request: Request| {
        writeln!(input, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        assert!(!line.contains("synthetic-fixture-secret"));
        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(response.error.is_none(), "{response:?}");
        response
    };
    let hello = call(Request::new(None, Operation::Capabilities {}));
    assert_eq!(
        hello.data.unwrap()["secret_bindings"],
        json!(["first", "second"])
    );
    let id = hello.session_id;
    call(Request::new(
        Some(id.clone()),
        Operation::Launch {
            program: probe.to_string_lossy().into_owned(),
            args: vec!["--exact".into(), "secret_environment_probe".into()],
            wait_ms: Some(0),
        },
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !output_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let result: Value = serde_json::from_slice(&std::fs::read(&output_path).unwrap()).unwrap();
    assert_eq!(
        result,
        json!({"secret_present":false,"public_preserved":true})
    );
    call(Request::new(Some(id), Operation::Close {}));
    drop(input);
    assert!(child.0.wait().unwrap().success());
    let stderr = child.0.stderr.take().unwrap();
    let stderr = std::io::read_to_string(stderr).unwrap();
    assert!(!stderr.contains("synthetic-fixture-secret"));
    drop(child);
    std::fs::remove_dir_all(directory).unwrap();
}

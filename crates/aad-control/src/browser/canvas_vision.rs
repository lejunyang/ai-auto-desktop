//! One-shot, host-selected local grounding for PNGs from `visible_canvas`.
//! The process reads one UTF-8 JSON object on stdin (through EOF) and writes
//! one graph on stdout. See canvas_vision.py for the contract.
use crate::{error, Result};
use aad_core::AutomationError;
use base64::Engine;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MAX_OUTPUT: u64 = 128 * 1024;

fn unavailable(reason: &str) -> AutomationError {
    error("CONTROL.CANVAS_VISION_UNAVAILABLE", reason)
}

fn timeout() -> AutomationError {
    error("CONTROL.TIMEOUT", "canvas vision deadline elapsed")
}

fn bad_output() -> AutomationError {
    error(
        "CONTROL.BAD_PROVIDER_OUTPUT",
        "invalid canvas vision provider response",
    )
}

fn command() -> Result<Command> {
    let interpreter = std::env::var_os("AAD_CANVAS_VISION_PYTHON")
        .unwrap_or_else(|| if cfg!(windows) { "python" } else { "python3" }.into());
    let mut python = Command::new(interpreter);
    python.args(["-I", "-B"]);
    if let Some(custom) = std::env::var_os("AAD_CANVAS_VISION_PROVIDER") {
        let path = PathBuf::from(custom);
        if !path.is_absolute() || !path.is_file() {
            return Err(unavailable(
                "AAD_CANVAS_VISION_PROVIDER must name an absolute local provider file",
            ));
        }
        if path.extension().is_some_and(|ext| ext == "py") {
            python.arg(path);
            return Ok(python);
        }
        return Ok(Command::new(path));
    }
    let model = std::env::var_os("AAD_CANVAS_VISION_MODEL").map(PathBuf::from);
    if !model.is_some_and(|p| p.is_absolute() && p.is_dir()) {
        return Err(unavailable("set AAD_CANVAS_VISION_MODEL to an absolute local OWL checkpoint directory, or set AAD_CANVAS_VISION_PROVIDER"));
    }
    python.args(["-c", include_str!("canvas_vision.py")]);
    Ok(python)
}

#[derive(Serialize)]
struct Request<'a> {
    description: &'a str,
    format: &'static str,
    width: u64,
    height: u64,
    data: &'a str,
}

fn request<'a>(shot: &'a Value, description: &'a str) -> Result<Request<'a>> {
    if description.trim().is_empty() || description.len() > 1024 {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "provide a description of 1..1024 UTF-8 bytes",
        ));
    }
    let invalid = || {
        error(
            "CONTROL.CANVAS_SCOPE_UNPROVEN",
            "expected a bounded verified canvas PNG",
        )
    };
    let width = shot["width"].as_u64().ok_or_else(invalid)?;
    let height = shot["height"].as_u64().ok_or_else(invalid)?;
    if shot["format"] != "png"
        || (shot["clipped"] != false && !shot["canvas_proof"].is_object())
        || !(1..=2048).contains(&width)
        || !(1..=2048).contains(&height)
        || width * height > 2_000_000
    {
        return Err(invalid());
    }
    let data = shot["data"]
        .as_str()
        .filter(|s| s.len() <= 12_000_000)
        .ok_or_else(invalid)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| invalid())?;
    let size = image::io::Reader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png)
        .into_dimensions()
        .map_err(|_| invalid())?;
    if size != (width as u32, height as u32) {
        return Err(invalid());
    }
    Ok(Request {
        description,
        format: "png",
        width,
        height,
        data,
    })
}

struct Worker {
    child: Child,
    #[cfg(windows)]
    _job: aad_plugin::windows_job::Job,
}

impl Drop for Worker {
    fn drop(&mut self) {
        #[cfg(unix)]
        // The worker was started as its own process group leader.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        self._job.terminate();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(mut command: Command) -> Result<Worker> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HF_HUB_OFFLINE", "1")
        .env("TRANSFORMERS_OFFLINE", "1")
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .env("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    let job = {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0004); // CREATE_NO_WINDOW | CREATE_SUSPENDED
        aad_plugin::windows_job::Job::create()
            .ok_or_else(|| unavailable("cannot contain canvas vision process"))?
    };
    let child = command.spawn().map_err(|_| {
        unavailable("cannot start local canvas vision executable or Python interpreter")
    })?;
    #[cfg(windows)]
    {
        if !job.assign(&child) {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable("cannot contain canvas vision process"));
        }
        aad_plugin::windows_job::resume(&child);
    }
    Ok(Worker {
        child,
        #[cfg(windows)]
        _job: job,
    })
}

fn run(command: Command, input: Vec<u8>, deadline: Instant) -> Result<Vec<u8>> {
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(timeout)
    };
    left()?;
    let mut worker = spawn(command)?;
    let mut stdin = worker.child.stdin.take().expect("piped stdin");
    let stdout = worker.child.stdout.take().expect("piped stdout");
    // Either pipe can block independently, including when a provider never reads its input.
    let (written_tx, written_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = written_tx.send(stdin.write_all(&input));
    });
    let (out_tx, out_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = stdout
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut buf)
            .map(|_| buf);
        let _ = out_tx.send(result);
    });
    let output = out_rx
        .recv_timeout(left()?)
        .map_err(|_| timeout())?
        .map_err(|_| bad_output())?;
    if output.len() as u64 > MAX_OUTPUT {
        return Err(bad_output());
    }
    let status = loop {
        if let Some(status) = worker.child.try_wait().map_err(|_| bad_output())? {
            break status;
        }
        std::thread::sleep(left()?.min(Duration::from_millis(5)));
    };
    match status.code() {
        Some(0) => (),
        Some(2) => return Err(error("CONTROL.INVALID_REQUEST", "provider rejected the PNG or description (OWL requires a short phrase)")),
        Some(3) => return Err(unavailable("cannot load local detector: check the Python environment and OWL checkpoint with safetensors weights")),
        _ => return Err(error("CONTROL.CANVAS_VISION_FAILED", "local canvas vision inference failed")),
    }
    written_rx
        .recv_timeout(left()?)
        .map_err(|_| timeout())?
        .map_err(|_| bad_output())?;
    Ok(output)
}

fn graph(bytes: &[u8]) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| bad_output())?;
    let objects = value["objects"]
        .as_array()
        .filter(|v| v.len() <= 64)
        .ok_or_else(bad_output)?;
    if value.as_object().map(|o| o.len()) != Some(2)
        || !value["edges"].as_array().is_some_and(Vec::is_empty)
    {
        return Err(bad_output());
    }
    let mut ids = HashSet::new();
    for object in objects {
        let confidence = object.get("confidence");
        if object.as_object().map(|o| o.len()) != Some(4 + usize::from(confidence.is_some()))
            || confidence.is_some_and(|c| !c.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v)))
        {
            return Err(bad_output());
        }
        for (key, limit) in [("id", 128), ("kind", 64), ("label", 1024)] {
            let s = object[key]
                .as_str()
                .filter(|s| !s.trim().is_empty() && s.len() <= limit)
                .ok_or_else(bad_output)?;
            if key == "id" && !ids.insert(s) {
                return Err(bad_output());
            }
        }
        let bounds = &object["box"];
        if bounds.as_object().map(|o| o.len()) != Some(4) {
            return Err(bad_output());
        }
        for (start, extent) in [("fx", "fw"), ("fy", "fh")] {
            let start = bounds[start].as_f64().ok_or_else(bad_output)?;
            let extent = bounds[extent].as_f64().ok_or_else(bad_output)?;
            if !(0.0..1.0).contains(&start) || extent <= 0.0 || start + extent > 1.0 {
                return Err(bad_output());
            }
        }
    }
    Ok(value)
}

/// Only the browser's scope- and pixel-verified screenshot may enter this boundary.
pub(super) fn detect(shot: &Value, description: &str, timeout_budget: Duration) -> Result<Value> {
    if timeout_budget.is_zero() {
        return Err(timeout());
    }
    let deadline = Instant::now() + timeout_budget.min(Duration::from_secs(120));
    let request = request(shot, description)?;
    let command = command()?;
    let input = serde_json::to_vec(&request)
        .map_err(|_| error("CONTROL.INVALID_REQUEST", "cannot serialize vision request"))?;
    let graph = graph(&run(command, input, deadline)?)?;
    if Instant::now() >= deadline {
        return Err(timeout());
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn optional_confidence_must_be_a_unit_interval_number() {
        let mut scene = json!({"objects":[{"id":"v1","kind":"visual","label":"bicycle",
            "box":{"fx":0.0,"fy":0.0,"fw":1.0,"fh":1.0}}],"edges":[]});
        let parse = |scene: &Value| graph(&serde_json::to_vec(scene).unwrap());
        assert_eq!(parse(&scene).unwrap(), scene);
        for score in [json!(0.0), json!(0.675), json!(1.0)] {
            scene["objects"][0]["confidence"] = score;
            assert_eq!(parse(&scene).unwrap(), scene);
        }
        for invalid in [Value::Null, json!("0.7"), json!(-0.01), json!(1.01)] {
            scene["objects"][0]["confidence"] = invalid;
            assert_eq!(
                parse(&scene).unwrap_err().code,
                "CONTROL.BAD_PROVIDER_OUTPUT"
            );
        }
    }

    #[test]
    fn reject_ambiguous_ids_and_boxes_extending_past_the_canvas() {
        let mut object = json!({"id":"v1","kind":"visual","label":"folder",
            "box":{"fx":0.8,"fy":0.3,"fw":0.2,"fh":0.1}});
        let duplicates = json!({"objects":[object,object],"edges":[]});
        assert!(graph(&serde_json::to_vec(&duplicates).unwrap()).is_err());
        object["box"]["fw"] = json!(0.25);
        let outside = json!({"objects":[object],"edges":[]});
        assert!(graph(&serde_json::to_vec(&outside).unwrap()).is_err());
    }
}

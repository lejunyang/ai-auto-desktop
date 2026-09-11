//! Native Tesseract OCR capability provider.

use aad_core::AutomationError;
use aad_plugin::{manifest, CapabilityManifest};
use aad_runtime::artifacts::{detect_media_type, ArtifactStore};
use aad_runtime::provider::Provider;
use image::ImageFormat;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

pub const PROVIDER_NAME: &str = "vision.ocr";
pub const PROVIDER_VERSION: &str = "0.1.1";
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 20_000;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_ENGINE_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENGINE_STDERR_BYTES: usize = 64 * 1024;
const MAX_TSV_ROWS: usize = 20_000;
const MAX_WORDS: usize = 20_000;
const MAX_LINES: usize = 10_000;
const MAX_TEXT_CHARS: usize = 1_000_000;
const MAX_WORD_TEXT_CHARS: usize = 4_096;
const MAX_MATCHES: usize = 10_000;
const MAX_COORDINATE: u64 = 1_000_000_000;
const RESPONSE_BUDGET: Duration = Duration::from_millis(1300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    x: u64,
    y: u64,
    width: u64,
    height: u64,
}

impl Bounds {
    fn to_json(self) -> Value {
        json!({"x": self.x, "y": self.y, "width": self.width, "height": self.height})
    }
}

#[derive(Clone, Debug)]
struct Word {
    text: String,
    confidence: f64,
    bounds: Bounds,
}

#[derive(Clone, Debug)]
struct Line {
    text: String,
    confidence: f64,
    bounds: Bounds,
    words: Vec<Word>,
}

pub struct OcrProvider {
    manifest: CapabilityManifest,
    engine: Vec<String>,
    allow_unsandboxed: bool,
    version: Mutex<Option<String>>,
}

impl OcrProvider {
    pub fn new() -> Result<Self, AutomationError> {
        let engine = engine_command()?;
        let allow_unsandboxed =
            std::env::var_os("OCR_ALLOW_UNSANDBOXED_ENGINE").is_some_and(|value| value == "1");
        Ok(Self {
            manifest: manifest::parse(&manifest_document()).map_err(|reason| {
                ocr_error(
                    "OCR.INTERNAL",
                    format!("built-in manifest is invalid: {reason}"),
                )
            })?,
            engine,
            allow_unsandboxed,
            version: Mutex::new(None),
        })
    }

    pub fn with_command(
        engine: Vec<String>,
        allow_unsandboxed: bool,
    ) -> Result<Self, AutomationError> {
        if engine.is_empty() || engine.iter().any(String::is_empty) {
            return Err(ocr_error(
                "OCR.ENGINE_UNAVAILABLE",
                "Tesseract command must be a non-empty argv array",
            ));
        }
        Ok(Self {
            manifest: manifest::parse(&manifest_document()).map_err(|reason| {
                ocr_error(
                    "OCR.INTERNAL",
                    format!("built-in manifest is invalid: {reason}"),
                )
            })?,
            engine,
            allow_unsandboxed,
            version: Mutex::new(None),
        })
    }

    fn recognize(
        &self,
        action: &str,
        args: Value,
        timeout: Duration,
        artifacts: Option<&ArtifactStore>,
    ) -> Result<Value, AutomationError> {
        if timeout <= RESPONSE_BUDGET {
            return Err(ocr_error(
                "OCR.TIMEOUT",
                "host deadline elapsed before OCR could complete",
            )
            .with_retryable(true));
        }
        let deadline = Instant::now() + timeout - RESPONSE_BUDGET;
        let args = args
            .as_object()
            .ok_or_else(|| invalid("args must be an object"))?;
        let region = parse_region(args.get("region"))?;
        let languages = parse_languages(args.get("languages"))?;
        let threshold = parse_threshold(args.get("minimum_confidence"))?;
        let patterns = parse_patterns(args.get("patterns"))?;
        let (bytes, source) = if action.ends_with("recognize_artifact@1") {
            require_keys(
                args,
                &[
                    "artifact",
                    "region",
                    "languages",
                    "minimum_confidence",
                    "patterns",
                ],
            )?;
            let reference = args
                .get("artifact")
                .ok_or_else(|| invalid("artifact is required"))?;
            let bytes = artifacts
                .ok_or_else(|| {
                    ocr_error(
                        "OCR.ARTIFACT_IPC",
                        "artifact action requires the host artifact boundary",
                    )
                })?
                .resolve(reference)
                .map_err(|cause| {
                    ocr_error(
                        "OCR.ARTIFACT_IPC",
                        "Host-managed artifact validation failed",
                    )
                    .with_cause(cause)
                })?;
            let reference = aad_runtime::ArtifactRef::from_value(reference).map_err(|cause| {
                ocr_error(
                    "OCR.ARTIFACT_IPC",
                    "Host-managed artifact reference is invalid",
                )
                .with_cause(cause)
            })?;
            (
                bytes,
                json!({
                    "kind": "artifact",
                    "digest": reference.digest,
                    "media_type": reference.media_type,
                    "size_bytes": reference.size_bytes,
                }),
            )
        } else {
            require_keys(
                args,
                &[
                    "image",
                    "artifact",
                    "region",
                    "languages",
                    "minimum_confidence",
                    "patterns",
                ],
            )?;
            read_legacy_source(args)?
        };
        remaining(deadline)?;
        let media_type = detect_media_type(&bytes).ok_or_else(|| {
            ocr_error(
                "OCR.IMAGE_UNSUPPORTED",
                "source file does not have a supported image signature",
            )
        })?;
        let format = image_format(media_type);
        reject_multiple_frames(&bytes, format)?;
        let image = image::load_from_memory_with_format(&bytes, format).map_err(|_| {
            ocr_error(
                "OCR.IMAGE_UNSUPPORTED",
                "source image could not be decoded safely",
            )
        })?;
        check_dimensions(image.width(), image.height(), "decoder_header")?;
        let (engine_bytes, offset) = if let Some(region) = region {
            if region.x.saturating_add(region.width) > u64::from(image.width())
                || region.y.saturating_add(region.height) > u64::from(image.height())
            {
                return Err(invalid("region falls outside the source image")
                    .with_detail("image_width", json!(image.width()))
                    .with_detail("image_height", json!(image.height())));
            }
            let cropped = image.crop_imm(
                region.x as u32,
                region.y as u32,
                region.width as u32,
                region.height as u32,
            );
            let mut encoded = Vec::new();
            cropped
                .write_to(&mut std::io::Cursor::new(&mut encoded), ImageFormat::Png)
                .map_err(|_| {
                    ocr_error("OCR.IMAGE_UNSUPPORTED", "source image could not be cropped")
                })?;
            (encoded, (region.x, region.y))
        } else {
            (bytes, (0, 0))
        };
        remaining(deadline)?;
        let temporary = TemporaryImage::new(&engine_bytes)?;
        let version = self.engine_version(deadline)?;
        let mut command = self.engine.clone();
        command.extend([temporary.path().display().to_string(), "stdout".into()]);
        if !languages.is_empty() {
            command.extend(["-l".into(), languages.join("+")]);
        }
        command.push("tsv".into());
        let (stdout, _) = run_engine(&command, deadline, self.allow_unsandboxed)?;
        let (text, confidence, lines) = parse_tsv(&stdout, offset, deadline)?;
        if confidence < threshold {
            return Err(ocr_error(
                "OCR.LOW_CONFIDENCE",
                "recognized text is below minimum_confidence",
            )
            .with_detail("confidence", json!(confidence))
            .with_detail("minimum_confidence", json!(threshold)));
        }
        let matches = find_matches(&text, &patterns, &lines, deadline)?;
        Ok(json!({
            "provider": "tesseract",
            "version": version,
            "source": source,
            "source_region": region.map(Bounds::to_json),
            "text": text,
            "confidence": confidence,
            "lines": lines.iter().map(line_json).collect::<Vec<_>>(),
            "matches": matches,
        }))
    }

    pub fn recognize_path(&self, args: Value, timeout: Duration) -> Result<Value, AutomationError> {
        self.recognize("vision.ocr.recognize@1", args, timeout, None)
    }

    pub fn recognize_artifact(
        &self,
        args: Value,
        timeout: Duration,
        artifacts: &ArtifactStore,
    ) -> Result<Value, AutomationError> {
        self.recognize(
            "vision.ocr.recognize_artifact@1",
            args,
            timeout,
            Some(artifacts),
        )
    }

    fn engine_version(&self, deadline: Instant) -> Result<String, AutomationError> {
        if let Ok(guard) = self.version.lock() {
            if let Some(version) = guard.clone() {
                return Ok(version);
            }
        }
        let mut command = self.engine.clone();
        command.push("--version".into());
        let (stdout, stderr) = run_engine(&command, deadline, self.allow_unsandboxed)?;
        let output = if stdout.is_empty() { &stderr } else { &stdout };
        let rendered = String::from_utf8_lossy(output);
        let first = rendered.lines().next().unwrap_or("unknown").to_string();
        let version = first
            .split_whitespace()
            .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("unknown")
            .to_string();
        if let Ok(mut guard) = self.version.lock() {
            *guard = Some(version.clone());
        }
        Ok(version)
    }
}

impl Provider for OcrProvider {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    fn invoke(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AutomationError> {
        if self.manifest.resolve(action).is_none() {
            return Err(invalid("unknown OCR action"));
        }
        self.recognize(
            action,
            args,
            timeout.unwrap_or(Duration::from_secs(30)),
            None,
        )
    }

    fn invoke_with_artifacts(
        &self,
        action: &str,
        args: Value,
        timeout: Option<Duration>,
        artifacts: &ArtifactStore,
    ) -> Result<Value, AutomationError> {
        if !action.ends_with("recognize_artifact@1") {
            return self.invoke(action, args, timeout);
        }
        self.recognize(
            action,
            args,
            timeout.unwrap_or(Duration::from_secs(30)),
            Some(artifacts),
        )
    }
}

pub fn manifest_json() -> Value {
    manifest_document()
}

pub fn register(registry: &mut aad_runtime::ProviderRegistry) -> Result<(), AutomationError> {
    registry.insert(std::sync::Arc::new(OcrProvider::new()?));
    Ok(())
}

fn engine_command() -> Result<Vec<String>, AutomationError> {
    let command = if let Ok(raw) = std::env::var("OCR_TESSERACT_COMMAND") {
        serde_json::from_str::<Vec<String>>(&raw).map_err(|_| {
            ocr_error(
                "OCR.ENGINE_UNAVAILABLE",
                "OCR_TESSERACT_COMMAND must be a JSON argv array",
            )
        })?
    } else {
        vec![std::env::var("TESSERACT_CMD").unwrap_or_else(|_| "tesseract".into())]
    };
    if command.is_empty() || command.iter().any(String::is_empty) {
        return Err(ocr_error(
            "OCR.ENGINE_UNAVAILABLE",
            "Tesseract command must be a non-empty argv array",
        ));
    }
    Ok(command)
}

fn run_engine(
    command: &[String],
    deadline: Instant,
    _allow_unsandboxed: bool,
) -> Result<(Vec<u8>, Vec<u8>), AutomationError> {
    remaining(deadline)?;
    let mut argv = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let prlimit = find_executable("prlimit").ok_or_else(|| {
            ocr_error(
                "OCR.ENGINE_ISOLATION_UNAVAILABLE",
                "Linux OCR requires the prlimit command",
            )
        })?;
        let cpu = remaining(deadline)?.as_secs().clamp(1, 30);
        argv.extend([
            prlimit,
            "--as=2147483648:2147483648".into(),
            format!("--cpu={cpu}:{cpu}"),
            "--fsize=16777216:16777216".into(),
            "--nofile=64:64".into(),
            "--".into(),
        ]);
    }
    #[cfg(not(target_os = "linux"))]
    if !_allow_unsandboxed {
        return Err(ocr_error(
            "OCR.ENGINE_ISOLATION_UNAVAILABLE",
            "this platform has no built-in OCR engine resource sandbox",
        ));
    }
    let executable = find_executable(&command[0]).ok_or_else(|| {
        ocr_error(
            "OCR.ENGINE_UNAVAILABLE",
            "Tesseract executable was not found",
        )
        .with_detail("executable", json!(command[0]))
    })?;
    argv.push(executable);
    argv.extend(command[1..].iter().cloned());
    let Some((program, args)) = argv.split_first() else {
        return Err(ocr_error(
            "OCR.ENGINE_UNAVAILABLE",
            "Tesseract command is empty",
        ));
    };
    let mut process = Command::new(program);
    process
        .args(args)
        .env_clear()
        .env("PATH", engine_path())
        .env("OMP_NUM_THREADS", "1")
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in ["LANG", "LC_ALL", "TESSDATA_PREFIX", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    for (name, value) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FAKE_TESSERACT_") {
            process.env(name, value);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = process
        .spawn()
        .map_err(|_| ocr_error("OCR.ENGINE_UNAVAILABLE", "Tesseract could not be started"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (sender, receiver) = mpsc::channel();
    let output_sender = sender.clone();
    let output_thread = std::thread::spawn(move || {
        let _ = output_sender.send(("stdout", read_bounded(&mut stdout, MAX_ENGINE_STDOUT_BYTES)));
    });
    let error_thread = std::thread::spawn(move || {
        let _ = sender.send(("stderr", read_bounded(&mut stderr, MAX_ENGINE_STDERR_BYTES)));
    });
    let mut stdout_result = None;
    let mut stderr_result = None;
    let mut status = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok((stream, result)) => match (stream, result) {
                ("stdout", Ok(value)) => stdout_result = Some(value),
                ("stderr", Ok(value)) => stderr_result = Some(value),
                (stream, Err(error)) => {
                    // A writer may continue forever after filling a pipe. Kill
                    // the whole session as soon as either bounded reader
                    // reports overflow; waiting for the child first can hang.
                    terminate_process_tree(&mut child);
                    let _ = output_thread.join();
                    let _ = error_thread.join();
                    return Err(error.with_detail("stream", json!(stream)));
                }
                _ => unreachable!(),
            },
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(_) => {
                    terminate_process_tree(&mut child);
                    let _ = output_thread.join();
                    let _ = error_thread.join();
                    return Err(ocr_error(
                        "OCR.ENGINE_FAILED",
                        "Tesseract process state could not be read",
                    ));
                }
            }
        }
        if status.is_some() && stdout_result.is_some() && stderr_result.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            terminate_process_tree(&mut child);
            let _ = output_thread.join();
            let _ = error_thread.join();
            return Err(ocr_error(
                "OCR.TIMEOUT",
                "host deadline elapsed while Tesseract was running",
            )
            .with_retryable(true));
        }
    }
    let _ = output_thread.join();
    let _ = error_thread.join();
    while let Ok((stream, result)) = receiver.try_recv() {
        match (stream, result) {
            ("stdout", Ok(value)) => stdout_result = Some(value),
            ("stderr", Ok(value)) => stderr_result = Some(value),
            (stream, Err(error)) => return Err(error.with_detail("stream", json!(stream))),
            _ => unreachable!(),
        }
    }
    let stdout = stdout_result.unwrap_or_default();
    let stderr = stderr_result.unwrap_or_default();
    let status = status.expect("process exited before both output pipes closed");
    if !status.success() {
        return Err(
            ocr_error("OCR.ENGINE_FAILED", "Tesseract exited unsuccessfully")
                .with_detail("returncode", json!(status.code()))
                .with_detail(
                    "stderr",
                    json!(String::from_utf8_lossy(&stderr)
                        .chars()
                        .take(8192)
                        .collect::<String>()),
                ),
        );
    }
    Ok((stdout, stderr))
}

fn engine_path() -> String {
    if cfg!(windows) {
        std::env::var("SystemRoot")
            .map(|root| format!("{};{}\\System32", root, root))
            .unwrap_or_default()
    } else {
        "/usr/bin:/bin".into()
    }
}

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let grace = Instant::now() + Duration::from_millis(200);
    while Instant::now() < grace {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_bounded(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>, AutomationError> {
    let mut output = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut output)
        .map_err(|_| ocr_error("OCR.OUTPUT_INVALID", "Tesseract output could not be read"))?;
    if output.len() > limit {
        return Err(ocr_error(
            "OCR.OUTPUT_INVALID",
            "Tesseract output exceeded the provider limit",
        )
        .with_detail("limit_bytes", json!(limit)));
    }
    Ok(output)
}

fn find_executable(name: &str) -> Option<String> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_string_lossy().into_owned());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|directory| {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
            #[cfg(windows)]
            {
                let executable = directory.join(format!("{name}.exe"));
                if executable.is_file() {
                    return Some(executable.to_string_lossy().into_owned());
                }
            }
            None
        })
    })
}

fn read_legacy_source(args: &Map<String, Value>) -> Result<(Vec<u8>, Value), AutomationError> {
    let choices: Vec<(&str, &Value)> = ["image", "artifact"]
        .into_iter()
        .filter_map(|name| args.get(name).map(|value| (name, value)))
        .collect();
    let [(kind, source)] = choices.as_slice() else {
        return Err(invalid("exactly one of image or artifact is required"));
    };
    let source = source
        .as_object()
        .ok_or_else(|| invalid(format!("{kind} must be an object")))?;
    require_keys(
        source,
        if *kind == "image" {
            &["path"]
        } else {
            &["path", "media_type"]
        },
    )?;
    let path = source
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("source path must be a non-empty string"))?;
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(invalid(
            "source path must be absolute; implicit screenshots are forbidden",
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| ocr_error("OCR.IMAGE_UNAVAILABLE", "source image cannot be accessed"))?;
    let mut file = std::fs::File::open(&canonical)
        .map_err(|_| ocr_error("OCR.IMAGE_UNAVAILABLE", "source image cannot be accessed"))?;
    let metadata = file
        .metadata()
        .map_err(|_| ocr_error("OCR.IMAGE_UNAVAILABLE", "source image cannot be accessed"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() as usize > MAX_IMAGE_BYTES {
        return Err(ocr_error(
            "OCR.IMAGE_UNAVAILABLE",
            "source image must be a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ocr_error("OCR.IMAGE_UNAVAILABLE", "source image could not be read"))?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(ocr_error(
            "OCR.IMAGE_UNAVAILABLE",
            "source image must be between 1 and 67108864 bytes",
        ));
    }
    let media_type = detect_media_type(&bytes).ok_or_else(|| {
        ocr_error(
            "OCR.IMAGE_UNSUPPORTED",
            "source file does not have a supported image signature",
        )
    })?;
    if let Some(declared) = source.get("media_type").and_then(Value::as_str) {
        let declared = declared.to_ascii_lowercase();
        let normalized = if declared == "image/jpg" {
            "image/jpeg"
        } else {
            declared.as_str()
        };
        if normalized != media_type {
            return Err(ocr_error(
                "OCR.IMAGE_UNSUPPORTED",
                "artifact media_type does not match the image content",
            ));
        }
    }
    let mut digest = Sha256::new();
    digest.update(&bytes);
    let provenance = json!({
        "kind": kind, "path": canonical, "digest": format!("sha256:{:x}", digest.finalize()),
        "media_type": media_type, "size_bytes": bytes.len(),
    });
    Ok((bytes, provenance))
}

fn parse_region(value: Option<&Value>) -> Result<Option<Bounds>, AutomationError> {
    let Some(value) = value else { return Ok(None) };
    let object = value
        .as_object()
        .ok_or_else(|| invalid("region must contain only x, y, width, and height"))?;
    if object.len() != 4
        || ["x", "y", "width", "height"]
            .iter()
            .any(|key| !object.contains_key(*key))
    {
        return Err(invalid("region must contain only x, y, width, and height"));
    }
    let integer = |name: &str, minimum: u64| -> Result<u64, AutomationError> {
        object
            .get(name)
            .and_then(Value::as_u64)
            .filter(|value| *value >= minimum && *value <= MAX_COORDINATE)
            .ok_or_else(|| invalid(format!("region.{name} is outside its allowed range")))
    };
    Ok(Some(Bounds {
        x: integer("x", 0)?,
        y: integer("y", 0)?,
        width: integer("width", 1)?,
        height: integer("height", 1)?,
    }))
}

fn parse_languages(value: Option<&Value>) -> Result<Vec<String>, AutomationError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .filter(|values| !values.is_empty() && values.len() <= 32)
        .ok_or_else(|| invalid("languages must be a non-empty array with at most 32 entries"))?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for value in values {
        let value = value
            .as_str()
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 64
                    && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .ok_or_else(|| invalid("each language must match [A-Za-z0-9_]{1,64}"))?;
        if !seen.insert(value) {
            return Err(invalid("languages must be unique"));
        }
        result.push(value.into());
    }
    Ok(result)
}

fn parse_threshold(value: Option<&Value>) -> Result<f64, AutomationError> {
    let Some(value) = value else { return Ok(0.0) };
    value
        .as_f64()
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .ok_or_else(|| invalid("minimum_confidence must be a number between 0 and 1"))
}

fn parse_patterns(value: Option<&Value>) -> Result<Vec<(String, String)>, AutomationError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .filter(|values| values.len() <= 128)
        .ok_or_else(|| invalid("patterns must be an array with at most 128 entries"))?;
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let object = value
            .as_object()
            .filter(|object| {
                object.len() == 2 && object.contains_key("id") && object.contains_key("value")
            })
            .ok_or_else(|| {
                invalid("each pattern must contain only id and value")
                    .with_detail("index", json!(index))
            })?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 128 && !value.contains('\0'))
            .ok_or_else(|| invalid("pattern id must be a bounded non-empty string"))?;
        let literal = object
            .get("value")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 1024 && !value.contains('\0'))
            .ok_or_else(|| invalid("pattern value must be a bounded non-empty string"))?;
        if !ids.insert(id) {
            return Err(invalid("pattern ids must be unique").with_detail("id", json!(id)));
        }
        result.push((id.into(), literal.into()));
    }
    Ok(result)
}

fn parse_tsv(
    payload: &[u8],
    offset: (u64, u64),
    deadline: Instant,
) -> Result<(String, f64, Vec<Line>), AutomationError> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| ocr_error("OCR.OUTPUT_INVALID", "Tesseract TSV is not UTF-8"))?
        .strip_prefix('\u{feff}')
        .unwrap_or_else(|| std::str::from_utf8(payload).unwrap());
    if text.contains('\0') {
        return Err(ocr_error(
            "OCR.OUTPUT_INVALID",
            "Tesseract TSV contains NUL",
        ));
    }
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .flexible(false)
        .from_reader(text.as_bytes());
    let columns = reader
        .headers()
        .map_err(|_| {
            ocr_error(
                "OCR.OUTPUT_INVALID",
                "Tesseract TSV is missing required columns",
            )
        })?
        .clone();
    let required = [
        "level",
        "page_num",
        "block_num",
        "par_num",
        "line_num",
        "left",
        "top",
        "width",
        "height",
        "conf",
        "text",
    ];
    let indexes: BTreeMap<&str, usize> = required
        .iter()
        .filter_map(|name| {
            columns
                .iter()
                .position(|column| column == *name)
                .map(|index| (*name, index))
        })
        .collect();
    if indexes.len() != required.len() {
        return Err(ocr_error(
            "OCR.OUTPUT_INVALID",
            "Tesseract TSV is missing required columns",
        ));
    }
    let mut groups: BTreeMap<(i64, i64, i64, i64), Vec<Word>> = BTreeMap::new();
    let mut word_count = 0usize;
    let mut text_chars = 0usize;
    for (index, row) in reader.records().enumerate() {
        if index >= MAX_TSV_ROWS {
            return Err(ocr_error(
                "OCR.OUTPUT_INVALID",
                "Tesseract TSV exceeded the row limit",
            ));
        }
        if index % 128 == 0 {
            remaining(deadline)?;
        }
        let fields = row.map_err(|_| {
            ocr_error(
                "OCR.OUTPUT_INVALID",
                "Tesseract TSV contains a malformed row",
            )
        })?;
        let integer = |name: &str| {
            fields[*indexes.get(name).unwrap()]
                .parse::<i64>()
                .map_err(|_| {
                    ocr_error(
                        "OCR.OUTPUT_INVALID",
                        "Tesseract TSV contains an invalid integer",
                    )
                })
        };
        if integer("level")? != 5 {
            continue;
        }
        let word_text = fields[*indexes.get("text").unwrap()].trim();
        if word_text.is_empty() {
            continue;
        }
        if word_text.chars().count() > MAX_WORD_TEXT_CHARS {
            return Err(ocr_error(
                "OCR.OUTPUT_INVALID",
                "Tesseract word text exceeded the provider limit",
            ));
        }
        let raw_confidence = fields[*indexes.get("conf").unwrap()]
            .parse::<f64>()
            .map_err(|_| {
                ocr_error(
                    "OCR.OUTPUT_INVALID",
                    "Tesseract TSV contains an invalid confidence",
                )
            })?;
        if !raw_confidence.is_finite() || raw_confidence < 0.0 {
            continue;
        }
        let values = [
            integer("left")?,
            integer("top")?,
            integer("width")?,
            integer("height")?,
        ];
        if values
            .iter()
            .any(|value| *value < 0 || *value as u64 > MAX_COORDINATE)
        {
            return Err(ocr_error(
                "OCR.OUTPUT_INVALID",
                "Tesseract TSV contains invalid coordinates",
            ));
        }
        if values[2] == 0 || values[3] == 0 {
            continue;
        }
        word_count += 1;
        text_chars += word_text.chars().count();
        if word_count > MAX_WORDS || text_chars > MAX_TEXT_CHARS {
            return Err(ocr_error(
                "OCR.OUTPUT_INVALID",
                "Tesseract text exceeded the provider limit",
            ));
        }
        let key = (
            integer("page_num")?,
            integer("block_num")?,
            integer("par_num")?,
            integer("line_num")?,
        );
        groups.entry(key).or_default().push(Word {
            text: word_text.into(),
            confidence: (raw_confidence / 100.0).clamp(0.0, 1.0),
            bounds: Bounds {
                x: values[0] as u64 + offset.0,
                y: values[1] as u64 + offset.1,
                width: values[2] as u64,
                height: values[3] as u64,
            },
        });
    }
    if groups.is_empty() {
        return Err(ocr_error(
            "OCR.NO_TEXT",
            "Tesseract did not recognize any text",
        ));
    }
    if groups.len() > MAX_LINES {
        return Err(ocr_error(
            "OCR.OUTPUT_INVALID",
            "Tesseract TSV exceeded the line limit",
        ));
    }
    let lines: Vec<Line> = groups
        .into_values()
        .map(|words| {
            let weight: usize = words
                .iter()
                .map(|word| word.text.chars().count().max(1))
                .sum();
            Line {
                text: words
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                confidence: words
                    .iter()
                    .map(|word| word.confidence * word.text.chars().count().max(1) as f64)
                    .sum::<f64>()
                    / weight as f64,
                bounds: union_bounds(words.iter().map(|word| word.bounds)),
                words,
            }
        })
        .collect();
    let aggregate = lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let weight: usize = lines
        .iter()
        .flat_map(|line| line.words.iter())
        .map(|word| word.text.chars().count().max(1))
        .sum();
    let confidence = lines
        .iter()
        .flat_map(|line| line.words.iter())
        .map(|word| word.confidence * word.text.chars().count().max(1) as f64)
        .sum::<f64>()
        / weight as f64;
    Ok((aggregate, confidence, lines))
}

fn find_matches(
    text: &str,
    patterns: &[(String, String)],
    lines: &[Line],
    deadline: Instant,
) -> Result<Vec<Value>, AutomationError> {
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for line in lines {
        let mut position = cursor;
        for (index, word) in line.words.iter().enumerate() {
            if index > 0 {
                position += 1;
            }
            let start = position;
            position += word.text.chars().count();
            spans.push((start, position, word));
        }
        cursor += line.text.chars().count() + 1;
    }
    let mut result = Vec::new();
    for (id, literal) in patterns {
        remaining(deadline)?;
        let mut byte_cursor = 0usize;
        while let Some(relative) = text[byte_cursor..].find(literal) {
            let byte_start = byte_cursor + relative;
            let start = text[..byte_start].chars().count();
            let end = start + literal.chars().count();
            let selected: Vec<&Word> = spans
                .iter()
                .filter(|(left, right, _)| *right > start && *left < end)
                .map(|(_, _, word)| *word)
                .collect();
            let (bounds, confidence) = if selected.is_empty() {
                (Value::Null, 0.0)
            } else {
                (
                    union_bounds(selected.iter().map(|word| word.bounds)).to_json(),
                    selected
                        .iter()
                        .map(|word| word.confidence)
                        .fold(1.0, f64::min),
                )
            };
            result.push(json!({"pattern_id": id, "text": literal, "span": {"start": start, "end": end}, "bounds": bounds, "confidence": confidence}));
            if result.len() > MAX_MATCHES {
                return Err(ocr_error(
                    "OCR.OUTPUT_INVALID",
                    "pattern matches exceeded the provider limit",
                ));
            }
            byte_cursor = byte_start
                + text[byte_start..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
        }
    }
    Ok(result)
}

fn line_json(line: &Line) -> Value {
    json!({"text": line.text, "confidence": line.confidence, "bounds": line.bounds.to_json()})
}

fn union_bounds(items: impl Iterator<Item = Bounds>) -> Bounds {
    let items: Vec<Bounds> = items.collect();
    let left = items.iter().map(|item| item.x).min().unwrap_or(0);
    let top = items.iter().map(|item| item.y).min().unwrap_or(0);
    let right = items
        .iter()
        .map(|item| item.x + item.width)
        .max()
        .unwrap_or(left);
    let bottom = items
        .iter()
        .map(|item| item.y + item.height)
        .max()
        .unwrap_or(top);
    Bounds {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

fn require_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), AutomationError> {
    let unexpected: Vec<&String> = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .collect();
    if unexpected.is_empty() {
        Ok(())
    } else {
        Err(invalid("request contains unsupported fields").with_detail("fields", json!(unexpected)))
    }
}

fn check_dimensions(width: u32, height: u32, phase: &str) -> Result<(), AutomationError> {
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(ocr_error(
            "OCR.IMAGE_LIMIT_EXCEEDED",
            "source image exceeds a hard decoded-size limit",
        )
        .with_detail("phase", json!(phase))
        .with_detail("width", json!(width))
        .with_detail("height", json!(height)));
    }
    Ok(())
}

fn reject_multiple_frames(bytes: &[u8], format: ImageFormat) -> Result<(), AutomationError> {
    use image::AnimationDecoder;
    let multiple = match format {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
            .ok()
            .and_then(|decoder| {
                decoder
                    .into_frames()
                    .take(2)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()
            })
            .is_some_and(|frames| frames.len() > 1),
        ImageFormat::Png => image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes))
            .ok()
            .filter(|decoder| decoder.is_apng())
            .and_then(|decoder| {
                decoder
                    .apng()
                    .into_frames()
                    .take(2)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()
            })
            .is_some_and(|frames| frames.len() > 1),
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes))
            .ok()
            .is_some_and(|decoder| decoder.has_animation()),
        ImageFormat::Tiff => tiff_has_multiple_directories(bytes),
        _ => false,
    };
    if multiple {
        Err(ocr_error(
            "OCR.IMAGE_LIMIT_EXCEEDED",
            "multi-frame images are not accepted",
        ))
    } else {
        Ok(())
    }
}

fn tiff_has_multiple_directories(bytes: &[u8]) -> bool {
    let little = bytes.starts_with(b"II*\0");
    let big = bytes.starts_with(b"MM\0*");
    if !little && !big || bytes.len() < 8 {
        return false;
    }
    let read_u16 = |slice: &[u8]| {
        if little {
            u16::from_le_bytes([slice[0], slice[1]])
        } else {
            u16::from_be_bytes([slice[0], slice[1]])
        }
    };
    let read_u32 = |slice: &[u8]| {
        if little {
            u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
        } else {
            u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]])
        }
    };
    let offset = read_u32(&bytes[4..8]) as usize;
    if offset.checked_add(2).is_none_or(|end| end > bytes.len()) {
        return false;
    }
    let entries = read_u16(&bytes[offset..offset + 2]) as usize;
    offset
        .checked_add(2)
        .and_then(|value| value.checked_add(entries.saturating_mul(12)))
        .filter(|offset| offset.checked_add(4).is_some_and(|end| end <= bytes.len()))
        .is_some_and(|offset| read_u32(&bytes[offset..offset + 4]) != 0)
}

fn remaining(deadline: Instant) -> Result<Duration, AutomationError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(ocr_error(
            "OCR.TIMEOUT",
            "host deadline elapsed before OCR could complete",
        )
        .with_retryable(true))
    } else {
        Ok(remaining)
    }
}

struct TemporaryImage(PathBuf);

impl TemporaryImage {
    fn new(bytes: &[u8]) -> Result<Self, AutomationError> {
        let path =
            std::env::temp_dir().join(format!("aad-ocr-{}.img", uuid::Uuid::new_v4().simple()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|_| {
            ocr_error(
                "OCR.IMAGE_UNAVAILABLE",
                "private image snapshot could not be created",
            )
        })?;
        file.write_all(bytes)
            .and_then(|()| file.flush())
            .map_err(|_| {
                ocr_error(
                    "OCR.IMAGE_UNAVAILABLE",
                    "private image snapshot could not be written",
                )
            })?;
        Ok(Self(path))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn image_format(media_type: &str) -> ImageFormat {
    match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/tiff" => ImageFormat::Tiff,
        "image/bmp" => ImageFormat::Bmp,
        "image/webp" => ImageFormat::WebP,
        "image/x-portable-anymap" => ImageFormat::Pnm,
        _ => unreachable!(),
    }
}

fn invalid(message: impl Into<String>) -> AutomationError {
    ocr_error("OCR.INVALID_REQUEST", message)
}
fn ocr_error(code: &str, message: impl Into<String>) -> AutomationError {
    AutomationError::new(code, message)
        .with_category("ocr")
        .with_effect("not_applied")
}

fn media_types() -> Value {
    json!([
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/tiff",
        "image/bmp",
        "image/webp",
        "image/x-portable-anymap"
    ])
}

fn bounds_schema() -> Value {
    json!({
        "type": "object",
        "description": "Pixel rectangle in source-image coordinates.",
        "required": ["x", "y", "width", "height"],
        "properties": {
            "x": {"type": "integer", "minimum": 0, "maximum": MAX_COORDINATE, "description": "Zero-based horizontal pixel offset."},
            "y": {"type": "integer", "minimum": 0, "maximum": MAX_COORDINATE, "description": "Zero-based vertical pixel offset."},
            "width": {"type": "integer", "minimum": 1, "maximum": MAX_COORDINATE, "description": "Rectangle width in pixels."},
            "height": {"type": "integer", "minimum": 1, "maximum": MAX_COORDINATE, "description": "Rectangle height in pixels."}
        },
        "additionalProperties": false
    })
}

fn common_input_properties() -> Map<String, Value> {
    json!({
        "region": bounds_schema(),
        "languages": {"type": "array", "items": {"type": "string", "pattern": "^[A-Za-z0-9_]+$", "maxLength": 64}, "minItems": 1, "maxItems": 32, "uniqueItems": true},
        "minimum_confidence": {"type": "number", "minimum": 0, "maximum": 1},
        "patterns": {"type": "array", "maxItems": 128, "items": {"type": "object", "required": ["id", "value"], "properties": {"id": {"type": "string", "minLength": 1, "maxLength": 128}, "value": {"type": "string", "minLength": 1, "maxLength": 1024}}, "additionalProperties": false}}
    })
    .as_object()
    .expect("common properties are an object")
    .clone()
}

fn artifact_ref_schema() -> Value {
    json!({
        "type": "object",
        "required": ["apiVersion", "kind", "artifactId", "digest", "mediaType", "sizeBytes"],
        "properties": {
            "apiVersion": {"const": "ai-auto-desktop.dev/v1alpha1"},
            "kind": {"const": "ArtifactRef"},
            "artifactId": {"type": "string", "pattern": "^art_[A-Za-z0-9_-]{32}$"},
            "digest": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$", "description": "SHA-256 digest of the private image snapshot."},
            "mediaType": {"enum": media_types()},
            "sizeBytes": {"type": "integer", "minimum": 1, "maximum": MAX_IMAGE_BYTES}
        },
        "additionalProperties": false
    })
}

fn source_schema(artifact: bool) -> Value {
    let mut required = vec!["kind", "digest", "media_type", "size_bytes"];
    let mut properties = json!({
        "kind": if artifact { json!({"const": "artifact"}) } else { json!({"enum": ["image", "artifact"], "description": "Which mutually exclusive input field supplied the image."}) },
        "digest": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$", "description": "SHA-256 digest of the private image snapshot."},
        "media_type": {"enum": media_types(), "description": "Media type detected from the image bytes."},
        "size_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_IMAGE_BYTES, "description": "Byte length of the private image snapshot."}
    })
    .as_object()
    .expect("source properties are an object")
    .clone();
    if !artifact {
        required.insert(1, "path");
        properties.insert(
            "path".into(),
            json!({"type": "string", "minLength": 1, "description": "Resolved absolute path supplied by the caller."}),
        );
    }
    json!({
        "type": "object",
        "description": if artifact { "Location-free provenance for the Host-mediated artifact snapshot." } else { "Provenance for the caller-supplied image snapshot read by this request." },
        "required": required,
        "properties": properties,
        "additionalProperties": false
    })
}

fn output_schema(artifact: bool) -> Value {
    let line = json!({
        "type": "object",
        "description": "One recognized text line in source-image coordinates.",
        "required": ["text", "confidence", "bounds"],
        "properties": {
            "text": {"type": "string", "minLength": 1, "maxLength": 1_019_999, "description": "Recognized words joined with one ASCII space."},
            "confidence": {"type": "number", "minimum": 0, "maximum": 1, "description": "Character-count-weighted confidence for the line."},
            "bounds": bounds_schema()
        },
        "additionalProperties": false
    });
    let matched = json!({
        "type": "object",
        "description": "One case-sensitive literal match requested by the caller.",
        "required": ["pattern_id", "text", "span", "bounds", "confidence"],
        "properties": {
            "pattern_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Caller-provided ID of the matching pattern."},
            "text": {"type": "string", "minLength": 1, "maxLength": 1024, "description": "Exact case-sensitive literal that matched."},
            "span": {"type": "object", "description": "Zero-based, end-exclusive character offsets into the aggregate text field.", "required": ["start", "end"], "properties": {"start": {"type": "integer", "minimum": 0, "maximum": 1_019_998, "description": "Inclusive match start offset."}, "end": {"type": "integer", "minimum": 1, "maximum": 1_019_999, "description": "Exclusive match end offset."}}, "additionalProperties": false},
            "bounds": {"oneOf": [bounds_schema(), {"type": "null"}], "description": "Union of matched word boxes, or null when a separator-only match has no word box."},
            "confidence": {"type": "number", "minimum": 0, "maximum": 1, "description": "Minimum confidence of covered words, or zero without a word box."}
        },
        "additionalProperties": false
    });
    json!({
        "type": "object",
        "required": ["provider", "version", "source", "source_region", "text", "confidence", "lines", "matches"],
        "properties": {
            "provider": {"const": "tesseract"},
            "version": {"type": "string", "minLength": 1, "description": "Version reported by the configured Tesseract CLI."},
            "source": source_schema(artifact),
            "source_region": {"oneOf": [bounds_schema(), {"type": "null"}]},
            "text": {"type": "string", "minLength": 1, "maxLength": 1_019_999},
            "confidence": {"type": "number", "minimum": 0, "maximum": 1},
            "lines": {"type": "array", "minItems": 1, "maxItems": MAX_LINES, "items": line},
            "matches": {"type": "array", "maxItems": MAX_MATCHES, "items": matched}
        },
        "additionalProperties": false
    })
}

fn error_contracts(include_artifact_ipc: bool) -> Vec<Value> {
    let mut contracts = vec![
        (
            "OCR.INVALID_REQUEST",
            "The image request or pattern is invalid.",
            false,
        ),
        (
            "OCR.IMAGE_UNAVAILABLE",
            "The explicit image cannot be read.",
            false,
        ),
        (
            "OCR.IMAGE_UNSUPPORTED",
            "The file is not a supported image.",
            false,
        ),
        (
            "OCR.IMAGE_LIMIT_EXCEEDED",
            "The decoded image exceeds a hard dimension, pixel, or frame limit.",
            false,
        ),
        (
            "OCR.IMAGE_VALIDATOR_UNAVAILABLE",
            "The required image validator is unavailable.",
            false,
        ),
        (
            "OCR.ENGINE_UNAVAILABLE",
            "The Tesseract CLI is unavailable.",
            false,
        ),
        (
            "OCR.ENGINE_ISOLATION_UNAVAILABLE",
            "Required engine resource isolation is unavailable.",
            false,
        ),
        (
            "OCR.ENGINE_FAILED",
            "Tesseract exited unsuccessfully.",
            false,
        ),
        (
            "OCR.OUTPUT_INVALID",
            "Tesseract emitted invalid or excessive TSV.",
            false,
        ),
        ("OCR.NO_TEXT", "No text was recognized.", false),
        (
            "OCR.LOW_CONFIDENCE",
            "OCR confidence is below the requested minimum.",
            false,
        ),
        ("OCR.TIMEOUT", "The host deadline elapsed during OCR.", true),
    ];
    if include_artifact_ipc {
        contracts.push((
            "OCR.ARTIFACT_IPC",
            "The Host-mediated artifact transfer failed.",
            false,
        ));
    }
    contracts
        .into_iter()
        .map(|(code, description, retryable)| {
            json!({"code": code, "description": description, "retryable": retryable, "effect": "not_applied", "data_schema": {"type": "object"}})
        })
        .collect()
}

fn manifest_document() -> Value {
    let mut path_properties = common_input_properties();
    path_properties.insert(
        "image".into(),
        json!({"type": "object", "required": ["path"], "properties": {"path": {"type": "string", "minLength": 1}}, "additionalProperties": false}),
    );
    path_properties.insert(
        "artifact".into(),
        json!({"type": "object", "required": ["path"], "properties": {"path": {"type": "string", "minLength": 1}, "media_type": {"type": "string", "pattern": "^image/[A-Za-z0-9.+-]+$"}}, "additionalProperties": false}),
    );
    let mut artifact_properties = common_input_properties();
    artifact_properties.insert("artifact".into(), artifact_ref_schema());
    json!({
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": PROVIDER_NAME, "version": PROVIDER_VERSION, "description": "Built-in native Tesseract OCR provider."},
        "actions": {
            "recognize": {
                "contract_major": 1,
                "description": "Recognize text in an explicit image file with Tesseract. This action never captures the screen.",
                "effect": {"default_class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
                "permissions": ["filesystem.read"],
                "input_schema": {"type": "object", "properties": path_properties, "oneOf": [{"required": ["image"], "not": {"required": ["artifact"]}}, {"required": ["artifact"], "not": {"required": ["image"]}}], "additionalProperties": false},
                "output_schema": output_schema(false),
                "errors": error_contracts(false)
            },
            "recognize_artifact": {
                "contract_major": 1,
                "description": "Recognize text in an explicit Host-managed image artifact with Tesseract. The provider receives verified bytes, never a Host path.",
                "effect": {"default_class": "read_only"},
                "risk": {"category": "observe", "level": "low"},
                "input_schema": {"type": "object", "required": ["artifact"], "properties": artifact_properties, "additionalProperties": false},
                "output_schema": output_schema(true),
                "artifacts": {"inputs": {"source": {"pointer": "/artifact", "media_types": media_types(), "max_size_bytes": MAX_IMAGE_BYTES}}},
                "errors": error_contracts(true)
            }
        },
        "runtime": {"kind": "builtin", "protocol": "rust-provider-v1"}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TSV: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n1\t1\t0\t0\t0\t0\t0\t0\t200\t80\t-1\t\n5\t1\t1\t1\t1\t1\t10\t20\t40\t12\t96.0\tInvoice\n5\t1\t1\t1\t1\t2\t55\t20\t50\t12\t84.0\tA-42\n5\t1\t1\t1\t2\t1\t10\t42\t35\t11\t90.0\tTotal\n5\t1\t1\t1\t2\t2\t50\t42\t45\t11\t80.0\t$12.50\n";

    #[test]
    fn tsv_lines_confidence_and_literal_matches_match_the_contract() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let (text, confidence, lines) = parse_tsv(TSV.as_bytes(), (0, 0), deadline).unwrap();
        let matches = find_matches(
            &text,
            &[("invoice_id".into(), "A-42".into())],
            &lines,
            deadline,
        )
        .unwrap();
        assert_eq!(text, "Invoice A-42\nTotal $12.50");
        assert!((confidence - 0.88).abs() < 0.001);
        assert_eq!(
            line_json(&lines[0])["bounds"],
            json!({"x":10,"y":20,"width":95,"height":12})
        );
        assert_eq!(
            matches[0]["bounds"],
            json!({"x":55,"y":20,"width":50,"height":12})
        );
    }

    #[test]
    fn match_spans_are_character_offsets_for_unicode_text() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\t状态\n5\t1\t1\t1\t1\t2\t12\t0\t10\t10\t90\tREADY\n";
        let deadline = Instant::now() + Duration::from_secs(1);
        let (text, _, lines) = parse_tsv(tsv.as_bytes(), (0, 0), deadline).unwrap();
        let matches =
            find_matches(&text, &[("ready".into(), "READY".into())], &lines, deadline).unwrap();
        assert_eq!(text, "状态 READY");
        assert_eq!(matches[0]["span"], json!({"start": 3, "end": 8}));
    }

    #[test]
    fn manifest_keeps_both_ocr_action_ids() {
        let document = manifest_document();
        let manifest = manifest::parse(&document).unwrap();
        assert!(manifest.resolve("vision.ocr.recognize@1").is_some());
        assert!(manifest
            .resolve("vision.ocr.recognize_artifact@1")
            .is_some());
        assert!(manifest.actions["recognize_artifact"].has_artifacts());

        let error_codes = |action: &str| {
            document["actions"][action]["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|error| error["code"].as_str().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            error_codes("recognize"),
            vec![
                "OCR.INVALID_REQUEST",
                "OCR.IMAGE_UNAVAILABLE",
                "OCR.IMAGE_UNSUPPORTED",
                "OCR.IMAGE_LIMIT_EXCEEDED",
                "OCR.IMAGE_VALIDATOR_UNAVAILABLE",
                "OCR.ENGINE_UNAVAILABLE",
                "OCR.ENGINE_ISOLATION_UNAVAILABLE",
                "OCR.ENGINE_FAILED",
                "OCR.OUTPUT_INVALID",
                "OCR.NO_TEXT",
                "OCR.LOW_CONFIDENCE",
                "OCR.TIMEOUT",
            ]
        );
        let artifact_errors = error_codes("recognize_artifact");
        assert_eq!(artifact_errors.last(), Some(&"OCR.ARTIFACT_IPC"));
        let reference =
            &document["actions"]["recognize_artifact"]["input_schema"]["properties"]["artifact"];
        assert_eq!(reference["additionalProperties"], false);
        assert_eq!(reference["required"].as_array().unwrap().len(), 6);
        assert_eq!(
            reference["properties"]["artifactId"]["pattern"],
            "^art_[A-Za-z0-9_-]{32}$"
        );
    }

    #[test]
    fn malformed_tsv_fails_closed() {
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            parse_tsv(&[0xff, 0xfe], (0, 0), deadline).unwrap_err().code,
            "OCR.OUTPUT_INVALID"
        );
        assert_eq!(
            parse_tsv(b"level\tpage_num\0", (0, 0), deadline)
                .unwrap_err()
                .code,
            "OCR.OUTPUT_INVALID"
        );
        let negative = "level\tpage_num\tblock_num\tpar_num\tline_num\tleft\ttop\twidth\theight\tconf\ttext\n5\t1\t1\t1\t1\t-1\t0\t10\t10\t90\tbad\n";
        assert_eq!(
            parse_tsv(negative.as_bytes(), (0, 0), deadline)
                .unwrap_err()
                .code,
            "OCR.OUTPUT_INVALID"
        );
        let no_text =
            "level\tpage_num\tblock_num\tpar_num\tline_num\tleft\ttop\twidth\theight\tconf\ttext\n";
        assert_eq!(
            parse_tsv(no_text.as_bytes(), (0, 0), deadline)
                .unwrap_err()
                .code,
            "OCR.NO_TEXT"
        );
    }

    #[test]
    fn region_offsets_are_applied_to_word_and_match_bounds() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let (text, _, lines) = parse_tsv(TSV.as_bytes(), (100, 200), deadline).unwrap();
        let matches = find_matches(
            &text,
            &[("invoice".into(), "Invoice".into())],
            &lines,
            deadline,
        )
        .unwrap();
        assert_eq!(
            line_json(&lines[0])["bounds"],
            json!({"x":110,"y":220,"width":95,"height":12})
        );
        assert_eq!(
            matches[0]["bounds"],
            json!({"x":110,"y":220,"width":40,"height":12})
        );
    }

    #[test]
    fn invalid_regions_and_image_limits_are_rejected() {
        for region in [
            json!({"x": -1, "y": 0, "width": 1, "height": 1}),
            json!({"x": 0, "y": 0, "width": 0, "height": 1}),
            json!({"x": 0, "y": 0, "width": 1, "height": 1, "extra": 1}),
        ] {
            assert_eq!(
                parse_region(Some(&region)).unwrap_err().code,
                "OCR.INVALID_REQUEST"
            );
        }
        assert_eq!(
            check_dimensions(MAX_IMAGE_DIMENSION + 1, 1, "test")
                .unwrap_err()
                .code,
            "OCR.IMAGE_LIMIT_EXCEEDED"
        );
        assert_eq!(
            check_dimensions(10_000, 10_000, "test").unwrap_err().code,
            "OCR.IMAGE_LIMIT_EXCEEDED"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn engine_timeout_terminates_the_process_tree() {
        let started = Instant::now();
        let error = run_engine(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            Instant::now() + Duration::from_millis(150),
            true,
        )
        .unwrap_err();
        assert_eq!(error.code, "OCR.TIMEOUT");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn engine_output_overflow_terminates_the_process_tree() {
        let started = Instant::now();
        let error = run_engine(
            &[
                "sh".into(),
                "-c".into(),
                "while :; do printf 0123456789abcdef; done".into(),
            ],
            Instant::now() + Duration::from_secs(5),
            true,
        )
        .unwrap_err();
        assert_eq!(error.code, "OCR.OUTPUT_INVALID");
        assert_eq!(error.details["stream"], "stdout");
        assert_eq!(error.details["limit_bytes"], MAX_ENGINE_STDOUT_BYTES);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_provider_runs_a_tesseract_compatible_engine() {
        let root = std::env::temp_dir().join(format!(
            "aad-ocr-native-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("fake-tesseract");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'tesseract 5.4.1'; exit 0; fi\nprintf '%b' '{}'\n",
                TSV.replace('\n', "\\n")
                    .replace('\t', "\\t")
                    .replace('\'', "'\\''")
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let image_path = root.join("input.png");
        image::DynamicImage::new_rgb8(200, 80)
            .save_with_format(&image_path, ImageFormat::Png)
            .unwrap();
        let provider = OcrProvider::with_command(vec![script.display().to_string()], true).unwrap();
        let result = provider
            .invoke(
                "vision.ocr.recognize@1",
                json!({
                    "image": {"path": image_path},
                    "languages": ["eng", "deu"],
                    "minimum_confidence": 0.8,
                    "patterns": [{"id": "invoice_id", "value": "A-42"}],
                }),
                Some(Duration::from_secs(5)),
            )
            .unwrap();

        assert_eq!(result["version"], "5.4.1");
        assert_eq!(result["text"], "Invoice A-42\nTotal $12.50");
        assert_eq!(result["matches"][0]["pattern_id"], "invoice_id");
        let _ = std::fs::remove_dir_all(root);
    }
}

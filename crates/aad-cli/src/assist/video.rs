//! Video for `aad assist`, encoded by an ffmpeg the host provides.
//!
//! This project ships no encoder. `--ffmpeg`, then `AAD_FFMPEG`, then `ffmpeg`
//! on PATH is used when it runs; each video codec is offered only when one of
//! its encoders passes a trial at startup, so a listed but unusable encoder
//! (NVENC without an NVIDIA GPU, AMF without AMD) is never chosen.
//!
//! Pictures are fed to ffmpeg one at a time as raw pixels, and only when the
//! screen changed: a still screen costs nothing, and a small change costs a
//! small frame, because the encoder codes unchanged blocks as skipped. Each
//! frame must come out before the next goes in; an encoder that holds frames
//! back (Media Foundation's H.264 does until its input ends) fails the trial.
//!
//! ffmpeg writes H.264 as FLV and VP9/AV1 as IVF: both put each frame's
//! length before it, so a frame is passed on the moment it is complete
//! instead of when the next one starts. H.264 leaves as Annex B with its
//! parameter sets in front of every keyframe, so any decoder can start there;
//! VP9 and AV1 frames leave as they are, the form WebCodecs decodes.
use super::{Codec, PixelFormat, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How video is encoded, from the command line.
#[derive(Clone, Debug)]
pub struct Settings {
    /// 1..100, mapped onto each encoder's own quantizer scale.
    pub quality: u8,
    /// Target bitrate in kbit/s; constant quality when not given.
    pub bitrate: Option<u32>,
    /// Frames between keyframes; 0 for only the first.
    pub gop: u32,
    /// The encoder's speed preset; each encoder's fastest when not given.
    pub preset: Option<String>,
    /// Full-resolution chroma (4:4:4) instead of 4:2:0.
    pub chroma444: bool,
    /// Output options passed to ffmpeg as they are.
    pub extra: Vec<String>,
    /// The frame rate ffmpeg is told; frames still come only on change.
    pub fps: u32,
    /// Largest output size; 0 for no limit.
    pub max_width: u32,
    pub max_height: u32,
}

/// An ffmpeg that ran.
#[derive(Clone, Debug)]
pub struct Ffmpeg {
    pub path: PathBuf,
    pub version: String,
}

/// ffmpeg from `explicit`, `AAD_FFMPEG` or PATH. `Ok(None)` when none was
/// asked for and none is on PATH; an error when the one asked for does not run.
pub fn locate(explicit: Option<&Path>) -> Result<Option<Ffmpeg>> {
    let from_env = std::env::var_os("AAD_FFMPEG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let (path, required) = match explicit.map(Path::to_path_buf).or(from_env) {
        Some(path) => (path, true),
        None => match on_path() {
            Some(path) => (path, false),
            None => return Ok(None),
        },
    };
    let output = command(&path)
        .args(["-hide_banner", "-version"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let version = text
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("ffmpeg version "))
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or("unknown")
                .to_string();
            Ok(Some(Ffmpeg { path, version }))
        }
        Ok(_) | Err(_) if !required => Ok(None),
        Ok(output) => Err(format!(
            "ffmpeg at {} exited with {}",
            path.display(),
            output.status
        )),
        Err(e) => Err(format!("cannot run ffmpeg at {}: {e}", path.display())),
    }
}

fn on_path() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing up when aad itself runs without one.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// What this project knows about one ffmpeg encoder.
struct Known {
    name: &'static str,
    codec: Codec,
    hardware: bool,
    /// Pixel format for 4:2:0, and for 4:4:4 when the encoder takes it.
    yuv420: &'static str,
    yuv444: Option<&'static str>,
    args: fn(&Settings) -> Vec<String>,
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| (*w).to_string()).collect()
}

/// `quality` 1..100 onto a quantizer from `best` to `worst`.
fn quantizer(quality: u8, best: u32, worst: u32) -> u32 {
    let quality = u32::from(quality.clamp(1, 100));
    worst - (worst - best) * (quality - 1) / 99
}

fn preset<'a>(settings: &'a Settings, fastest: &'a str) -> &'a str {
    settings.preset.as_deref().unwrap_or(fastest)
}

fn bitrate(kbps: u32) -> Vec<String> {
    vec![
        "-b:v".into(),
        format!("{kbps}k"),
        "-maxrate".into(),
        format!("{kbps}k"),
        "-bufsize".into(),
        format!("{}k", kbps.max(2) / 2),
    ]
}

fn x264(s: &Settings) -> Vec<String> {
    let mut args = words(&["-preset", preset(s, "ultrafast"), "-tune", "zerolatency"]);
    args.extend(words(&["-bf", "0"]));
    match s.bitrate {
        Some(kbps) => args.extend(bitrate(kbps)),
        None => args.extend(["-crf".into(), quantizer(s.quality, 12, 51).to_string()]),
    }
    if s.chroma444 {
        args.extend(words(&["-profile:v", "high444"]));
    }
    args
}

fn nvenc(s: &Settings) -> Vec<String> {
    let mut args = words(&["-preset", preset(s, "p1"), "-tune", "ull"]);
    args.extend(words(&["-zerolatency", "1", "-delay", "0", "-bf", "0"]));
    match s.bitrate {
        Some(kbps) => {
            args.extend(words(&["-rc", "cbr"]));
            args.extend(bitrate(kbps));
        }
        None => {
            args.extend(words(&["-rc", "constqp", "-qp"]));
            args.push(quantizer(s.quality, 12, 51).to_string());
        }
    }
    args
}

fn h264_nvenc(s: &Settings) -> Vec<String> {
    let mut args = nvenc(s);
    if s.chroma444 {
        args.extend(words(&["-profile:v", "high444p"]));
    }
    args
}

fn qsv(s: &Settings) -> Vec<String> {
    let mut args = words(&["-preset", preset(s, "veryfast"), "-async_depth", "1"]);
    args.extend(words(&["-look_ahead", "0", "-bf", "0"]));
    match s.bitrate {
        Some(kbps) => args.extend(bitrate(kbps)),
        // -q:v asks for constant QP.
        None => args.extend(["-q:v".into(), quantizer(s.quality, 12, 51).to_string()]),
    }
    args
}

fn amf(s: &Settings) -> Vec<String> {
    let mut args = words(&["-usage", "ultralowlatency", "-quality"]);
    args.push(preset(s, "speed").to_string());
    args.extend(words(&["-bf", "0"]));
    match s.bitrate {
        Some(kbps) => {
            args.extend(words(&["-rc", "cbr"]));
            args.extend(bitrate(kbps));
        }
        None => {
            let qp = quantizer(s.quality, 12, 51).to_string();
            args.extend(words(&["-rc", "cqp", "-qp_i", &qp, "-qp_p", &qp]));
        }
    }
    args
}

fn videotoolbox(s: &Settings) -> Vec<String> {
    let mut args = words(&["-realtime", "1", "-prio_speed", "1", "-bf", "0"]);
    match s.bitrate {
        Some(kbps) => args.extend(bitrate(kbps)),
        None => args.extend(["-q:v".into(), s.quality.clamp(1, 100).to_string()]),
    }
    args
}

fn libvpx(s: &Settings) -> Vec<String> {
    let mut args = words(&["-deadline", "realtime", "-cpu-used", preset(s, "8")]);
    args.extend(words(&["-row-mt", "1", "-lag-in-frames", "0"]));
    match s.bitrate {
        Some(kbps) => args.extend(bitrate(kbps)),
        None => {
            args.extend(["-crf".into(), quantizer(s.quality, 10, 63).to_string()]);
            args.extend(words(&["-b:v", "0"]));
        }
    }
    args
}

fn svtav1(s: &Settings) -> Vec<String> {
    // Low-delay prediction: no frame waits for a later one.
    let mut args = words(&[
        "-preset",
        preset(s, "12"),
        "-svtav1-params",
        "pred-struct=1",
    ]);
    match s.bitrate {
        Some(kbps) => args.extend(["-b:v".into(), format!("{kbps}k")]),
        None => args.extend(["-crf".into(), quantizer(s.quality, 10, 63).to_string()]),
    }
    args
}

fn aom(s: &Settings) -> Vec<String> {
    let mut args = words(&["-usage", "realtime", "-cpu-used", preset(s, "8")]);
    args.extend(words(&["-row-mt", "1", "-lag-in-frames", "0"]));
    match s.bitrate {
        Some(kbps) => args.extend(bitrate(kbps)),
        None => {
            args.extend(["-crf".into(), quantizer(s.quality, 10, 63).to_string()]);
            args.extend(words(&["-b:v", "0"]));
        }
    }
    args
}

/// An encoder this table does not know: only the generic options, the rest
/// from --ffmpeg-arg.
fn generic(s: &Settings) -> Vec<String> {
    s.bitrate.map(bitrate).unwrap_or_default()
}

/// Every encoder tried when none is named, hardware first. Media Foundation's
/// H.264 is left out: it holds frames back until its input ends.
const KNOWN: &[Known] = &[
    Known {
        name: "h264_nvenc",
        codec: Codec::H264,
        hardware: true,
        yuv420: "yuv420p",
        yuv444: Some("yuv444p"),
        args: h264_nvenc,
    },
    Known {
        name: "h264_qsv",
        codec: Codec::H264,
        hardware: true,
        yuv420: "nv12",
        yuv444: None,
        args: qsv,
    },
    Known {
        name: "h264_amf",
        codec: Codec::H264,
        hardware: true,
        yuv420: "nv12",
        yuv444: None,
        args: amf,
    },
    Known {
        name: "h264_videotoolbox",
        codec: Codec::H264,
        hardware: true,
        yuv420: "nv12",
        yuv444: None,
        args: videotoolbox,
    },
    Known {
        name: "libx264",
        codec: Codec::H264,
        hardware: false,
        yuv420: "yuv420p",
        yuv444: Some("yuv444p"),
        args: x264,
    },
    Known {
        name: "vp9_qsv",
        codec: Codec::Vp9,
        hardware: true,
        yuv420: "nv12",
        yuv444: None,
        args: qsv,
    },
    Known {
        name: "libvpx-vp9",
        codec: Codec::Vp9,
        hardware: false,
        yuv420: "yuv420p",
        yuv444: Some("yuv444p"),
        args: libvpx,
    },
    Known {
        name: "av1_nvenc",
        codec: Codec::Av1,
        hardware: true,
        yuv420: "yuv420p",
        yuv444: None,
        args: nvenc,
    },
    Known {
        name: "av1_qsv",
        codec: Codec::Av1,
        hardware: true,
        yuv420: "nv12",
        yuv444: None,
        args: qsv,
    },
    Known {
        name: "av1_amf",
        codec: Codec::Av1,
        hardware: true,
        yuv420: "nv12",
        yuv444: None,
        args: amf,
    },
    Known {
        name: "libsvtav1",
        codec: Codec::Av1,
        hardware: false,
        yuv420: "yuv420p",
        yuv444: None,
        args: svtav1,
    },
    Known {
        name: "libaom-av1",
        codec: Codec::Av1,
        hardware: false,
        yuv420: "yuv420p",
        yuv444: Some("yuv444p"),
        args: aom,
    },
];

fn known(name: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.name == name)
}

/// The codec an encoder named on the command line produces.
pub fn codec_of(encoder: &str) -> Result<Codec> {
    if let Some(known) = known(encoder) {
        return Ok(known.codec);
    }
    let lower = encoder.to_ascii_lowercase();
    if lower.contains("264") {
        Ok(Codec::H264)
    } else if lower.contains("vp9") {
        Ok(Codec::Vp9)
    } else if lower.contains("av1") {
        Ok(Codec::Av1)
    } else {
        Err(format!(
            "--encoder {encoder}: not an H.264, VP9 or AV1 encoder this server can stream"
        ))
    }
}

/// Encoders to try for `codec`: those named for it, else every known one.
fn candidates(codec: Codec, named: &[String]) -> Vec<String> {
    let mine: Vec<String> = named
        .iter()
        .filter(|name| codec_of(name).ok() == Some(codec))
        .cloned()
        .collect();
    if !mine.is_empty() {
        return mine;
    }
    KNOWN
        .iter()
        .filter(|k| k.codec == codec)
        .map(|k| k.name.to_string())
        .collect()
}

/// The encoders ffmpeg was built with.
fn listed(ffmpeg: &Ffmpeg) -> Vec<String> {
    let Ok(output) = command(&ffmpeg.path)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let flags = parts.next()?;
            let name = parts.next()?;
            (flags.len() == 6 && flags.starts_with('V')).then(|| name.to_string())
        })
        .collect()
}

/// Which encoder each video codec uses, and what was tried.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub ffmpeg: Option<Ffmpeg>,
    pub chosen: Vec<(Codec, String)>,
    pub tried: Vec<Value>,
}

impl Probe {
    pub fn encoder(&self, codec: Codec) -> Option<&str> {
        self.chosen
            .iter()
            .find(|(c, _)| *c == codec)
            .map(|(_, name)| name.as_str())
    }

    pub fn report(&self) -> Value {
        let chosen: serde_json::Map<String, Value> = self
            .chosen
            .iter()
            .map(|(codec, name)| (codec.name().to_string(), json!(name)))
            .collect();
        json!({
            "ffmpeg": self.ffmpeg.as_ref().map(|f| json!({"path": f.path, "version": f.version})),
            "encoders": chosen,
            "tried": self.tried,
        })
    }
}

/// Finds a working encoder for each of `codecs`, trying codecs in parallel and
/// each codec's candidates in order.
pub fn probe(
    ffmpeg: Option<Ffmpeg>,
    codecs: &[Codec],
    named: &[String],
    settings: &Settings,
) -> Probe {
    let Some(ffmpeg) = ffmpeg else {
        return Probe::default();
    };
    let listed = listed(&ffmpeg);
    let threads: Vec<_> = codecs
        .iter()
        .map(|&codec| {
            let names: Vec<String> = candidates(codec, named)
                .into_iter()
                .filter(|name| listed.contains(name))
                .collect();
            let (ffmpeg, settings) = (ffmpeg.clone(), settings.clone());
            std::thread::spawn(move || {
                let mut tried = Vec::new();
                for name in names {
                    let hardware = known(&name).map(|k| k.hardware);
                    let began = Instant::now();
                    let result = trial(&ffmpeg, &name, &settings);
                    let ms = began.elapsed().as_millis() as u64;
                    match result {
                        Ok(()) => {
                            tried.push(json!({"encoder": name, "hardware": hardware, "ok": true, "ms": ms}));
                            return (codec, Some(name), tried);
                        }
                        Err(error) => tried.push(
                            json!({"encoder": name, "hardware": hardware, "ok": false, "error": error, "ms": ms}),
                        ),
                    }
                }
                (codec, None, tried)
            })
        })
        .collect();
    let mut probe = Probe {
        ffmpeg: Some(ffmpeg),
        ..Probe::default()
    };
    for thread in threads {
        if let Ok((codec, chosen, tried)) = thread.join() {
            if let Some(name) = chosen {
                probe.chosen.push((codec, name));
            }
            probe.tried.extend(tried);
        }
    }
    probe
}

/// Encodes two small pictures one at a time; each must come out before the
/// next goes in, the first as a keyframe.
fn trial(ffmpeg: &Ffmpeg, encoder: &str, settings: &Settings) -> Result<()> {
    let (width, height) = (256, 256);
    let mut video = Encoder::start(ffmpeg, encoder, settings, width, height, PixelFormat::Bgra)?;
    for (index, shade) in [0x40u8, 0xc0].into_iter().enumerate() {
        let mut pixels = vec![0x20u8; (width * height * 4) as usize];
        pixels[..(width * 16 * 4) as usize].fill(shade);
        video.push(Arc::new(pixels));
        // Hardware encoders can take a second or two to open.
        let wait = if index == 0 { 15 } else { 5 };
        let packet = video.next(Duration::from_secs(wait))?.ok_or_else(|| {
            format!(
                "frame {} did not come out before the next went in",
                index + 1
            )
        })?;
        if index == 0 && !packet.key {
            return Err("the first frame was not a keyframe".into());
        }
    }
    Ok(())
}

/// The output size for an input: scaled down to fit, and even, which 4:2:0
/// needs.
pub fn output_size(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let mut scale: f64 = 1.0;
    if max_width > 0 && width > max_width {
        scale = scale.min(f64::from(max_width) / f64::from(width));
    }
    if max_height > 0 && height > max_height {
        scale = scale.min(f64::from(max_height) / f64::from(height));
    }
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (
        even(f64::from(width) * scale),
        even(f64::from(height) * scale),
    )
}

/// One encoded frame.
pub struct Packet {
    pub data: Vec<u8>,
    pub key: bool,
    /// The WebCodecs codec string, known from the stream's first keyframe.
    pub webcodecs: Option<String>,
}

#[derive(Default)]
struct Slot {
    next: Option<Arc<Vec<u8>>>,
    writing: bool,
    closed: bool,
}

/// One ffmpeg encoding pictures of one size and pixel format.
pub struct Encoder {
    child: Child,
    slot: Arc<(Mutex<Slot>, Condvar)>,
    packets: Receiver<Result<Packet>>,
    errors: Arc<Mutex<VecDeque<String>>>,
    pub name: String,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub input: (u32, u32, PixelFormat),
}

impl Encoder {
    pub fn start(
        ffmpeg: &Ffmpeg,
        encoder: &str,
        settings: &Settings,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<Self> {
        let codec = codec_of(encoder)?;
        let entry = known(encoder);
        let pixel_format = match (entry, settings.chroma444) {
            (Some(k), false) => k.yuv420,
            (Some(k), true) => k
                .yuv444
                .ok_or_else(|| format!("{encoder} cannot encode 4:4:4; use --chroma 420"))?,
            (None, false) => "yuv420p",
            (None, true) => "yuv444p",
        };
        let (out_width, out_height) =
            output_size(width, height, settings.max_width, settings.max_height);
        let filter = if (out_width, out_height) == (width, height) {
            None
        } else if width - out_width <= 1 && height - out_height <= 1 {
            // Only odd: drop the last column or row rather than resample.
            Some(format!("crop={out_width}:{out_height}:0:0"))
        } else {
            Some(format!("scale={out_width}:{out_height}:flags=bicubic"))
        };
        let gop = if settings.gop == 0 {
            1_000_000
        } else {
            settings.gop
        };
        let mut args = words(&["-hide_banner", "-loglevel", "error"]);
        // Without these ffmpeg reads ahead to measure its input before it
        // encodes anything: below 5 fps nothing comes out for 20 frames.
        args.extend(words(&["-probesize", "32", "-fpsprobesize", "0"]));
        args.extend(words(&["-f", "rawvideo", "-pix_fmt", format.ffmpeg()]));
        args.extend(["-s".into(), format!("{width}x{height}")]);
        args.extend(["-framerate".into(), settings.fps.clamp(1, 60).to_string()]);
        args.extend(words(&["-i", "-", "-an"]));
        if let Some(filter) = filter {
            args.extend(["-vf".into(), filter]);
        }
        args.extend(["-c:v".into(), encoder.to_string()]);
        args.extend(["-pix_fmt".into(), pixel_format.to_string()]);
        args.extend(["-g".into(), gop.to_string()]);
        args.extend((entry
            .map_or(generic as fn(&Settings) -> Vec<String>, |k| k.args))(
            settings,
        ));
        args.extend(settings.extra.iter().cloned());
        args.extend(words(&["-flush_packets", "1"]));
        match codec {
            Codec::H264 => args.extend(words(&["-f", "flv", "-flvflags", "no_duration_filesize"])),
            _ => args.extend(words(&["-f", "ivf"])),
        }
        args.push("-".into());

        let mut child = command(&ffmpeg.path)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
        let stdin = child.stdin.take().ok_or("ffmpeg has no input")?;
        let stdout = child.stdout.take().ok_or("ffmpeg has no output")?;
        let stderr = child.stderr.take().ok_or("ffmpeg has no error output")?;

        let errors = Arc::new(Mutex::new(VecDeque::new()));
        {
            let errors = errors.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr)
                    .lines()
                    .map_while(std::io::Result::ok)
                {
                    let mut errors = errors.lock().unwrap();
                    if errors.len() == 8 {
                        errors.pop_front();
                    }
                    errors.push_back(line);
                }
            });
        }
        let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        {
            let slot = slot.clone();
            std::thread::spawn(move || feed(stdin, &slot));
        }
        let (sender, packets) = mpsc::channel();
        std::thread::spawn(move || {
            let result = match codec {
                Codec::H264 => read_flv(stdout, &sender),
                _ => read_ivf(stdout, codec, &sender),
            };
            if let Err(error) = result {
                let _ = sender.send(Err(error));
            }
        });
        Ok(Self {
            child,
            slot,
            packets,
            errors,
            name: encoder.to_string(),
            codec,
            width: out_width,
            height: out_height,
            input: (width, height, format),
        })
    }

    /// Queues a picture, replacing one not yet taken: a slow encoder gets the
    /// newest screen rather than a backlog.
    pub fn push(&self, pixels: Arc<Vec<u8>>) {
        let (lock, wake) = &*self.slot;
        lock.lock().unwrap().next = Some(pixels);
        wake.notify_one();
    }

    /// Whether a picture is still waiting or being written.
    pub fn busy(&self) -> bool {
        let slot = self.slot.0.lock().unwrap();
        slot.next.is_some() || slot.writing
    }

    /// The next frame, if one is ready.
    pub fn poll(&mut self) -> Result<Option<Packet>> {
        self.next(Duration::ZERO)
    }

    fn next(&mut self, wait: Duration) -> Result<Option<Packet>> {
        let received = if wait.is_zero() {
            self.packets.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            })
        } else {
            self.packets.recv_timeout(wait)
        };
        match received {
            Ok(Ok(packet)) => Ok(Some(packet)),
            Ok(Err(error)) => Err(self.failure(&error)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(self.failure("ffmpeg stopped")),
        }
    }

    fn failure(&mut self, what: &str) -> String {
        // Give the error output a moment to arrive.
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(30));
        let errors: Vec<String> = self.errors.lock().unwrap().iter().cloned().collect();
        if errors.is_empty() {
            format!("{}: {what}", self.name)
        } else {
            format!("{}: {what}: {}", self.name, errors.join(" | "))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let (lock, wake) = &*self.slot;
        lock.lock().unwrap().closed = true;
        wake.notify_one();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes queued pictures to ffmpeg until the encoder is dropped.
fn feed(mut stdin: ChildStdin, slot: &(Mutex<Slot>, Condvar)) {
    let (lock, wake) = slot;
    loop {
        let pixels = {
            let mut state = lock.lock().unwrap();
            loop {
                if state.closed {
                    return;
                }
                if let Some(pixels) = state.next.take() {
                    state.writing = true;
                    break pixels;
                }
                state = wake.wait(state).unwrap();
            }
        };
        let written = stdin.write_all(&pixels).and_then(|()| stdin.flush());
        lock.lock().unwrap().writing = false;
        if written.is_err() {
            return;
        }
    }
}

fn exact(reader: &mut impl Read, n: usize) -> Result<Option<Vec<u8>>> {
    let mut buffer = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err("ffmpeg's output ended inside a frame".into()),
            Ok(read) => filled += read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(Some(buffer))
}

/// The SPS and PPS from an AVCDecoderConfigurationRecord, and the NAL length size.
pub(crate) fn parse_avcc(record: &[u8]) -> Result<(usize, Vec<Vec<u8>>)> {
    let bad = || "bad H.264 configuration from ffmpeg".to_string();
    if record.len() < 7 || record[0] != 1 {
        return Err(bad());
    }
    let length_size = usize::from(record[4] & 3) + 1;
    let mut sets = Vec::new();
    let mut at = 5;
    for mask in [0x1f, 0xff] {
        let count = usize::from(*record.get(at).ok_or_else(bad)? & mask);
        at += 1;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes(
                record.get(at..at + 2).ok_or_else(bad)?.try_into().unwrap(),
            ));
            sets.push(record.get(at + 2..at + 2 + len).ok_or_else(bad)?.to_vec());
            at += 2 + len;
        }
    }
    Ok((length_size, sets))
}

/// Length-prefixed NAL units as Annex B.
pub(crate) fn annex_b(data: &[u8], length_size: usize, out: &mut Vec<u8>) -> Result<()> {
    let mut at = 0;
    while at < data.len() {
        let head = data
            .get(at..at + length_size)
            .ok_or("bad H.264 frame from ffmpeg")?;
        let len = head
            .iter()
            .fold(0usize, |acc, b| acc << 8 | usize::from(*b));
        let unit = data
            .get(at + length_size..at + length_size + len)
            .ok_or("bad H.264 frame from ffmpeg")?;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
        at += length_size + len;
    }
    Ok(())
}

fn read_flv(mut stdout: impl Read, sender: &mpsc::Sender<Result<Packet>>) -> Result<()> {
    let header = exact(&mut stdout, 9)?.ok_or("ffmpeg wrote nothing")?;
    if &header[..3] != b"FLV" {
        return Err("ffmpeg did not write FLV".into());
    }
    let offset = u32::from_be_bytes(header[5..9].try_into().unwrap()) as usize;
    exact(&mut stdout, offset.saturating_sub(9) + 4)?;
    let mut config: Option<(usize, Vec<Vec<u8>>)> = None;
    let mut codec_string = None;
    while let Some(tag) = exact(&mut stdout, 11)? {
        let size = usize::from(tag[1]) << 16 | usize::from(tag[2]) << 8 | usize::from(tag[3]);
        let data = exact(&mut stdout, size + 4)?.ok_or("ffmpeg's output ended inside a frame")?;
        let data = &data[..size];
        if tag[0] != 9 || data.len() < 5 || data[0] & 0x0f != 7 {
            continue;
        }
        match data[1] {
            0 => {
                let (length_size, sets) = parse_avcc(&data[5..])?;
                if let Some(sps) = sets.iter().find(|s| s.first().map(|b| b & 0x1f) == Some(7)) {
                    if sps.len() >= 4 {
                        codec_string =
                            Some(format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]));
                    }
                }
                config = Some((length_size, sets));
            }
            1 => {
                let (length_size, sets) = config
                    .as_ref()
                    .ok_or("H.264 frame before its configuration")?;
                let key = data[0] >> 4 == 1;
                let mut out = Vec::with_capacity(data.len() + 64);
                if key {
                    for set in sets {
                        out.extend_from_slice(&[0, 0, 0, 1]);
                        out.extend_from_slice(set);
                    }
                }
                annex_b(&data[5..], *length_size, &mut out)?;
                let packet = Packet {
                    data: out,
                    key,
                    webcodecs: codec_string.clone(),
                };
                if sender.send(Ok(packet)).is_err() {
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether a VP9 frame is a keyframe, and its profile.
pub(crate) fn vp9_key(frame: &[u8]) -> (bool, u8) {
    let Some(&b) = frame.first() else {
        return (false, 0);
    };
    let profile = (b >> 5 & 1) | (b >> 4 & 1) << 1;
    let (show_existing, frame_type) = if profile == 3 {
        (b >> 2 & 1, b >> 1 & 1)
    } else {
        (b >> 3 & 1, b >> 2 & 1)
    };
    (show_existing == 0 && frame_type == 0, profile)
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            value = value << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(value)
    }
}

/// The OBUs of an AV1 temporal unit: (type, payload).
fn obus(unit: &[u8]) -> Vec<(u8, &[u8])> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < unit.len() {
        let header = unit[at];
        let kind = header >> 3 & 0x0f;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        at += 1 + usize::from(extension);
        let size = if has_size {
            let mut size = 0usize;
            let mut shift = 0;
            loop {
                let Some(&byte) = unit.get(at) else {
                    return found;
                };
                at += 1;
                size |= usize::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 || shift > 56 {
                    break;
                }
            }
            size
        } else {
            unit.len().saturating_sub(at)
        };
        let Some(payload) = unit.get(at..at + size) else {
            return found;
        };
        found.push((kind, payload));
        at += size;
    }
    found
}

/// Whether an AV1 temporal unit starts with a keyframe, and the codec string
/// its sequence header gives.
pub(crate) fn av1_key(unit: &[u8]) -> (bool, Option<String>) {
    let units = obus(unit);
    let codec = units
        .iter()
        .find(|(kind, _)| *kind == 1)
        .map(|(_, header)| {
            let mut bits = Bits {
                data: header,
                at: 0,
            };
            let profile = bits.take(3).unwrap_or(0);
            let _still = bits.take(1);
            let reduced = bits.take(1).unwrap_or(0);
            let (level, tier) = if reduced == 1 {
                (bits.take(5), 0)
            } else if bits.take(1) == Some(0) {
                // No timing info: initial_display_delay_present, the operating
                // point count, the first point's idc, then its level and tier.
                let _delay = bits.take(1);
                let _points = bits.take(5);
                let _idc = bits.take(12);
                let level = bits.take(5);
                let tier = match level {
                    Some(level) if level > 7 => bits.take(1).unwrap_or(0),
                    _ => 0,
                };
                (level, tier)
            } else {
                (None, 0)
            };
            let tier = if tier == 1 { "H" } else { "M" };
            format!("av01.{profile}.{:02}{tier}.08", level.unwrap_or(8))
        });
    let key = units
        .iter()
        .find(|(kind, _)| *kind == 3 || *kind == 6)
        .and_then(|(_, header)| header.first())
        .is_some_and(|b| b >> 7 == 0 && b >> 5 & 3 == 0);
    (key, codec)
}

fn read_ivf(
    mut stdout: impl Read,
    codec: Codec,
    sender: &mpsc::Sender<Result<Packet>>,
) -> Result<()> {
    let header = exact(&mut stdout, 32)?.ok_or("ffmpeg wrote nothing")?;
    if &header[..4] != b"DKIF" {
        return Err("ffmpeg did not write IVF".into());
    }
    let length = usize::from(u16::from_le_bytes(header[6..8].try_into().unwrap()));
    exact(&mut stdout, length.saturating_sub(32))?;
    let mut codec_string = None;
    while let Some(head) = exact(&mut stdout, 12)? {
        let size = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
        let data = exact(&mut stdout, size)?.ok_or("ffmpeg's output ended inside a frame")?;
        let key = match codec {
            Codec::Vp9 => {
                let (key, profile) = vp9_key(&data);
                if key {
                    codec_string = Some(if profile == 1 {
                        "vp09.01.41.08.03.01.01.01.00".to_string()
                    } else {
                        format!("vp09.0{profile}.41.08")
                    });
                }
                key
            }
            _ => {
                let (key, string) = av1_key(&data);
                if string.is_some() {
                    codec_string = string;
                }
                key
            }
        };
        let packet = Packet {
            data,
            key,
            webcodecs: codec_string.clone(),
        };
        if sender.send(Ok(packet)).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_maps_onto_each_quantizer_scale() {
        assert_eq!(quantizer(100, 12, 51), 12);
        assert_eq!(quantizer(1, 12, 51), 51);
        assert_eq!(quantizer(70, 10, 63), 27);
        assert_eq!(quantizer(0, 12, 51), 51);
    }

    #[test]
    fn outputs_fit_the_limits_and_stay_even() {
        assert_eq!(output_size(3840, 2160, 1600, 0), (1600, 900));
        assert_eq!(output_size(1365, 767, 0, 0), (1364, 766));
        assert_eq!(output_size(1000, 3000, 1600, 1000), (332, 1000));
        assert_eq!(output_size(1, 1, 0, 0), (2, 2));
    }

    #[test]
    fn encoders_are_matched_to_their_codec() {
        assert_eq!(codec_of("h264_nvenc"), Ok(Codec::H264));
        assert_eq!(codec_of("libopenh264"), Ok(Codec::H264));
        assert_eq!(codec_of("libvpx-vp9"), Ok(Codec::Vp9));
        assert_eq!(codec_of("librav1e"), Ok(Codec::Av1));
        assert!(codec_of("mjpeg").is_err());
        let named = vec!["libx264".to_string()];
        assert_eq!(candidates(Codec::H264, &named), vec!["libx264"]);
        // Naming an H.264 encoder leaves the other codecs on their defaults.
        assert!(candidates(Codec::Vp9, &named).contains(&"libvpx-vp9".to_string()));
        assert!(!candidates(Codec::H264, &[]).contains(&"h264_mf".to_string()));
    }

    #[test]
    fn h264_leaves_as_annex_b_with_parameter_sets() {
        let record = [
            1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x1f, 1, 0, 2, 0x68, 0xee,
        ];
        let (length_size, sets) = parse_avcc(&record).unwrap();
        assert_eq!(length_size, 4);
        assert_eq!(sets, vec![vec![0x67, 0x64, 0, 0x1f], vec![0x68, 0xee]]);
        let mut out = Vec::new();
        annex_b(&[0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x06], 4, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x06]);
        assert!(annex_b(&[0, 0, 0, 9, 0x65], 4, &mut out).is_err());
    }

    #[test]
    fn keyframes_are_recognised_in_vp9_and_av1() {
        // VP9 profile 0: frame marker 10, profile 00, show_existing 0, frame_type 0.
        assert_eq!(vp9_key(&[0b1000_0000]), (true, 0));
        assert_eq!(vp9_key(&[0b1000_0100]), (false, 0));
        assert_eq!(vp9_key(&[0b1010_0000]), (true, 1));
        // AV1: temporal delimiter, sequence header (profile 0, level 8), frame (key).
        let unit = [
            0x12,
            0x00,
            0x0a,
            0x04,
            0x00,
            0x00,
            0x00,
            0x40,
            0x32,
            0x01,
            0b0001_0000,
        ];
        let (key, codec) = av1_key(&unit);
        assert!(key);
        assert_eq!(codec.as_deref(), Some("av01.0.08M.08"));
        let inter = [0x12, 0x00, 0x32, 0x01, 0b0010_0000];
        assert!(!av1_key(&inter).0);
    }
}

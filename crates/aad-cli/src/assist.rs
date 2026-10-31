//! `aad assist`: show a person the screen and pass their mouse and keyboard on.
//!
//! For the moments an agent needs a human: a login it cannot do, a captcha, a
//! judgement call. The screen of one browser page (CDP screencast) or of the
//! Windows desktop is streamed over a WebSocket on a loopback port, and the
//! viewer's input comes back the same way. A random token in the URL is
//! required for every connection; a host such as omem proxies it to its own
//! page. This is a channel for people and works in screen positions, so it is
//! not offered over MCP, where an agent must act on observed elements.
//!
//! Protocol. Control messages are JSON text frames; the screen is binary frames.
//! - server text: `{"type":"ready","source":..,"frames":{"header":"aad-frame/1",
//!   "codec":"webp","codecs":["webp","jpeg","h264",..]},"video":{"h264":"h264_nvenc",..}}`;
//!   `{"type":"stream","codec":"h264","webcodecs":"avc1.42c01f","encoder":..,
//!   "width":..,"height":..}` before the first frame of every video stream (a
//!   new stream starts with a keyframe); and `{"type":"error","message":..}`.
//! - server binary: one frame per message, a [`HEADER_LEN`]-byte big-endian
//!   header then the payload: version u8 (1), codec u8 (1 = jpeg, 2 = webp,
//!   each a whole picture; 3 = h264 as Annex B, 4 = vp9, 5 = av1 as OBUs,
//!   each one video frame), flags u8 (bit 0: keyframe, decodable on its own),
//!   reserved u8, width u32, height u32, header length u16 (24), reserved u16,
//!   timestamp u64 (milliseconds since the stream started). A reader skips
//!   `header length` bytes, so later versions can append fields; a codec it
//!   does not know is skipped, not misdrawn.
//! - viewer text: optionally first `{"type":"hello","accept":["h264","webp"]}`,
//!   naming the codecs it can draw in order of preference (the first offered
//!   codec, WebP by default, when not said); a later hello switches codec.
//!   `{"type":"keyframe"}` asks for a frame decodable on its own, after a
//!   decoding error. Then
//!   `{"type":"mouse","action":"move|down|up|wheel","x":0..1,"y":0..1,
//!   "button":"left|right|middle","dx":px,"dy":px}`,
//!   `{"type":"key","action":"down|up","key":"Enter","code":"Enter"}` for keys
//!   that are not text, and `{"type":"text","text":"..."}` for typed text.
//!
//! Nothing is sent while the screen stays the same. Pictures are sent whole;
//! video (see [`video`]) codes what did not change as skipped blocks, so a
//! small change costs a small frame.
use clap::Args;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod video;

#[derive(Args)]
pub struct AssistArgs {
    /// Stream this browser's page: its loopback CDP port.
    #[arg(long, value_name = "PORT", conflicts_with = "desktop")]
    browser_port: Option<u16>,
    /// Which page of that browser (metadata.page_id from `aad apps`); the
    /// first page when left out.
    #[arg(long, requires = "browser_port")]
    page: Option<String>,
    /// Stream the whole desktop instead (Windows).
    #[arg(long)]
    desktop: bool,
    /// Loopback address to listen on.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: String,
    /// Most frames per second (1..60); 5 for pictures and 30 for video when
    /// not given. An unchanged screen is not sent again.
    #[arg(long)]
    fps: Option<u32>,
    /// Picture and video quality (1..100). For video it sets the encoder's
    /// constant quantizer unless --bitrate is given.
    #[arg(long, default_value_t = 70)]
    quality: u8,
    /// Only show the screen; ignore the viewer's mouse and keyboard.
    #[arg(long)]
    view_only: bool,
    /// Codecs to offer, comma separated, from webp, jpeg, h264, vp9 and av1.
    /// A viewer that sends no hello gets the first. `auto` offers webp, jpeg
    /// and every video codec ffmpeg can encode here.
    #[arg(long, default_value = "auto", value_delimiter = ',')]
    codecs: Vec<String>,
    /// The ffmpeg that encodes video; else AAD_FFMPEG, else ffmpeg on PATH.
    /// This project does not ship one: without it only pictures are sent.
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,
    /// An ffmpeg encoder to use, such as libx264, h264_nvenc or libsvtav1;
    /// repeat to give fallbacks or one per codec. Hardware encoders are tried
    /// before software ones when none is named for a codec.
    #[arg(long = "encoder", value_name = "NAME")]
    encoders: Vec<String>,
    /// Video bitrate in kbit/s, held constant; constant quality from
    /// --quality when not given.
    #[arg(long, value_name = "KBPS")]
    bitrate: Option<u32>,
    /// Frames between video keyframes; 0 sends one only when a viewer starts
    /// or asks for one.
    #[arg(long, default_value_t = 0)]
    gop: u32,
    /// The encoder's speed preset, such as x264 `veryfast` or NVENC `p4`;
    /// each encoder's fastest when not given.
    #[arg(long)]
    preset: Option<String>,
    /// Video chroma: 420, or 444 for sharper coloured text (fewer encoders
    /// and decoders take it).
    #[arg(long, default_value = "420", value_parser = ["420", "444"])]
    chroma: String,
    /// One more ffmpeg output option, passed as it is; repeat for each word,
    /// e.g. `--ffmpeg-arg=-x264-params --ffmpeg-arg=ref=2`.
    #[arg(long = "ffmpeg-arg", value_name = "ARG", allow_hyphen_values = true)]
    ffmpeg_args: Vec<String>,
    /// Widest picture sent; a wider screen is scaled down. 0 for no limit.
    #[arg(long, default_value_t = 1600)]
    max_width: u32,
    /// Tallest picture sent; 0 for no limit.
    #[arg(long, default_value_t = 0)]
    max_height: u32,
    /// Report which video codecs and encoders work here, then exit.
    #[arg(long)]
    probe_video: bool,
}

type Result<T> = std::result::Result<T, String>;

/// Bytes before each binary frame's payload.
pub const HEADER_LEN: usize = 24;
const HEADER_VERSION: u8 = 1;

/// How a frame's payload is encoded. Numbers are part of the wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    /// A whole JPEG picture.
    Jpeg = 1,
    /// A whole lossy WebP picture: about a third smaller than JPEG at the
    /// same quality, and sharper on text.
    Webp = 2,
    /// One H.264 access unit, Annex B, parameter sets before each keyframe.
    H264 = 3,
    /// One VP9 frame.
    Vp9 = 4,
    /// One AV1 temporal unit (low-overhead OBUs).
    Av1 = 5,
}

impl Codec {
    const ALL: [Codec; 5] = [
        Codec::Webp,
        Codec::Jpeg,
        Codec::H264,
        Codec::Vp9,
        Codec::Av1,
    ];
    const VIDEO: [Codec; 3] = [Codec::H264, Codec::Vp9, Codec::Av1];

    fn name(self) -> &'static str {
        match self {
            Codec::Jpeg => "jpeg",
            Codec::Webp => "webp",
            Codec::H264 => "h264",
            Codec::Vp9 => "vp9",
            Codec::Av1 => "av1",
        }
    }

    fn named(name: &str) -> Option<Codec> {
        Codec::ALL.into_iter().find(|c| c.name() == name)
    }

    fn is_video(self) -> bool {
        Codec::VIDEO.contains(&self)
    }
}

/// The first codec in the viewer's list that this server offers.
fn choose(accept: &[Value], offered: &[Codec]) -> Option<Codec> {
    accept
        .iter()
        .find_map(|wanted| offered.iter().copied().find(|codec| wanted == codec.name()))
}

/// The codecs `--codecs` asks for, with the encoders found for video.
fn offered(asked: &[String], probe: &video::Probe) -> Result<Vec<Codec>> {
    let mut codecs = Vec::new();
    for name in asked.iter().map(|n| n.trim().to_ascii_lowercase()) {
        if name == "auto" {
            codecs.extend([Codec::Webp, Codec::Jpeg]);
            codecs.extend(
                Codec::VIDEO
                    .into_iter()
                    .filter(|c| probe.encoder(*c).is_some()),
            );
            continue;
        }
        let codec = Codec::named(&name).ok_or_else(|| {
            format!("--codecs: unknown codec {name:?}; use webp, jpeg, h264, vp9, av1 or auto")
        })?;
        if codec.is_video() && probe.encoder(codec).is_none() {
            return Err(match &probe.ffmpeg {
                None => format!(
                    "--codecs {name}: video needs ffmpeg; pass --ffmpeg PATH or set AAD_FFMPEG"
                ),
                Some(_) => format!(
                    "--codecs {name}: no {name} encoder worked here: {}",
                    json!(probe.tried)
                ),
            });
        }
        codecs.push(codec);
    }
    let mut seen = Vec::new();
    codecs.retain(|c| {
        let new = !seen.contains(c);
        seen.push(*c);
        new
    });
    if codecs.is_empty() {
        return Err("--codecs: offer at least one codec".into());
    }
    Ok(codecs)
}

/// How pixels are laid out in a [`Raw`] picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PixelFormat {
    Bgra,
    Rgba,
}

impl PixelFormat {
    fn ffmpeg(self) -> &'static str {
        match self {
            PixelFormat::Bgra => "bgra",
            PixelFormat::Rgba => "rgba",
        }
    }
}

/// Uncompressed pixels, four bytes each, top row first.
#[derive(Clone)]
struct Raw {
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    format: PixelFormat,
}

impl Raw {
    fn same(&self, other: &Raw) -> bool {
        (self.width, self.height, self.format) == (other.width, other.height, other.format)
            && (Arc::ptr_eq(&self.pixels, &other.pixels) || self.pixels == other.pixels)
    }
}

/// When an unchanged picture is sent again anyway, so a late viewer sees it.
/// Video needs no repeat: a viewer starts on a keyframe of the current screen.
const RESEND: Duration = Duration::from_secs(3);

/// Paces captures and drops pictures identical to the last one sent.
struct Pace {
    interval: Duration,
    last: Instant,
    last_digest: u64,
    last_sent: Instant,
}

impl Pace {
    fn new(fps: u32) -> Self {
        let long_ago = Instant::now() - Duration::from_secs(60);
        Self {
            interval: Self::interval(fps),
            last: long_ago,
            last_digest: 0,
            last_sent: long_ago,
        }
    }

    fn interval(fps: u32) -> Duration {
        Duration::from_millis(1000 / u64::from(fps.clamp(1, 60)))
    }

    fn rate(&mut self, fps: u32) {
        self.interval = Self::interval(fps);
    }

    /// Whether it is time to take another picture.
    fn due(&mut self) -> bool {
        if self.last.elapsed() < self.interval {
            return false;
        }
        self.last = Instant::now();
        true
    }

    /// Whether a picture with these bytes should be sent.
    fn changed(&mut self, bytes: &[u8]) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        let digest = hasher.finish();
        if digest == self.last_digest && self.last_sent.elapsed() < RESEND {
            return false;
        }
        self.last_digest = digest;
        self.last_sent = Instant::now();
        true
    }

    /// Send the next picture even if it looks the same, e.g. after a codec switch.
    fn reset(&mut self) {
        self.last_digest = 0;
    }
}

#[derive(Clone)]
struct Frame {
    codec: Codec,
    /// Decodable without earlier frames.
    keyframe: bool,
    data: Vec<u8>,
    width: u32,
    height: u32,
}

/// The binary message for one frame: header, then payload.
fn encode(frame: &Frame, at_ms: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(HEADER_LEN + frame.data.len());
    message.push(HEADER_VERSION);
    message.push(frame.codec as u8);
    message.push(u8::from(frame.keyframe));
    message.push(0);
    message.extend_from_slice(&frame.width.to_be_bytes());
    message.extend_from_slice(&frame.height.to_be_bytes());
    message.extend_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    message.extend_from_slice(&0u16.to_be_bytes());
    message.extend_from_slice(&at_ms.to_be_bytes());
    message.extend_from_slice(&frame.data);
    message
}

/// What the stream needs from a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    /// Pictures in this codec, which a source may encode itself.
    Picture(Codec),
    /// Uncompressed pixels, for a video encoder.
    Raw,
}

enum Picture {
    Encoded(Frame),
    Raw(Raw),
}

/// Where frames come from and where input goes.
trait Source {
    fn name(&self) -> Value;
    /// The screen, if it changed since the last capture. A source may answer
    /// [`Want::Picture`] with raw pixels, which are then encoded here.
    fn capture(&mut self, want: Want) -> Result<Option<Picture>>;
    /// Most captures per second.
    fn rate(&mut self, fps: u32);
    fn input(&mut self, event: &Value) -> Result<()>;
}

/// Everything about the stream decided at startup.
struct Config {
    offered: Vec<Codec>,
    probe: video::Probe,
    video: video::Settings,
    quality: u8,
    max_width: u32,
    max_height: u32,
    fps: Option<u32>,
    view_only: bool,
}

impl Config {
    fn fps(&self, codec: Codec) -> u32 {
        self.fps
            .unwrap_or(if codec.is_video() { 30 } else { 5 })
            .clamp(1, 60)
    }
}

fn video_settings(args: &AssistArgs) -> video::Settings {
    video::Settings {
        quality: args.quality.clamp(1, 100),
        bitrate: args.bitrate,
        gop: args.gop,
        preset: args.preset.clone(),
        chroma444: args.chroma == "444",
        extra: args.ffmpeg_args.clone(),
        fps: args.fps.unwrap_or(30).clamp(1, 60),
        max_width: args.max_width,
        max_height: args.max_height,
    }
}

/// Finds ffmpeg and a working encoder for each video codec `--codecs` asks for.
fn probe(args: &AssistArgs, settings: &video::Settings) -> Result<video::Probe> {
    for encoder in &args.encoders {
        video::codec_of(encoder)?;
    }
    let wanted: Vec<Codec> = Codec::VIDEO
        .into_iter()
        .filter(|codec| {
            args.probe_video
                || args.codecs.iter().any(|name| {
                    let name = name.trim().to_ascii_lowercase();
                    name == "auto" || name == codec.name()
                })
        })
        .collect();
    if wanted.is_empty() {
        return Ok(video::Probe::default());
    }
    let ffmpeg = video::locate(args.ffmpeg.as_deref())?;
    Ok(video::probe(ffmpeg, &wanted, &args.encoders, settings))
}

pub fn run(args: &AssistArgs) -> Result<Value> {
    let settings = video_settings(args);
    if args.probe_video {
        return Ok(probe(args, &settings)?.report());
    }
    let listen: std::net::SocketAddr = args.listen.parse().map_err(|e| format!("--listen: {e}"))?;
    if !listen.ip().is_loopback() {
        return Err("--listen must be a loopback address; proxy it to reach it elsewhere".into());
    }
    let probe = probe(args, &settings)?;
    let config = Config {
        offered: offered(&args.codecs, &probe)?,
        probe,
        video: settings,
        quality: args.quality.clamp(1, 100),
        max_width: args.max_width,
        max_height: args.max_height,
        fps: args.fps,
        view_only: args.view_only,
    };
    let first = config.offered[0];
    let mut source: Box<dyn Source> = match (args.browser_port, args.desktop) {
        (Some(port), false) => Box::new(browser::Page::start(
            port,
            args.page.as_deref(),
            config.fps(first),
            config.quality,
            (config.max_width, config.max_height),
        )?),
        (None, true) => desktop::open(config.fps(first))?,
        _ => return Err("pass --browser-port PORT or --desktop".into()),
    };
    let token = uuid::Uuid::new_v4().simple().to_string();
    let listener = TcpListener::bind(listen).map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let announce = json!({
        "url": format!("http://{address}/?token={token}"),
        "websocket": format!("ws://{address}/ws?token={token}"),
        "token": token,
        "source": source.name(),
        "view_only": args.view_only,
        "codecs": config.offered.iter().map(|c| c.name()).collect::<Vec<_>>(),
        "video": config.probe.report(),
    });
    println!("{announce}");
    let _ = std::io::stdout().flush();

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst));
    }
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                // One viewer at a time: a connection is served to its end.
                if let Err(error) = serve(stream, &token, source.as_mut(), &config, &stop) {
                    eprintln!("aad assist: {error}");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(json!({"status": "stopped"}))
}

/// Messages to the viewer, written on their own thread so a slow network
/// never holds up input. Captures wait while frames are queued, so the viewer
/// gets the newest screen rather than a backlog, and no video frame is ever
/// dropped from the middle of a stream.
#[derive(Clone)]
struct Outbox {
    sender: mpsc::Sender<(u8, Vec<u8>)>,
    frames: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
}

impl Outbox {
    fn start(mut stream: TcpStream) -> (Self, std::thread::JoinHandle<()>) {
        let (sender, outgoing) = mpsc::channel::<(u8, Vec<u8>)>();
        let frames = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let writer = {
            let (frames, failed) = (frames.clone(), failed.clone());
            std::thread::spawn(move || {
                for (opcode, payload) in outgoing {
                    let written = write_frame(&mut stream, opcode, &payload);
                    if opcode == 2 {
                        frames.fetch_sub(1, Ordering::SeqCst);
                    }
                    if written.is_err() {
                        failed.store(true, Ordering::SeqCst);
                        break;
                    }
                    if opcode == 8 {
                        break;
                    }
                }
            })
        };
        (
            Self {
                sender,
                frames,
                failed,
            },
            writer,
        )
    }

    fn text(&self, message: &Value) {
        let _ = self.sender.send((1, message.to_string().into_bytes()));
    }

    fn frame(&self, message: Vec<u8>) {
        self.frames.fetch_add(1, Ordering::SeqCst);
        if self.sender.send((2, message)).is_err() {
            self.frames.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn control(&self, opcode: u8, payload: Vec<u8>) {
        let _ = self.sender.send((opcode, payload));
    }

    /// Frames not yet written to the network.
    fn queued(&self) -> usize {
        self.frames.load(Ordering::SeqCst)
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }
}

fn serve(
    mut stream: TcpStream,
    token: &str,
    source: &mut dyn Source,
    config: &Config,
    stop: &AtomicBool,
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut request = String::new();
    reader.read_line(&mut request).map_err(|e| e.to_string())?;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        if headers.len() > 64 {
            return Err("too many headers".into());
        }
    }
    let target = request.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let given = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .unwrap_or_default();
    if !same(given.as_bytes(), token.as_bytes()) {
        let _ = stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return Ok(());
    }
    if path != "/ws" {
        let page = VIEWER.as_bytes();
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", page.len());
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(page);
        return Ok(());
    }
    let key = headers
        .iter()
        .find(|(name, _)| name == "sec-websocket-key")
        .map(|(_, value)| value.clone())
        .ok_or("not a WebSocket request")?;
    let accept = {
        use base64::Engine;
        use sha1::Digest;
        let mut hash = sha1::Sha1::new();
        hash.update(key.as_bytes());
        hash.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        base64::engine::general_purpose::STANDARD.encode(hash.finalize())
    };
    stream
        .write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    // On the handle the reader holds: on Windows a cloned socket keeps its
    // own timeout, and a viewer that only watches would be cut off after 5 s.
    reader
        .get_ref()
        .set_read_timeout(None)
        .map_err(|e| e.to_string())?;
    let (outbox, writer) = Outbox::start(stream.try_clone().map_err(|e| e.to_string())?);

    // The viewer's messages are read on their own thread; captures and input
    // are handled here, so the source is only ever used from one thread.
    let (sender, inbox) = mpsc::channel::<Value>();
    let closed = Arc::new(AtomicBool::new(false));
    {
        let closed = closed.clone();
        let outbox = outbox.clone();
        let mut reader = reader;
        std::thread::spawn(move || {
            while let Ok(Some((opcode, payload))) = read_frame(&mut reader) {
                match opcode {
                    1 => {
                        if let Ok(message) = serde_json::from_slice::<Value>(&payload) {
                            if sender.send(message).is_err() {
                                break;
                            }
                        }
                    }
                    8 => break,
                    9 => outbox.control(10, payload),
                    _ => {}
                }
            }
            closed.store(true, Ordering::SeqCst);
        });
    }
    let codecs: Vec<&str> = config.offered.iter().map(|c| c.name()).collect();
    let video: serde_json::Map<String, Value> = config
        .probe
        .chosen
        .iter()
        .filter(|(codec, _)| config.offered.contains(codec))
        .map(|(codec, name)| (codec.name().to_string(), json!(name)))
        .collect();
    let mut pipeline = Pipeline::new(config, config.offered[0]);
    source.rate(config.fps(pipeline.codec));
    outbox.text(
        &json!({"type": "ready", "source": source.name(), "view_only": config.view_only,
        "frames": {"header": "aad-frame/1", "codec": pipeline.codec.name(), "codecs": codecs},
        "video": video}),
    );
    let started = Instant::now();
    let result = (|| -> Result<()> {
        while !closed.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) && !outbox.failed() {
            while let Ok(event) = inbox.try_recv() {
                match event["type"].as_str() {
                    Some("hello") => match event["accept"].as_array().map(|a| choose(a, &config.offered)) {
                        None => {}
                        Some(Some(chosen)) => {
                            pipeline.switch(chosen);
                            source.rate(config.fps(chosen));
                        }
                        Some(None) => outbox.text(&json!({"type": "error",
                            "message": format!("no common codec; this server sends {}", codecs.join(", "))})),
                    },
                    Some("keyframe") => pipeline.keyframe(),
                    _ if config.view_only => {}
                    _ => {
                        if let Err(error) = source.input(&event) {
                            outbox.text(&json!({"type": "error", "message": error}));
                        }
                    }
                }
            }
            if outbox.queued() <= 1 && !pipeline.busy() {
                if let Some(picture) = source.capture(pipeline.want())? {
                    if let Err(error) = pipeline.feed(picture) {
                        pipeline.fail(&error, &outbox, source)?;
                    }
                }
            }
            loop {
                match pipeline.poll() {
                    Ok(Some(Output::Text(message))) => outbox.text(&message),
                    Ok(Some(Output::Frame(frame))) => {
                        outbox.frame(encode(&frame, started.elapsed().as_millis() as u64));
                    }
                    Ok(None) => break,
                    Err(error) => pipeline.fail(&error, &outbox, source)?,
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if outbox.failed() {
            return Err("lost the viewer: a frame could not be sent".into());
        }
        Ok(())
    })();
    outbox.control(8, Vec::new());
    drop(outbox);
    let _ = writer.join();
    let _ = stream.shutdown(std::net::Shutdown::Both);
    result
}

enum Output {
    Text(Value),
    Frame(Frame),
}

enum Stage {
    None,
    Pictures(Pictures),
    Video(video::Encoder),
}

/// Turns captures into frames in the codec the viewer chose, sending only
/// what changed.
struct Pipeline<'a> {
    config: &'a Config,
    codec: Codec,
    stage: Stage,
    /// The last raw picture sent on, to compare the next with and to start a
    /// new stream from.
    last: Option<Raw>,
    last_sent: Instant,
    /// The last picture that arrived encoded, to send again on request.
    last_frame: Option<Frame>,
    /// A new video stream has not been announced yet.
    announce: bool,
    out: VecDeque<Output>,
}

impl<'a> Pipeline<'a> {
    fn new(config: &'a Config, codec: Codec) -> Self {
        Self {
            config,
            codec,
            stage: Stage::None,
            last: None,
            last_sent: Instant::now(),
            last_frame: None,
            announce: false,
            out: VecDeque::new(),
        }
    }

    fn want(&self) -> Want {
        if self.codec.is_video() {
            Want::Raw
        } else {
            Want::Picture(self.codec)
        }
    }

    fn busy(&self) -> bool {
        match &self.stage {
            Stage::None => false,
            Stage::Pictures(pictures) => pictures.busy(),
            Stage::Video(encoder) => encoder.busy(),
        }
    }

    fn switch(&mut self, codec: Codec) {
        if codec != self.codec {
            self.codec = codec;
            self.stage = Stage::None;
            self.last_frame = None;
            self.restart();
        }
    }

    /// The current screen again, from a keyframe.
    fn keyframe(&mut self) {
        if self.codec.is_video() {
            self.stage = Stage::None;
        }
        if let Some(frame) = self.last_frame.clone() {
            self.out.push_back(Output::Frame(frame));
        }
        self.restart();
    }

    /// Sends the last raw picture into the current stage again.
    fn restart(&mut self) {
        if let Some(raw) = self.last.take() {
            if let Err(error) = self.raw(raw.clone(), true) {
                self.out
                    .push_back(Output::Text(json!({"type": "error", "message": error})));
            }
            self.last = Some(raw);
        }
    }

    fn feed(&mut self, picture: Picture) -> Result<()> {
        match picture {
            Picture::Encoded(frame) => {
                self.last_frame = Some(frame.clone());
                self.out.push_back(Output::Frame(frame));
                Ok(())
            }
            Picture::Raw(raw) => self.raw(raw, false),
        }
    }

    fn raw(&mut self, raw: Raw, force: bool) -> Result<()> {
        let unchanged = self.last.as_ref().is_some_and(|last| last.same(&raw));
        if unchanged && !force && (self.codec.is_video() || self.last_sent.elapsed() < RESEND) {
            return Ok(());
        }
        self.last = Some(raw.clone());
        self.last_sent = Instant::now();
        if self.codec.is_video() {
            let shape = (raw.width, raw.height, raw.format);
            if !matches!(&self.stage, Stage::Video(encoder) if encoder.input == shape) {
                // A new size or a first picture: a new stream, from a keyframe.
                self.stage = Stage::None;
                let ffmpeg = self.config.probe.ffmpeg.as_ref().ok_or("no ffmpeg")?;
                let name = self
                    .config
                    .probe
                    .encoder(self.codec)
                    .ok_or_else(|| format!("no {} encoder", self.codec.name()))?;
                let encoder = video::Encoder::start(
                    ffmpeg,
                    name,
                    &self.config.video,
                    raw.width,
                    raw.height,
                    raw.format,
                )?;
                self.stage = Stage::Video(encoder);
                self.announce = true;
            }
            if let Stage::Video(encoder) = &self.stage {
                encoder.push(raw.pixels);
            }
        } else {
            if !matches!(&self.stage, Stage::Pictures(p) if p.codec == self.codec) {
                self.stage = Stage::Pictures(Pictures::start(self.codec, self.config));
            }
            if let Stage::Pictures(pictures) = &self.stage {
                pictures.push(raw);
            }
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<Output>> {
        if let Some(output) = self.out.pop_front() {
            return Ok(Some(output));
        }
        match &mut self.stage {
            Stage::None => Ok(None),
            Stage::Pictures(pictures) => Ok(pictures.poll()?.map(Output::Frame)),
            Stage::Video(encoder) => {
                let Some(packet) = encoder.poll()? else {
                    return Ok(None);
                };
                let frame = Frame {
                    codec: encoder.codec,
                    keyframe: packet.key,
                    data: packet.data,
                    width: encoder.width,
                    height: encoder.height,
                };
                if self.announce {
                    self.announce = false;
                    self.out.push_back(Output::Frame(frame));
                    return Ok(Some(Output::Text(json!({"type": "stream",
                        "codec": encoder.codec.name(), "webcodecs": packet.webcodecs,
                        "encoder": encoder.name, "width": encoder.width, "height": encoder.height}))));
                }
                Ok(Some(Output::Frame(frame)))
            }
        }
    }

    /// A video encoder failed: say so, and go on with pictures if any are offered.
    fn fail(&mut self, error: &str, outbox: &Outbox, source: &mut dyn Source) -> Result<()> {
        let fallback = self.config.offered.iter().copied().find(|c| !c.is_video());
        let message = match fallback {
            Some(codec) => format!("{error}; sending {} instead", codec.name()),
            None => error.to_string(),
        };
        outbox.text(&json!({"type": "error", "message": message}));
        let Some(codec) = fallback.filter(|_| self.codec.is_video()) else {
            return Err(error.to_string());
        };
        self.stage = Stage::None;
        self.switch(codec);
        source.rate(self.config.fps(codec));
        Ok(())
    }
}

#[derive(Default)]
struct PictureSlot {
    next: Option<Raw>,
    working: bool,
    closed: bool,
}

/// Encodes raw pictures as WebP or JPEG on its own thread.
struct Pictures {
    codec: Codec,
    slot: Arc<(Mutex<PictureSlot>, Condvar)>,
    frames: mpsc::Receiver<Result<Frame>>,
}

impl Pictures {
    fn start(codec: Codec, config: &Config) -> Self {
        let slot = Arc::new((Mutex::new(PictureSlot::default()), Condvar::new()));
        let (sender, frames) = mpsc::channel();
        let (quality, max) = (config.quality, (config.max_width, config.max_height));
        {
            let slot = slot.clone();
            std::thread::spawn(move || {
                let (lock, wake) = &*slot;
                loop {
                    let raw = {
                        let mut state = lock.lock().unwrap();
                        loop {
                            if state.closed {
                                return;
                            }
                            if let Some(raw) = state.next.take() {
                                state.working = true;
                                break raw;
                            }
                            state = wake.wait(state).unwrap();
                        }
                    };
                    let frame = picture(&raw, codec, quality, max);
                    let sent = sender.send(frame);
                    lock.lock().unwrap().working = false;
                    if sent.is_err() {
                        return;
                    }
                }
            });
        }
        Self {
            codec,
            slot,
            frames,
        }
    }

    fn push(&self, raw: Raw) {
        let (lock, wake) = &*self.slot;
        lock.lock().unwrap().next = Some(raw);
        wake.notify_one();
    }

    fn busy(&self) -> bool {
        let state = self.slot.0.lock().unwrap();
        state.next.is_some() || state.working
    }

    fn poll(&self) -> Result<Option<Frame>> {
        match self.frames.try_recv() {
            Ok(frame) => frame.map(Some),
            Err(_) => Ok(None),
        }
    }
}

impl Drop for Pictures {
    fn drop(&mut self) {
        let (lock, wake) = &*self.slot;
        lock.lock().unwrap().closed = true;
        wake.notify_one();
    }
}

/// The largest size within the limits that keeps the aspect ratio.
fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let mut scale: f64 = 1.0;
    if max_width > 0 && width > max_width {
        scale = scale.min(f64::from(max_width) / f64::from(width));
    }
    if max_height > 0 && height > max_height {
        scale = scale.min(f64::from(max_height) / f64::from(height));
    }
    let side = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
    (side(width), side(height))
}

/// One raw picture as a whole WebP or JPEG.
fn picture(
    raw: &Raw,
    codec: Codec,
    quality: u8,
    (max_width, max_height): (u32, u32),
) -> Result<Frame> {
    let (r, b) = match raw.format {
        PixelFormat::Bgra => (2, 0),
        PixelFormat::Rgba => (0, 2),
    };
    let rgb: Vec<u8> = raw
        .pixels
        .chunks_exact(4)
        .flat_map(|p| [p[r], p[1], p[b]])
        .collect();
    let image = image::RgbImage::from_raw(raw.width, raw.height, rgb).ok_or("bad screen buffer")?;
    let (width, height) = fit(raw.width, raw.height, max_width, max_height);
    let image = if (width, height) == (raw.width, raw.height) {
        image
    } else {
        image::imageops::thumbnail(&image, width, height)
    };
    let data = match codec {
        Codec::Webp => webp::Encoder::from_rgb(image.as_raw(), image.width(), image.height())
            .encode(f32::from(quality))
            .to_vec(),
        Codec::Jpeg => {
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
                .encode_image(&image)
                .map_err(|e| e.to_string())?;
            jpeg
        }
        _ => return Err(format!("{} is not a picture codec", codec.name())),
    };
    Ok(Frame {
        codec,
        keyframe: true,
        data,
        width: image.width(),
        height: image.height(),
    })
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One frame from the viewer, unmasked. `None` at end of stream.
fn read_frame(reader: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 2];
    if reader.read_exact(&mut head).is_err() {
        return Ok(None);
    }
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut length = u64::from(head[1] & 0x7f);
    if length == 126 {
        let mut ext = [0u8; 2];
        reader.read_exact(&mut ext)?;
        length = u64::from(u16::from_be_bytes(ext));
    } else if length == 127 {
        let mut ext = [0u8; 8];
        reader.read_exact(&mut ext)?;
        length = u64::from_be_bytes(ext);
    }
    if length > 1024 * 1024 {
        return Err(std::io::Error::other("viewer message too large"));
    }
    let mut mask = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload)?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(Some((opcode, payload)))
}

fn write_frame(writer: &mut impl Write, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => head.push(n as u8),
        n if n <= 0xffff => {
            head.push(126);
            head.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            head.push(127);
            head.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    writer.write_all(&head)?;
    writer.write_all(payload)?;
    writer.flush()
}

fn fraction(event: &Value, key: &str) -> Result<f64> {
    event[key]
        .as_f64()
        .filter(|v| (0.0..=1.0).contains(v))
        .ok_or_else(|| format!("{key} must be a fraction from 0 to 1"))
}

/// Windows virtual-key code and DOM code for keys that are not text.
fn named_key(key: &str) -> Option<(u16, &'static str)> {
    Some(match key {
        "Enter" => (0x0d, "Enter"),
        "Backspace" => (0x08, "Backspace"),
        "Tab" => (0x09, "Tab"),
        "Escape" => (0x1b, "Escape"),
        "Delete" => (0x2e, "Delete"),
        "Insert" => (0x2d, "Insert"),
        "Home" => (0x24, "Home"),
        "End" => (0x23, "End"),
        "PageUp" => (0x21, "PageUp"),
        "PageDown" => (0x22, "PageDown"),
        "ArrowLeft" => (0x25, "ArrowLeft"),
        "ArrowUp" => (0x26, "ArrowUp"),
        "ArrowRight" => (0x27, "ArrowRight"),
        "ArrowDown" => (0x28, "ArrowDown"),
        "Shift" => (0x10, "ShiftLeft"),
        "Control" => (0x11, "ControlLeft"),
        "Alt" => (0x12, "AltLeft"),
        "Meta" => (0x5b, "MetaLeft"),
        "F1" => (0x70, "F1"),
        "F2" => (0x71, "F2"),
        "F3" => (0x72, "F3"),
        "F4" => (0x73, "F4"),
        "F5" => (0x74, "F5"),
        "F6" => (0x75, "F6"),
        "F7" => (0x76, "F7"),
        "F8" => (0x77, "F8"),
        "F9" => (0x78, "F9"),
        "F10" => (0x79, "F10"),
        "F11" => (0x7a, "F11"),
        "F12" => (0x7b, "F12"),
        // A letter or digit pressed with Ctrl or Alt, for shortcuts.
        one if one.len() == 1 && one.as_bytes()[0].is_ascii_alphanumeric() => {
            let upper = one.as_bytes()[0].to_ascii_uppercase();
            (u16::from(upper), "")
        }
        _ => return None,
    })
}

mod browser {
    use super::{
        fraction, named_key, Codec, Frame, Pace, Picture, PixelFormat, Raw, Result, Source, Want,
    };
    use aad_cdp::Client;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// One page. Chrome's screencast pushes a frame whenever the page paints,
    /// as JPEG or PNG: JPEG is sent as it is, PNG is decoded for a video
    /// encoder. Screencast offers no WebP, so WebP pictures are taken with
    /// captureScreenshot at the paced rate instead.
    pub struct Page {
        client: Client,
        page: String,
        pace: Pace,
        quality: u8,
        max: (u32, u32),
        /// The format the screencast is running in.
        screencast: Option<&'static str>,
        /// The newest screencast frame not yet passed on.
        pending: Option<Value>,
        /// The page's size in CSS pixels.
        size: (f64, f64),
        buttons: u32,
        modifiers: u32,
    }

    fn cdp(e: aad_cdp::CdpError) -> String {
        e.to_string()
    }

    fn decode(data: &Value) -> Result<Vec<u8>> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(data.as_str().unwrap_or_default())
            .map_err(|e| format!("bad picture from the browser: {e}"))
    }

    impl Page {
        pub fn start(
            port: u16,
            page: Option<&str>,
            fps: u32,
            quality: u8,
            max: (u32, u32),
        ) -> Result<Self> {
            let targets = Client::list_targets(port).map_err(cdp)?;
            let target = targets
                .iter()
                .filter(|t| t.kind == "page")
                .find(|t| page.is_none_or(|id| t.id == id))
                .ok_or("no such page in that browser")?;
            let mut client = Client::attach(target).map_err(cdp)?;
            client.set_deadline(Instant::now() + Duration::from_secs(5));
            client.command("Page.enable", json!({})).map_err(cdp)?;
            Ok(Self {
                client,
                page: target.id.clone(),
                pace: Pace::new(fps),
                quality,
                max,
                screencast: None,
                pending: None,
                size: (0.0, 0.0),
                buttons: 0,
                modifiers: 0,
            })
        }

        fn call(&mut self, method: &str, params: Value) -> Result<Value> {
            self.client
                .set_deadline(Instant::now() + Duration::from_secs(3));
            self.client.command(method, params).map_err(cdp)
        }

        fn command(&mut self, method: &str, params: Value) -> Result<()> {
            self.call(method, params).map(|_| ())
        }

        fn screencast(&mut self, format: Option<&'static str>) -> Result<()> {
            if format != self.screencast {
                if self.screencast.is_some() {
                    self.command("Page.stopScreencast", json!({}))?;
                    self.pending = None;
                }
                if let Some(format) = format {
                    let mut params = json!({"format": format, "everyNthFrame": 1});
                    if format == "jpeg" {
                        params["quality"] = json!(self.quality);
                    }
                    if self.max.0 > 0 {
                        params["maxWidth"] = json!(self.max.0);
                    }
                    if self.max.1 > 0 {
                        params["maxHeight"] = json!(self.max.1);
                    }
                    self.command("Page.startScreencast", params)?;
                }
                self.screencast = format;
                self.pace.reset();
            }
            Ok(())
        }

        /// The newest painted frame, at most once per paced interval; a frame
        /// that arrives early waits rather than being lost.
        fn screencast_frame(&mut self) -> Result<Option<Vec<u8>>> {
            let batch = self.client.drain_events();
            if let Some(failure) = batch.failure {
                return Err(format!("the page went away: {failure}"));
            }
            for event in batch.events {
                if event["method"] != "Page.screencastFrame" {
                    continue;
                }
                let params = &event["params"];
                // Unacknowledged frames stop the stream.
                let _ = self.command(
                    "Page.screencastFrameAck",
                    json!({"sessionId": params["sessionId"]}),
                );
                self.pending = Some(params.clone());
            }
            if self.pending.is_none() || !self.pace.due() {
                return Ok(None);
            }
            let params = self.pending.take().unwrap_or_default();
            let metadata = &params["metadata"];
            self.size = (
                metadata["deviceWidth"].as_f64().unwrap_or(0.0),
                metadata["deviceHeight"].as_f64().unwrap_or(0.0),
            );
            decode(&params["data"]).map(Some)
        }

        fn screenshot(&mut self) -> Result<Option<Frame>> {
            // Events still arrive and would pile up unread.
            if let Some(failure) = self.client.drain_events().failure {
                return Err(format!("the page went away: {failure}"));
            }
            if !self.pace.due() {
                return Ok(None);
            }
            let metrics = self.call("Page.getLayoutMetrics", json!({}))?;
            let viewport = &metrics["cssVisualViewport"];
            self.size = (
                viewport["clientWidth"].as_f64().unwrap_or(0.0),
                viewport["clientHeight"].as_f64().unwrap_or(0.0),
            );
            let quality = self.quality;
            let shot = self.call(
                "Page.captureScreenshot",
                json!({"format": "webp", "quality": quality, "optimizeForSpeed": true}),
            )?;
            let data = decode(&shot["data"])?;
            if !self.pace.changed(&data) {
                return Ok(None);
            }
            let (width, height) =
                picture_size(&data).unwrap_or((self.size.0 as u32, self.size.1 as u32));
            Ok(Some(Frame {
                codec: Codec::Webp,
                keyframe: true,
                data,
                width,
                height,
            }))
        }
    }

    /// The pixel size an encoded picture declares.
    fn picture_size(data: &[u8]) -> Option<(u32, u32)> {
        image::io::Reader::new(std::io::Cursor::new(data))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()
    }

    impl Source for Page {
        fn name(&self) -> Value {
            json!({"kind": "browser", "page_id": self.page})
        }

        fn rate(&mut self, fps: u32) {
            self.pace.rate(fps);
        }

        fn capture(&mut self, want: Want) -> Result<Option<Picture>> {
            match want {
                Want::Picture(Codec::Jpeg) => {
                    self.screencast(Some("jpeg"))?;
                    let Some(data) = self.screencast_frame()? else {
                        return Ok(None);
                    };
                    let (width, height) =
                        picture_size(&data).unwrap_or((self.size.0 as u32, self.size.1 as u32));
                    Ok(Some(Picture::Encoded(Frame {
                        codec: Codec::Jpeg,
                        keyframe: true,
                        data,
                        width,
                        height,
                    })))
                }
                Want::Picture(_) => {
                    self.screencast(None)?;
                    Ok(self.screenshot()?.map(Picture::Encoded))
                }
                Want::Raw => {
                    self.screencast(Some("png"))?;
                    let Some(data) = self.screencast_frame()? else {
                        return Ok(None);
                    };
                    let image = image::load_from_memory_with_format(&data, image::ImageFormat::Png)
                        .map_err(|e| format!("bad picture from the browser: {e}"))?
                        .to_rgba8();
                    let (width, height) = image.dimensions();
                    Ok(Some(Picture::Raw(Raw {
                        pixels: Arc::new(image.into_raw()),
                        width,
                        height,
                        format: PixelFormat::Rgba,
                    })))
                }
            }
        }

        fn input(&mut self, event: &Value) -> Result<()> {
            match event["type"].as_str() {
                Some("mouse") => {
                    let (x, y) = (
                        fraction(event, "x")? * self.size.0,
                        fraction(event, "y")? * self.size.1,
                    );
                    let (button, mask) = match event["button"].as_str() {
                        Some("right") => ("right", 2),
                        Some("middle") => ("middle", 4),
                        _ => ("left", 1),
                    };
                    let kind = match event["action"].as_str() {
                        Some("down") => {
                            self.buttons |= mask;
                            "mousePressed"
                        }
                        Some("up") => {
                            self.buttons &= !mask;
                            "mouseReleased"
                        }
                        Some("wheel") => "mouseWheel",
                        _ => "mouseMoved",
                    };
                    let mut params = json!({"type": kind, "x": x, "y": y, "modifiers": self.modifiers, "buttons": self.buttons});
                    if kind == "mousePressed" || kind == "mouseReleased" {
                        params["button"] = json!(button);
                        params["clickCount"] =
                            json!(event["count"].as_u64().unwrap_or(1).clamp(1, 3));
                    }
                    if kind == "mouseWheel" {
                        params["deltaX"] = json!(event["dx"].as_f64().unwrap_or(0.0));
                        params["deltaY"] = json!(event["dy"].as_f64().unwrap_or(0.0));
                    }
                    self.command("Input.dispatchMouseEvent", params)
                }
                Some("key") => {
                    let key = event["key"].as_str().unwrap_or_default();
                    let (code, dom) =
                        named_key(key).ok_or_else(|| format!("unknown key {key:?}"))?;
                    let bit = match key {
                        "Alt" => 1,
                        "Control" => 2,
                        "Meta" => 4,
                        "Shift" => 8,
                        _ => 0,
                    };
                    let down = event["action"] != "up";
                    if down {
                        self.modifiers |= bit;
                    } else {
                        self.modifiers &= !bit;
                    }
                    let text = if key == "Enter" && down && self.modifiers == 0 {
                        "\r"
                    } else {
                        ""
                    };
                    let dom = if dom.is_empty() {
                        format!("Key{}", key.to_ascii_uppercase())
                    } else {
                        dom.to_string()
                    };
                    self.command(
                        "Input.dispatchKeyEvent",
                        json!({"type": if down {"keyDown"} else {"keyUp"}, "key": key, "code": dom,
                               "windowsVirtualKeyCode": code, "modifiers": self.modifiers, "text": text}),
                    )
                }
                Some("text") => {
                    let text = event["text"].as_str().unwrap_or_default();
                    if text.len() > 4096 {
                        return Err("text is limited to 4096 bytes".into());
                    }
                    self.command("Input.insertText", json!({"text": text}))
                }
                _ => Err("unknown event type".into()),
            }
        }
    }
}

#[cfg(not(windows))]
mod desktop {
    use super::{Result, Source};
    pub fn open(_fps: u32) -> Result<Box<dyn Source>> {
        Err("desktop assistance is available on Windows so far; use --browser-port for a browser page".into())
    }
}

#[cfg(windows)]
mod desktop {
    use super::{fraction, named_key, Pace, Picture, PixelFormat, Raw, Result, Source, Want};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    pub struct Desktop {
        pace: Pace,
    }

    pub fn open(fps: u32) -> Result<Box<dyn Source>> {
        Ok(Box::new(Desktop {
            pace: Pace::new(fps),
        }))
    }

    /// The virtual screen as BGRA, with its size.
    fn grab() -> Result<(Vec<u8>, u32, u32)> {
        unsafe {
            let (left, top) = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            );
            let (width, height) = (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            );
            if width <= 0 || height <= 0 {
                return Err("no screen to capture".into());
            }
            let screen = GetDC(HWND::default());
            let memory = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let old = SelectObject(memory, bitmap);
            let copied = BitBlt(
                memory,
                0,
                0,
                width,
                height,
                screen,
                left,
                top,
                SRCCOPY | CAPTUREBLT,
            );
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bgra = vec![0u8; width as usize * height as usize * 4];
            let lines = GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                Some(bgra.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            SelectObject(memory, old);
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(memory);
            ReleaseDC(HWND::default(), screen);
            copied.map_err(|e| format!("could not copy the screen: {e}"))?;
            if lines == 0 {
                return Err("could not read the screen".into());
            }
            Ok((bgra, width as u32, height as u32))
        }
    }

    fn send(inputs: &[INPUT]) -> Result<()> {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err("Windows refused the input (a higher-privileged window may be in front)".into())
        }
    }

    fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    impl Source for Desktop {
        fn name(&self) -> Value {
            json!({"kind": "desktop"})
        }

        fn rate(&mut self, fps: u32) {
            self.pace.rate(fps);
        }

        /// Raw pixels whatever is wanted: pictures are encoded off this thread.
        fn capture(&mut self, _want: Want) -> Result<Option<Picture>> {
            if !self.pace.due() {
                return Ok(None);
            }
            let (bgra, width, height) = grab()?;
            Ok(Some(Picture::Raw(Raw {
                pixels: Arc::new(bgra),
                width,
                height,
                format: PixelFormat::Bgra,
            })))
        }

        fn input(&mut self, event: &Value) -> Result<()> {
            match event["type"].as_str() {
                Some("mouse") => {
                    let x = (fraction(event, "x")? * 65535.0).round() as i32;
                    let y = (fraction(event, "y")? * 65535.0).round() as i32;
                    let at = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                    let (down, up) = match event["button"].as_str() {
                        Some("right") => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                        Some("middle") => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
                        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                    };
                    let mut inputs = vec![mouse(x, y, 0, at)];
                    match event["action"].as_str() {
                        Some("down") => inputs.push(mouse(x, y, 0, at | down)),
                        Some("up") => inputs.push(mouse(x, y, 0, at | up)),
                        Some("wheel") => {
                            // Browsers report about 100 pixels per notch; Windows 120 units.
                            let notch = |pixels: f64| (-pixels / 100.0 * 120.0).round() as i32;
                            let dy = event["dy"].as_f64().unwrap_or(0.0);
                            let dx = event["dx"].as_f64().unwrap_or(0.0);
                            if dy != 0.0 {
                                inputs.push(mouse(0, 0, notch(dy), MOUSEEVENTF_WHEEL));
                            }
                            if dx != 0.0 {
                                inputs.push(mouse(0, 0, -notch(dx), MOUSEEVENTF_HWHEEL));
                            }
                        }
                        _ => {}
                    }
                    send(&inputs)
                }
                Some("key") => {
                    let name = event["key"].as_str().unwrap_or_default();
                    let (vk, _) = named_key(name).ok_or_else(|| format!("unknown key {name:?}"))?;
                    let flags = if event["action"] == "up" {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    };
                    send(&[key(vk, 0, flags)])
                }
                Some("text") => {
                    let text = event["text"].as_str().unwrap_or_default();
                    if text.len() > 4096 {
                        return Err("text is limited to 4096 bytes".into());
                    }
                    let inputs: Vec<INPUT> = text
                        .encode_utf16()
                        .flat_map(|unit| {
                            [
                                key(0, unit, KEYEVENTF_UNICODE),
                                key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                            ]
                        })
                        .collect();
                    send(&inputs)
                }
                _ => Err("unknown event type".into()),
            }
        }
    }
}

/// A minimal viewer, for using `aad assist` directly in a browser. Hosts such
/// as omem draw their own from the same messages.
const VIEWER: &str = include_str!("assist/viewer.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_header_carries_what_a_decoder_needs() {
        let frame = Frame {
            codec: Codec::H264,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x67],
            width: 1536,
            height: 960,
        };
        let message = encode(&frame, 1234);
        assert_eq!(message.len(), HEADER_LEN + 5);
        assert_eq!(&message[..4], &[1, 3, 1, 0]);
        assert_eq!(u32::from_be_bytes(message[4..8].try_into().unwrap()), 1536);
        assert_eq!(u32::from_be_bytes(message[8..12].try_into().unwrap()), 960);
        assert_eq!(
            usize::from(u16::from_be_bytes(message[12..14].try_into().unwrap())),
            HEADER_LEN
        );
        assert_eq!(
            u64::from_be_bytes(message[16..24].try_into().unwrap()),
            1234
        );
        assert_eq!(&message[HEADER_LEN..], &[0, 0, 0, 1, 0x67]);
    }

    #[test]
    fn the_viewer_picks_among_the_offered_codecs() {
        let pictures = [Codec::Webp, Codec::Jpeg];
        assert_eq!(choose(&[json!("jpeg")], &pictures), Some(Codec::Jpeg));
        assert_eq!(
            choose(&[json!("h264"), json!("webp")], &pictures),
            Some(Codec::Webp)
        );
        assert_eq!(choose(&[json!("h264")], &pictures), None);
        let all = [Codec::Webp, Codec::Jpeg, Codec::H264];
        assert_eq!(
            choose(&[json!("h264"), json!("webp")], &all),
            Some(Codec::H264)
        );
    }

    #[test]
    fn codecs_are_offered_only_when_they_can_be_sent() {
        let none = video::Probe::default();
        let auto = offered(&["auto".into()], &none).unwrap();
        assert_eq!(auto, vec![Codec::Webp, Codec::Jpeg]);
        let found = video::Probe {
            chosen: vec![(Codec::Av1, "libsvtav1".into())],
            ..video::Probe::default()
        };
        assert_eq!(
            offered(&["auto".into()], &found).unwrap(),
            vec![Codec::Webp, Codec::Jpeg, Codec::Av1]
        );
        assert_eq!(
            offered(&["av1".into(), "webp".into(), "av1".into()], &found).unwrap(),
            vec![Codec::Av1, Codec::Webp]
        );
        assert!(offered(&["h264".into()], &none)
            .unwrap_err()
            .contains("ffmpeg"));
        assert!(offered(&["png".into()], &none).is_err());
    }

    #[test]
    fn pictures_shrink_to_fit_and_report_their_real_size() {
        assert_eq!(fit(3840, 2160, 1600, 0), (1600, 900));
        assert_eq!(fit(800, 600, 1600, 0), (800, 600));
        assert_eq!(fit(800, 600, 0, 300), (400, 300));
        let raw = Raw {
            pixels: Arc::new([255, 0, 0, 255].repeat(40 * 20)),
            width: 40,
            height: 20,
            format: PixelFormat::Rgba,
        };
        let frame = picture(&raw, Codec::Jpeg, 80, (20, 0)).unwrap();
        assert_eq!((frame.width, frame.height), (20, 10));
        assert_eq!(&frame.data[..3], &[0xff, 0xd8, 0xff]);
        let decoded = image::load_from_memory(&frame.data).unwrap().to_rgb8();
        // Red stays red: the channel order of RGBA input is respected.
        let pixel = decoded.get_pixel(10, 5);
        assert!(pixel[0] > 200 && pixel[2] < 60, "{pixel:?}");
        let webp = picture(&raw, Codec::Webp, 80, (0, 0)).unwrap();
        assert_eq!(&webp.data[8..12], b"WEBP");
    }

    #[test]
    fn identical_pictures_are_recognised() {
        let pixels = Arc::new(vec![1u8; 16]);
        let a = Raw {
            pixels: pixels.clone(),
            width: 2,
            height: 2,
            format: PixelFormat::Bgra,
        };
        let b = Raw {
            pixels: Arc::new(vec![1u8; 16]),
            ..a.clone()
        };
        let c = Raw {
            pixels: Arc::new(vec![2u8; 16]),
            ..a.clone()
        };
        assert!(a.same(&b));
        assert!(!a.same(&c));
        assert!(!a.same(&Raw {
            width: 4,
            height: 1,
            ..a.clone()
        }));
    }
}

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
//!   "codec":"webp","codecs":["webp","jpeg","tiles","h264",..]},"mode":{"codec":"webp"},
//!   "video":{"h264":"h264_nvenc",..}}`; `{"type":"mode","mode":{"codec":"tiles",
//!   "video":"h264"}}` after a hello; `{"type":"stream","codec":"h264",
//!   "webcodecs":"avc1.42c01f","encoder":..,"width":..,"height":..}` before the
//!   first frame of every video stream (a new stream starts with a keyframe);
//!   `{"type":"cursor","x":0..1,"y":0..1,"visible":true,"shape":{"png":..,
//!   "width":..,"height":..,"hotspot":[x,y],"screen":[w,h]}}` where the desktop's
//!   pointer is (`shape` when it changed, in screen pixels); and
//!   `{"type":"error","message":..}`.
//! - server binary: one frame per message, a big-endian header then the
//!   payload: version u8 (1), codec u8 (1 = jpeg, 2 = webp, each a whole
//!   picture; 3 = h264 as Annex B, 4 = vp9, 5 = av1 as OBUs, each one video
//!   frame; 6 = tiles, patches over the last frame, see [`tiles`]), flags u8
//!   (bit 0: keyframe, decodable on its own; bit 1: regions follow), reserved
//!   u8, width u32, height u32, header length u16, reserved u16, timestamp u64
//!   (milliseconds since the stream started): [`HEADER_LEN`] bytes. With
//!   regions, a u16 count and x, y, w, h u16 each follow: only there did the
//!   picture change, and a video frame shown beside tiles is drawn only there.
//!   A reader skips `header length` bytes, so later versions can append
//!   fields; a codec it does not know is skipped, not misdrawn.
//! - viewer text: optionally first `{"type":"hello","accept":["tiles","h264","webp"]}`,
//!   naming the codecs it can draw in order of preference (the first offered
//!   codec, WebP by default, when not said); tiles first with a video codec
//!   after them means both, adaptively. A later hello switches.
//!   `{"type":"keyframe"}` asks for a frame decodable on its own, after a
//!   decoding error. Then
//!   `{"type":"mouse","action":"move|down|up|wheel","x":0..1,"y":0..1,
//!   "button":"left|right|middle","count":1..3,"dx":px,"dy":px}` (`count`
//!   makes double and triple clicks on a page),
//!   `{"type":"key","action":"down|up","key":"/","code":"Slash"}` for keys that
//!   type nothing and for shortcuts (see [`keys`]: `code` names the physical
//!   key, so shortcuts work on any layout), `{"type":"text","text":"..."}` for
//!   typed text, `{"type":"release"}` to let go of every key and button the
//!   viewer holds (also done when it leaves), and `{"type":"clipboard"}`,
//!   answered with `{"type":"clipboard","text":..,"characters":..,
//!   "truncated":..}` (the desktop; not with --view-only or --no-clipboard).
//!
//! Nothing is sent while the screen stays the same. Pictures are sent whole;
//! tiles send only the 32-pixel tiles that changed, and moved regions as
//! copies; video (see [`video`]) codes what did not change as skipped blocks.
//! Adaptively, small changes go as tiles and large or rapid ones as video,
//! and once the screen settles what video drew is sent again as sharp tiles.
use clap::Args;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod damage;
mod keys;
mod scale;
#[cfg(windows)]
mod screen;
mod tiles;
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
    /// Codecs to offer, comma separated, from webp, jpeg, tiles, h264, vp9
    /// and av1. A viewer that sends no hello gets the first. `auto` offers
    /// webp, jpeg, tiles and every video codec ffmpeg can encode here.
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
    /// For a viewer taking tiles and video: send a change as video when it
    /// covers at least this percentage of the screen.
    #[arg(long, default_value_t = 30, value_name = "PERCENT")]
    video_area: u32,
    /// ... or when changes of at least 1% of the screen come this many times
    /// a second (scrolling, playback). Once the screen settles, what video
    /// drew is sent again as sharp tiles.
    #[arg(long, default_value_t = 8, value_name = "PER_SECOND")]
    video_rate: u32,
    /// Do not send where the desktop's mouse pointer is and what it looks like.
    #[arg(long)]
    hide_cursor: bool,
    /// Do not let the viewer read this side's clipboard (it may after a copy
    /// shortcut, unless --view-only).
    #[arg(long)]
    no_clipboard: bool,
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
    /// Only the regions that changed, drawn over the previous frame (see
    /// [`tiles`]).
    Tiles = 6,
}

impl Codec {
    const ALL: [Codec; 6] = [
        Codec::Webp,
        Codec::Jpeg,
        Codec::Tiles,
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
            Codec::Tiles => "tiles",
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
            codecs.extend([Codec::Webp, Codec::Jpeg, Codec::Tiles]);
            codecs.extend(
                Codec::VIDEO
                    .into_iter()
                    .filter(|c| probe.encoder(*c).is_some()),
            );
            continue;
        }
        let codec = Codec::named(&name).ok_or_else(|| {
            format!(
                "--codecs: unknown codec {name:?}; use webp, jpeg, tiles, h264, vp9, av1 or auto"
            )
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
    /// What changed since the source's previous picture, when the source
    /// knows; the pictures are compared otherwise.
    damage: Option<damage::Damage>,
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
    /// Where the picture changed, in frame pixels: a video frame shown beside
    /// tiles is drawn only there, so what tiles drew sharp elsewhere stays.
    regions: Option<Vec<damage::Rect>>,
}

/// Most regions sent with a frame; more are joined into one around them all.
const MAX_REGIONS: usize = 256;

/// The binary message for one frame: header, then payload. With regions,
/// flag bit 1 is set and the header grows by a u16 count and x, y, w, h u16
/// each.
fn encode(frame: &Frame, at_ms: u64) -> Vec<u8> {
    let regions: Option<Vec<damage::Rect>> = frame.regions.as_ref().map(|regions| {
        if regions.len() <= MAX_REGIONS {
            return regions.clone();
        }
        let left = regions.iter().map(|r| r.x).min().unwrap_or(0);
        let top = regions.iter().map(|r| r.y).min().unwrap_or(0);
        let right = regions.iter().map(|r| r.x + r.w).max().unwrap_or(0);
        let bottom = regions.iter().map(|r| r.y + r.h).max().unwrap_or(0);
        vec![damage::Rect {
            x: left,
            y: top,
            w: right - left,
            h: bottom - top,
        }]
    });
    let header = HEADER_LEN + regions.as_ref().map_or(0, |r| 2 + 8 * r.len());
    let mut message = Vec::with_capacity(header + frame.data.len());
    message.push(HEADER_VERSION);
    message.push(frame.codec as u8);
    message.push(u8::from(frame.keyframe) | if regions.is_some() { 2 } else { 0 });
    message.push(0);
    message.extend_from_slice(&frame.width.to_be_bytes());
    message.extend_from_slice(&frame.height.to_be_bytes());
    message.extend_from_slice(&(header as u16).to_be_bytes());
    message.extend_from_slice(&0u16.to_be_bytes());
    message.extend_from_slice(&at_ms.to_be_bytes());
    if let Some(regions) = &regions {
        message.extend_from_slice(&(regions.len() as u16).to_be_bytes());
        for r in regions {
            for value in [r.x, r.y, r.w, r.h] {
                message.extend_from_slice(&(value.min(u32::from(u16::MAX)) as u16).to_be_bytes());
            }
        }
    }
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
    /// A `cursor` message when the pointer changed; `fresh` for a new viewer.
    fn cursor(&mut self, _fresh: bool) -> Option<Value> {
        None
    }
    /// Lets go of every key and button the viewer still holds: when it
    /// leaves or loses focus, nothing stays pressed on this side.
    fn release(&mut self) {}
    /// The clipboard's text, for a viewer copying from this side.
    fn clipboard(&mut self) -> Result<Value> {
        Err("this source has no clipboard to read".into())
    }
}

/// Most clipboard text handed to a viewer at once.
const CLIPBOARD_CHARACTERS: usize = 100_000;

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
    video_area: u32,
    video_rate: u32,
    hide_cursor: bool,
    no_clipboard: bool,
}

impl Config {
    /// Whole pictures are costly, so they come 5 a second by default; the
    /// other modes send only changes and default to 30.
    fn fps(&self, mode: Mode) -> u32 {
        self.fps
            .unwrap_or(if matches!(mode, Mode::Picture(_)) {
                5
            } else {
                30
            })
            .clamp(1, 60)
    }

    /// What a viewer that says nothing gets: the first codec offered.
    fn first(&self) -> Mode {
        Mode::of(self.offered[0], &[], &self.offered)
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
        video_area: args.video_area.clamp(1, 100),
        video_rate: args.video_rate.max(1),
        hide_cursor: args.hide_cursor,
        no_clipboard: args.no_clipboard,
    };
    let first = config.first();
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
    let mut pipeline = Pipeline::new(config, config.first());
    source.rate(config.fps(pipeline.mode));
    outbox.text(
        &json!({"type": "ready", "source": source.name(), "view_only": config.view_only,
        "frames": {"header": "aad-frame/1", "codec": config.offered[0].name(), "codecs": codecs},
        "mode": pipeline.mode.describe(), "video": video}),
    );
    let mut fresh = true;
    let started = Instant::now();
    let result = (|| -> Result<()> {
        while !closed.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) && !outbox.failed() {
            while let Ok(event) = inbox.try_recv() {
                match event["type"].as_str() {
                    Some("hello") => match event["accept"].as_array().map(|a| (choose(a, &config.offered), a)) {
                        None => {}
                        Some((Some(chosen), accept)) => {
                            let mode = Mode::of(chosen, accept, &config.offered);
                            pipeline.switch(mode);
                            source.rate(config.fps(mode));
                            outbox.text(&json!({"type": "mode", "mode": mode.describe()}));
                        }
                        Some((None, _)) => outbox.text(&json!({"type": "error",
                            "message": format!("no common codec; this server sends {}", codecs.join(", "))})),
                    },
                    Some("keyframe") => pipeline.keyframe(),
                    _ if config.view_only => {}
                    Some("release") => source.release(),
                    Some("clipboard") if config.no_clipboard => outbox.text(&json!({"type": "error",
                        "message": "this server does not share its clipboard (--no-clipboard)"})),
                    Some("clipboard") => match source.clipboard() {
                        Ok(clipboard) => outbox.text(&clipboard),
                        Err(error) => outbox.text(&json!({"type": "error", "message": error})),
                    },
                    _ => {
                        if let Err(error) = source.input(&event) {
                            outbox.text(&json!({"type": "error", "message": error}));
                        }
                    }
                }
            }
            if !config.hide_cursor {
                if let Some(cursor) = source.cursor(fresh) {
                    outbox.text(&cursor);
                }
                fresh = false;
            }
            pipeline.tick();
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
    // Whatever the viewer still held down is let go, however it left.
    source.release();
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

/// How frames are made for one viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Whole pictures in this codec.
    Picture(Codec),
    /// Every change as video.
    Video(Codec),
    /// Only the changed regions, as patches.
    Tiles,
    /// Patches for small changes, this video codec for large or rapid ones,
    /// and patches again, sharp, for what video drew once the screen settles.
    Adaptive(Codec),
}

impl Mode {
    /// The mode for a codec the viewer chose: tiles become adaptive when the
    /// viewer also takes a video codec this server offers.
    fn of(chosen: Codec, accept: &[Value], offered: &[Codec]) -> Mode {
        match chosen {
            Codec::Tiles => accept
                .iter()
                .find_map(|wanted| {
                    offered
                        .iter()
                        .copied()
                        .find(|c| c.is_video() && wanted == c.name())
                })
                .map_or(Mode::Tiles, Mode::Adaptive),
            c if c.is_video() => Mode::Video(c),
            c => Mode::Picture(c),
        }
    }

    fn describe(self) -> Value {
        match self {
            Mode::Picture(c) | Mode::Video(c) => json!({"codec": c.name()}),
            Mode::Tiles => json!({"codec": "tiles"}),
            Mode::Adaptive(c) => json!({"codec": "tiles", "video": c.name()}),
        }
    }
}

/// How long the screen must stay still before what video drew is sent
/// again as patches.
const SETTLE: Duration = Duration::from_millis(400);

/// Which stage the next frame comes from, so frames leave in the order their
/// pictures went in even when the two stages finish out of order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    Tiles,
    Video,
}

/// Turns captures into frames for the viewer's mode, sending only what
/// changed.
struct Pipeline<'a> {
    config: &'a Config,
    mode: Mode,
    pictures: Option<Pictures>,
    tiles: Option<tiles::Tiles>,
    video: Option<video::Encoder>,
    /// The last raw picture sent on: what the next is compared with, and what
    /// a new stream starts from.
    last: Option<Raw>,
    last_sent: Instant,
    /// The last picture that arrived encoded, to send again on request.
    last_frame: Option<Frame>,
    /// A new video stream has not been announced yet.
    announce: bool,
    /// Adaptive: regions last drawn by video, to send again sharp.
    blurred: Option<damage::Mask>,
    /// Adaptive: when large changes happened in the last second.
    changes: VecDeque<Instant>,
    /// Adaptive: the stages frames are awaited from, oldest first.
    order: VecDeque<Next>,
    /// The screen at the size pictures and video are sent at, kept up to date
    /// from each change.
    scaled: scale::Downscale,
    /// Adaptive: where each video frame in flight changed the screen.
    regions: VecDeque<Vec<damage::Rect>>,
    out: VecDeque<Output>,
}

impl<'a> Pipeline<'a> {
    fn new(config: &'a Config, mode: Mode) -> Self {
        Self {
            config,
            mode,
            pictures: None,
            tiles: None,
            video: None,
            last: None,
            last_sent: Instant::now(),
            last_frame: None,
            announce: false,
            blurred: None,
            changes: VecDeque::new(),
            order: VecDeque::new(),
            scaled: scale::Downscale::default(),
            regions: VecDeque::new(),
            out: VecDeque::new(),
        }
    }

    fn want(&self) -> Want {
        match self.mode {
            Mode::Picture(codec) => Want::Picture(codec),
            _ => Want::Raw,
        }
    }

    fn busy(&self) -> bool {
        self.pictures.as_ref().is_some_and(Pictures::busy)
            || self.tiles.as_ref().is_some_and(tiles::Tiles::busy)
            || self.video.as_ref().is_some_and(video::Encoder::busy)
            // Adaptive: one picture at a time, so patches never land under
            // a video frame of an older screen.
            || (matches!(self.mode, Mode::Adaptive(_))
                && self.video.as_ref().is_some_and(video::Encoder::in_flight))
    }

    fn switch(&mut self, mode: Mode) {
        if mode != self.mode {
            self.mode = mode;
            self.pictures = None;
            self.tiles = None;
            self.video = None;
            self.blurred = None;
            self.order.clear();
            self.regions.clear();
            self.scaled = scale::Downscale::default();
            self.last_frame = None;
            self.restart();
        }
    }

    /// The current screen again, from a keyframe.
    fn keyframe(&mut self) {
        self.video = None;
        self.order.retain(|next| *next != Next::Video);
        self.regions.clear();
        if let Some(frame) = self.last_frame.clone() {
            self.out.push_back(Output::Frame(frame));
        }
        self.restart();
    }

    /// Sends the last raw picture again, whole.
    fn restart(&mut self) {
        if let Some(raw) = self.last.clone() {
            if let Err(error) = self.send(raw, None) {
                self.out
                    .push_back(Output::Text(json!({"type": "error", "message": error})));
            }
        }
    }

    fn feed(&mut self, picture: Picture) -> Result<()> {
        match picture {
            Picture::Encoded(frame) => {
                self.last_frame = Some(frame.clone());
                self.out.push_back(Output::Frame(frame));
                Ok(())
            }
            Picture::Raw(raw) => self.raw(raw),
        }
    }

    fn raw(&mut self, raw: Raw) -> Result<()> {
        let damage = match &self.last {
            Some(last)
                if (last.width, last.height, last.format)
                    == (raw.width, raw.height, raw.format) =>
            {
                Some(match &raw.damage {
                    Some(damage) => {
                        damage::refine(&last.pixels, &raw.pixels, raw.width, raw.height, damage)
                    }
                    None => damage::Damage {
                        moves: Vec::new(),
                        dirty: damage::diff(&last.pixels, &raw.pixels, raw.width, raw.height)
                            .rects(),
                    },
                })
            }
            _ => None,
        };
        if damage.as_ref().is_some_and(damage::Damage::is_empty) {
            // Pictures are repeated now and then for a viewer that missed one.
            if matches!(self.mode, Mode::Picture(_)) && self.last_sent.elapsed() >= RESEND {
                return self.send(raw, None);
            }
            return Ok(());
        }
        self.send(raw, damage)
    }

    /// Sends a picture on; `damage` is what changed since the last one, or
    /// `None` for a whole new picture.
    fn send(&mut self, raw: Raw, damage: Option<damage::Damage>) -> Result<()> {
        self.last = Some(raw.clone());
        self.last_sent = Instant::now();
        let (max_width, max_height) = (self.config.max_width, self.config.max_height);
        match self.mode {
            Mode::Picture(codec) => {
                let size = fit(raw.width, raw.height, max_width, max_height);
                let raw = self.scaled(raw, damage.as_ref(), size);
                if !self.pictures.as_ref().is_some_and(|p| p.codec == codec) {
                    self.pictures = Some(Pictures::start(codec, self.config));
                }
                if let Some(pictures) = &self.pictures {
                    pictures.push(raw);
                }
            }
            Mode::Video(codec) => {
                let size = video::output_size(raw.width, raw.height, max_width, max_height);
                let raw = self.scaled(raw, damage.as_ref(), size);
                self.video(codec, &raw)?;
            }
            Mode::Tiles => self.tiles(raw, damage),
            Mode::Adaptive(codec) => {
                // Kept up to date through tiles too, for when video takes over.
                let small = video::output_size(raw.width, raw.height, max_width, max_height);
                let small = self.scaled(raw.clone(), damage.as_ref(), small);
                let Some(damage) = damage else {
                    self.blurred = None;
                    self.tiles(raw, None);
                    return Ok(());
                };
                let size = (raw.width, raw.height);
                // Moves cost nothing as patches when the picture is not scaled.
                let unscaled = video::output_size(
                    size.0,
                    size.1,
                    self.config.max_width,
                    self.config.max_height,
                ) == size;
                let counted = if unscaled {
                    damage::Damage {
                        moves: Vec::new(),
                        dirty: damage.dirty.clone(),
                    }
                } else {
                    damage.clone()
                };
                let share = damage::Mask::of(&counted, size.0, size.1).share();
                let now = Instant::now();
                if share >= 0.01 {
                    self.changes.push_back(now);
                }
                while self
                    .changes
                    .front()
                    .is_some_and(|at| now - *at > Duration::from_secs(1))
                {
                    self.changes.pop_front();
                }
                let motion = share * 100.0 >= f64::from(self.config.video_area)
                    || (share >= 0.01 && self.changes.len() >= self.config.video_rate as usize);
                if motion {
                    // Only the changed part of the decoded frame is drawn.
                    let scale = (
                        f64::from(small.width) / f64::from(size.0),
                        f64::from(small.height) / f64::from(size.1),
                    );
                    let regions = damage::Mask::of(&damage, size.0, size.1)
                        .rects()
                        .into_iter()
                        .filter_map(|r| r.scale(scale).clip(small.width, small.height))
                        .collect();
                    self.video(codec, &small)?;
                    self.regions.push_back(regions);
                    let mask = damage::Mask::of(&damage, size.0, size.1);
                    match &mut self.blurred {
                        Some(blurred) => blurred.union(&mask),
                        None => self.blurred = Some(mask),
                    }
                } else {
                    let mut damage = damage;
                    if let Some(blurred) = self.blurred.take() {
                        damage.dirty.extend(blurred.rects());
                    }
                    self.tiles(raw, Some(damage));
                }
            }
        }
        Ok(())
    }

    /// The picture at `size`, rescaling only what `damage` covers.
    fn scaled(&mut self, raw: Raw, damage: Option<&damage::Damage>, size: (u32, u32)) -> Raw {
        if size == (raw.width, raw.height) {
            raw
        } else {
            self.scaled.apply(&raw, damage, size)
        }
    }

    fn tiles(&mut self, raw: Raw, damage: Option<damage::Damage>) {
        let config = self.config;
        let adaptive = matches!(self.mode, Mode::Adaptive(_));
        let tiles = self.tiles.get_or_insert_with(|| {
            tiles::Tiles::start(
                config.quality,
                (config.max_width, config.max_height),
                adaptive,
            )
        });
        let full = damage.is_none();
        tiles.push(raw, damage.unwrap_or_default(), full);
        if matches!(self.mode, Mode::Adaptive(_)) {
            self.order.push_back(Next::Tiles);
        }
    }

    fn video(&mut self, codec: Codec, raw: &Raw) -> Result<()> {
        let shape = (raw.width, raw.height, raw.format);
        if !self
            .video
            .as_ref()
            .is_some_and(|encoder| encoder.input == shape)
        {
            // A new size or a first picture: a new stream, from a keyframe.
            self.video = None;
            self.order.retain(|next| *next != Next::Video);
            self.regions.clear();
            let ffmpeg = self.config.probe.ffmpeg.as_ref().ok_or("no ffmpeg")?;
            let name = self
                .config
                .probe
                .encoder(codec)
                .ok_or_else(|| format!("no {} encoder", codec.name()))?;
            self.video = Some(video::Encoder::start(
                ffmpeg,
                name,
                &self.config.video,
                raw.width,
                raw.height,
                raw.format,
            )?);
            self.announce = true;
        }
        if let Some(encoder) = &mut self.video {
            encoder.push(raw.pixels.clone());
        }
        if matches!(self.mode, Mode::Adaptive(_)) {
            self.order.push_back(Next::Video);
        }
        Ok(())
    }

    /// Adaptive: once the screen settles, what video drew is sent again as
    /// sharp patches.
    fn tick(&mut self) {
        if matches!(self.mode, Mode::Adaptive(_))
            && self.blurred.is_some()
            && self.last_sent.elapsed() >= SETTLE
            && !self.busy()
        {
            if let (Some(raw), Some(blurred)) = (self.last.clone(), self.blurred.take()) {
                let dirty = blurred.rects();
                self.tiles(
                    raw,
                    Some(damage::Damage {
                        moves: Vec::new(),
                        dirty,
                    }),
                );
            }
        }
    }

    fn poll(&mut self) -> Result<Option<Output>> {
        if let Some(output) = self.out.pop_front() {
            return Ok(Some(output));
        }
        if let Some(pictures) = &self.pictures {
            if let Some(frame) = pictures.poll()? {
                return Ok(Some(Output::Frame(frame)));
            }
        }
        let adaptive = matches!(self.mode, Mode::Adaptive(_));
        let next = self.order.front().copied();
        if !adaptive || next == Some(Next::Tiles) {
            if let Some(tiles) = &self.tiles {
                if let Some(frame) = tiles.poll()? {
                    self.order.pop_front();
                    return Ok(Some(Output::Frame(frame)));
                }
            }
        }
        if !adaptive || next == Some(Next::Video) {
            if let Some(encoder) = &mut self.video {
                if let Some(packet) = encoder.poll()? {
                    self.order.pop_front();
                    let frame = Frame {
                        codec: encoder.codec,
                        keyframe: packet.key,
                        data: packet.data,
                        width: encoder.width,
                        height: encoder.height,
                        regions: if adaptive {
                            self.regions.pop_front()
                        } else {
                            None
                        },
                    };
                    if self.announce {
                        self.announce = false;
                        self.out.push_back(Output::Frame(frame));
                        return Ok(Some(Output::Text(json!({"type": "stream",
                            "codec": encoder.codec.name(), "webcodecs": packet.webcodecs,
                            "encoder": encoder.name, "width": encoder.width, "height": encoder.height}))));
                    }
                    return Ok(Some(Output::Frame(frame)));
                }
                // An encoder that dropped a picture is not waited for.
                if adaptive && !encoder.in_flight() && !encoder.busy() {
                    self.order.pop_front();
                    self.regions.pop_front();
                }
            } else if adaptive {
                self.order.pop_front();
            }
        }
        Ok(None)
    }

    /// A video encoder failed: say so, and go on without video if the viewer
    /// can.
    fn fail(&mut self, error: &str, outbox: &Outbox, source: &mut dyn Source) -> Result<()> {
        let fallback = match self.mode {
            Mode::Adaptive(_) => Some(Mode::Tiles),
            Mode::Video(_) => self
                .config
                .offered
                .iter()
                .copied()
                .find(|c| !c.is_video())
                .map(|c| Mode::of(c, &[], &self.config.offered)),
            _ => None,
        };
        let message = match fallback {
            Some(mode) => format!(
                "{error}; sending {} instead",
                mode.describe()["codec"].as_str().unwrap_or("pictures")
            ),
            None => error.to_string(),
        };
        outbox.text(&json!({"type": "error", "message": message}));
        let Some(mode) = fallback else {
            return Err(error.to_string());
        };
        self.video = None;
        self.switch(mode);
        source.rate(self.config.fps(mode));
        outbox.text(&json!({"type": "mode", "mode": mode.describe()}));
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
        regions: None,
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

mod browser {
    use super::{
        fraction, keys, unknown, Codec, Frame, Pace, Picture, PixelFormat, Raw, Result, Source,
        Want,
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
        /// Where the pointer last was, in CSS pixels.
        pointer: (f64, f64),
        /// The key-up event for each key held down.
        held: Vec<Value>,
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
                pointer: (0.0, 0.0),
                held: Vec::new(),
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
                regions: None,
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
                        regions: None,
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
                        // A screencast frame says nothing of what changed.
                        damage: None,
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
                    self.pointer = (x, y);
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
                    let key = keys::of(event).ok_or_else(|| unknown(event))?;
                    let down = event["action"] != "up";
                    if down {
                        self.modifiers |= key.modifier();
                    } else {
                        self.modifiers &= !key.modifier();
                    }
                    // The key as the viewer's layout names it, as Chrome reports it to the page.
                    let name = event["key"]
                        .as_str()
                        .filter(|k| !k.is_empty())
                        .unwrap_or(key.code)
                        .to_string();
                    let mut params = json!({"key": name, "code": key.code,
                        "windowsVirtualKeyCode": key.chrome_vk(), "nativeVirtualKeyCode": key.chrome_vk(),
                        "location": key.location(), "modifiers": self.modifiers});
                    self.held.retain(|held| held["code"] != key.code);
                    if down {
                        let mut up = params.clone();
                        up["type"] = json!("keyUp");
                        self.held.push(up);
                        // Enter types a line break unless Ctrl, Alt or Meta is held.
                        let enter = matches!(key.code, "Enter" | "NumpadEnter");
                        if enter && self.modifiers & 0b0111 == 0 {
                            params["type"] = json!("keyDown");
                            params["text"] = json!("\r");
                            params["unmodifiedText"] = json!("\r");
                        } else {
                            params["type"] = json!("rawKeyDown");
                        }
                    } else {
                        params["type"] = json!("keyUp");
                    }
                    self.command("Input.dispatchKeyEvent", params)
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

        fn release(&mut self) {
            for mut up in std::mem::take(&mut self.held).into_iter().rev() {
                let key = up["code"].as_str().and_then(keys::by_code);
                self.modifiers &= !key.map_or(0, keys::Key::modifier);
                up["modifiers"] = json!(self.modifiers);
                let _ = self.command("Input.dispatchKeyEvent", up);
            }
            self.modifiers = 0;
            for (button, mask) in [("left", 1), ("right", 2), ("middle", 4)] {
                if self.buttons & mask != 0 {
                    self.buttons &= !mask;
                    let (x, y) = self.pointer;
                    let _ = self.command(
                        "Input.dispatchMouseEvent",
                        json!({"type": "mouseReleased", "x": x, "y": y, "button": button,
                               "buttons": self.buttons, "clickCount": 1}),
                    );
                }
            }
        }
    }
}

/// The error for a key message no key matches.
fn unknown(event: &Value) -> String {
    let named = event["code"]
        .as_str()
        .or(event["key"].as_str())
        .unwrap_or_default();
    format!("unknown key {named:?}")
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
    use super::screen::{self, Pointer, Screen};
    use super::{
        fraction, keys, unknown, Pace, Picture, Result, Source, Want, CLIPBOARD_CHARACTERS,
    };
    use serde_json::{json, Value};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };

    pub struct Desktop {
        pace: Pace,
        screen: Screen,
        pointer: Pointer,
        /// Keys and mouse buttons (1 left, 2 right, 4 middle) held down.
        held: Vec<keys::Key>,
        buttons: u8,
    }

    pub fn open(fps: u32) -> Result<Box<dyn Source>> {
        screen::dpi_aware();
        Ok(Box::new(Desktop {
            pace: Pace::new(fps),
            screen: Screen::open(),
            pointer: Pointer::default(),
            held: Vec::new(),
            buttons: 0,
        }))
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

    /// A physical key going down or up, with its scan code, which some
    /// programs read instead of the virtual key.
    fn press(key: keys::Key, up: bool) -> INPUT {
        let scan = unsafe { MapVirtualKeyW(u32::from(key.vk), MAPVK_VK_TO_VSC) } as u16;
        let mut flags = if up {
            KEYEVENTF_KEYUP
        } else {
            KEYBD_EVENT_FLAGS(0)
        };
        if key.extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        self::key(key.vk, scan, flags)
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
            let capture = if self.screen.duplicating() {
                "dxgi"
            } else {
                "gdi"
            };
            let mut name = json!({"kind": "desktop", "capture": capture});
            if let Some(reason) = &self.screen.fallback {
                name["fallback"] = json!(reason);
            }
            name
        }

        fn rate(&mut self, fps: u32) {
            self.pace.rate(fps);
        }

        /// Raw pixels whatever is wanted: pictures are encoded off this thread.
        fn capture(&mut self, _want: Want) -> Result<Option<Picture>> {
            if !self.pace.due() {
                return Ok(None);
            }
            Ok(self.screen.capture()?.map(Picture::Raw))
        }

        fn cursor(&mut self, fresh: bool) -> Option<Value> {
            self.pointer.message(fresh)
        }

        fn input(&mut self, event: &Value) -> Result<()> {
            match event["type"].as_str() {
                Some("mouse") => {
                    let x = (fraction(event, "x")? * 65535.0).round() as i32;
                    let y = (fraction(event, "y")? * 65535.0).round() as i32;
                    let at = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                    let (down, up, mask) = match event["button"].as_str() {
                        Some("right") => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 2),
                        Some("middle") => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 4),
                        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 1),
                    };
                    let mut inputs = vec![mouse(x, y, 0, at)];
                    match event["action"].as_str() {
                        Some("down") => {
                            self.buttons |= mask;
                            inputs.push(mouse(x, y, 0, at | down));
                        }
                        Some("up") => {
                            self.buttons &= !mask;
                            inputs.push(mouse(x, y, 0, at | up));
                        }
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
                    let key = keys::of(event).ok_or_else(|| unknown(event))?;
                    let up = event["action"] == "up";
                    self.held.retain(|held| held.code != key.code);
                    if !up {
                        self.held.push(key);
                    }
                    send(&[press(key, up)])
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

        fn release(&mut self) {
            let mut inputs: Vec<INPUT> = std::mem::take(&mut self.held)
                .into_iter()
                .rev()
                .map(|key| press(key, true))
                .collect();
            for (mask, up) in [
                (1, MOUSEEVENTF_LEFTUP),
                (2, MOUSEEVENTF_RIGHTUP),
                (4, MOUSEEVENTF_MIDDLEUP),
            ] {
                if self.buttons & mask != 0 {
                    inputs.push(mouse(0, 0, 0, up));
                }
            }
            self.buttons = 0;
            if !inputs.is_empty() {
                let _ = send(&inputs);
            }
        }

        fn clipboard(&mut self) -> Result<Value> {
            let (text, characters) = aad_uia::windows::clipboard_text(CLIPBOARD_CHARACTERS)
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"type": "clipboard", "text": text, "characters": characters,
                      "truncated": characters > CLIPBOARD_CHARACTERS}),
            )
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
            regions: None,
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
    fn regions_grow_the_header_and_old_readers_skip_them() {
        let frame = Frame {
            codec: Codec::H264,
            keyframe: false,
            data: vec![9, 9],
            width: 100,
            height: 50,
            regions: Some(vec![damage::Rect {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
            }]),
        };
        let message = encode(&frame, 7);
        assert_eq!(message[2], 2);
        let skip = usize::from(u16::from_be_bytes(message[12..14].try_into().unwrap()));
        assert_eq!(skip, HEADER_LEN + 2 + 8);
        assert_eq!(&message[24..26], &[0, 1]);
        assert_eq!(&message[26..34], &[0, 1, 0, 2, 0, 3, 0, 4]);
        assert_eq!(&message[skip..], &[9, 9]);
        let many = Frame {
            regions: Some(
                (0..MAX_REGIONS as u32 + 1)
                    .map(|i| damage::Rect {
                        x: i,
                        y: i,
                        w: 1,
                        h: 1,
                    })
                    .collect(),
            ),
            ..frame
        };
        let message = encode(&many, 7);
        // Too many are joined into one around them all.
        assert_eq!(&message[24..34], &[0, 1, 0, 0, 0, 0, 1, 1, 1, 1]);
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
        assert_eq!(auto, vec![Codec::Webp, Codec::Jpeg, Codec::Tiles]);
        let found = video::Probe {
            chosen: vec![(Codec::Av1, "libsvtav1".into())],
            ..video::Probe::default()
        };
        assert_eq!(
            offered(&["auto".into()], &found).unwrap(),
            vec![Codec::Webp, Codec::Jpeg, Codec::Tiles, Codec::Av1]
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
            damage: None,
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
    fn tiles_with_video_become_adaptive() {
        let all = [Codec::Webp, Codec::Tiles, Codec::H264, Codec::Av1];
        let accept = [json!("tiles"), json!("av1"), json!("h264"), json!("webp")];
        assert_eq!(
            Mode::of(Codec::Tiles, &accept, &all),
            Mode::Adaptive(Codec::Av1)
        );
        assert_eq!(Mode::of(Codec::Tiles, &[json!("tiles")], &all), Mode::Tiles);
        assert_eq!(
            Mode::of(Codec::H264, &accept, &all),
            Mode::Video(Codec::H264)
        );
        assert_eq!(
            Mode::of(Codec::Webp, &accept, &all),
            Mode::Picture(Codec::Webp)
        );
        // Video the server does not offer is not used.
        let pictures = [Codec::Tiles, Codec::Webp];
        assert_eq!(Mode::of(Codec::Tiles, &accept, &pictures), Mode::Tiles);
    }
}

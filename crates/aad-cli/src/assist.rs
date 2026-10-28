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
//!   "codec":"webp","codecs":["webp","jpeg"]}}`, and `{"type":"error","message":..}`.
//! - server binary: one picture per message, a [`HEADER_LEN`]-byte big-endian
//!   header then the payload: version u8 (1), codec u8 (1 = jpeg, 2 = webp,
//!   each a whole picture), flags u8 (bit 0: keyframe, decodable on its own), reserved u8,
//!   width u32, height u32, header length u16 (24), reserved u16, timestamp
//!   u64 (milliseconds since the stream started). A reader skips `header
//!   length` bytes, so later versions can append fields; a codec it does not
//!   know is skipped, not misdrawn.
//! - viewer text: optionally first `{"type":"hello","accept":["webp","jpeg"]}`,
//!   naming the codecs it can draw in order of preference (WebP when not
//!   said); a later hello switches codec. Then
//!   `{"type":"mouse","action":"move|down|up|wheel","x":0..1,"y":0..1,
//!   "button":"left|right|middle","dx":px,"dy":px}`,
//!   `{"type":"key","action":"down|up","key":"Enter","code":"Enter"}` for keys
//!   that are not text, and `{"type":"text","text":"..."}` for typed text.
//!
//! Every frame is a whole picture today, so every frame is a keyframe. A video codec
//! (H.264, VP8) would be another [`Codec`] with delta frames, offered when the
//! viewer's `hello` accepts it; the header already carries what a decoder
//! needs to start at a keyframe and to time the frames.
use clap::Args;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

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
    /// Most frames per second (1..15). An unchanged screen is not sent again.
    #[arg(long, default_value_t = 5)]
    fps: u32,
    /// Picture quality for lossy codecs (1..100).
    #[arg(long, default_value_t = 70)]
    quality: u8,
    /// Only show the screen; ignore the viewer's mouse and keyboard.
    #[arg(long)]
    view_only: bool,
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
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Codec::Jpeg => "jpeg",
            Codec::Webp => "webp",
        }
    }
}

/// The codecs this build can send, in order of preference.
const CODECS: [Codec; 2] = [Codec::Webp, Codec::Jpeg];

/// The first codec in the viewer's list that this build can send.
fn choose(accept: &[Value]) -> Option<Codec> {
    accept
        .iter()
        .find_map(|wanted| CODECS.into_iter().find(|codec| wanted == codec.name()))
}

/// When an unchanged screen is sent again anyway, so a late viewer sees it.
const RESEND: std::time::Duration = std::time::Duration::from_secs(3);

/// Paces frames and drops pictures identical to the last one sent.
struct Pace {
    interval: std::time::Duration,
    last: std::time::Instant,
    last_digest: u64,
    last_sent: std::time::Instant,
}

impl Pace {
    fn new(fps: u32) -> Self {
        let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(60);
        Self {
            interval: std::time::Duration::from_millis(1000 / u64::from(fps.clamp(1, 15))),
            last: long_ago,
            last_digest: 0,
            last_sent: long_ago,
        }
    }

    /// Whether it is time to take another picture.
    fn due(&mut self) -> bool {
        if self.last.elapsed() < self.interval {
            return false;
        }
        self.last = std::time::Instant::now();
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
        self.last_sent = std::time::Instant::now();
        true
    }

    /// Send the next picture even if it looks the same, e.g. after a codec switch.
    fn reset(&mut self) {
        self.last_digest = 0;
    }
}

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

/// Where frames come from and where input goes.
trait Source {
    fn name(&self) -> Value;
    /// A new frame in `codec`, if the screen changed since the last one.
    fn frame(&mut self, codec: Codec) -> Result<Option<Frame>>;
    fn input(&mut self, event: &Value) -> Result<()>;
}

pub fn run(args: &AssistArgs) -> Result<Value> {
    let listen: std::net::SocketAddr = args.listen.parse().map_err(|e| format!("--listen: {e}"))?;
    if !listen.ip().is_loopback() {
        return Err("--listen must be a loopback address; proxy it to reach it elsewhere".into());
    }
    let mut source: Box<dyn Source> = match (args.browser_port, args.desktop) {
        (Some(port), false) => Box::new(browser::Page::start(
            port,
            args.page.as_deref(),
            args.fps,
            args.quality.clamp(1, 100),
        )?),
        (None, true) => desktop::open(args.fps, args.quality.clamp(1, 100))?,
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
                if let Err(error) = serve(stream, &token, source.as_mut(), args.view_only, &stop) {
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

fn serve(
    mut stream: TcpStream,
    token: &str,
    source: &mut dyn Source,
    view_only: bool,
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
    stream.set_read_timeout(None).map_err(|e| e.to_string())?;

    // The viewer's messages are read on their own thread; frames and input are
    // handled here, so the source is only ever used from one thread.
    let (sender, inbox) = mpsc::channel::<Value>();
    let closed = Arc::new(AtomicBool::new(false));
    {
        let closed = closed.clone();
        let mut reader = reader;
        let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
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
                    9 => {
                        let _ = write_frame(&mut writer, 10, &payload);
                    }
                    _ => {}
                }
            }
            closed.store(true, Ordering::SeqCst);
        });
    }
    let codecs: Vec<&str> = CODECS.iter().map(|c| c.name()).collect();
    write_frame(
        &mut stream,
        1,
        json!({"type": "ready", "source": source.name(), "view_only": view_only,
               "frames": {"header": "aad-frame/1", "codec": CODECS[0].name(), "codecs": codecs}})
        .to_string()
        .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let started = std::time::Instant::now();
    let mut codec = CODECS[0];
    while !closed.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        while let Ok(event) = inbox.try_recv() {
            if event["type"] == "hello" {
                match event["accept"].as_array().map(|accept| choose(accept)) {
                    None => {}
                    Some(Some(chosen)) => codec = chosen,
                    Some(None) => {
                        let message = json!({"type": "error", "message": format!("no common codec; this server sends {}", codecs.join(", "))});
                        let _ = write_frame(&mut stream, 1, message.to_string().as_bytes());
                    }
                }
                continue;
            }
            if view_only {
                continue;
            }
            if let Err(error) = source.input(&event) {
                let _ = write_frame(
                    &mut stream,
                    1,
                    json!({"type": "error", "message": error})
                        .to_string()
                        .as_bytes(),
                );
            }
        }
        if let Some(frame) = source.frame(codec)? {
            let at = started.elapsed().as_millis() as u64;
            if write_frame(&mut stream, 2, &encode(&frame, at)).is_err() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let _ = write_frame(&mut stream, 8, &[]);
    Ok(())
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
    use super::{fraction, named_key, Codec, Frame, Pace, Result, Source};
    use aad_cdp::Client;
    use serde_json::{json, Value};
    use std::time::{Duration, Instant};

    /// One page. JPEG comes from Chrome's screencast, which pushes a frame
    /// whenever the page paints; screencast offers no WebP, so WebP pictures
    /// are taken with captureScreenshot at the paced rate instead.
    pub struct Page {
        client: Client,
        page: String,
        pace: Pace,
        quality: u8,
        /// Whether the JPEG screencast is running.
        screencast: bool,
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
        pub fn start(port: u16, page: Option<&str>, fps: u32, quality: u8) -> Result<Self> {
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
                screencast: false,
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

        fn screencast(&mut self, on: bool) -> Result<()> {
            if on != self.screencast {
                if on {
                    let quality = self.quality;
                    self.command(
                        "Page.startScreencast",
                        json!({"format": "jpeg", "quality": quality, "maxWidth": 1600, "maxHeight": 1600, "everyNthFrame": 1}),
                    )?;
                } else {
                    self.command("Page.stopScreencast", json!({}))?;
                }
                self.screencast = on;
                self.pace.reset();
            }
            Ok(())
        }

        fn screencast_frame(&mut self) -> Result<Option<Frame>> {
            let batch = self.client.drain_events();
            if let Some(failure) = batch.failure {
                return Err(format!("the page went away: {failure}"));
            }
            let mut latest = None;
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
                latest = Some(params.clone());
            }
            let Some(params) = latest else {
                return Ok(None);
            };
            let metadata = &params["metadata"];
            self.size = (
                metadata["deviceWidth"].as_f64().unwrap_or(0.0),
                metadata["deviceHeight"].as_f64().unwrap_or(0.0),
            );
            Ok(Some(Frame {
                codec: Codec::Jpeg,
                keyframe: true,
                data: decode(&params["data"])?,
                width: self.size.0 as u32,
                height: self.size.1 as u32,
            }))
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
            Ok(Some(Frame {
                codec: Codec::Webp,
                keyframe: true,
                data,
                width: self.size.0 as u32,
                height: self.size.1 as u32,
            }))
        }
    }

    impl Source for Page {
        fn name(&self) -> Value {
            json!({"kind": "browser", "page_id": self.page})
        }

        fn frame(&mut self, codec: Codec) -> Result<Option<Frame>> {
            self.screencast(codec == Codec::Jpeg)?;
            match codec {
                Codec::Jpeg => self.screencast_frame(),
                Codec::Webp => self.screenshot(),
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
    pub fn open(_fps: u32, _quality: u8) -> Result<Box<dyn Source>> {
        Err("desktop assistance is available on Windows so far; use --browser-port for a browser page".into())
    }
}

#[cfg(windows)]
mod desktop {
    use super::{fraction, named_key, Codec, Frame, Pace, Result, Source};
    use serde_json::{json, Value};
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

    const MAX_WIDTH: u32 = 1600;

    pub struct Desktop {
        pace: Pace,
        quality: u8,
    }

    pub fn open(fps: u32, quality: u8) -> Result<Box<dyn Source>> {
        Ok(Box::new(Desktop {
            pace: Pace::new(fps),
            quality,
        }))
    }

    /// The virtual screen as RGB, with its size.
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
            let rgb = bgra
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0]])
                .collect();
            Ok((rgb, width as u32, height as u32))
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

        fn frame(&mut self, codec: Codec) -> Result<Option<Frame>> {
            if !self.pace.due() {
                return Ok(None);
            }
            let (rgb, width, height) = grab()?;
            if !self.pace.changed(&rgb) {
                return Ok(None);
            }
            let image = image::RgbImage::from_raw(width, height, rgb).ok_or("bad screen buffer")?;
            let image = if width > MAX_WIDTH {
                let scaled = height * MAX_WIDTH / width;
                image::imageops::thumbnail(&image, MAX_WIDTH, scaled.max(1))
            } else {
                image
            };
            let data = match codec {
                Codec::Webp => {
                    webp::Encoder::from_rgb(image.as_raw(), image.width(), image.height())
                        .encode(f32::from(self.quality))
                        .to_vec()
                }
                Codec::Jpeg => {
                    let mut jpeg = Vec::new();
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, self.quality)
                        .encode_image(&image)
                        .map_err(|e| e.to_string())?;
                    jpeg
                }
            };
            Ok(Some(Frame {
                codec,
                keyframe: true,
                data,
                width,
                height,
            }))
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
const VIEWER: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Assist</title>
<style>
:root{color-scheme:light dark;--bg:#f6f6f4;--fg:#1d1d1b;--muted:#6b6b66}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ecece8;--muted:#9a9a93}}
body{margin:0;background:var(--bg);color:var(--fg);font:14px system-ui,sans-serif;display:flex;flex-direction:column;height:100vh}
header{padding:8px 16px;color:var(--muted)}
main{flex:1;display:flex;align-items:center;justify-content:center;min-height:0;padding:0 16px 16px}
canvas{max-width:100%;max-height:100%;outline:none;cursor:crosshair;box-shadow:0 1px 6px rgba(0,0,0,.25)}
</style></head><body>
<header id="status">Connecting…</header>
<main><canvas id="screen" tabindex="0" aria-label="Shared screen" width="1" height="1"></canvas></main>
<script>
const token=new URLSearchParams(location.search).get('token');
const socket=new WebSocket(`ws://${location.host}/ws?token=${token}`);
socket.binaryType='arraybuffer';
const screen=document.getElementById('screen'), status=document.getElementById('status'), pen=screen.getContext('2d');
const send=m=>socket.readyState===1&&socket.send(JSON.stringify(m));
const MIME={1:'image/jpeg',2:'image/webp'};
let drawing=false;
socket.onopen=()=>send({type:'hello',accept:['webp','jpeg']});
async function draw(buffer){
  const head=new DataView(buffer);
  const type=MIME[head.getUint8(1)], skip=head.getUint16(12);
  if(!type||drawing) return; // an unknown codec is skipped; a busy canvas drops the frame
  drawing=true;
  try{const picture=await createImageBitmap(new Blob([new Uint8Array(buffer,skip)],{type}));
    if(screen.width!==picture.width||screen.height!==picture.height){screen.width=picture.width;screen.height=picture.height;}
    pen.drawImage(picture,0,0);picture.close();}
  finally{drawing=false;}
}
socket.onmessage=e=>{if(e.data instanceof ArrayBuffer) return void draw(e.data);
  const m=JSON.parse(e.data);
  if(m.type==='ready') status.textContent=(m.view_only?'Viewing ':'Sharing ')+m.source.kind+(m.view_only?'':' — click the picture to control it');
  if(m.type==='error') status.textContent=m.message;};
socket.onclose=()=>status.textContent='Disconnected';
const at=e=>{const r=screen.getBoundingClientRect();return{x:Math.min(1,Math.max(0,(e.clientX-r.left)/r.width)),y:Math.min(1,Math.max(0,(e.clientY-r.top)/r.height))};};
const button=e=>['left','middle','right'][e.button]||'left';
let last=0;
screen.addEventListener('mousemove',e=>{const now=Date.now();if(now-last<40)return;last=now;send({type:'mouse',action:'move',...at(e)});});
screen.addEventListener('mousedown',e=>{e.preventDefault();screen.focus();send({type:'mouse',action:'down',button:button(e),...at(e)});});
screen.addEventListener('mouseup',e=>send({type:'mouse',action:'up',button:button(e),...at(e)}));
screen.addEventListener('contextmenu',e=>e.preventDefault());
screen.addEventListener('wheel',e=>{e.preventDefault();send({type:'mouse',action:'wheel',dx:e.deltaX,dy:e.deltaY,...at(e)});},{passive:false});
screen.addEventListener('keydown',e=>{e.preventDefault();
  if(e.key.length===1&&!e.ctrlKey&&!e.metaKey&&!e.altKey) send({type:'text',text:e.key});
  else send({type:'key',action:'down',key:e.key,code:e.code});});
screen.addEventListener('keyup',e=>{e.preventDefault();if(!(e.key.length===1&&!e.ctrlKey&&!e.metaKey&&!e.altKey))send({type:'key',action:'up',key:e.key,code:e.code});});
screen.addEventListener('paste',e=>send({type:'text',text:e.clipboardData.getData('text')}));
</script></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_header_carries_what_a_decoder_needs() {
        let frame = Frame {
            codec: Codec::Jpeg,
            keyframe: true,
            data: vec![0xff, 0xd8, 0xff],
            width: 1536,
            height: 960,
        };
        let message = encode(&frame, 1234);
        assert_eq!(message.len(), HEADER_LEN + 3);
        assert_eq!(&message[..4], &[1, 1, 1, 0]);
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
        assert_eq!(&message[HEADER_LEN..], &[0xff, 0xd8, 0xff]);
    }

    #[test]
    fn the_viewer_picks_the_codec_and_webp_comes_first() {
        assert_eq!(CODECS[0], Codec::Webp);
        assert_eq!(choose(&[json!("jpeg")]), Some(Codec::Jpeg));
        assert_eq!(choose(&[json!("h264"), json!("webp")]), Some(Codec::Webp));
        assert_eq!(choose(&[json!("h264")]), None);
    }
}

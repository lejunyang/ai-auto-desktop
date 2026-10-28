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
//! Protocol, JSON text frames both ways:
//! - server: `{"type":"ready","source":..}`, then
//!   `{"type":"frame","format":"jpeg","data":base64,"width":w,"height":h}`
//! - viewer: `{"type":"mouse","action":"move|down|up|wheel","x":0..1,"y":0..1,
//!   "button":"left|right|middle","dx":px,"dy":px}`,
//!   `{"type":"key","action":"down|up","key":"Enter","code":"Enter"}` for keys
//!   that are not text, and `{"type":"text","text":"..."}` for typed text.
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
    /// Frames per second for the desktop (1..15).
    #[arg(long, default_value_t = 5)]
    fps: u32,
    /// Only show the screen; ignore the viewer's mouse and keyboard.
    #[arg(long)]
    view_only: bool,
}

type Result<T> = std::result::Result<T, String>;

struct Frame {
    jpeg: String,
    width: u32,
    height: u32,
}

/// Where frames come from and where input goes.
trait Source {
    fn name(&self) -> Value;
    /// A new frame, if the screen changed since the last one.
    fn frame(&mut self) -> Result<Option<Frame>>;
    fn input(&mut self, event: &Value) -> Result<()>;
}

pub fn run(args: &AssistArgs) -> Result<Value> {
    let listen: std::net::SocketAddr = args.listen.parse().map_err(|e| format!("--listen: {e}"))?;
    if !listen.ip().is_loopback() {
        return Err("--listen must be a loopback address; proxy it to reach it elsewhere".into());
    }
    let mut source: Box<dyn Source> = match (args.browser_port, args.desktop) {
        (Some(port), false) => Box::new(browser::Screencast::start(port, args.page.as_deref())?),
        (None, true) => desktop::open(args.fps.clamp(1, 15))?,
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
    write_frame(
        &mut stream,
        1,
        json!({"type": "ready", "source": source.name(), "view_only": view_only})
            .to_string()
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    while !closed.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
        while let Ok(event) = inbox.try_recv() {
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
        if let Some(frame) = source.frame()? {
            let message = json!({"type": "frame", "format": "jpeg", "data": frame.jpeg, "width": frame.width, "height": frame.height});
            if write_frame(&mut stream, 1, message.to_string().as_bytes()).is_err() {
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
    use super::{fraction, named_key, Frame, Result, Source};
    use aad_cdp::Client;
    use serde_json::{json, Value};
    use std::time::{Duration, Instant};

    pub struct Screencast {
        client: Client,
        page: String,
        /// The page's size in CSS pixels, from the last frame.
        size: (f64, f64),
        buttons: u32,
        modifiers: u32,
    }

    fn cdp(e: aad_cdp::CdpError) -> String {
        e.to_string()
    }

    impl Screencast {
        pub fn start(port: u16, page: Option<&str>) -> Result<Self> {
            let targets = Client::list_targets(port).map_err(cdp)?;
            let target = targets
                .iter()
                .filter(|t| t.kind == "page")
                .find(|t| page.is_none_or(|id| t.id == id))
                .ok_or("no such page in that browser")?;
            let mut client = Client::attach(target).map_err(cdp)?;
            client.set_deadline(Instant::now() + Duration::from_secs(5));
            client.command("Page.enable", json!({})).map_err(cdp)?;
            client
                .command(
                    "Page.startScreencast",
                    json!({"format": "jpeg", "quality": 70, "maxWidth": 1600, "maxHeight": 1600, "everyNthFrame": 1}),
                )
                .map_err(cdp)?;
            Ok(Self {
                client,
                page: target.id.clone(),
                size: (0.0, 0.0),
                buttons: 0,
                modifiers: 0,
            })
        }

        fn command(&mut self, method: &str, params: Value) -> Result<()> {
            self.client
                .set_deadline(Instant::now() + Duration::from_secs(3));
            self.client.command(method, params).map(|_| ()).map_err(cdp)
        }
    }

    impl Source for Screencast {
        fn name(&self) -> Value {
            json!({"kind": "browser", "page_id": self.page})
        }

        fn frame(&mut self) -> Result<Option<Frame>> {
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
                jpeg: params["data"].as_str().unwrap_or_default().to_string(),
                width: self.size.0 as u32,
                height: self.size.1 as u32,
            }))
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
    use super::{fraction, named_key, Frame, Result, Source};
    use serde_json::{json, Value};
    use std::time::{Duration, Instant};
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
        interval: Duration,
        last: Instant,
        last_digest: u64,
        last_sent: Instant,
    }

    pub fn open(fps: u32) -> Result<Box<dyn Source>> {
        Ok(Box::new(Desktop {
            interval: Duration::from_millis(1000 / u64::from(fps)),
            last: Instant::now() - Duration::from_secs(10),
            last_digest: 0,
            last_sent: Instant::now() - Duration::from_secs(10),
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

        fn frame(&mut self) -> Result<Option<Frame>> {
            if self.last.elapsed() < self.interval {
                return Ok(None);
            }
            self.last = Instant::now();
            let (rgb, width, height) = grab()?;
            let digest = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                rgb.hash(&mut hasher);
                hasher.finish()
            };
            // Unchanged screens are not sent again, except now and then.
            if digest == self.last_digest && self.last_sent.elapsed() < Duration::from_secs(3) {
                return Ok(None);
            }
            self.last_digest = digest;
            self.last_sent = Instant::now();
            let image = image::RgbImage::from_raw(width, height, rgb).ok_or("bad screen buffer")?;
            let image = if width > MAX_WIDTH {
                let scaled = height * MAX_WIDTH / width;
                image::imageops::thumbnail(&image, MAX_WIDTH, scaled.max(1))
            } else {
                image
            };
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 70)
                .encode_image(&image)
                .map_err(|e| e.to_string())?;
            use base64::Engine;
            Ok(Some(Frame {
                jpeg: base64::engine::general_purpose::STANDARD.encode(jpeg),
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
img{max-width:100%;max-height:100%;outline:none;cursor:crosshair;box-shadow:0 1px 6px rgba(0,0,0,.25)}
</style></head><body>
<header id="status">Connecting…</header>
<main><img id="screen" tabindex="0" alt="Shared screen"></main>
<script>
const token=new URLSearchParams(location.search).get('token');
const socket=new WebSocket(`ws://${location.host}/ws?token=${token}`);
const screen=document.getElementById('screen'), status=document.getElementById('status');
const send=m=>socket.readyState===1&&socket.send(JSON.stringify(m));
socket.onmessage=e=>{const m=JSON.parse(e.data);
  if(m.type==='frame') screen.src='data:image/jpeg;base64,'+m.data;
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

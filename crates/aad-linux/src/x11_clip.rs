//! The X11 clipboard (the CLIPBOARD selection) in every format, for `aad
//! assist`: a paste owns it with all of a [`Clip`]'s formats, answers only the
//! client of the focused window, and afterwards owns it again with what it
//! held before; a copy reads its formats back.
//!
//! Each runs on a connection of its own, so the shared screen's connection
//! never waits on another client. A selection owner must answer requests
//! while the paste key is pressed, so the key is pressed from another thread.
use aad_platform::clip::{Clip, Copied, Pasting, Press};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

type Result<T> = std::result::Result<T, String>;

fn failed(what: &str) -> impl Fn(&dyn std::fmt::Display) -> String + '_ {
    move |e| format!("X11 clipboard {what}: {e}")
}

/// A connection with a window of its own to own or receive the selection.
struct Side {
    conn: RustConnection,
    window: Window,
    root: Window,
}

impl Side {
    fn open() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| failed("display")(&e))?;
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().map_err(|e| failed("window")(&e))?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| failed("window")(&e))?;
        conn.flush().map_err(|e| failed("window")(&e))?;
        Ok(Self { conn, window, root })
    }

    fn atom(&self, name: &str) -> Result<Atom> {
        Ok(self
            .conn
            .intern_atom(false, name.as_bytes())
            .map_err(|e| failed("atom")(&e))?
            .reply()
            .map_err(|e| failed("atom")(&e))?
            .atom)
    }

    fn name(&self, atom: Atom) -> String {
        self.conn
            .get_atom_name(atom)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_default()
    }

    fn owner(&self, selection: Atom) -> Window {
        self.conn
            .get_selection_owner(selection)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or(x11rb::NONE, |r| r.owner)
    }

    /// A server timestamp (ICCCM forbids CurrentTime for ownership).
    fn now(&self) -> u32 {
        let stamp = self
            .atom("AAD_TIMESTAMP")
            .unwrap_or(AtomEnum::STRING.into());
        let _ =
            self.conn
                .change_property8(PropMode::APPEND, self.window, stamp, AtomEnum::STRING, &[]);
        let _ = self.conn.flush();
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until {
            match self.conn.poll_for_event() {
                Ok(Some(Event::PropertyNotify(e))) if e.window == self.window => return e.time,
                Ok(Some(_)) => {}
                _ => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        x11rb::CURRENT_TIME
    }

    /// Asks the CLIPBOARD's owner for `target`; its type and bytes. Large
    /// answers that come in pieces (INCR) are joined, up to `most` bytes.
    fn convert(&self, clipboard: Atom, target: Atom, most: usize) -> Option<(Atom, Vec<u8>)> {
        let property = self.atom("AAD_CLIPBOARD").ok()?;
        let incr = self.atom("INCR").ok()?;
        self.conn.delete_property(self.window, property).ok()?;
        self.conn
            .convert_selection(
                self.window,
                clipboard,
                target,
                property,
                x11rb::CURRENT_TIME,
            )
            .ok()?;
        self.conn.flush().ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.conn.poll_for_event().ok()? {
                Some(Event::SelectionNotify(n)) if n.requestor == self.window => {
                    if n.property == x11rb::NONE {
                        return None;
                    }
                    break;
                }
                Some(_) => {}
                None if Instant::now() > deadline => return None,
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        let read = |delete: bool| {
            self.conn
                .get_property(
                    delete,
                    self.window,
                    property,
                    AtomEnum::ANY,
                    0,
                    u32::MAX / 4,
                )
                .ok()?
                .reply()
                .ok()
        };
        let first = read(true)?;
        if first.type_ != incr {
            return (first.value.len() <= most).then_some((first.type_, first.value));
        }
        // INCR: each piece is a new value of the property, deleted to ask
        // for the next; an empty one ends it.
        let _ = self.conn.flush();
        let mut bytes = Vec::new();
        let mut kind = x11rb::NONE;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.conn.poll_for_event().ok()? {
                Some(Event::PropertyNotify(e))
                    if e.window == self.window
                        && e.atom == property
                        && e.state == Property::NEW_VALUE =>
                {
                    let piece = read(true)?;
                    let _ = self.conn.flush();
                    if piece.value.is_empty() {
                        return Some((kind, bytes));
                    }
                    kind = piece.type_;
                    bytes.extend_from_slice(&piece.value);
                    if bytes.len() > most {
                        return None;
                    }
                }
                Some(_) => {}
                None if Instant::now() > deadline => return None,
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    fn targets(&self, clipboard: Atom) -> Vec<Atom> {
        let Ok(targets) = self.atom("TARGETS") else {
            return Vec::new();
        };
        self.convert(clipboard, targets, 64 * 1024)
            .map(|(_, bytes)| {
                bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One format offered: the target asked for, the type answered, its bytes
/// (format 8, or 32 for atoms).
#[derive(Clone)]
struct Item {
    target: Atom,
    kind: Atom,
    format: u8,
    bytes: Vec<u8>,
}

/// Answers one request from `items`; whether content (not just the list of
/// targets) was given.
fn serve(side: &Side, request: &SelectionRequestEvent, items: &[Item], targets: Atom) -> bool {
    let property = if request.property == x11rb::NONE {
        request.target
    } else {
        request.property
    };
    let mut given = x11rb::NONE;
    let mut content = false;
    if request.target == targets {
        let mut atoms = vec![targets];
        atoms.extend(items.iter().map(|i| i.target));
        if side
            .conn
            .change_property32(
                PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::ATOM,
                &atoms,
            )
            .is_ok()
        {
            given = property;
        }
    } else if let Some(item) = items.iter().find(|i| i.target == request.target) {
        let changed = if item.format == 32 {
            let words: Vec<u32> = item
                .bytes
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            side.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    item.kind,
                    &words,
                )
                .is_ok()
        } else {
            side.conn
                .change_property8(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    item.kind,
                    &item.bytes,
                )
                .is_ok()
        };
        if changed {
            given = property;
            content = true;
        }
    }
    let _ = side.conn.send_event(
        false,
        request.requestor,
        EventMask::NO_EVENT,
        SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: given,
        },
    );
    let _ = side.conn.flush();
    content
}

/// A path as a `file://` URI.
fn uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The path a `file://` URI names.
fn path_of(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.trim().strip_prefix("file://")?;
    // A host part, when given, ends at the first slash.
    let rest = &rest[rest.find('/')?..];
    let mut bytes = Vec::new();
    let raw = rest.as_bytes();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' && at + 2 < raw.len() + 1 {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// Every format of `clip` as X11 targets, in the order programs should
/// prefer them; those larger than one request can carry are left out.
fn items_of(side: &Side, clip: &Clip) -> Result<Vec<Item>> {
    let most = side.conn.maximum_request_bytes().saturating_sub(1024);
    let mut items = Vec::new();
    let mut add = |name: &str, kind: &str, bytes: Vec<u8>| -> Result<()> {
        if bytes.len() <= most {
            let target = side.atom(name)?;
            let kind = side.atom(kind)?;
            items.push(Item {
                target,
                kind,
                format: 8,
                bytes,
            });
        }
        Ok(())
    };
    if !clip.files.is_empty() {
        let uris: Vec<String> = clip.files.iter().map(|p| uri(p)).collect();
        add(
            "x-special/gnome-copied-files",
            "x-special/gnome-copied-files",
            format!("copy\n{}", uris.join("\n")).into_bytes(),
        )?;
        add(
            "text/uri-list",
            "text/uri-list",
            format!("{}\r\n", uris.join("\r\n")).into_bytes(),
        )?;
    }
    if let Some(html) = &clip.html {
        add("text/html", "text/html", html.as_bytes().to_vec())?;
    }
    if let Some(rtf) = &clip.rtf {
        add("text/rtf", "text/rtf", rtf.as_bytes().to_vec())?;
    }
    if let Some(png) = &clip.image {
        add("image/png", "image/png", png.clone())?;
    }
    if let Some(text) = &clip.text {
        for (name, kind) in [
            ("UTF8_STRING", "UTF8_STRING"),
            ("text/plain;charset=utf-8", "text/plain;charset=utf-8"),
            ("TEXT", "UTF8_STRING"),
            ("STRING", "UTF8_STRING"),
        ] {
            add(name, kind, text.as_bytes().to_vec())?;
        }
    }
    if items.is_empty() {
        return Err("nothing in this paste fits the X server's requests".into());
    }
    Ok(items)
}

/// What CLIPBOARD holds now, as items to own it with again later.
fn save(side: &Side, clipboard: Atom) -> Vec<Item> {
    let mut saved = Vec::new();
    let mut total = 0usize;
    let most = side.conn.maximum_request_bytes().saturating_sub(1024);
    for target in side.targets(clipboard) {
        let name = side.name(target);
        if matches!(
            name.as_str(),
            "TARGETS" | "MULTIPLE" | "TIMESTAMP" | "SAVE_TARGETS" | "DELETE" | ""
        ) || name.starts_with("INSERT_")
        {
            continue;
        }
        if let Some((kind, bytes)) = side.convert(clipboard, target, most) {
            total += bytes.len();
            if total > 32 * 1024 * 1024 {
                break;
            }
            let format = if side.name(kind) == "ATOM" { 32 } else { 8 };
            saved.push(Item {
                target,
                kind,
                format,
                bytes,
            });
        }
    }
    saved
}

/// The base of the client that owns the focused window: every window of
/// one client shares the bits outside the server's resource mask.
fn focused_client(side: &Side) -> Option<u32> {
    let active = side.atom("_NET_ACTIVE_WINDOW").ok()?;
    let reply = side
        .conn
        .get_property(false, side.root, active, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let window = reply.value32()?.next()?;
    let mask = side.conn.setup().resource_id_mask;
    (window != 0).then_some(window & !mask)
}

/// Owns CLIPBOARD with `saved` until another client takes it (or an hour
/// passes), so what the person copied before the paste is still there.
fn restore(saved: Vec<Item>) {
    std::thread::spawn(move || {
        let Ok(side) = Side::open() else {
            return;
        };
        let (Ok(clipboard), Ok(targets)) = (side.atom("CLIPBOARD"), side.atom("TARGETS")) else {
            return;
        };
        let time = side.now();
        let _ = side.conn.set_selection_owner(side.window, clipboard, time);
        let _ = side.conn.flush();
        let until = Instant::now() + Duration::from_secs(3600);
        while Instant::now() < until {
            match side.conn.poll_for_event() {
                Ok(Some(Event::SelectionClear(_))) | Err(_) => return,
                Ok(Some(Event::SelectionRequest(request))) => {
                    serve(&side, &request, &saved, targets);
                }
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    });
}

/// Pastes every format of `clip` into the focused window: owns CLIPBOARD
/// with them, has the paste key pressed, answers that window's client
/// (anyone else gets only a marker clipboard managers skip), and owns it
/// again with what it held before once the target has taken what it wants.
pub fn paste(clip: Clip, press: Press) -> Pasting {
    std::thread::spawn(move || {
        let side = Side::open()?;
        let clipboard = side.atom("CLIPBOARD")?;
        let targets = side.atom("TARGETS")?;
        let hint = side.atom("x-kde-passwordManagerHint")?;
        let items = items_of(&side, &clip)?;
        let saved = if side.owner(clipboard) == x11rb::NONE {
            Vec::new()
        } else {
            save(&side, clipboard)
        };
        let target = focused_client(&side);
        let mask = side.conn.setup().resource_id_mask;
        let time = side.now();
        side.conn
            .set_selection_owner(side.window, clipboard, time)
            .map_err(|e| failed("owner")(&e))?;
        side.conn.flush().map_err(|e| failed("owner")(&e))?;
        if side.owner(clipboard) != side.window {
            return Err("another client kept the clipboard; nothing was pasted".into());
        }
        // Clipboard managers asking are given only the marker that tells them
        // to keep nothing.
        let marker = vec![Item {
            target: hint,
            kind: hint,
            format: 8,
            bytes: b"secret".to_vec(),
        }];
        let pressing = std::thread::spawn(press);
        let files = clip.has_files();
        let deadline = Instant::now() + Duration::from_secs(if files { 10 } else { 3 });
        let quiet = Duration::from_millis(if files { 1_500 } else { 400 });
        let mut taken: Option<Instant> = None;
        let mut refused = 0u32;
        let mut lost = false;
        loop {
            match side.conn.poll_for_event() {
                Ok(Some(Event::SelectionRequest(request))) => {
                    let from_target = target.is_none_or(|base| request.requestor & !mask == base);
                    if from_target {
                        if serve(&side, &request, &items, targets) {
                            taken = Some(Instant::now());
                        }
                    } else {
                        refused += 1;
                        serve(&side, &request, &marker, targets);
                    }
                }
                Ok(Some(Event::SelectionClear(_))) => {
                    lost = true;
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(failed("events")(&e)),
            }
            let now = Instant::now();
            if taken.is_some_and(|at| now - at >= quiet) || (taken.is_none() && now >= deadline) {
                break;
            }
        }
        let pressed = pressing
            .join()
            .unwrap_or_else(|_| Err("pressing the paste key failed".into()));
        let restored = !lost;
        if !lost {
            if saved.is_empty() {
                let _ = side
                    .conn
                    .set_selection_owner(x11rb::NONE, clipboard, x11rb::CURRENT_TIME);
                let _ = side.conn.flush();
            } else {
                restore(saved);
            }
        }
        pressed?;
        let names: Vec<&str> = [
            ("text", clip.text.is_some()),
            ("html", clip.html.is_some()),
            ("rtf", clip.rtf.is_some()),
            ("image", clip.image.is_some()),
            ("files", files),
        ]
        .into_iter()
        .filter_map(|(n, on)| on.then_some(n))
        .collect();
        Ok(
            serde_json::json!({"pasted": taken.is_some(), "formats": names,
            "restored": restored, "refused": refused}),
        )
    })
}

/// Text in the encoding a target's type says: UTF-16 when it starts with a
/// byte-order mark (Firefox's text/html), else UTF-8.
fn decode(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && (bytes[..2] == [0xff, 0xfe] || bytes[..2] == [0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| {
                if little {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// What CLIPBOARD holds: text, HTML, a PNG picture and copied files; nothing
/// when its owner marked it as a password manager's.
pub fn copied() -> Result<Copied> {
    let side = Side::open()?;
    let clipboard = side.atom("CLIPBOARD")?;
    let mut copied = Copied::default();
    if side.owner(clipboard) == x11rb::NONE {
        return Ok(copied);
    }
    let targets = side.targets(clipboard);
    let named: Vec<String> = targets.iter().map(|t| side.name(*t)).collect();
    let has = |name: &str| named.iter().position(|n| n == name).map(|i| targets[i]);
    if has("x-kde-passwordManagerHint").is_some() {
        copied.protected = true;
        return Ok(copied);
    }
    const MOST: usize = 64 * 1024 * 1024;
    let text_target = has("UTF8_STRING")
        .or_else(|| has("text/plain;charset=utf-8"))
        .or_else(|| has("STRING"));
    if let Some(target) = text_target {
        copied.text = side
            .convert(clipboard, target, MOST)
            .map(|(_, b)| decode(&b));
    }
    if let Some(target) = has("text/html") {
        copied.html = side
            .convert(clipboard, target, MOST)
            .map(|(_, b)| decode(&b));
    }
    if let Some(target) = has("image/png") {
        copied.image = side
            .convert(clipboard, target, MOST)
            .map(|(_, b)| b)
            .filter(|b| b.starts_with(b"\x89PNG"));
    }
    let files = has("x-special/gnome-copied-files").or_else(|| has("text/uri-list"));
    if let Some(target) = files {
        if let Some((_, bytes)) = side.convert(clipboard, target, 4 * 1024 * 1024) {
            copied.files = decode(&bytes)
                .lines()
                .filter(|l| l.starts_with("file://"))
                .filter_map(path_of)
                .collect();
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_uris_go_both_ways() {
        let path = PathBuf::from("/tmp/a b/报告#1.txt");
        let uri = uri(&path);
        assert_eq!(uri, "file:///tmp/a%20b/%E6%8A%A5%E5%91%8A%231.txt");
        assert_eq!(path_of(&uri), Some(path));
        assert_eq!(path_of("file://host/x/y"), Some(PathBuf::from("/x/y")));
        assert_eq!(path_of("https://x/y"), None);
    }

    #[test]
    fn html_in_utf16_with_a_mark_is_read() {
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend("<b>x</b>".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode(&bytes), "<b>x</b>");
        assert_eq!(decode("<i>é</i>".as_bytes()), "<i>é</i>");
    }
}

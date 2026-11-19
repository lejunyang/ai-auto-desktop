//! The X11 clipboard (the CLIPBOARD selection) in every format, for `aad
//! assist`: a paste owns it with all of a [`Clip`]'s formats, answers only the
//! client of the focused window, and afterwards owns it again with what it
//! held before; a copy reads its formats back.
//!
//! Each runs on a connection of its own, so the shared screen's connection
//! never waits on another client. A selection owner must answer requests
//! while the paste key is pressed, so the key is pressed from another thread.
//!
//! Desktops often run programs that take CLIPBOARD as soon as anyone else
//! owns it (xrdp-chansrv when the RDP client announces its own clipboard; a
//! clipboard manager putting its last entry back). A paste takes it back,
//! presses the key only once it has kept it for a moment, and when it cannot
//! keep it says so, so text can be typed instead.
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

    /// A window for a person to recognise: its id, and its WM_CLASS when it
    /// has one (the selection windows of clipboard programs often have none).
    fn describe(&self, window: Window) -> String {
        let class = self
            .conn
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| {
                r.value
                    .split(|b| *b == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .unwrap_or_default();
        if class.is_empty() {
            format!("0x{window:x}")
        } else {
            format!("0x{window:x} ({class})")
        }
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
        self.convert_by(
            clipboard,
            target,
            most,
            Instant::now() + Duration::from_secs(2),
        )
    }

    /// [`Side::convert`], given up when no answer has come by `deadline`
    /// (INCR pieces then get up to ten seconds more).
    fn convert_by(
        &self,
        clipboard: Atom,
        target: Atom,
        most: usize,
        deadline: Instant,
    ) -> Option<(Atom, Vec<u8>)> {
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
        let deadline = deadline.max(Instant::now()) + Duration::from_secs(10);
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
        self.targets_by(clipboard, Instant::now() + Duration::from_secs(2))
    }

    fn targets_by(&self, clipboard: Atom, deadline: Instant) -> Vec<Atom> {
        let Ok(targets) = self.atom("TARGETS") else {
            return Vec::new();
        };
        self.convert_by(clipboard, targets, 64 * 1024, deadline)
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

/// What CLIPBOARD holds now, as items to own it with again later, read for
/// at most `budget`: its owner may fetch each format from far away (xrdp's
/// from the RDP client), and a paste must not wait on that.
fn save(side: &Side, clipboard: Atom, budget: Duration) -> Vec<Item> {
    let mut saved = Vec::new();
    let mut total = 0usize;
    let most = side.conn.maximum_request_bytes().saturating_sub(1024);
    let deadline = Instant::now() + budget;
    for target in side.targets_by(clipboard, deadline) {
        if Instant::now() >= deadline {
            break;
        }
        let name = side.name(target);
        if matches!(
            name.as_str(),
            "TARGETS" | "MULTIPLE" | "TIMESTAMP" | "SAVE_TARGETS" | "DELETE" | ""
        ) || name.starts_with("INSERT_")
        {
            continue;
        }
        if let Some((kind, bytes)) = side.convert_by(clipboard, target, most, deadline) {
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

/// The bases of the clients the paste key may go to: the one owning the
/// window the window manager calls active, and the one owning the window
/// with the keyboard focus (they differ when the window manager keeps the
/// focus on a frame or a toolkit on a helper window). Every window of one
/// client shares the bits outside the server's resource mask.
fn focused_clients(side: &Side) -> Vec<u32> {
    let mask = side.conn.setup().resource_id_mask;
    let active = side.atom("_NET_ACTIVE_WINDOW").ok().and_then(|active| {
        side.conn
            .get_property(false, side.root, active, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()
    });
    let focus = side
        .conn
        .get_input_focus()
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.focus);
    let mut bases = Vec::new();
    // 0 is None and 1 PointerRoot: neither names a client.
    for window in [active, focus].into_iter().flatten() {
        if window > 1 && window != side.root && !bases.contains(&(window & !mask)) {
            bases.push(window & !mask);
        }
    }
    bases
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

/// How long a paste keeps CLIPBOARD before pressing the key: a program that
/// takes it whenever someone else owns it has done so by then.
const SETTLE: Duration = Duration::from_millis(150);
/// How often a paste takes CLIPBOARD (back) before it gives up.
const MOST_TAKES: u32 = 6;
/// How long reading what CLIPBOARD held before may take.
const SAVE_BUDGET: Duration = Duration::from_millis(1_500);

/// CLIPBOARD owned for one paste: answers requests, and notes who asked
/// and who took it away.
struct Owner<'a> {
    side: &'a Side,
    clipboard: Atom,
    targets: Atom,
    items: &'a [Item],
    /// What clients other than the target are given: only the marker that
    /// tells clipboard managers to keep nothing.
    marker: Vec<Item>,
    clients: Vec<u32>,
    mask: u32,
    taken: Option<Instant>,
    takes: u32,
    refused: u32,
    /// Who asked for what (the first few), for the report.
    asked: Vec<String>,
    /// Who took CLIPBOARD away, each time.
    takers: Vec<String>,
}

impl Owner<'_> {
    /// Owns CLIPBOARD (again); whether the server says it does.
    fn take(&mut self) -> Result<bool> {
        self.takes += 1;
        let time = self.side.now();
        self.side
            .conn
            .set_selection_owner(self.side.window, self.clipboard, time)
            .map_err(|e| failed("owner")(&e))?;
        self.side.conn.flush().map_err(|e| failed("owner")(&e))?;
        Ok(self.side.owner(self.clipboard) == self.side.window)
    }

    /// Handles what came next; true when CLIPBOARD was taken away.
    fn step(&mut self) -> Result<bool> {
        match self.side.conn.poll_for_event() {
            Ok(Some(Event::SelectionRequest(request))) => {
                let from_target = self.clients.is_empty()
                    || self.clients.contains(&(request.requestor & !self.mask));
                if self.asked.len() < 16 {
                    self.asked.push(format!(
                        "{} {} {}",
                        self.side.describe(request.requestor),
                        self.side.name(request.target),
                        if from_target { "served" } else { "refused" }
                    ));
                }
                if from_target {
                    if serve(self.side, &request, self.items, self.targets) {
                        self.taken = Some(Instant::now());
                    }
                } else {
                    self.refused += 1;
                    serve(self.side, &request, &self.marker, self.targets);
                }
                Ok(false)
            }
            Ok(Some(Event::SelectionClear(_))) => {
                let owner = self.side.owner(self.clipboard);
                if self.takers.len() < 16 {
                    self.takers.push(if owner == x11rb::NONE {
                        "nobody".into()
                    } else {
                        self.side.describe(owner)
                    });
                }
                Ok(true)
            }
            Ok(Some(_)) => Ok(false),
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(2));
                Ok(false)
            }
            Err(e) => Err(failed("events")(&e)),
        }
    }

    /// Keeps CLIPBOARD for [`SETTLE`] in a row, taking it back each time
    /// another program takes it, at most [`MOST_TAKES`] times; whether it
    /// managed.
    fn settle(&mut self) -> Result<bool> {
        self.take()?;
        let mut since = Instant::now();
        loop {
            if self.step()? {
                if self.takes >= MOST_TAKES {
                    return Ok(false);
                }
                self.take()?;
                since = Instant::now();
            }
            if since.elapsed() >= SETTLE {
                return Ok(self.side.owner(self.clipboard) == self.side.window);
            }
        }
    }
}

/// Pastes every format of `clip` into the focused window: owns CLIPBOARD
/// with them, has the paste key pressed, answers that window's client
/// (anyone else gets only a marker clipboard managers skip), and owns it
/// again with what it held before once the target has taken what it wants.
///
/// The report says whether it was `pasted`; when not, `reason` says why:
/// `contended` (another program kept taking CLIPBOARD, so the key was not
/// pressed: the program in front would have pasted what that one holds),
/// `lost` (another program took it after the key, before the program in
/// front read it) or `not_asked` (the program in front did not ask), and
/// `type_instead` that the paste is plain text that can be typed instead.
pub fn paste(clip: Clip, press: Press) -> Pasting {
    std::thread::spawn(move || {
        let side = Side::open()?;
        let clipboard = side.atom("CLIPBOARD")?;
        let targets = side.atom("TARGETS")?;
        let hint = side.atom("x-kde-passwordManagerHint")?;
        let items = items_of(&side, &clip)?;
        let files = clip.has_files();
        let typeable =
            clip.text.as_deref().is_some_and(|t| !t.is_empty()) && clip.image.is_none() && !files;
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
        let saving = Instant::now();
        let saved = if side.owner(clipboard) == x11rb::NONE {
            Vec::new()
        } else {
            save(&side, clipboard, SAVE_BUDGET)
        };
        let saved_ms = u64::try_from(saving.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut owner = Owner {
            side: &side,
            clipboard,
            targets,
            items: &items,
            marker: vec![Item {
                target: hint,
                kind: hint,
                format: 8,
                bytes: b"secret".to_vec(),
            }],
            clients: focused_clients(&side),
            mask: side.conn.setup().resource_id_mask,
            taken: None,
            takes: 0,
            refused: 0,
            asked: Vec::new(),
            takers: Vec::new(),
        };
        if !owner.settle()? {
            return Ok(serde_json::json!({"pasted": false, "reason": "contended",
                "type_instead": typeable, "formats": names, "takes": owner.takes,
                "takers": owner.takers, "asked": owner.asked, "saved_ms": saved_ms}));
        }
        let pressing = std::thread::spawn(press);
        let deadline = Instant::now() + Duration::from_secs(if files { 10 } else { 5 });
        let quiet = Duration::from_millis(if files { 1_500 } else { 400 });
        let mut lost = false;
        loop {
            if owner.step()? {
                // Taken away before the program in front read it: take it
                // back while that program may still ask.
                if owner.taken.is_none() && owner.takes < 2 * MOST_TAKES && owner.take()? {
                    continue;
                }
                lost = true;
                break;
            }
            let now = Instant::now();
            if owner.taken.is_some_and(|at| now - at >= quiet)
                || (owner.taken.is_none() && now >= deadline)
            {
                break;
            }
        }
        let pressed = pressing
            .join()
            .unwrap_or_else(|_| Err("pressing the paste key failed".into()));
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
        let pasted = owner.taken.is_some();
        let mut report = serde_json::json!({"pasted": pasted, "formats": names,
            "restored": !lost, "refused": owner.refused, "takes": owner.takes,
            "takers": owner.takers, "asked": owner.asked, "saved_ms": saved_ms});
        if !pasted {
            report["reason"] = (if lost { "lost" } else { "not_asked" }).into();
            report["type_instead"] = typeable.into();
        }
        Ok(report)
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

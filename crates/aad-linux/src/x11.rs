//! The X11 desktop (Linux), spoken to directly.
//!
//! The DAMAGE extension names the regions redrawn since the last capture,
//! and only those are read back with GetImage into a double-buffered
//! picture; a still screen costs one round trip. Input goes through XTest:
//! keys are found by keysym in the server's keyboard map, and a character
//! the map lacks is typed through a spare keycode remapped for it, as
//! xdotool does. XFixes gives the pointer's position and image, RandR the
//! displays, `_NET_CLIENT_LIST` the windows. Without DAMAGE the whole screen
//! is read and compared instead.
use crate::Result;
use aad_platform::buffer::{Bounds, Crop, Target};
use aad_platform::damage::{Damage, Rect};
use aad_platform::{keys, Area, PixelFormat, Raw};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, ImageFormat, Mapping, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

fn failed(what: &str) -> impl Fn(&dyn std::fmt::Display) -> String + '_ {
    move |e| format!("X11 {what}: {e}")
}

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION: u8 = 6;

/// The shared part, resolved.
enum Part {
    All,
    Monitor(Bounds),
    Window(Window),
}

pub struct X11 {
    conn: RustConnection,
    root: Window,
    size: (u32, u32),
    /// The damage object and the region its changes are moved into.
    damage: Option<(u32, u32)>,
    frame: Option<Arc<Vec<u8>>>,
    spare: Option<Arc<Vec<u8>>>,
    stale: Vec<Rect>,
    refresh: bool,
    part: Part,
    crop: Crop,
    /// Where the shared part was at the last capture, and the whole screen.
    pub shown: (Bounds, Bounds),
    /// Keycodes with nothing on them, borrowed for characters the map lacks.
    spare_keys: Vec<SpareKey>,
    keysyms_per_keycode: u8,
    min_keycode: u8,
    keymap: Vec<u32>,
    /// Keycodes and buttons held down for the viewer.
    held_keys: Vec<u8>,
    held_buttons: Vec<u8>,
    cursor: Option<(i16, i16, bool, u32)>,
    cursor_sent: Option<Instant>,
    /// A window of our own, to receive the clipboard into.
    window: Window,
}

impl X11 {
    pub fn open(area: &Area) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| failed("display")(&e))?;
        let (root, width, height, depth) = {
            let s = &conn.setup().roots[screen];
            (
                s.root,
                u32::from(s.width_in_pixels),
                u32::from(s.height_in_pixels),
                s.root_depth,
            )
        };
        if depth != 24 && depth != 32 {
            return Err(format!(
                "the X11 screen is {depth}-bit; only 24- and 32-bit are shared"
            ));
        }
        let have = |name: &str| {
            conn.query_extension(name.as_bytes())
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.present)
        };
        if !have("XTEST") {
            return Err("the X server has no XTEST extension; input cannot be passed on".into());
        }
        let fixes = have("XFIXES")
            && conn
                .xfixes_query_version(5, 0)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some();
        let damage = if fixes && have("DAMAGE") {
            conn.damage_query_version(1, 1)
                .map_err(|e| failed("damage")(&e))?
                .reply()
                .map_err(|e| failed("damage")(&e))?;
            let damage = conn.generate_id().map_err(|e| failed("id")(&e))?;
            conn.damage_create(damage, root, ReportLevel::NON_EMPTY)
                .map_err(|e| failed("damage")(&e))?;
            let region = conn.generate_id().map_err(|e| failed("id")(&e))?;
            conn.xfixes_create_region(region, &[])
                .map_err(|e| failed("region")(&e))?;
            Some((damage, region))
        } else {
            None
        };
        let window = conn.generate_id().map_err(|e| failed("id")(&e))?;
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(|e| failed("window")(&e))?;
        let (min_keycode, max_keycode) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let mapping = conn
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)
            .map_err(|e| failed("keyboard")(&e))?
            .reply()
            .map_err(|e| failed("keyboard")(&e))?;
        conn.flush().map_err(|e| failed("flush")(&e))?;
        let desktop = (0, 0, width, height);
        let mut x11 = Self {
            conn,
            root,
            size: (width, height),
            damage,
            frame: None,
            spare: None,
            stale: Vec::new(),
            refresh: false,
            part: Part::All,
            crop: Crop::default(),
            shown: (desktop, desktop),
            spare_keys: Vec::new(),
            keysyms_per_keycode: mapping.keysyms_per_keycode,
            min_keycode,
            keymap: mapping.keysyms,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            cursor: None,
            cursor_sent: None,
            window,
        };
        x11.part = x11.resolve(area)?;
        x11.shown = (x11.bounds()?.unwrap_or(desktop), desktop);
        Ok(x11)
    }

    pub fn damage_known(&self) -> bool {
        self.damage.is_some()
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

    /// The displays, in the order --monitor numbers them.
    fn monitors(&self) -> Vec<(Bounds, bool, String)> {
        let Some(reply) = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return vec![((0, 0, self.size.0, self.size.1), true, "screen".into())];
        };
        reply
            .monitors
            .iter()
            .map(|m| {
                let name = self
                    .conn
                    .get_atom_name(m.name)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                    .unwrap_or_default();
                (
                    (
                        i32::from(m.x),
                        i32::from(m.y),
                        u32::from(m.width),
                        u32::from(m.height),
                    ),
                    m.primary,
                    name,
                )
            })
            .collect()
    }

    /// The window manager's client windows with their titles.
    fn windows(&self) -> Vec<(Window, String)> {
        let Ok(list) = self.atom("_NET_CLIENT_LIST") else {
            return Vec::new();
        };
        let Some(reply) = self
            .conn
            .get_property(false, self.root, list, AtomEnum::WINDOW, 0, 4096)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return Vec::new();
        };
        reply
            .value32()
            .map(|windows| windows.map(|w| (w, self.title(w))).collect())
            .unwrap_or_default()
    }

    fn title(&self, window: Window) -> String {
        let utf8 = self.atom("UTF8_STRING").unwrap_or(0);
        for (property, kind) in [
            (self.atom("_NET_WM_NAME").unwrap_or(0), utf8),
            (u32::from(AtomEnum::WM_NAME), u32::from(AtomEnum::STRING)),
        ] {
            if let Some(reply) = self
                .conn
                .get_property(false, window, property, kind, 0, 1024)
                .ok()
                .and_then(|c| c.reply().ok())
            {
                if !reply.value.is_empty() {
                    return String::from_utf8_lossy(&reply.value).into_owned();
                }
            }
        }
        String::new()
    }

    /// A window's place on the screen; `None` when it is gone or unmapped.
    fn window_bounds(&self, window: Window) -> Option<Bounds> {
        let attributes = self.conn.get_window_attributes(window).ok()?.reply().ok()?;
        if attributes.map_state != x11rb::protocol::xproto::MapState::VIEWABLE {
            return None;
        }
        let geometry = self.conn.get_geometry(window).ok()?.reply().ok()?;
        let at = self
            .conn
            .translate_coordinates(window, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some((
            i32::from(at.dst_x),
            i32::from(at.dst_y),
            u32::from(geometry.width),
            u32::from(geometry.height),
        ))
    }

    /// What `--list-screens` prints.
    pub fn screens(&self) -> Value {
        let monitors: Vec<Value> = self
            .monitors()
            .into_iter()
            .enumerate()
            .map(|(index, (b, primary, name))| {
                json!({"monitor": index, "name": name, "primary": primary,
                       "left": b.0, "top": b.1, "width": b.2, "height": b.3})
            })
            .collect();
        let windows: Vec<Value> = self
            .windows()
            .into_iter()
            .filter_map(|(window, title)| {
                let b = self.window_bounds(window)?;
                Some(json!({"window": format!("0x{window:x}"), "title": title,
                            "left": b.0, "top": b.1, "width": b.2, "height": b.3}))
            })
            .collect();
        json!({"monitors": monitors, "windows": windows})
    }

    fn resolve(&self, area: &Area) -> Result<Part> {
        Ok(match area {
            Area::All => Part::All,
            Area::Monitor(index) => {
                let all = self.monitors();
                let (bounds, _, _) = all.get(*index).ok_or_else(|| {
                    format!(
                        "--monitor {index}: there are {} displays (see --list-screens)",
                        all.len()
                    )
                })?;
                Part::Monitor(*bounds)
            }
            Area::Window(wanted) => {
                let id = wanted
                    .strip_prefix("0x")
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .unwrap_or_else(|| wanted.parse::<u32>())
                    .ok()
                    .filter(|id| self.window_bounds(*id).is_some());
                match id {
                    Some(window) => Part::Window(window),
                    None => {
                        let lower = wanted.to_lowercase();
                        let matching: Vec<(Window, String)> = self
                            .windows()
                            .into_iter()
                            .filter(|(_, title)| title.to_lowercase().contains(&lower))
                            .collect();
                        match matching.as_slice() {
                            [(one, _)] => Part::Window(*one),
                            [] => return Err(format!("--window {wanted}: no window with that id or title (see --list-screens)")),
                            many => {
                                let titles: Vec<&str> = many.iter().take(5).map(|(_, t)| t.as_str()).collect();
                                return Err(format!("--window {wanted}: {} windows match: {}; give an id from --list-screens", many.len(), titles.join(" | ")));
                            }
                        }
                    }
                }
            }
        })
    }

    /// Where the shared part is now; `None` while a window is not shown.
    fn bounds(&self) -> Result<Option<Bounds>> {
        let desktop = (0, 0, self.size.0, self.size.1);
        let wanted = match &self.part {
            Part::All => return Ok(Some(desktop)),
            Part::Monitor(bounds) => *bounds,
            Part::Window(window) => {
                if self
                    .conn
                    .get_geometry(*window)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_none()
                {
                    return Err("the shared window was closed".into());
                }
                match self.window_bounds(*window) {
                    Some(bounds) => bounds,
                    None => return Ok(None),
                }
            }
        };
        let left = wanted.0.max(0);
        let top = wanted.1.max(0);
        let right = (wanted.0 + wanted.2 as i32).min(desktop.2 as i32);
        let bottom = (wanted.1 + wanted.3 as i32).min(desktop.3 as i32);
        Ok((right > left && bottom > top)
            .then(|| (left, top, (right - left) as u32, (bottom - top) as u32)))
    }

    pub fn refresh(&mut self) {
        self.refresh = true;
    }

    /// The shared part if it changed, with what changed when known.
    pub fn capture(&mut self) -> Result<Option<Raw>> {
        let desktop = (0, 0, self.size.0, self.size.1);
        let Some(at) = self.bounds()? else {
            return Ok(None);
        };
        self.shown = (at, desktop);
        if at == desktop {
            return self.capture_screen();
        }
        if self.crop.at != Some(at) {
            self.refresh = true;
        }
        Ok(self
            .capture_screen()?
            .map(|whole| self.crop.cut(&whole, desktop, at)))
    }

    /// Reads a rectangle of the screen as BGRA rows.
    fn image(&self, r: Rect) -> Result<Vec<u8>> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                r.x as i16,
                r.y as i16,
                r.w as u16,
                r.h as u16,
                !0,
            )
            .map_err(|e| failed("image")(&e))?
            .reply()
            .map_err(|e| failed("image")(&e))?;
        if reply.data.len() < (r.w * r.h * 4) as usize {
            return Err("the X server sent a short image".into());
        }
        Ok(reply.data)
    }

    fn capture_screen(&mut self) -> Result<Option<Raw>> {
        // Events are not needed: damage is fetched below. Drain them.
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
        let whole = Rect {
            x: 0,
            y: 0,
            w: self.size.0,
            h: self.size.1,
        };
        let first = self.frame.is_none();
        let rects = match self.damage {
            Some((damage, region)) => {
                self.conn
                    .damage_subtract(damage, x11rb::NONE, region)
                    .map_err(|e| failed("damage")(&e))?;
                let fetched = self
                    .conn
                    .xfixes_fetch_region(region)
                    .map_err(|e| failed("region")(&e))?
                    .reply()
                    .map_err(|e| failed("region")(&e))?;
                if first {
                    vec![whole]
                } else {
                    fetched
                        .rectangles
                        .iter()
                        .filter_map(|r| {
                            Rect {
                                x: r.x.max(0) as u32,
                                y: r.y.max(0) as u32,
                                w: u32::from(r.width),
                                h: u32::from(r.height),
                            }
                            .clip(self.size.0, self.size.1)
                        })
                        .collect()
                }
            }
            None => {
                // No damage reports: read everything and let it be compared.
                let pixels = self.image(whole)?;
                return Ok(Some(Raw {
                    pixels: Arc::new(pixels),
                    width: self.size.0,
                    height: self.size.1,
                    format: PixelFormat::Bgra,
                    damage: None,
                }));
            }
        };
        if rects.is_empty() {
            let again = std::mem::take(&mut self.refresh);
            return Ok(self.frame.clone().filter(|_| again).map(|pixels| Raw {
                pixels,
                width: self.size.0,
                height: self.size.1,
                format: PixelFormat::Bgra,
                damage: Some(Damage::default()),
            }));
        }
        let images: Vec<(Rect, Vec<u8>)> = rects
            .iter()
            .map(|r| self.image(*r).map(|data| (*r, data)))
            .collect::<Result<_>>()?;
        let width = self.size.0 as usize;
        let mut target = Target {
            frame: &mut self.frame,
            spare: &mut self.spare,
            stale: &mut self.stale,
            width,
            height: self.size.1 as usize,
            ready: false,
        };
        let pixels = target.pixels();
        for (r, data) in &images {
            for y in 0..r.h as usize {
                let from = &data[y * r.w as usize * 4..(y + 1) * r.w as usize * 4];
                let to = ((r.y as usize + y) * width + r.x as usize) * 4;
                pixels[to..to + from.len()].copy_from_slice(from);
                // X leaves the padding byte as it likes; make it opaque.
                for alpha in pixels[to + 3..to + from.len()].iter_mut().step_by(4) {
                    *alpha = 255;
                }
            }
        }
        self.stale = rects.clone();
        self.refresh = false;
        Ok(Some(Raw {
            pixels: self.frame.clone().unwrap_or_default(),
            width: self.size.0,
            height: self.size.1,
            format: PixelFormat::Bgra,
            damage: (!first).then(|| Damage {
                moves: Vec::new(),
                dirty: rects,
            }),
        }))
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map_err(|e| failed("input")(&e))?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        self.conn.sync().map_err(|e| failed("input")(&e))
    }

    /// Moves the pointer to a place in the shared part (fractions).
    pub fn point(&self, fx: f64, fy: f64) -> Result<()> {
        let at = self.shown.0;
        let x = at.0 + (fx * f64::from(at.2.max(1) - 1)).round() as i32;
        let y = at.1 + (fy * f64::from(at.3.max(1) - 1)).round() as i32;
        self.fake(MOTION, 0, x as i16, y as i16)?;
        self.flush()
    }

    pub fn button(&mut self, button: u8, down: bool) -> Result<()> {
        self.held_buttons.retain(|b| *b != button);
        if down {
            self.held_buttons.push(button);
        }
        self.fake(
            if down { BUTTON_PRESS } else { BUTTON_RELEASE },
            button,
            0,
            0,
        )?;
        self.flush()
    }

    /// Scroll notches: X buttons 4 and 5 go up and down, 6 and 7 left and right.
    pub fn wheel(&self, dx: f64, dy: f64) -> Result<()> {
        let notches =
            |pixels: f64| ((pixels.abs() / 100.0).round() as u32).max(u32::from(pixels != 0.0));
        for (button, count) in [
            (if dy < 0.0 { 4 } else { 5 }, notches(dy)),
            (if dx < 0.0 { 6 } else { 7 }, notches(dx)),
        ] {
            for _ in 0..count {
                self.fake(BUTTON_PRESS, button, 0, 0)?;
                self.fake(BUTTON_RELEASE, button, 0, 0)?;
            }
        }
        self.flush()
    }

    /// The keycode with this keysym and whether Shift is needed for it.
    fn keycode(&self, keysym: u32) -> Option<(u8, bool)> {
        let per = usize::from(self.keysyms_per_keycode.max(1));
        self.keymap
            .chunks(per)
            .enumerate()
            .find_map(|(index, syms)| {
                let code = self.min_keycode.checked_add(index as u8)?;
                if syms.first() == Some(&keysym) {
                    Some((code, false))
                } else if syms.get(1) == Some(&keysym) {
                    Some((code, true))
                } else {
                    None
                }
            })
    }

    /// A keycode for a keysym the map lacks: a spare one, remapped to it.
    ///
    /// A client reads the changed map only when it next decodes a key, which
    /// can be well after the press was sent: a key's keysym must stay until
    /// then. So each character gets a spare keycode of its own, kept until
    /// the viewer lets go (`release`), and a spare is only reused once its
    /// last press has had `SPARE_KEY_SETTLE` to be read.
    fn remapped(&mut self, keysym: u32) -> Result<u8> {
        if let Some(spare) = self.spare_keys.iter().find(|s| s.keysym == keysym) {
            return Ok(spare.code);
        }
        let per = usize::from(self.keysyms_per_keycode.max(1));
        let unused = if self.spare_keys.len() < MAX_SPARE_KEYS {
            self.keymap
                .chunks(per)
                .enumerate()
                .rev()
                .filter(|(_, syms)| syms.iter().all(|s| *s == 0))
                .map(|(index, _)| self.min_keycode + index as u8)
                .find(|code| self.spare_keys.iter().all(|s| s.code != *code))
        } else {
            None
        };
        let index = match unused {
            Some(code) => {
                self.spare_keys.push(SpareKey {
                    code,
                    original: vec![0; per],
                    keysym: 0,
                    pressed: None,
                });
                self.spare_keys.len() - 1
            }
            None => {
                let index = (0..self.spare_keys.len())
                    .min_by_key(|i| self.spare_keys[*i].pressed)
                    .ok_or("the keyboard map has no spare keycode to type this character")?;
                settle(self.spare_keys[index].pressed);
                index
            }
        };
        let code = self.spare_keys[index].code;
        self.conn
            .change_keyboard_mapping(1, code, per as u8, &vec![keysym; per])
            .map_err(|e| failed("keyboard")(&e))?;
        self.flush()?;
        self.spare_keys[index].keysym = keysym;
        self.await_mapping_notify(code);
        Ok(code)
    }

    /// Wait until the server has announced a changed key. On a busy server
    /// the announcement can reach clients after a key event sent 20 ms after
    /// the change, and a client then reads that press with the old, empty
    /// map. It goes to every client at once, so ours arriving means theirs
    /// is on its way ahead of the press; then a moment for them to read it.
    fn await_mapping_notify(&mut self, code: u8) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match self.conn.poll_for_event() {
                Ok(Some(Event::MappingNotify(notify)))
                    if notify.request == Mapping::KEYBOARD
                        && (u16::from(notify.first_keycode)
                            ..u16::from(notify.first_keycode) + u16::from(notify.count))
                            .contains(&u16::from(code)) =>
                {
                    break
                }
                // Nothing else here needs events (capture fetches damage).
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(_) => break,
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    pub fn key(&mut self, key: keys::Key, down: bool) -> Result<()> {
        let keysym = key.keysym();
        let (code, _) = self
            .keycode(keysym)
            .ok_or_else(|| format!("the X11 keyboard map has no key {}", key.code))?;
        self.held_keys.retain(|k| *k != code);
        if down {
            self.held_keys.push(code);
        }
        self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, code, 0, 0)?;
        self.flush()
    }

    pub fn text(&mut self, text: &str) -> Result<()> {
        let shift = self.keycode(0xFFE1).map(|(code, _)| code);
        for ch in text.chars() {
            let keysym = match ch {
                '\n' | '\r' => 0xFF0D,
                '\t' => 0xFF09,
                c if (' '..='~').contains(&c) || ('\u{a0}'..='\u{ff}').contains(&c) => c as u32,
                c => 0x0100_0000 + c as u32,
            };
            let (code, shifted) = match self.keycode(keysym) {
                Some(found) => found,
                None => (self.remapped(keysym)?, false),
            };
            if shifted {
                if let Some(shift) = shift {
                    self.fake(KEY_PRESS, shift, 0, 0)?;
                }
            }
            self.fake(KEY_PRESS, code, 0, 0)?;
            self.fake(KEY_RELEASE, code, 0, 0)?;
            if shifted {
                if let Some(shift) = shift {
                    self.fake(KEY_RELEASE, shift, 0, 0)?;
                }
            }
            self.flush()?;
            if let Some(spare) = self.spare_keys.iter_mut().find(|s| s.code == code) {
                spare.pressed = Some(Instant::now());
            }
        }
        Ok(())
    }

    /// Give the borrowed keycodes back what they held, once their last
    /// presses have had time to be read.
    fn restore_spare_keys(&mut self) -> Result<()> {
        if self.spare_keys.is_empty() {
            return Ok(());
        }
        settle(self.spare_keys.iter().filter_map(|s| s.pressed).max());
        for spare in std::mem::take(&mut self.spare_keys) {
            self.conn
                .change_keyboard_mapping(1, spare.code, spare.original.len() as u8, &spare.original)
                .map_err(|e| failed("keyboard")(&e))?;
        }
        self.flush()
    }

    pub fn release(&mut self) {
        for code in std::mem::take(&mut self.held_keys).into_iter().rev() {
            let _ = self.fake(KEY_RELEASE, code, 0, 0);
        }
        for button in std::mem::take(&mut self.held_buttons) {
            let _ = self.fake(BUTTON_RELEASE, button, 0, 0);
        }
        let _ = self.flush();
        let _ = self.restore_spare_keys();
    }

    /// A `cursor` message when the pointer moved or changed, at most 60 a
    /// second; `fresh` sends everything again.
    pub fn cursor(&mut self, fresh: bool) -> Option<Value> {
        if fresh {
            self.cursor = None;
            self.cursor_sent = None;
        }
        if self
            .cursor_sent
            .is_some_and(|at| at.elapsed() < Duration::from_millis(16))
        {
            return None;
        }
        let image = self.conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
        let at = self.shown.0;
        let (x, y) = (i32::from(image.x), i32::from(image.y));
        let visible = x >= at.0 && y >= at.1 && x < at.0 + at.2 as i32 && y < at.1 + at.3 as i32;
        let new_shape = self
            .cursor
            .is_none_or(|(_, _, _, serial)| serial != image.cursor_serial);
        let state = (image.x, image.y, visible, image.cursor_serial);
        if self.cursor == Some(state) {
            return None;
        }
        self.cursor = Some(state);
        self.cursor_sent = Some(Instant::now());
        let fraction = |v: i32, origin: i32, size: u32| {
            (f64::from(v - origin) / f64::from(size.max(1))).clamp(0.0, 1.0)
        };
        let mut message = json!({"type": "cursor", "visible": visible,
            "x": fraction(x, at.0, at.2), "y": fraction(y, at.1, at.3)});
        if new_shape && image.width > 0 && image.height > 0 {
            // Premultiplied ARGB, one u32 per pixel.
            let mut rgba = Vec::with_capacity(image.cursor_image.len() * 4);
            for argb in &image.cursor_image {
                let a = (argb >> 24) & 0xFF;
                let unmultiply = |c: u32| (c * 255).checked_div(a).map_or(0, |v| v.min(255) as u8);
                rgba.extend_from_slice(&[
                    unmultiply((argb >> 16) & 0xFF),
                    unmultiply((argb >> 8) & 0xFF),
                    unmultiply(argb & 0xFF),
                    a as u8,
                ]);
            }
            if let Some(picture) =
                image::RgbaImage::from_raw(u32::from(image.width), u32::from(image.height), rgba)
            {
                let mut png = Vec::new();
                if picture
                    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                    .is_ok()
                {
                    use base64::Engine;
                    message["shape"] = json!({"png": base64::engine::general_purpose::STANDARD.encode(png),
                        "width": image.width, "height": image.height, "hotspot": [image.xhot, image.yhot],
                        "screen": [at.2, at.3]});
                }
            }
        }
        Some(message)
    }

    /// The CLIPBOARD selection as text, asked of its owner.
    pub fn clipboard(&mut self, max_characters: usize) -> Result<Value> {
        let clipboard = self.atom("CLIPBOARD")?;
        let utf8 = self.atom("UTF8_STRING")?;
        let property = self.atom("AAD_ASSIST_CLIPBOARD")?;
        let owner = self
            .conn
            .get_selection_owner(clipboard)
            .map_err(|e| failed("clipboard")(&e))?
            .reply()
            .map_err(|e| failed("clipboard")(&e))?
            .owner;
        if owner == x11rb::NONE {
            return Ok(
                json!({"type": "clipboard", "text": null, "characters": 0, "truncated": false}),
            );
        }
        self.conn
            .convert_selection(self.window, clipboard, utf8, property, x11rb::CURRENT_TIME)
            .map_err(|e| failed("clipboard")(&e))?;
        self.conn.flush().map_err(|e| failed("clipboard")(&e))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self
                .conn
                .poll_for_event()
                .map_err(|e| failed("clipboard")(&e))?
            {
                Some(Event::SelectionNotify(notify)) if notify.requestor == self.window => {
                    if notify.property == x11rb::NONE {
                        return Ok(
                            json!({"type": "clipboard", "text": null, "characters": 0, "truncated": false}),
                        );
                    }
                    break;
                }
                Some(_) => {}
                None if Instant::now() > deadline => {
                    return Err("the clipboard's owner did not answer".into())
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        let reply = self
            .conn
            .get_property(
                true,
                self.window,
                property,
                AtomEnum::ANY,
                0,
                (4 * max_characters as u32).div_ceil(4) + 1,
            )
            .map_err(|e| failed("clipboard")(&e))?
            .reply()
            .map_err(|e| failed("clipboard")(&e))?;
        if reply.type_ == self.atom("INCR")? {
            return Err("the clipboard holds more text than is handed over at once".into());
        }
        let text = String::from_utf8_lossy(&reply.value).into_owned();
        let characters = text.chars().count() + reply.bytes_after as usize;
        Ok(
            json!({"type": "clipboard", "text": text.chars().take(max_characters).collect::<String>(),
                  "characters": characters, "truncated": characters > max_characters}),
        )
    }
}

/// A keycode borrowed for a character the keyboard map lacks.
struct SpareKey {
    code: u8,
    /// What it held before.
    original: Vec<u32>,
    /// The keysym it holds now.
    keysym: u32,
    pressed: Option<Instant>,
}

/// How many keycodes a viewer's text may borrow at once.
const MAX_SPARE_KEYS: usize = 16;
/// How long a borrowed key keeps its keysym after a press, for a client that
/// reads the changed map only when it decodes that press.
const SPARE_KEY_SETTLE: Duration = Duration::from_millis(250);

fn settle(pressed: Option<Instant>) {
    if let Some(wait) = pressed.and_then(|at| SPARE_KEY_SETTLE.checked_sub(at.elapsed())) {
        std::thread::sleep(wait);
    }
}

impl Drop for X11 {
    fn drop(&mut self) {
        self.release();
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

//! Observe and revalidate a local file dialog and its browser owner.
use crate::{
    atspi::{Gtk4Picker, GtkFileDialog, KdeDialog},
    window_geometry,
};
use aad_platform::picker::{Kind, NewFile};
use std::cell::Cell;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, EventMask,
    MapState, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
mod portal;
use portal::FilePortal;

enum Chooser {
    Gtk3,
    Gtk4(Box<Gtk4Picker>),
    Gtk4Save(String),
    Portal(FilePortal),
}

enum Surface {
    X11(Box<OpenPicker>),
    Wayland(FilePortal),
    WaylandKde(FilePortal, Box<KdeDialog>),
}

pub struct FilePicker {
    surface: Surface,
    controls: Option<GtkFileDialog>,
    kind: Kind,
}

impl FilePicker {
    pub fn observe(
        pid: u32,
        expected: Option<u64>,
        bounds: [i32; 4],
        kind: Kind,
        deadline: Instant,
    ) -> Result<Self> {
        let (surface, controls) = match std::env::var("XDG_SESSION_TYPE").as_deref() {
            Ok("x11") => {
                let expected = expected.map(u32::try_from).transpose()?;
                let window = OpenPicker::observe_kind(pid, expected, bounds, kind, deadline)?;
                let controls = match (&window.chooser, kind) {
                    (Chooser::Gtk4Save(class), Kind::Save) => Some(
                        GtkFileDialog::observe_x11_gtk4_save(pid, class.clone(), deadline)?,
                    ),
                    (Chooser::Portal(portal), Kind::Save) => {
                        Some(GtkFileDialog::observe(portal.picker_pid, kind, deadline)?)
                    }
                    (_, Kind::Save) => Some(GtkFileDialog::observe(pid, kind, deadline)?),
                    (_, Kind::Open | Kind::Directory | Kind::DirectoryUpload) => None,
                };
                (Surface::X11(Box::new(window)), controls)
            }
            Ok("wayland") => {
                if expected.is_some_and(|id| id != 0) {
                    return Err("Wayland file portals have no numeric XID".into());
                }
                match FilePortal::for_browser(pid, deadline) {
                    Ok(portal) => {
                        let controls = GtkFileDialog::observe(portal.picker_pid, kind, deadline)?;
                        (Surface::Wayland(portal), Some(controls))
                    }
                    Err(gtk_error) => {
                        let portal = FilePortal::kde(pid, deadline).map_err(|kde_error|
                            format!("could not bind the Chromium request to GTK ({gtk_error}) or KDE ({kde_error})"))?;
                        let dialog = KdeDialog::observe(portal.picker_pid, kind, deadline)?;
                        (Surface::WaylandKde(portal, Box::new(dialog)), None)
                    }
                }
            }
            _ => return Err("expected a local X11 or Wayland session".into()),
        };
        let picker = Self {
            surface,
            controls,
            kind,
        };
        picker.revalidate(deadline)?;
        Ok(picker)
    }

    pub fn id(&self) -> u64 {
        match &self.surface {
            Surface::Wayland(_) | Surface::WaylandKde(..) => 0,
            Surface::X11(window) => window.xid().into(),
        }
    }

    /// Wayland portals identify the browser process but expose no transient XID.
    pub fn process_only(&self) -> bool {
        !matches!(self.surface, Surface::X11(_))
    }

    pub fn revalidate(&self, deadline: Instant) -> Result<()> {
        match &self.surface {
            Surface::X11(window) => {
                window.revalidate(deadline)?;
                if let Some(gtk) = &self.controls {
                    if window.title()? != gtk.title().as_bytes() {
                        return Err("GTK dialog and X11 window titles differ".into());
                    }
                }
            }
            Surface::Wayland(portal) => portal.revalidate(deadline)?,
            Surface::WaylandKde(portal, dialog) => {
                portal.revalidate(deadline)?;
                dialog.revalidate(deadline)?;
            }
        }
        if let Some(gtk) = &self.controls {
            gtk.revalidate(deadline)?;
        }
        Ok(())
    }

    pub fn dismiss(&self, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Open {
            return Err("expected the observed Open picker".into());
        }
        if let Surface::X11(window) = &self.surface {
            return window.dismiss(deadline);
        }
        if let Surface::WaylandKde(portal, dialog) = &self.surface {
            if !dialog.gone(deadline)? {
                self.revalidate(deadline)?;
                dialog.respond(false, deadline)?;
            }
            return wait_kde(portal, dialog, deadline);
        }
        let gtk = self
            .controls
            .as_ref()
            .ok_or("missing native Open response")?;
        if !self.closed(gtk, deadline)? {
            self.revalidate(deadline)?;
            gtk.respond(deadline)?;
        }
        self.wait_closed(gtk, deadline)
    }

    pub fn save_new(&self, path: &Path, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Save {
            return Err("expected the observed Save picker".into());
        }
        let destination =
            NewFile::under(path.parent().ok_or("missing destination directory")?, path)?;
        if destination.path().as_os_str() != path.as_os_str() {
            return Err("expected a fully resolved save destination".into());
        }
        let text = path.to_str().ok_or("filename is not UTF-8")?;
        if let Surface::WaylandKde(portal, dialog) = &self.surface {
            return self.select_kde(portal, dialog, text, deadline, || {
                Ok(destination.revalidate()?)
            });
        }
        let gtk = self
            .controls
            .as_ref()
            .ok_or("missing native Save controls")?;
        self.revalidate(deadline)?;
        destination.revalidate()?;
        gtk.fill(text, deadline)?;
        self.revalidate(deadline)?;
        gtk.verify_text(text)?;
        destination.revalidate()?;
        gtk.respond(deadline)?;
        self.wait_closed(gtk, deadline)
    }

    pub fn open_file(&self, path: &Path, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Open {
            return Err("expected the observed Open picker".into());
        }
        self.select_existing(path, deadline)
    }

    pub fn choose_directory(&self, path: &Path, deadline: Instant) -> Result<()> {
        if !matches!(self.kind, Kind::Directory | Kind::DirectoryUpload) {
            return Err("expected the observed directory picker".into());
        }
        self.select_existing(path, deadline)
    }

    fn select_existing(&self, path: &Path, deadline: Instant) -> Result<()> {
        let text = path.to_str().ok_or("file path is not UTF-8")?;
        if text.chars().any(char::is_control) {
            return Err("invalid characters in native file path".into());
        }
        let identity = || -> Result<_> {
            let meta = path.symlink_metadata()?;
            let expected = if self.kind != Kind::Open {
                meta.is_dir()
            } else {
                meta.is_file()
            };
            if !expected || path.canonicalize()?.as_os_str() != path.as_os_str() {
                return Err("the path or its filesystem type changed".into());
            }
            Ok((meta.dev(), meta.ino()))
        };
        let observed_file = identity()?;
        if let Surface::WaylandKde(portal, dialog) = &self.surface {
            return self.select_kde(portal, dialog, text, deadline, || {
                if identity()? != observed_file {
                    return Err("the granted file changed".into());
                }
                Ok(())
            });
        }
        let local;
        let gtk = match (&self.surface, &self.controls) {
            (_, Some(gtk)) => gtk,
            (Surface::X11(window), None) => {
                if let Some(gtk) = &window.open_controls {
                    gtk
                } else {
                    let owner = match &window.chooser {
                        Chooser::Portal(portal) => portal.picker_pid,
                        _ => window.pid,
                    };
                    local = GtkFileDialog::observe(owner, self.kind, deadline)?;
                    &local
                }
            }
            _ => return Err("missing observable native file chooser".into()),
        };
        let revalidate = || -> Result<()> {
            self.revalidate(deadline)?;
            gtk.revalidate(deadline)?;
            if let Surface::X11(window) = &self.surface {
                if window.title()? != gtk.title().as_bytes() {
                    return Err("GTK location dialog differs from the X11 window".into());
                }
            }
            if identity()? != observed_file {
                return Err("the granted file changed".into());
            }
            Ok(())
        };
        revalidate()?;
        let location = match gtk.location(deadline)? {
            Some(location) => location,
            None => {
                gtk.show_location(deadline)?;
                revalidate()?;
                gtk.location(deadline)?
                    .ok_or("the chooser did not expose a location field")?
            }
        };
        revalidate()?;
        gtk.fill_location(&location, text, deadline)?;
        loop {
            revalidate()?;
            if gtk.accept_location(&location, text, deadline)? {
                break;
            }
            // GTK asynchronously resolves and validates the location before
            // making the response sensitive. No click was sent on this path.
            if Instant::now() >= deadline {
                return Err("the observed Open response stayed disabled".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.wait_closed(gtk, deadline)
    }

    fn closed(&self, gtk: &GtkFileDialog, deadline: Instant) -> Result<bool> {
        if !gtk.gone(deadline)? {
            return Ok(false);
        }
        match &self.surface {
            Surface::Wayland(portal) => portal.finished(deadline),
            Surface::WaylandKde(..) => Err("expected GTK controls".into()),
            Surface::X11(window) => {
                if !window.ended()? {
                    return Ok(false);
                }
                if let Chooser::Portal(portal) = &window.chooser {
                    return portal.finished(deadline);
                }
                Ok(true)
            }
        }
    }
    fn select_kde(
        &self,
        portal: &FilePortal,
        dialog: &KdeDialog,
        text: &str,
        deadline: Instant,
        check_path: impl Fn() -> Result<()>,
    ) -> Result<()> {
        self.revalidate(deadline)
            .map_err(|e| format!("KDE dialog before entry: {e}"))?;
        check_path()?;
        dialog
            .fill(text, deadline)
            .map_err(|e| format!("KDE filename entry: {e}"))?;
        self.revalidate(deadline)
            .map_err(|e| format!("KDE dialog after entry: {e}"))?;
        dialog.verify(text)?;
        check_path()?;
        dialog
            .respond(true, deadline)
            .map_err(|e| format!("KDE file confirmation: {e}"))?;
        wait_kde(portal, dialog, deadline).map_err(|e| format!("KDE dialog closure: {e}").into())
    }
    fn wait_closed(&self, gtk: &GtkFileDialog, deadline: Instant) -> Result<()> {
        while Instant::now() < deadline {
            if self.closed(gtk, deadline)? {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err("the observed chooser did not disappear after its response".into())
    }
}

fn wait_kde(portal: &FilePortal, dialog: &KdeDialog, deadline: Instant) -> Result<()> {
    while Instant::now() < deadline {
        if portal.finished(deadline)? && dialog.gone(deadline)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("the KDE chooser and request did not disappear".into())
}

x11rb::atom_manager! {
    Atoms: AtomCookies {
        _NET_ACTIVE_WINDOW, _NET_WM_NAME, _NET_WM_PID, _NET_WM_STATE, _NET_WM_STATE_MODAL,
        _NET_WM_WINDOW_TYPE, _NET_WM_WINDOW_TYPE_DIALOG, _NET_WM_WINDOW_TYPE_NORMAL,
        UTF8_STRING, WM_DELETE_WINDOW, WM_PROTOCOLS, WM_WINDOW_ROLE,
    }
}

pub struct OpenPicker {
    conn: RustConnection,
    atoms: Atoms,
    root: Window,
    picker: Window,
    owner: Window,
    pid: u32,
    bounds: [i32; 4],
    chooser: Chooser,
    kind: Kind,
    ended: Cell<bool>,
    open_controls: Option<Box<GtkFileDialog>>,
}

impl OpenPicker {
    /// Bind the active dialog to the uniquely matching CDP page window.
    pub fn observe(
        pid: u32,
        expected: Option<Window>,
        bounds: [i32; 4],
        deadline: Instant,
    ) -> Result<Self> {
        Self::observe_kind(pid, expected, bounds, Kind::Open, deadline)
    }
    fn observe_kind(
        pid: u32,
        expected: Option<Window>,
        bounds: [i32; 4],
        kind: Kind,
        deadline: Instant,
    ) -> Result<Self> {
        if std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("x11") {
            return Err("the native picker requires an X11 session".into());
        }
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn)?.reply()?;
        let active = conn
            .get_property(
                false,
                root,
                atoms._NET_ACTIVE_WINDOW,
                AtomEnum::WINDOW,
                0,
                2,
            )?
            .reply()?;
        let picker = active
            .value32()
            .and_then(|mut ids| ids.next())
            .filter(|&w| w != 0)
            .ok_or("there is no active X11 window")?;
        if expected.is_some_and(|w| w != picker) {
            return Err("the active window differs from --window".into());
        }
        // Subscribe before reading identity. Unmapping or destroying the object
        // ends this observation even if the client later reuses the same XID.
        conn.change_window_attributes(
            picker,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        let parent = conn
            .get_property(
                false,
                picker,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?;
        let owner = parent
            .value32()
            .and_then(|mut ids| ids.next())
            .filter(|&w| w != 0 && w != picker && parent.bytes_after == 0)
            .ok_or("chooser has no exact transient parent")?;
        let mut observed = Self {
            conn,
            atoms,
            root,
            picker,
            owner,
            pid,
            bounds,
            chooser: Chooser::Gtk3,
            kind,
            ended: Cell::new(false),
            open_controls: None,
        };
        let pids = observed.words(picker, atoms._NET_WM_PID, AtomEnum::CARDINAL)?;
        let [picker_pid] = pids.as_slice() else {
            return Err("missing picker PID".into());
        };
        observed.check_window(picker, atoms._NET_WM_WINDOW_TYPE_DIALOG, *picker_pid)?;
        let role = observed.bytes(picker, atoms.WM_WINDOW_ROLE, AtomEnum::STRING.into())?;
        if *picker_pid != pid {
            if role != b"GtkFileChooserDialog" {
                return Err("unexpected external dialog role".into());
            }
            observed.chooser = Chooser::Portal(FilePortal::observe(pid, *picker_pid, deadline)?);
            let title = observed.bytes(picker, atoms._NET_WM_NAME, atoms.UTF8_STRING)?;
            if kind == Kind::Open && !standard_open_title(&title) {
                let gtk = GtkFileDialog::observe(*picker_pid, kind, deadline)?;
                if title != gtk.title().as_bytes() {
                    return Err("GTK portal and X11 dialog titles differ".into());
                }
                observed.open_controls = Some(Box::new(gtk));
            }
        } else if role.is_empty() {
            let class =
                observed.bytes(picker, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into())?;
            let name = class
                .split(|&b| b == 0)
                .next()
                .filter(|s| !s.is_empty())
                .ok_or("missing GTK window class")?;
            let class = std::str::from_utf8(name)?.to_owned();
            observed.chooser = match kind {
                Kind::Directory | Kind::DirectoryUpload => return Err("this GTK 4 dialog does not expose a verified directory entry; use the desktop file portal".into()),
                Kind::Save => Chooser::Gtk4Save(class),
                Kind::Open => Chooser::Gtk4(Box::new(Gtk4Picker::observe(
                    pid,
                    class,
                    String::from_utf8(observed.title()?)?,
                    deadline,
                )?)),
            };
        }
        observed.revalidate(deadline)?;
        Ok(observed)
    }

    pub fn xid(&self) -> Window {
        self.picker
    }

    fn words(&self, id: Window, name: Atom, kind: AtomEnum) -> Result<Vec<u32>> {
        let p = self
            .conn
            .get_property(false, id, name, kind, 0, 256)?
            .reply()?;
        if p.bytes_after != 0 || p.type_ != u32::from(kind) {
            return Err("missing or incomplete X11 window property".into());
        }
        let words = p.value32().ok_or("invalid X11 property format")?;
        Ok(words.collect())
    }

    fn bytes(&self, id: Window, name: Atom, kind: Atom) -> Result<Vec<u8>> {
        let p = self
            .conn
            .get_property(false, id, name, kind, 0, 256)?
            .reply()?;
        if p.type_ == 0 {
            return Ok(Vec::new());
        }
        if p.type_ != kind || p.format != 8 || p.bytes_after != 0 {
            return Err("invalid X11 text property".into());
        }
        Ok(p.value)
    }

    fn check_window(&self, id: Window, kind: Atom, pid: u32) -> Result<()> {
        let ids = self
            .conn
            .res_query_client_ids(&[ClientIdSpec {
                client: id,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            }])?
            .reply()?
            .ids;
        if ids.len() != 1
            || ids[0].value != [pid]
            || self.words(id, self.atoms._NET_WM_PID, AtomEnum::CARDINAL)? != [pid]
            || self
                .words(id, self.atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM)?
                .first()
                != Some(&kind)
            || self.conn.get_window_attributes(id)?.reply()?.map_state != MapState::VIEWABLE
        {
            return Err(
                "X11 window ownership, type or visibility differs from the observation".into(),
            );
        }
        Ok(())
    }

    fn title(&self) -> Result<Vec<u8>> {
        let title = self.bytes(self.picker, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING)?;
        if title.is_empty()
            || (self.kind == Kind::Open
                && !standard_open_title(&title)
                && self
                    .open_controls
                    .as_ref()
                    .map(|gtk| gtk.title().as_bytes())
                    != Some(&title))
        {
            return Err("unexpected file dialog title".into());
        }
        Ok(title)
    }

    fn check_pinned(&self) -> Result<()> {
        let a = &self.atoms;
        let picker_pid = match &self.chooser {
            Chooser::Portal(p) => p.picker_pid,
            _ => self.pid,
        };
        self.check_window(self.picker, a._NET_WM_WINDOW_TYPE_DIALOG, picker_pid)?;
        self.check_window(self.owner, a._NET_WM_WINDOW_TYPE_NORMAL, self.pid)?;
        let active = self.words(self.root, a._NET_ACTIVE_WINDOW, AtomEnum::WINDOW)?;
        let title = self.title()?;
        let role = self.bytes(self.picker, a.WM_WINDOW_ROLE, AtomEnum::STRING.into())?;
        let chooser = match &self.chooser {
            Chooser::Gtk4Save(class) => {
                role.is_empty()
                    && self.bytes(
                        self.picker,
                        AtomEnum::WM_CLASS.into(),
                        AtomEnum::STRING.into(),
                    )? == format!("{class}\0{class}\0").as_bytes()
            }
            Chooser::Gtk4(gtk) => {
                role.is_empty()
                    && gtk.matches_window(
                        &self.bytes(
                            self.picker,
                            AtomEnum::WM_CLASS.into(),
                            AtomEnum::STRING.into(),
                        )?,
                        &title,
                    )
            }
            Chooser::Gtk3 => role == b"GtkFileChooserDialog",
            Chooser::Portal(_) => {
                role == b"GtkFileChooserDialog"
                    && self.bytes(
                        self.picker,
                        AtomEnum::WM_CLASS.into(),
                        AtomEnum::STRING.into(),
                    )? == b"xdg-desktop-portal-gtk\0Xdg-desktop-portal-gtk\0"
            }
        };
        if active.first() != Some(&self.picker)
            || active.iter().skip(1).any(|&w| w != 0)
            || self.words(
                self.picker,
                AtomEnum::WM_TRANSIENT_FOR.into(),
                AtomEnum::WINDOW,
            )? != [self.owner]
            || !self
                .words(self.picker, a._NET_WM_STATE, AtomEnum::ATOM)?
                .contains(&a._NET_WM_STATE_MODAL)
            || !chooser
            || !self
                .words(self.picker, a.WM_PROTOCOLS, AtomEnum::ATOM)?
                .contains(&a.WM_DELETE_WINDOW)
        {
            return Err(
                "expected the observed active modal Open File dialog of this browser window".into(),
            );
        }
        Ok(())
    }

    fn ended(&self) -> Result<bool> {
        if self.ended.get() {
            return Ok(true);
        }
        // A round trip puts prior StructureNotify events in our queue.
        self.conn.get_input_focus()?.reply()?;
        while let Some(event) = self.conn.poll_for_event()? {
            if matches!(event, Event::DestroyNotify(e) if e.window == self.picker)
                || matches!(event, Event::UnmapNotify(e) if e.window == self.picker)
            {
                self.ended.set(true);
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn revalidate(&self, deadline: Instant) -> Result<()> {
        let mut windows = window_geometry::clients(self.pid, deadline)?;
        let mut owners = windows
            .iter()
            .filter(|w| [w.x, w.y, w.width, w.height] == self.bounds);
        if owners.next().map(|w| w.id) != Some(self.owner) || owners.next().is_some() {
            return Err("CDP page bounds do not identify a unique transient owner on X11".into());
        }
        if let Chooser::Portal(portal) = &self.chooser {
            windows.extend(window_geometry::clients(portal.picker_pid, deadline)?);
        }
        for window in windows.iter().filter(|w| w.id != self.picker) {
            if self.bytes(
                window.id,
                self.atoms.WM_WINDOW_ROLE,
                AtomEnum::STRING.into(),
            )? == b"GtkFileChooserDialog"
                || (!matches!(self.chooser, Chooser::Gtk3)
                    && self
                        .words(window.id, self.atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM)?
                        .contains(&self.atoms._NET_WM_WINDOW_TYPE_DIALOG))
            {
                return Err("multiple native file choosers belong to this browser".into());
            }
        }
        match &self.chooser {
            Chooser::Gtk3 | Chooser::Gtk4Save(_) => (),
            Chooser::Gtk4(gtk) => gtk.revalidate(deadline)?,
            Chooser::Portal(portal) => portal.revalidate(deadline)?,
        }
        if let Some(gtk) = &self.open_controls {
            gtk.revalidate(deadline)?;
        }
        self.check_pinned()?;
        if self.ended()? {
            return Err("the observed picker was unmapped or destroyed".into());
        }
        Ok(())
    }

    /// Consume the observation. Deliver WM_DELETE only to the original XID,
    /// then wait for that window's unmap/destroy event, without retrying input.
    pub fn dismiss(&self, deadline: Instant) -> Result<()> {
        if self.kind != Kind::Open {
            return Err("expected an Open dialog".into());
        }
        if self.ended()? {
            return Ok(());
        }
        self.revalidate(deadline)?;
        // Keep the final identity check and SendEvent together on the server.
        // The grab covers only requests on this connection; never CDP or a wait.
        self.conn.grab_server()?.check()?;
        let sent = (|| -> Result<()> {
            self.check_pinned()?;
            if self.ended()? {
                return Err("picker lifetime ended during dismissal".into());
            }
            let message = ClientMessageEvent::new(
                32,
                self.picker,
                self.atoms.WM_PROTOCOLS,
                [self.atoms.WM_DELETE_WINDOW, x11rb::CURRENT_TIME, 0, 0, 0],
            );
            self.conn
                .send_event(false, self.picker, EventMask::NO_EVENT, message)?
                .check()?;
            Ok(())
        })();
        self.conn.ungrab_server()?.check()?;
        sent?;
        while Instant::now() < deadline {
            if self.ended()? {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err("the observed chooser did not unmap after WM_DELETE_WINDOW".into())
    }
}

fn standard_open_title(title: &[u8]) -> bool {
    matches!(
        std::str::from_utf8(title),
        Ok("Open File" | "Open Files" | "打开文件")
    )
}

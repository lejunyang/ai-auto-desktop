use super::{AtspiBackend, LinuxBackend, NativeRef, TextProxy};
use aad_platform::picker::Kind;
use atspi::{RelationType, Role, State, StateSet};
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(PartialEq)]
struct Controls {
    app: NativeRef,
    window: NativeRef,
    title: String,
    widget: NativeRef,
    cancel: NativeRef,
    accept: NativeRef,
    name: Option<(NativeRef, NativeRef)>,
}

pub(crate) struct GtkFileDialog {
    backend: LinuxBackend,
    pid: u32,
    kind: Kind,
    toolkit: Toolkit,
    controls: Controls,
}

enum Toolkit {
    Gtk3,
    X11Gtk4(String),
}

impl Toolkit {
    fn shows(&self, states: StateSet) -> bool {
        match self {
            Self::Gtk3 => shown(states),
            // GTK 4.8 reports Visible and Sensitive. The X11 provider checks
            // active XID, viewability, modality and transient owner separately.
            Self::X11Gtk4(_) => states.contains(State::Visible),
        }
    }
    fn enables(&self, states: StateSet) -> bool {
        match self {
            Self::Gtk3 => enabled(states),
            Self::X11Gtk4(_) => states.contains(State::Sensitive),
        }
    }
}

#[derive(PartialEq)]
pub(crate) struct Location {
    layer: NativeRef,
    entry: NativeRef,
}

fn shown(states: StateSet) -> bool {
    states.contains(State::Showing) && states.contains(State::Visible)
}
fn enabled(states: StateSet) -> bool {
    states.contains(State::Enabled) && states.contains(State::Sensitive)
}
fn unique<T>(items: Vec<T>, message: &'static str) -> Result<T> {
    let [item]: [T; 1] = items.try_into().map_err(|_| message)?;
    Ok(item)
}

impl GtkFileDialog {
    pub(crate) fn observe(pid: u32, kind: Kind, deadline: Instant) -> Result<Self> {
        Self::bind(pid, kind, Toolkit::Gtk3, deadline)
    }
    pub(crate) fn observe_x11_gtk4_save(
        pid: u32,
        class: String,
        deadline: Instant,
    ) -> Result<Self> {
        Self::bind(pid, Kind::Save, Toolkit::X11Gtk4(class), deadline)
    }
    fn bind(pid: u32, kind: Kind, toolkit: Toolkit, deadline: Instant) -> Result<Self> {
        let backend = LinuxBackend::new()?;
        let controls = identify(&backend, pid, kind, &toolkit, deadline)?;
        Ok(Self {
            backend,
            pid,
            kind,
            toolkit,
            controls,
        })
    }
    pub(crate) fn title(&self) -> &str {
        &self.controls.title
    }
    pub(crate) fn revalidate(&self, deadline: Instant) -> Result<()> {
        if identify(&self.backend, self.pid, self.kind, &self.toolkit, deadline)? != self.controls {
            return Err("the observed file chooser or its controls changed".into());
        }
        Ok(())
    }
    pub(crate) fn gone(&self, deadline: Instant) -> Result<bool> {
        let c = &self.controls;
        if self.backend.process_id(&c.app.bus_name) != Some(self.pid) {
            return Err("GTK application disconnected".into());
        }
        let mut visible = false;
        for top in self.backend.children(&c.app, deadline)? {
            if top.bus_name != c.app.bus_name {
                return Err("foreign GTK window".into());
            }
            let proxy = self.backend.accessible(&top)?;
            let states = zbus::block_on(proxy.get_state())?;
            if !self.toolkit.shows(states) {
                continue;
            }
            if top == c.window {
                visible = true;
            } else if states.contains(State::Modal) {
                return Err("a different modal or confirmation appeared".into());
            }
        }
        Ok(!visible)
    }
    pub(crate) fn fill(&self, path: &str, deadline: Instant) -> Result<()> {
        let (entry, _) = self
            .controls
            .name
            .as_ref()
            .ok_or("missing observed filename entry")?;
        self.backend.set_text(entry, path, deadline)?;
        self.verify_text(path)
    }
    pub(crate) fn verify_text(&self, path: &str) -> Result<()> {
        let (entry, _) = self
            .controls
            .name
            .as_ref()
            .ok_or("missing observed filename entry")?;
        self.verify_entry(entry, path)
    }
    fn verify_entry(&self, entry: &NativeRef, path: &str) -> Result<()> {
        let proxy = zbus::block_on(
            TextProxy::builder(self.backend.connection.inner())
                .destination(entry.bus_name.as_str())?
                .path(entry.object_path.as_str())?
                .build(),
        )?;
        let count = i32::try_from(path.chars().count())?;
        if zbus::block_on(proxy.character_count())? != count
            || zbus::block_on(proxy.get_text(0, count))? != path
        {
            return Err("GTK filename differs from the authorized path".into());
        }
        Ok(())
    }
    pub(crate) fn location(&self, deadline: Instant) -> Result<Option<Location>> {
        // The GTK browse header contains a named stack and a Location Layer.
        // File rows, the search field and the Connect to Server entry are
        // outside that layer. Never descend into lists, tables or scroll panes.
        let mut panels = vec![(self.controls.widget.clone(), false)];
        let mut locations = Vec::new();
        let mut visited = 0;
        while let Some((panel, in_header_stack)) = panels.pop() {
            for child in self.backend.children(&panel, deadline)? {
                visited += 1;
                if visited > 128 || child.bus_name != self.controls.widget.bus_name {
                    return Err("incomplete location hierarchy".into());
                }
                let proxy = self.backend.accessible(&child)?;
                let role = zbus::block_on(proxy.get_role())?;
                if !shown(zbus::block_on(proxy.get_state())?) {
                    continue;
                }
                match role {
                    Role::Filler
                        if in_header_stack && zbus::block_on(proxy.name())? == "Location Layer" =>
                    {
                        let entry = unique(
                            self.backend.children(&child, deadline)?,
                            "ambiguous GTK location field",
                        )?;
                        if entry.bus_name != child.bus_name {
                            return Err("foreign location field".into());
                        }
                        let field = self.backend.accessible(&entry)?;
                        let state = zbus::block_on(field.get_state())?;
                        if zbus::block_on(field.get_role())? != Role::Text
                            || !shown(state)
                            || !enabled(state)
                            || !state.contains(State::Editable)
                            || !state.contains(State::Focused)
                            || !state.contains(State::SingleLine)
                        {
                            return Err("the GTK location field is not focused and editable".into());
                        }
                        locations.push(Location {
                            layer: child,
                            entry,
                        });
                    }
                    Role::Filler | Role::Panel | Role::SplitPane => {
                        let stack = role == Role::Panel
                            && zbus::block_on(proxy.name())? == "Browse Header Stack";
                        panels.push((child, stack));
                    }
                    _ => (),
                }
            }
        }
        match locations.len() {
            0 => Ok(None),
            1 => Ok(locations.pop()),
            _ => Err("ambiguous GTK location layers".into()),
        }
    }
    pub(crate) fn show_location(&self, deadline: Instant) -> Result<()> {
        self.backend
            .show_file_location(&self.controls.widget, deadline)?;
        Ok(())
    }
    pub(crate) fn fill_location(
        &self,
        location: &Location,
        path: &str,
        deadline: Instant,
    ) -> Result<()> {
        if self.location(deadline)?.as_ref() != Some(location) {
            return Err("the GTK location field changed".into());
        }
        self.backend.set_text(&location.entry, path, deadline)?;
        self.verify_entry(&location.entry, path)
    }
    pub(crate) fn accept_location(
        &self,
        location: &Location,
        path: &str,
        deadline: Instant,
    ) -> Result<bool> {
        if self.location(deadline)?.as_ref() != Some(location) {
            return Err("the GTK location field changed".into());
        }
        self.verify_entry(&location.entry, path)?;
        let state = zbus::block_on(self.backend.accessible(&self.controls.accept)?.get_state())?;
        if !shown(state) {
            return Err("the observed Open response disappeared".into());
        }
        if !enabled(state) {
            return Ok(false);
        }
        self.backend
            .invoke_canonical(&self.controls.accept, "click", deadline)?;
        Ok(true)
    }
    pub(crate) fn respond(&self, deadline: Instant) -> Result<()> {
        let button = match self.kind {
            Kind::Open => &self.controls.cancel,
            Kind::Save | Kind::Directory => &self.controls.accept,
        };
        self.backend.invoke_canonical(button, "click", deadline)?;
        Ok(())
    }
}

fn identify(
    backend: &LinuxBackend,
    pid: u32,
    kind: Kind,
    toolkit: &Toolkit,
    deadline: Instant,
) -> Result<Controls> {
    let mut windows = Vec::new();
    for app in backend.children(&LinuxBackend::root_ref(), deadline)? {
        if backend.process_id(&app.bus_name) != Some(pid) {
            continue;
        }
        let info = backend.application_info(&app, deadline)?;
        let version = info["toolkit_version"].as_str().unwrap_or("");
        let app_matches = match toolkit {
            Toolkit::Gtk3 => info["toolkit_name"] == "gtk" && version.starts_with("3."),
            Toolkit::X11Gtk4(class) => {
                info["toolkit_name"] == "GTK"
                    && version.starts_with("4.")
                    && info["name"].as_str() == Some(class)
            }
        };
        if !app_matches {
            continue;
        }
        for top in backend.children(&app, deadline)? {
            if top.bus_name != app.bus_name {
                return Err("foreign GTK application window".into());
            }
            let proxy = backend.accessible(&top)?;
            let states = zbus::block_on(proxy.get_state())?;
            if !toolkit.shows(states) || !states.contains(State::Modal) {
                continue;
            }
            let role = zbus::block_on(proxy.get_role())?;
            let dialog = match toolkit {
                Toolkit::Gtk3 => role == Role::FileChooser && states.contains(State::Active),
                // Repeated Chromium saves can expose the top-level GtkWindow
                // as a filler. GTK4 reports the Window layer only for GtkWindow.
                // The X11 owner/class/title and the chooser, filename entry and
                // response controls are independently pinned and revalidated.
                Toolkit::X11Gtk4(_) => {
                    role == Role::Dialog
                        || (kind == Kind::Save
                            && role == Role::Filler
                            && zbus::block_on(backend.component_proxy(&top)?.get_layer())?
                                == atspi::Layer::Window)
                }
            };
            if !toolkit.enables(states) || !dialog {
                return Err("another modal or confirmation belongs to the application".into());
            }
            windows.push((app.clone(), top.clone(), zbus::block_on(proxy.name())?));
        }
    }
    let (app, window, title) = unique(windows, "expected one active GTK file chooser")?;
    if kind == Kind::Directory && !aad_platform::picker::directory_title(&title) {
        return Err("expected a Chromium directory selection dialog".into());
    }
    // Chromium directory pickers also expose Select and show_location. Bind
    // the browser's file-specific operation title as well as the GTK controls.
    if kind == Kind::Open
        && !matches!(
            title.as_str(),
            "Open File"
                | "Open Files"
                | "打开文件"
                | "Select a file this site can read"
                | "Select files this site can read"
                | "选择允许此网站读取的文件"
        )
    {
        return Err("expected a Chromium file-open dialog".into());
    }
    let (mut widgets, mut accept, mut cancel, mut entries) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut panels = vec![(window.clone(), false)];
    let mut visited = 0;
    while let Some((panel, inside_chooser)) = panels.pop() {
        for child in backend.children(&panel, deadline)? {
            visited += 1;
            if visited > 128 || child.bus_name != window.bus_name {
                return Err("incomplete GTK chooser hierarchy".into());
            }
            let proxy = backend.accessible(&child)?;
            let states = zbus::block_on(proxy.get_state())?;
            if !toolkit.shows(states) {
                continue;
            }
            let role = zbus::block_on(proxy.get_role())?;
            let widget = match toolkit {
                Toolkit::Gtk3 => role == Role::FileChooser,
                Toolkit::X11Gtk4(_) => {
                    role == Role::Filler && zbus::block_on(proxy.name())? == "GtkFileChooserWidget"
                }
            };
            match role {
                _ if widget && !inside_chooser => {
                    widgets.push(child.clone());
                    // With a traditional action area the filename entry is
                    // inside the chooser. Header-bar dialogs place it outside.
                    if kind == Kind::Save {
                        panels.push((child, true));
                    }
                }
                Role::Panel | Role::Filler => panels.push((child, inside_chooser)),
                Role::Button if !inside_chooser => {
                    let name = zbus::block_on(proxy.name())?;
                    match name.as_str() {
                        "_Cancel Cancel" | "Cancel" | "取消" if toolkit.enables(states) => {
                            cancel.push(child)
                        }
                        "_Save Save" | "Save" | "保存"
                            if kind == Kind::Save && toolkit.enables(states) =>
                        {
                            accept.push(child)
                        }
                        "Select" | "Open" | "选择" | "打开" if kind != Kind::Save => {
                            accept.push(child)
                        }
                        "_Save Save" | "_Open Open" | "Save" | "保存" | "Select" | "Open"
                        | "选择" | "打开" => {
                            return Err("unexpected chooser response button".into())
                        }
                        _ => (),
                    }
                }
                Role::Text
                    if states.contains(State::Editable) && states.contains(State::Focused) =>
                {
                    if kind != Kind::Save
                        || !toolkit.enables(states)
                        || states.contains(State::MultiLine)
                        || (matches!(toolkit, Toolkit::Gtk3) && !states.contains(State::SingleLine))
                    {
                        return Err("unexpected or unfocused chooser entry".into());
                    }
                    let labels = zbus::block_on(proxy.get_relation_set())?
                        .into_iter()
                        .filter(|(relation, _)| *relation == RelationType::LabelledBy)
                        .flat_map(|(_, targets)| targets)
                        .map(LinuxBackend::native)
                        .collect();
                    let label = unique(labels, "filename entry needs a unique label")?;
                    if label.bus_name != child.bus_name {
                        return Err("foreign filename label".into());
                    }
                    let caption = backend.accessible(&label)?;
                    if zbus::block_on(caption.get_role())? != Role::Label
                        || !matches!(
                            zbus::block_on(caption.name())?.as_str(),
                            "Name" | "Name:" | "名称" | "名称：" | "文件名" | "文件名："
                        )
                    {
                        return Err("expected a labelled filename entry".into());
                    }
                    entries.push((child, label));
                }
                _ => (),
            }
        }
    }
    Ok(Controls {
        app,
        window,
        title,
        widget: unique(widgets, "expected one GTK chooser widget")?,
        accept: unique(accept, "missing or ambiguous chooser response")?,
        cancel: unique(cancel, "missing or ambiguous Cancel button")?,
        name: if kind == Kind::Save {
            Some(unique(entries, "missing or ambiguous filename entry")?)
        } else {
            None
        },
    })
}

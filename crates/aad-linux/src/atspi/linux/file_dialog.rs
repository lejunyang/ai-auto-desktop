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
    controls: Controls,
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
        let backend = LinuxBackend::new()?;
        let controls = identify(&backend, pid, kind, deadline)?;
        Ok(Self {
            backend,
            pid,
            kind,
            controls,
        })
    }
    pub(crate) fn title(&self) -> &str {
        &self.controls.title
    }
    pub(crate) fn revalidate(&self, deadline: Instant) -> Result<()> {
        if identify(&self.backend, self.pid, self.kind, deadline)? != self.controls {
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
            if !shown(states) {
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
        let proxy = zbus::block_on(
            TextProxy::builder(self.backend.connection.inner())
                .destination(entry.bus_name.as_str())?
                .path(entry.object_path.as_str())?
                .build(),
        )?;
        if zbus::block_on(proxy.get_text(0, -1))? != path {
            return Err("GTK filename differs from the authorized path".into());
        }
        Ok(())
    }
    pub(crate) fn respond(&self, deadline: Instant) -> Result<()> {
        let button = match self.kind {
            Kind::Open => &self.controls.cancel,
            Kind::Save => &self.controls.accept,
        };
        self.backend.invoke_canonical(button, "click", deadline)?;
        Ok(())
    }
}

fn identify(backend: &LinuxBackend, pid: u32, kind: Kind, deadline: Instant) -> Result<Controls> {
    let mut windows = Vec::new();
    for app in backend.children(&LinuxBackend::root_ref(), deadline)? {
        if backend.process_id(&app.bus_name) != Some(pid) {
            continue;
        }
        let info = backend.application_info(&app, deadline)?;
        if info["toolkit_name"] != "gtk"
            || !info["toolkit_version"]
                .as_str()
                .is_some_and(|v| v.starts_with("3."))
        {
            continue;
        }
        for top in backend.children(&app, deadline)? {
            if top.bus_name != app.bus_name {
                return Err("foreign GTK application window".into());
            }
            let proxy = backend.accessible(&top)?;
            let states = zbus::block_on(proxy.get_state())?;
            if !shown(states) || !states.contains(State::Modal) {
                continue;
            }
            if !enabled(states)
                || !states.contains(State::Active)
                || zbus::block_on(proxy.get_role())? != Role::FileChooser
            {
                return Err("another modal or confirmation belongs to the application".into());
            }
            windows.push((app.clone(), top.clone(), zbus::block_on(proxy.name())?));
        }
    }
    let (app, window, title) = unique(windows, "expected one active GTK file chooser")?;
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
            if !shown(states) {
                continue;
            }
            match zbus::block_on(proxy.get_role())? {
                Role::Panel | Role::Filler => panels.push((child, inside_chooser)),
                Role::FileChooser if !inside_chooser => {
                    widgets.push(child.clone());
                    // With a traditional action area the filename entry is
                    // inside the chooser. Header-bar dialogs place it outside.
                    if kind == Kind::Save {
                        panels.push((child, true));
                    }
                }
                Role::Button if !inside_chooser => {
                    let name = zbus::block_on(proxy.name())?;
                    match name.as_str() {
                        "Cancel" | "取消" if enabled(states) => cancel.push(child),
                        "Save" | "保存" if kind == Kind::Save && enabled(states) => {
                            accept.push(child)
                        }
                        "Select" | "Open" | "选择" | "打开" if kind == Kind::Open => {
                            accept.push(child)
                        }
                        "Save" | "保存" | "Select" | "Open" | "选择" | "打开" => {
                            return Err("unexpected chooser response button".into())
                        }
                        _ => (),
                    }
                }
                Role::Text if states.contains(State::Editable) => {
                    if kind != Kind::Save
                        || !enabled(states)
                        || !states.contains(State::Focused)
                        || !states.contains(State::SingleLine)
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

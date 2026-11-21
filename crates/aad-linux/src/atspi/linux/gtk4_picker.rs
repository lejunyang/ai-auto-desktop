use super::{LinuxBackend, NativeRef};
use atspi::{Role, State};
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(PartialEq)]
struct Identity {
    dialog: NativeRef,
    chooser: NativeRef,
    open: NativeRef,
}

pub(crate) struct Gtk4Picker {
    backend: LinuxBackend,
    pid: u32,
    class: String,
    title: String,
    identity: Identity,
}

impl Gtk4Picker {
    pub(crate) fn observe(
        pid: u32,
        class: String,
        title: String,
        deadline: Instant,
    ) -> Result<Self> {
        let backend = LinuxBackend::new()?;
        let identity = identify(&backend, pid, &class, &title, deadline)?;
        Ok(Self {
            backend,
            pid,
            class,
            title,
            identity,
        })
    }

    pub(crate) fn revalidate(&self, deadline: Instant) -> Result<()> {
        if identify(&self.backend, self.pid, &self.class, &self.title, deadline)? != self.identity {
            return Err("the observed GTK 4 chooser or its Open action changed".into());
        }
        Ok(())
    }

    pub(crate) fn matches_window(&self, class: &[u8], title: &[u8]) -> bool {
        let mut fields = class.split(|&b| b == 0);
        fields.next() == Some(self.class.as_bytes())
            && fields.next() == Some(self.class.as_bytes())
            && fields.next() == Some(&[][..])
            && fields.next().is_none()
            && title == self.title.as_bytes()
    }
}

fn identify(
    backend: &LinuxBackend,
    pid: u32,
    class: &str,
    title: &str,
    deadline: Instant,
) -> Result<Identity> {
    let mut matches = Vec::new();
    for app in backend.children(&LinuxBackend::root_ref(), deadline)? {
        if backend.process_id(&app.bus_name) != Some(pid) {
            continue;
        }
        let info = backend.application_info(&app, deadline)?;
        if info["toolkit_name"] != "GTK"
            || !info["toolkit_version"]
                .as_str()
                .is_some_and(|v| v.starts_with("4."))
            || info["name"] != class
        {
            continue;
        }
        for top in backend.children(&app, deadline)? {
            if top.bus_name != app.bus_name {
                return Err("foreign GTK application child".into());
            }
            let window = backend.accessible(&top)?;
            if zbus::block_on(window.get_role())? != Role::Dialog {
                continue;
            }
            let states = zbus::block_on(window.get_state())?;
            if !states.contains(State::Visible) {
                continue;
            }
            if !states.contains(State::Modal) || zbus::block_on(window.name())? != title {
                return Err("another visible GTK dialog belongs to the browser".into());
            }
            matches.push(top);
        }
    }
    let [dialog] = matches.as_slice() else {
        return Err("expected one browser-owned GTK 4 modal".into());
    };
    // Only inspect structural panels of the dialog. File names and buttons
    // nested inside the chooser cannot impersonate the dialog's response button.
    let mut panels = vec![dialog.clone()];
    let (mut choosers, mut buttons) = (Vec::new(), Vec::new());
    let mut visited = 0;
    while let Some(panel) = panels.pop() {
        for child in backend.children(&panel, deadline)? {
            visited += 1;
            if visited > 64 || child.bus_name != dialog.bus_name {
                return Err("incomplete GTK dialog structure".into());
            }
            let proxy = backend.accessible(&child)?;
            match zbus::block_on(proxy.get_role())? {
                Role::Panel => panels.push(child),
                Role::Filler if zbus::block_on(proxy.name())? == "GtkFileChooserWidget" => {
                    choosers.push(child)
                }
                Role::Button
                    if matches!(
                        zbus::block_on(proxy.name())?.as_str(),
                        "_Open Open" | "Open"
                    ) =>
                {
                    buttons.push(child)
                }
                _ => (),
            }
        }
    }
    let ([chooser], [open]) = (choosers.as_slice(), buttons.as_slice()) else {
        return Err("expected a GTK file chooser and its unique Open response".into());
    };
    Ok(Identity {
        dialog: dialog.clone(),
        chooser: chooser.clone(),
        open: open.clone(),
    })
}

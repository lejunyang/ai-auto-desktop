use super::{AtspiBackend, LinuxBackend, NativeRef, TextProxy};
use aad_platform::picker::Kind;
use atspi::{RelationType, Role, State, StateSet};
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(PartialEq)]
struct Identity {
    app: NativeRef,
    window: NativeRef,
    title: String,
    input: NativeRef,
    combo: NativeRef,
    layout: Layout,
    accept: NativeRef,
    cancel: NativeRef,
}

#[derive(PartialEq)]
enum Layout {
    File {
        label: NativeRef,
        filter: NativeRef,
    },
    Directory {
        tree: NativeRef,
        shell: Option<NativeRef>,
    },
}

pub(crate) struct KdeDialog {
    backend: LinuxBackend,
    pid: u32,
    kind: Kind,
    identity: Identity,
}

fn single<T>(items: Vec<T>, error: &'static str) -> Result<T> {
    let [item]: [T; 1] = items.try_into().map_err(|_| error)?;
    Ok(item)
}

fn shown(s: StateSet) -> bool {
    s.contains(State::Visible) && s.contains(State::Showing)
}

fn enabled(s: StateSet) -> bool {
    s.contains(State::Enabled) && s.contains(State::Sensitive)
}

impl KdeDialog {
    pub(crate) fn observe(pid: u32, kind: Kind, deadline: Instant) -> Result<Self> {
        let backend = LinuxBackend::new()?;
        let identity = identify(&backend, pid, kind, deadline)?;
        Ok(Self {
            backend,
            pid,
            kind,
            identity,
        })
    }
    pub(crate) fn revalidate(&self, deadline: Instant) -> Result<()> {
        if self.identity != identify(&self.backend, self.pid, self.kind, deadline)? {
            return Err("KDE file dialog or controls changed".into());
        }
        Ok(())
    }
    pub(crate) fn fill(&self, text: &str, deadline: Instant) -> Result<()> {
        // KFileWidget interprets URL, wildcard and quoted-selection syntax.
        // Limit native input to unambiguous local filename components.
        if !text.split('/').all(|name| {
            name.trim() == name
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || " ._-".contains(c))
        }) {
            return Err("unsupported characters in KDE native file path".into());
        }
        self.backend
            .set_text(&self.identity.input, text, deadline)?;
        self.verify(text)
    }
    pub(crate) fn verify(&self, text: &str) -> Result<()> {
        let input = &self.identity.input;
        let proxy = zbus::block_on(
            TextProxy::builder(self.backend.connection.inner())
                .destination(input.bus_name.as_str())?
                .path(input.object_path.as_str())?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build(),
        )?;
        let count = i32::try_from(text.chars().count())?;
        if zbus::block_on(proxy.character_count())? != count
            || zbus::block_on(proxy.get_text(0, count))? != text
        {
            return Err("KDE filename differs from the authorized path".into());
        }
        Ok(())
    }
    pub(crate) fn respond(&self, accept: bool, deadline: Instant) -> Result<()> {
        let button = if accept {
            &self.identity.accept
        } else {
            &self.identity.cancel
        };
        let states = zbus::block_on(self.backend.accessible(button)?.get_state())?;
        if !shown(states) || !enabled(states) {
            return Err("the observed KDE response is unavailable".into());
        }
        self.backend.invoke_canonical(button, "Press", deadline)?;
        Ok(())
    }
    pub(crate) fn gone(&self, deadline: Instant) -> Result<bool> {
        let app = &self.identity.app;
        if self.backend.process_id(&app.bus_name) != Some(self.pid) {
            return Err("KDE portal accessibility owner changed".into());
        }
        let mut present = false;
        for top in self.backend.children(app, deadline)? {
            if top.bus_name != app.bus_name {
                return Err("foreign KDE window".into());
            }
            if shown(zbus::block_on(self.backend.accessible(&top)?.get_state())?) {
                let shell = matches!(&self.identity.layout, Layout::Directory { shell: Some(id), .. } if id == &top);
                if top != self.identity.window && !shell {
                    return Err("another KDE window or confirmation appeared".into());
                }
                present = true;
            }
        }
        Ok(!present)
    }
}

fn identify(b: &LinuxBackend, pid: u32, kind: Kind, deadline: Instant) -> Result<Identity> {
    let directory_title: Option<fn(&str) -> bool> = match kind {
        Kind::Directory => Some(aad_platform::picker::directory_title),
        Kind::DirectoryUpload => Some(aad_platform::picker::directory_upload_title),
        _ => None,
    };
    let mut apps = Vec::new();
    for app in b.children(&LinuxBackend::root_ref(), deadline)? {
        if b.process_id(&app.bus_name) == Some(pid) {
            let info = b.application_info(&app, deadline)?;
            if info["toolkit_name"] == "Qt" && info["name"] == "xdg-desktop-portal-kde" {
                apps.push(app);
            }
        }
    }
    let app = single(apps, "expected the KDE portal's Qt application")?;
    let mut dialogs = Vec::new();
    let mut shell = None;
    for top in b.children(&app, deadline)? {
        if top.bus_name != app.bus_name {
            return Err("foreign KDE window".into());
        }
        let proxy = b.accessible(&top)?;
        let states = zbus::block_on(proxy.get_state())?;
        if shown(states) {
            let role = zbus::block_on(proxy.get_role())?;
            if directory_title.is_some()
                && role == Role::Dialog
                && !states.contains(State::Active)
                && b.children(&top, deadline)?.is_empty()
            {
                let title = zbus::block_on(proxy.name())?;
                if !directory_title.is_some_and(|matches_title| matches_title(&title)) {
                    return Err("unexpected inactive KDE dialog".into());
                }
                if shell.replace(top).is_some() {
                    return Err("ambiguous KDE directory shells".into());
                }
                continue;
            }
            if role != Role::Dialog || !enabled(states) || !states.contains(State::Active) {
                return Err(format!(
                    "expected a focused KDE dialog, observed {role:?} with {states:?}"
                )
                .into());
            }
            let title = zbus::block_on(proxy.name())?;
            dialogs.push((top, title));
        }
    }
    let (window, title) = single(dialogs, "expected one active KDE file dialog")?;
    if let Some(matches_title) = directory_title {
        if !matches_title(&title) {
            return Err("unexpected Chromium directory chooser operation".into());
        }
        return directory_controls(b, app, window, title, shell, kind, deadline);
    }
    if title.is_empty()
        || (kind == Kind::Open
            && !matches!(
                title.as_str(),
                "Open File"
                    | "Open Files"
                    | "打开文件"
                    | "Select a file this site can read"
                    | "Select files this site can read"
                    | "选择允许此网站读取的文件"
            ))
    {
        return Err("unexpected KDE file dialog operation".into());
    }
    let (mut fields, mut filters, mut accept, mut cancel) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut visited = 0;
    for group in b.children(&window, deadline)? {
        if group.bus_name != app.bus_name {
            return Err("foreign Qt layout".into());
        }
        let layout = b.accessible(&group)?;
        let area = zbus::block_on(layout.get_role())?;
        if !shown(zbus::block_on(layout.get_state())?)
            || !matches!(area, Role::Filler | Role::Panel)
        {
            continue;
        }
        for control in b.children(&group, deadline)? {
            visited += 1;
            if visited > 96 || control.bus_name != app.bus_name {
                return Err("incomplete KDE dialog".into());
            }
            let proxy = b.accessible(&control)?;
            let states = zbus::block_on(proxy.get_state())?;
            if !shown(states) {
                continue;
            }
            match (area, zbus::block_on(proxy.get_role())?) {
                (Role::Panel, Role::Button) => match zbus::block_on(proxy.name())?.as_str() {
                    "Cancel" | "取消" if enabled(states) => cancel.push(control),
                    "Save" | "保存" if kind == Kind::Save && enabled(states) => {
                        accept.push(control)
                    }
                    "Open" | "打开" | "Select" | "选择" if kind == Kind::Open => {
                        accept.push(control)
                    }
                    _ => return Err("unexpected KDE dialog response".into()),
                },
                (Role::Filler, Role::CheckBox)
                    if kind == Kind::Save && states.contains(State::Checked) =>
                {
                    return Err("KDE automatic filename extension must be unchecked".into());
                }
                (Role::Filler, Role::ComboBox) => {
                    let labels = zbus::block_on(proxy.get_relation_set())?
                        .into_iter()
                        .filter(|(rel, _)| *rel == RelationType::LabelledBy)
                        .flat_map(|(_, refs)| refs)
                        .map(LinuxBackend::native)
                        .collect();
                    let label = single(labels, "missing KDE field label")?;
                    if label.bus_name != app.bus_name {
                        return Err("foreign KDE field label".into());
                    }
                    let caption = b.accessible(&label)?;
                    if zbus::block_on(caption.get_role())? != Role::Label {
                        return Err("unexpected KDE field label".into());
                    }
                    match zbus::block_on(caption.name())?.as_str() {
                        "Filter:" | "Filter" | "筛选(F)：" => filters.push(control),
                        "Name:" | "Name" | "名称(N)："
                            if enabled(states) && states.contains(State::Focused) =>
                        {
                            let mut inputs = Vec::new();
                            for child in b.children(&control, deadline)? {
                                if child.bus_name != app.bus_name {
                                    return Err("foreign Qt entry".into());
                                }
                                let input = b.accessible(&child)?;
                                let states = zbus::block_on(input.get_state())?;
                                if zbus::block_on(input.get_role())? == Role::Text
                                    && shown(states)
                                    && enabled(states)
                                    && states.contains(State::Editable)
                                    && states.contains(State::Focused)
                                {
                                    inputs.push(child);
                                }
                            }
                            fields.push((
                                control,
                                label,
                                single(inputs, "expected one focused KDE filename entry")?,
                            ));
                        }
                        _ => return Err("unexpected or unfocused KDE file field".into()),
                    }
                }
                _ => (),
            }
        }
    }
    let (combo, label, input) = single(fields, "ambiguous KDE filename field")?;
    Ok(Identity {
        app,
        window,
        title,
        input,
        combo,
        layout: Layout::File {
            label,
            filter: single(filters, "expected one KDE file filter")?,
        },
        accept: single(accept, "ambiguous KDE response")?,
        cancel: single(cancel, "ambiguous KDE Cancel response")?,
    })
}

fn directory_controls(
    b: &LinuxBackend,
    app: NativeRef,
    window: NativeRef,
    title: String,
    shell: Option<NativeRef>,
    kind: Kind,
    deadline: Instant,
) -> Result<Identity> {
    // KDirSelectDialog has a directory tree and a single path combo in its
    // frame. Do not enumerate tree rows or select a folder by basename.
    let children = |node: &NativeRef| -> Result<Vec<NativeRef>> {
        let children = b.children(node, deadline)?;
        if children.len() > 32 || children.iter().any(|c| c.bus_name != app.bus_name) {
            return Err("unexpected KDE directory dialog hierarchy".into());
        }
        Ok(children)
    };
    let (mut paths, mut trees, mut accepts, mut cancels) = (vec![], vec![], vec![], vec![]);
    for group in children(&window)? {
        let group_proxy = b.accessible(&group)?;
        let group_role = zbus::block_on(group_proxy.get_role())?;
        if !shown(zbus::block_on(group_proxy.get_state())?) {
            continue;
        }
        for control in children(&group)? {
            let proxy = b.accessible(&control)?;
            let state = zbus::block_on(proxy.get_state())?;
            if !shown(state) || !enabled(state) {
                continue;
            }
            match (group_role, zbus::block_on(proxy.get_role())?) {
                (Role::Frame, Role::Tree) => trees.push(control),
                (Role::Frame, Role::ComboBox) => {
                    let mut entries = Vec::new();
                    for child in children(&control)? {
                        let entry = b.accessible(&child)?;
                        let state = zbus::block_on(entry.get_state())?;
                        if zbus::block_on(entry.get_role())? == Role::Text
                            && shown(state)
                            && enabled(state)
                            && state.contains(State::Editable)
                        {
                            entries.push(child);
                        }
                    }
                    paths.push((
                        control,
                        single(entries, "expected one editable KDE directory path")?,
                    ));
                }
                (Role::Panel, Role::Button) => match zbus::block_on(proxy.name())?.as_str() {
                    "OK" | "确定"
                        if kind == Kind::Directory && state.contains(State::IsDefault) =>
                    {
                        accepts.push(control)
                    }
                    "Upload" | "上传"
                        if kind == Kind::DirectoryUpload && state.contains(State::IsDefault) =>
                    {
                        accepts.push(control)
                    }
                    "Cancel" | "取消" => cancels.push(control),
                    "New Folder..." | "新建文件夹..." | "新建文件夹…" => (),
                    _ => return Err("unexpected KDE directory response".into()),
                },
                _ => (),
            }
        }
    }
    let (combo, input) = single(paths, "expected a unique KDE directory field")?;
    Ok(Identity {
        app,
        window,
        title,
        combo,
        input,
        layout: Layout::Directory {
            tree: single(trees, "expected a directory tree")?,
            shell,
        },
        accept: single(accepts, "expected a default directory confirmation")?,
        cancel: single(cancels, "expected a directory cancel button")?,
    })
}

use super::Result;
use std::time::{Duration, Instant};
use zbus::blocking::{connection, Connection, Proxy};

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const GTK: &str = "org.freedesktop.impl.portal.desktop.gtk";
const KDE: &str = "org.freedesktop.impl.portal.desktop.kde";
const ROOT: &str = "/org/freedesktop/portal/desktop/request";

#[derive(PartialEq)]
struct Request {
    frontend: String,
    backend: String,
    path: String,
}

pub(super) struct FilePortal {
    bus: Connection,
    browser_pid: u32,
    pub(super) picker_pid: u32,
    request: Request,
    service: &'static str,
}

impl FilePortal {
    pub(super) fn for_browser(browser_pid: u32, deadline: Instant) -> Result<Self> {
        Self::bind(browser_pid, None, GTK, deadline)
    }
    pub(super) fn kde(browser_pid: u32, deadline: Instant) -> Result<Self> {
        Self::bind(browser_pid, None, KDE, deadline)
    }
    pub(super) fn observe(browser_pid: u32, picker_pid: u32, deadline: Instant) -> Result<Self> {
        Self::bind(browser_pid, Some(picker_pid), GTK, deadline)
    }
    fn bind(
        browser_pid: u32,
        expected_pid: Option<u32>,
        service: &'static str,
        deadline: Instant,
    ) -> Result<Self> {
        let bus = connection::Builder::session()?
            .method_timeout(Duration::from_secs(2))
            .build()?;
        let dbus = Proxy::new(
            &bus,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )?;
        let picker_pid: u32 = dbus.call("GetConnectionUnixProcessID", &(service,))?;
        if expected_pid.is_some_and(|pid| pid != picker_pid) {
            return Err("unexpected GTK portal window owner".into());
        }
        let request = identify(&bus, browser_pid, picker_pid, service, deadline)?;
        Ok(Self {
            bus,
            browser_pid,
            picker_pid,
            request,
            service,
        })
    }

    pub(super) fn revalidate(&self, deadline: Instant) -> Result<()> {
        if identify(
            &self.bus,
            self.browser_pid,
            self.picker_pid,
            self.service,
            deadline,
        )? != self.request
        {
            return Err("the active desktop portal request changed".into());
        }
        Ok(())
    }

    pub(super) fn finished(&self, deadline: Instant) -> Result<bool> {
        // Introspect the original unique bus owners. A service disconnect or
        // observation failure is not evidence that the dialog closed normally.
        for (owner, interface) in [
            (&self.request.frontend, "org.freedesktop.portal.Request"),
            (&self.request.backend, "org.freedesktop.impl.portal.Request"),
        ] {
            if owner == &self.request.backend && self.service == KDE {
                // Qt's virtual Request object does not answer Introspect. Its
                // parent publishes the exact request until the response.
                let (parent, token) = self
                    .request
                    .path
                    .rsplit_once('/')
                    .ok_or("invalid request")?;
                return match inspect(&self.bus, owner, parent, deadline) {
                    Ok(node) => Ok(!node.nodes().iter().any(|n| n.name() == Some(token))),
                    // Qt also removes a sender's parent after its last request.
                    Err(e)
                        if matches!(e.downcast_ref::<zbus::Error>(),
                        Some(zbus::Error::MethodError(name, ..))
                            if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject") =>
                    {
                        Ok(true)
                    }
                    Err(e) => Err(e),
                };
            }
            if inspect(&self.bus, owner, &self.request.path, deadline)?
                .interfaces()
                .iter()
                .any(|i| i.name().as_str() == interface)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn inspect(
    bus: &Connection,
    dest: &str,
    path: &str,
    deadline: Instant,
) -> Result<zbus_xml::Node<'static>> {
    if Instant::now() >= deadline {
        return Err("portal observation timed out".into());
    }
    let proxy = Proxy::new(bus, dest, path, "org.freedesktop.DBus.Introspectable")?;
    let xml: String = proxy.call("Introspect", &())?;
    if xml.len() > 65536 {
        return Err("portal introspection exceeds the observation limit".into());
    }
    Ok(zbus_xml::Node::from_reader(xml.as_bytes())?)
}

fn only_child(bus: &Connection, dest: &str, parent: &str, deadline: Instant) -> Result<String> {
    let node = inspect(bus, dest, parent, deadline)?;
    let [child] = node.nodes() else {
        return Err("expected one active file portal request".into());
    };
    let name = child
        .name()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 255
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .ok_or("invalid portal request path")?;
    Ok(name.into())
}

fn identify(
    bus: &Connection,
    browser_pid: u32,
    picker_pid: u32,
    service: &str,
    deadline: Instant,
) -> Result<Request> {
    let dbus = Proxy::new(
        bus,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?;
    let frontend: String = dbus.call("GetNameOwner", &(DESKTOP,))?;
    let backend: String = dbus.call("GetNameOwner", &(service,))?;
    let owner_pid: u32 = dbus.call("GetConnectionUnixProcessID", &(&backend,))?;
    if owner_pid != picker_pid {
        return Err("the file portal process changed".into());
    }
    let sender = only_child(bus, &backend, ROOT, deadline)?;
    let serial = sender
        .strip_prefix("1_")
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .ok_or("unrecognized portal sender identity")?;
    let caller_pid: u32 = dbus.call("GetConnectionUnixProcessID", &(format!(":1.{serial}"),))?;
    if caller_pid != browser_pid {
        return Err("the portal request belongs to a different process".into());
    }
    let parent = format!("{ROOT}/{sender}");
    let token = only_child(bus, &backend, &parent, deadline)?;
    let path = format!("{parent}/{token}");
    if only_child(bus, &frontend, ROOT, deadline)? != sender
        || only_child(bus, &frontend, &parent, deadline)? != token
    {
        return Err("portal services disagree on the unique live request".into());
    }
    for (dest, interface) in [
        (&frontend, "org.freedesktop.portal.Request"),
        (&backend, "org.freedesktop.impl.portal.Request"),
    ] {
        if dest == &backend && service == KDE {
            // only_child above resolved this exact handle from the KDE owner.
            continue;
        }
        if !inspect(bus, dest, &path, deadline)?
            .interfaces()
            .iter()
            .any(|i| i.name().as_str() == interface)
        {
            return Err("missing live file portal request interface".into());
        }
    }
    Ok(Request {
        frontend,
        backend,
        path,
    })
}

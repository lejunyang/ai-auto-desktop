use super::*;
use futures_core::Stream;
use std::future::{poll_fn, Future};
use std::os::fd::OwnedFd;
use std::pin::{pin, Pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream};

const NAME: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
const DEVICES: u32 = 3;

struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
fn wait<F: Future>(future: F, deadline: Instant, cancel: &AtomicBool) -> Result<F::Output> {
    let mut future = pin!(future);
    let wake = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut cx = Context::from_waker(&wake);
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(PortalError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(PortalError::Deadline);
        }
        if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
            if cancel.load(Ordering::SeqCst) {
                return Err(PortalError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(PortalError::Deadline);
            }
            return Ok(result);
        }
        std::thread::park_timeout(Duration::from_millis(5));
    }
}
fn poll_message(stream: &mut MessageStream) -> Poll<Option<zbus::Result<zbus::Message>>> {
    let wake = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    Pin::new(stream).poll_next(&mut Context::from_waker(&wake))
}
fn response(stream: &mut MessageStream, deadline: Instant, cancel: &AtomicBool) -> Result<Dict> {
    let message = wait(
        poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)),
        deadline,
        cancel,
    )?
    .ok_or(PortalError::Transport)?
    .map_err(|_| PortalError::Transport)?;
    let (code, result): (u32, Dict) = message
        .body()
        .deserialize()
        .map_err(|_| PortalError::InvalidResponse)?;
    match code {
        0 => Ok(result),
        1 => Err(PortalError::Denied),
        2 => Err(PortalError::Denied),
        _ => Err(PortalError::InvalidResponse),
    }
}

/// Connecting and probing do not create a session or show an authorization UI.
pub struct PortalClient {
    connection: Connection,
    owner: String,
    owner_events: MessageStream,
    sender: String,
}
impl PortalClient {
    pub fn connect(deadline: Instant, cancel: &AtomicBool) -> Result<Self> {
        let connection =
            wait(Connection::session(), deadline, cancel)?.map_err(|_| PortalError::Transport)?;
        let owner_reply = wait(
            connection.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &(NAME,),
            ),
            deadline,
            cancel,
        )?
        .map_err(|_| PortalError::Transport)?;
        let owner: String = owner_reply
            .body()
            .deserialize()
            .map_err(|_| PortalError::InvalidResponse)?;
        if !owner.starts_with(':') {
            return Err(PortalError::InvalidResponse);
        }
        let sender = connection
            .unique_name()
            .ok_or(PortalError::Transport)?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")
            .map_err(|_| PortalError::Transport)?
            .interface("org.freedesktop.DBus")
            .map_err(|_| PortalError::Transport)?
            .member("NameOwnerChanged")
            .map_err(|_| PortalError::Transport)?
            .add_arg(NAME)
            .map_err(|_| PortalError::Transport)?
            .build();
        let owner_events = wait(
            MessageStream::for_match_rule(rule, &connection, Some(16)),
            deadline,
            cancel,
        )?
        .map_err(|_| PortalError::Transport)?;
        let confirmed = wait(
            connection.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &(NAME,),
            ),
            deadline,
            cancel,
        )?
        .map_err(|_| PortalError::Transport)?;
        let confirmed: String = confirmed
            .body()
            .deserialize()
            .map_err(|_| PortalError::InvalidResponse)?;
        if confirmed != owner {
            return Err(PortalError::Revoked);
        }
        Ok(Self {
            connection,
            owner,
            owner_events,
            sender,
        })
    }
    fn subscription(
        &self,
        path: &str,
        interface: &str,
        member: &str,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<MessageStream> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(self.owner.as_str())
            .map_err(|_| PortalError::Transport)?
            .path(path)
            .map_err(|_| PortalError::InvalidResponse)?
            .interface(interface)
            .map_err(|_| PortalError::Transport)?
            .member(member)
            .map_err(|_| PortalError::Transport)?
            .build();
        wait(
            MessageStream::for_match_rule(rule, &self.connection, Some(16)),
            deadline,
            cancel,
        )?
        .map_err(|_| PortalError::Transport)
    }
    fn property(
        &self,
        interface: &str,
        name: &str,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<u32> {
        let message = wait(
            self.connection.call_method(
                Some(self.owner.as_str()),
                PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(interface, name),
            ),
            deadline,
            cancel,
        )?
        .map_err(|_| PortalError::Transport)?;
        let value: OwnedValue = message
            .body()
            .deserialize()
            .map_err(|_| PortalError::InvalidResponse)?;
        value.try_into().map_err(|_| PortalError::InvalidResponse)
    }
    fn close_path(&self, path: &str, interface: &str) {
        // Cleanup is independent of the cancelled operation's token/budget.
        let _ = wait(
            self.connection.call_method(
                Some(self.owner.as_str()),
                path,
                Some(interface),
                "Close",
                &(),
            ),
            Instant::now() + Duration::from_millis(250),
            &AtomicBool::new(false),
        );
    }
    fn request(
        &self,
        route: (&str, &str),
        session: &OwnedObjectPath,
        parent: &str,
        mut options: Dict,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Dict> {
        let (interface, method) = route;
        let token = format!("aad_{}", uuid::Uuid::new_v4().simple());
        let path = format!("{PATH}/request/{}/{}", self.sender, token);
        options.insert("handle_token".into(), owned(Value::from(token))?);
        // Subscribe first: a fast portal can emit Response before its reply.
        let mut events = self.subscription(&path, REQUEST, "Response", deadline, cancel)?;
        let result = (|| {
            let reply = match method {
                "CreateSession" => wait(
                    self.connection.call_method(
                        Some(self.owner.as_str()),
                        PATH,
                        Some(interface),
                        method,
                        &(options,),
                    ),
                    deadline,
                    cancel,
                )?,
                "Start" => wait(
                    self.connection.call_method(
                        Some(self.owner.as_str()),
                        PATH,
                        Some(interface),
                        method,
                        &(session, parent, options),
                    ),
                    deadline,
                    cancel,
                )?,
                _ => wait(
                    self.connection.call_method(
                        Some(self.owner.as_str()),
                        PATH,
                        Some(interface),
                        method,
                        &(session, options),
                    ),
                    deadline,
                    cancel,
                )?,
            }
            .map_err(|_| PortalError::Transport)?;
            let returned: OwnedObjectPath = reply
                .body()
                .deserialize()
                .map_err(|_| PortalError::InvalidResponse)?;
            if returned.as_str() != path {
                return Err(PortalError::InvalidResponse);
            }
            response(&mut events, deadline, cancel)
        })();
        if result.is_err() {
            self.close_path(&path, REQUEST);
        }
        result
    }
    pub fn authorize(
        self,
        _intent: AuthorizationIntent,
        options: AuthorizationOptions,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<AuthorizedSession> {
        if options.parent_window.len() > 512
            || options.parent_window.contains('\0')
            || (!options.parent_window.is_empty() && !options.parent_window.starts_with("wayland:"))
        {
            return Err(PortalError::Unsupported);
        }
        if self.property(REMOTE, "version", deadline, cancel)? < 2
            || self.property(REMOTE, "AvailableDeviceTypes", deadline, cancel)? & DEVICES != DEVICES
            || self.property(SCREEN, "AvailableSourceTypes", deadline, cancel)? & 1 == 0
        {
            return Err(PortalError::Unsupported);
        }
        let token = format!("aad_{}", uuid::Uuid::new_v4().simple());
        let session =
            OwnedObjectPath::try_from(format!("{PATH}/session/{}/{}", self.sender, token))
                .map_err(|_| PortalError::InvalidResponse)?;
        let closed_events =
            self.subscription(session.as_str(), SESSION, "Closed", deadline, cancel)?;
        let result = (|| {
            let create =
                HashMap::from([("session_handle_token".into(), owned(Value::from(token))?)]);
            let mut created = self.request(
                (REMOTE, "CreateSession"),
                &session,
                "",
                create,
                deadline,
                cancel,
            )?;
            let handle = created
                .remove("session_handle")
                .ok_or(PortalError::InvalidResponse)?;
            let handle = String::try_from(handle).map_err(|_| PortalError::InvalidResponse)?;
            if handle != session.as_str() {
                return Err(PortalError::InvalidResponse);
            }
            let devices = HashMap::from([
                ("types".into(), OwnedValue::from(DEVICES)),
                ("persist_mode".into(), OwnedValue::from(0u32)),
            ]);
            self.request(
                (REMOTE, "SelectDevices"),
                &session,
                "",
                devices,
                deadline,
                cancel,
            )?;
            let sources = HashMap::from([
                ("types".into(), OwnedValue::from(1u32)),
                ("multiple".into(), OwnedValue::from(false)),
            ]);
            self.request(
                (SCREEN, "SelectSources"),
                &session,
                "",
                sources,
                deadline,
                cancel,
            )?;
            let mut started = self.request(
                (REMOTE, "Start"),
                &session,
                &options.parent_window,
                Dict::new(),
                deadline,
                cancel,
            )?;
            let granted = started
                .remove("devices")
                .ok_or(PortalError::InvalidResponse)?;
            let granted = u32::try_from(granted).map_err(|_| PortalError::InvalidResponse)?;
            if granted == 0 || granted & !DEVICES != 0 {
                return Err(PortalError::InvalidResponse);
            }
            let streams = super::streams(&mut started)?;
            Ok((granted, streams))
        })();
        match result {
            Ok((devices, streams)) => {
                let mut result = AuthorizedSession {
                    client: self,
                    session,
                    closed_events,
                    devices,
                    streams,
                    generation: SessionGeneration::new(),
                    active: true,
                    eis: None,
                    pipewire: None,
                };
                result.refresh()?;
                Ok(result)
            }
            Err(error) => {
                self.close_path(session.as_str(), SESSION);
                Err(error)
            }
        }
    }
}
impl Drop for PortalClient {
    fn drop(&mut self) {
        // Closing the unique client connection also revokes sessions if an
        // asynchronous Close reply never arrived during bounded cleanup.
        let _ = wait(
            self.connection.clone().close(),
            Instant::now() + Duration::from_millis(250),
            &AtomicBool::new(false),
        );
    }
}

/// An authorization grant is not an input permit. This object exposes no
/// Notify methods or EI event dispatcher, and has no secret-bearing Debug.
pub struct AuthorizedSession {
    client: PortalClient,
    session: OwnedObjectPath,
    closed_events: MessageStream,
    devices: u32,
    streams: Vec<AuthorizedStream>,
    generation: SessionGeneration,
    active: bool,
    eis: Option<OwnedFd>,
    pipewire: Option<OwnedFd>,
}
impl AuthorizedSession {
    pub fn refresh(&mut self) -> Result<()> {
        if !self.active {
            return Err(PortalError::Revoked);
        }
        if !matches!(poll_message(&mut self.closed_events), Poll::Pending)
            || !matches!(poll_message(&mut self.client.owner_events), Poll::Pending)
        {
            self.invalidate();
            self.client.close_path(self.session.as_str(), SESSION);
            return Err(PortalError::Revoked);
        }
        Ok(())
    }
    fn invalidate(&mut self) {
        self.active = false;
        self.generation.invalidate();
        self.eis.take();
        self.pipewire.take();
    }
    pub fn streams(&mut self) -> Result<&[AuthorizedStream]> {
        self.refresh()?;
        Ok(&self.streams)
    }
    pub fn devices(&mut self) -> Result<u32> {
        self.refresh()?;
        Ok(self.devices)
    }
    pub fn generation(&mut self) -> Result<SessionGeneration> {
        self.refresh()?;
        Ok(self.generation)
    }
    pub fn connect_eis(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<()> {
        self.refresh()?;
        if self.eis.is_some() {
            return Err(PortalError::InvalidState);
        }
        let fd = self.open_fd(REMOTE, "ConnectToEIS", deadline, cancel)?;
        self.eis = Some(fd);
        self.refresh()
    }
    pub fn open_pipewire(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<()> {
        self.refresh()?;
        if self.pipewire.is_some() {
            return Err(PortalError::InvalidState);
        }
        let fd = self.open_fd(SCREEN, "OpenPipeWireRemote", deadline, cancel)?;
        self.pipewire = Some(fd);
        self.refresh()
    }
    fn open_fd(
        &mut self,
        interface: &str,
        method: &str,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<OwnedFd> {
        let result = (|| {
            let reply = wait(
                self.client.connection.call_method(
                    Some(self.client.owner.as_str()),
                    PATH,
                    Some(interface),
                    method,
                    &(&self.session, Dict::new()),
                ),
                deadline,
                cancel,
            )?
            .map_err(|_| PortalError::Transport)?;
            let fd: zbus::zvariant::OwnedFd = reply
                .body()
                .deserialize()
                .map_err(|_| PortalError::InvalidResponse)?;
            self.refresh()?;
            Ok(fd.into())
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn close(&mut self) {
        let was_active = self.active;
        self.invalidate();
        if was_active {
            self.client.close_path(self.session.as_str(), SESSION);
        }
    }
}
impl Drop for AuthorizedSession {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

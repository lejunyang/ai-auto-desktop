use super::*;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::{atomic::AtomicUsize, Mutex};
use zbus::message::Header;

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
    Happy,
    Denied,
    InvalidStreams,
    WrongPath,
    Silent,
    Revoked,
}
struct State {
    scenario: Scenario,
    created: AtomicUsize,
    requests_closed: AtomicUsize,
    sessions_closed: AtomicUsize,
    session: Mutex<String>,
    peers: Mutex<Vec<UnixStream>>,
}
struct Request(Arc<State>);
#[zbus::interface(name = "org.freedesktop.portal.Request")]
impl Request {
    fn close(&self) {
        self.0.requests_closed.fetch_add(1, Ordering::SeqCst);
    }
}
struct Session(Arc<State>);
#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl Session {
    fn close(&self) {
        self.0.sessions_closed.fetch_add(1, Ordering::SeqCst);
    }
}
fn sender(header: &Header<'_>) -> String {
    header
        .sender()
        .unwrap()
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_")
}
fn string(options: &Dict, key: &str) -> String {
    options[key].try_clone().unwrap().try_into().unwrap()
}
async fn respond(
    connection: &Connection,
    header: &Header<'_>,
    state: &Arc<State>,
    options: Dict,
    code: u32,
    result: Dict,
    silent: bool,
) -> zbus::fdo::Result<OwnedObjectPath> {
    let path = format!(
        "{PATH}/request/{}/{}",
        sender(header),
        string(&options, "handle_token")
    );
    connection
        .object_server()
        .at(path.as_str(), Request(state.clone()))
        .await
        .unwrap();
    if !silent {
        // Deliberately emit before returning the method reply.
        connection
            .emit_signal(
                None::<&str>,
                path.as_str(),
                REQUEST,
                "Response",
                &(code, result),
            )
            .await
            .unwrap();
    }
    if state.scenario == Scenario::WrongPath {
        return Ok(OwnedObjectPath::try_from(format!("{path}_wrong")).unwrap());
    }
    Ok(OwnedObjectPath::try_from(path).unwrap())
}
struct Remote(Arc<State>);
#[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
impl Remote {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
    #[zbus(property)]
    fn available_device_types(&self) -> u32 {
        3
    }
    async fn create_session(
        &self,
        options: Dict,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.0.created.fetch_add(1, Ordering::SeqCst);
        let path = format!(
            "{PATH}/session/{}/{}",
            sender(&header),
            string(&options, "session_handle_token")
        );
        connection
            .object_server()
            .at(path.as_str(), Session(self.0.clone()))
            .await
            .unwrap();
        *self.0.session.lock().unwrap() = path.clone();
        respond(
            connection,
            &header,
            &self.0,
            options,
            0,
            HashMap::from([("session_handle".into(), owned(Value::from(path)).unwrap())]),
            false,
        )
        .await
    }
    async fn select_devices(
        &self,
        session: OwnedObjectPath,
        options: Dict,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        assert_eq!(session.as_str(), self.0.session.lock().unwrap().as_str());
        assert_eq!(
            u32::try_from(options["types"].try_clone().unwrap()).unwrap(),
            3
        );
        assert_eq!(
            u32::try_from(options["persist_mode"].try_clone().unwrap()).unwrap(),
            0
        );
        assert!(!options.contains_key("restore_token"));
        respond(connection, &header, &self.0, options, 0, Dict::new(), false).await
    }
    async fn start(
        &self,
        session: OwnedObjectPath,
        parent: &str,
        options: Dict,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        assert!(parent.is_empty());
        let mut result = HashMap::from([("devices".into(), OwnedValue::from(3u32))]);
        let properties = HashMap::from([
            ("source_type".to_string(), OwnedValue::from(1u32)),
            ("pipewire-serial".to_string(), OwnedValue::from(123u64)),
            (
                "mapping_id".to_string(),
                owned(Value::from("monitor")).unwrap(),
            ),
            (
                "position".to_string(),
                owned(Value::from((-100i32, 0i32))).unwrap(),
            ),
            (
                "size".to_string(),
                owned(Value::from((100i32, 200i32))).unwrap(),
            ),
        ]);
        result.insert(
            "streams".into(),
            owned(Value::from(vec![(7u32, properties)])).unwrap(),
        );
        // An unsolicited secret must never appear in diagnostics or retained state.
        result.insert(
            "restore_token".into(),
            owned(Value::from("test-secret-never-log")).unwrap(),
        );
        if self.0.scenario == Scenario::InvalidStreams {
            result.remove("streams");
        }
        if self.0.scenario == Scenario::Revoked {
            connection
                .emit_signal(
                    None::<&str>,
                    session.as_str(),
                    SESSION,
                    "Closed",
                    &(Dict::new(),),
                )
                .await
                .unwrap();
        }
        respond(
            connection,
            &header,
            &self.0,
            options,
            if self.0.scenario == Scenario::Denied {
                1
            } else {
                0
            },
            result,
            self.0.scenario == Scenario::Silent,
        )
        .await
    }
    #[zbus(name = "ConnectToEIS")]
    fn connect_to_eis(&self, session: OwnedObjectPath, _options: Dict) -> zbus::zvariant::OwnedFd {
        assert_eq!(session.as_str(), self.0.session.lock().unwrap().as_str());
        fd(&self.0)
    }
}
fn fd(state: &State) -> zbus::zvariant::OwnedFd {
    let (local, peer) = UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    state.peers.lock().unwrap().push(peer);
    OwnedFd::from(local).into()
}
struct Screen(Arc<State>);
#[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
impl Screen {
    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        1
    }
    async fn select_sources(
        &self,
        session: OwnedObjectPath,
        options: Dict,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        assert_eq!(session.as_str(), self.0.session.lock().unwrap().as_str());
        assert_eq!(
            u32::try_from(options["types"].try_clone().unwrap()).unwrap(),
            1
        );
        assert!(!bool::try_from(options["multiple"].try_clone().unwrap()).unwrap());
        assert!(!options.contains_key("persist_mode"));
        respond(connection, &header, &self.0, options, 0, Dict::new(), false).await
    }
    fn open_pipe_wire_remote(
        &self,
        _session: OwnedObjectPath,
        _options: Dict,
    ) -> zbus::zvariant::OwnedFd {
        fd(&self.0)
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(3)
}
fn server(scenario: Scenario) -> (Connection, Arc<State>) {
    let state = Arc::new(State {
        scenario,
        created: AtomicUsize::new(0),
        requests_closed: AtomicUsize::new(0),
        sessions_closed: AtomicUsize::new(0),
        session: Mutex::new(String::new()),
        peers: Mutex::new(Vec::new()),
    });
    let builder = zbus::connection::Builder::session()
        .unwrap()
        .name(NAME)
        .unwrap()
        .serve_at(PATH, Remote(state.clone()))
        .unwrap()
        .serve_at(PATH, Screen(state.clone()))
        .unwrap();
    (
        wait(builder.build(), deadline(), &AtomicBool::new(false))
            .unwrap()
            .unwrap(),
        state,
    )
}
fn authorize(
    client: PortalClient,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<AuthorizedSession> {
    client.authorize(
        AuthorizationIntent::UserRequested,
        AuthorizationOptions::unparented(),
        deadline,
        cancel,
    )
}

#[test]
#[ignore = "run only in private dbus-run-session using verify-wayland-portal.ps1"]
fn private_portal_transport() {
    assert_eq!(std::env::var("AAD_PORTAL_TEST_PRIVATE").as_deref(), Ok("1"));
    for scenario in [
        Scenario::Happy,
        Scenario::Denied,
        Scenario::InvalidStreams,
        Scenario::WrongPath,
        Scenario::Revoked,
        Scenario::Silent,
    ] {
        let (service, state) = server(scenario);
        let cancel = AtomicBool::new(false);
        let client = PortalClient::connect(deadline(), &cancel).unwrap();
        assert_eq!(
            state.created.load(Ordering::SeqCst),
            0,
            "connecting must not request authorization"
        );
        let result = authorize(
            client,
            if scenario == Scenario::Silent {
                Instant::now() + Duration::from_millis(150)
            } else {
                deadline()
            },
            &cancel,
        );
        match scenario {
            Scenario::Happy => {
                let mut session = result.unwrap();
                assert_eq!(session.devices().unwrap(), 3);
                assert_eq!(session.streams().unwrap()[0].node_id, 7);
                session.connect_eis(deadline(), &cancel).unwrap();
                assert_eq!(
                    session.connect_eis(deadline(), &cancel),
                    Err(PortalError::InvalidState)
                );
                session.open_pipewire(deadline(), &cancel).unwrap();
                assert_eq!(state.peers.lock().unwrap().len(), 2);
                wait(
                    service.emit_signal(
                        None::<&str>,
                        state.session.lock().unwrap().as_str(),
                        SESSION,
                        "Closed",
                        &(Dict::new(),),
                    ),
                    deadline(),
                    &cancel,
                )
                .unwrap()
                .unwrap();
                for _ in 0..100 {
                    if session.refresh().is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                assert_eq!(session.refresh(), Err(PortalError::Revoked));
                for peer in state.peers.lock().unwrap().iter_mut() {
                    assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
                }
            }
            _ => {
                let error = result.err().expect("must reject");
                let expected = match scenario {
                    Scenario::Denied => PortalError::Denied,
                    Scenario::Silent => PortalError::Deadline,
                    Scenario::Revoked => PortalError::Revoked,
                    _ => PortalError::InvalidResponse,
                };
                assert_eq!(error, expected);
                assert!(!format!("{error:?} {error}").contains("test-secret"));
            }
        }
        assert!(
            state.sessions_closed.load(Ordering::SeqCst) > 0,
            "session cleanup must occur"
        );
        if scenario == Scenario::Silent
            || scenario == Scenario::WrongPath
            || scenario == Scenario::Denied
        {
            assert!(state.requests_closed.load(Ordering::SeqCst) > 0);
        }
        wait(service.close(), deadline(), &cancel).unwrap().unwrap();
    }
    // Cancellation must interrupt a pending Response and still clean up.
    let (service, state) = server(Scenario::Silent);
    let cancel = Arc::new(AtomicBool::new(false));
    let client = PortalClient::connect(deadline(), &cancel).unwrap();
    let setter = cancel.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        setter.store(true, Ordering::SeqCst);
    });
    assert_eq!(
        authorize(client, deadline(), &cancel).err(),
        Some(PortalError::Cancelled)
    );
    thread.join().unwrap();
    assert!(state.sessions_closed.load(Ordering::SeqCst) > 0);
    assert!(state.requests_closed.load(Ordering::SeqCst) > 0);
    wait(service.close(), deadline(), &AtomicBool::new(false))
        .unwrap()
        .unwrap();

    // Two real authorizations with identical stream metadata remain distinct.
    let (service, _) = server(Scenario::Happy);
    let cancel = AtomicBool::new(false);
    let mut first = authorize(
        PortalClient::connect(deadline(), &cancel).unwrap(),
        deadline(),
        &cancel,
    )
    .unwrap();
    let generation = first.generation().unwrap();
    let stream = first.streams().unwrap()[0].clone();
    let region = EiRegion {
        mapping_id: "monitor".into(),
        logical_origin: (-100, 0),
        logical_size: (100, 200),
    };
    let frame = FrameGeometry {
        pixel_size: (200, 400),
        pipewire_serial: 123,
        untransformed: true,
    };
    let mapping =
        StreamMapping::bind(generation, &stream, std::slice::from_ref(&region), frame).unwrap();
    let mut second = authorize(
        PortalClient::connect(deadline(), &cancel).unwrap(),
        deadline(),
        &cancel,
    )
    .unwrap();
    assert_eq!(second.streams().unwrap()[0], stream);
    assert!(mapping
        .map_fraction(
            second.generation().unwrap(),
            &stream,
            &region,
            frame,
            (0.5, 0.5)
        )
        .is_err());
    first.close();
    second.close();
    wait(service.close(), deadline(), &cancel).unwrap().unwrap();

    // A compositor/portal restart invalidates the grant even without Closed.
    let (service, state) = server(Scenario::Happy);
    let cancel = AtomicBool::new(false);
    let client = PortalClient::connect(deadline(), &cancel).unwrap();
    let mut session = authorize(client, deadline(), &cancel).unwrap();
    session.connect_eis(deadline(), &cancel).unwrap();
    wait(service.close(), deadline(), &cancel).unwrap().unwrap();
    for _ in 0..100 {
        if session.refresh().is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(session.refresh(), Err(PortalError::Revoked));
    assert_eq!(
        state.peers.lock().unwrap()[0].read(&mut [0u8; 1]).unwrap(),
        0
    );
}

#[test]
fn mapping_rejects_stale_ambiguous_and_transformed_frames() {
    let stream = AuthorizedStream {
        node_id: 7,
        pipewire_serial: Some(123),
        mapping_id: Some("monitor".into()),
        source_type: 1,
        logical_origin: Some((-100, 0)),
        logical_size: Some((100, 200)),
    };
    let region = EiRegion {
        mapping_id: "monitor".into(),
        logical_origin: (-100, 0),
        logical_size: (100, 200),
    };
    let frame = FrameGeometry {
        pixel_size: (200, 400),
        pipewire_serial: 123,
        untransformed: true,
    };
    let generation = SessionGeneration::new();
    let mapping =
        StreamMapping::bind(generation, &stream, std::slice::from_ref(&region), frame).unwrap();
    assert_eq!(
        mapping
            .map_fraction(generation, &stream, &region, frame, (0.5, 0.5))
            .unwrap(),
        (-50.0, 100.0)
    );
    let mut next_revision = generation;
    next_revision.invalidate();
    assert!(mapping
        .map_fraction(next_revision, &stream, &region, frame, (0.5, 0.5))
        .is_err());
    let moved_region = EiRegion {
        logical_origin: (0, 0),
        ..region.clone()
    };
    assert!(mapping
        .map_fraction(generation, &stream, &moved_region, frame, (0.5, 0.5))
        .is_err());
    assert!(mapping
        .map_fraction(
            SessionGeneration::new(),
            &stream,
            &region,
            frame,
            (0.5, 0.5)
        )
        .is_err());
    assert!(mapping
        .map_fraction(generation, &stream, &region, frame, (f64::NAN, 0.5))
        .is_err());
    assert!(mapping
        .map_fraction(generation, &stream, &region, frame, (1.0, 0.5))
        .is_err());
    assert!(StreamMapping::bind(
        generation,
        &stream,
        &[region.clone(), region.clone()],
        frame
    )
    .is_err());
    assert!(StreamMapping::bind(
        generation,
        &stream,
        std::slice::from_ref(&region),
        FrameGeometry {
            untransformed: false,
            ..frame
        }
    )
    .is_err());
    assert!(StreamMapping::bind(
        generation,
        &stream,
        &[region],
        FrameGeometry {
            pipewire_serial: 124,
            ..frame
        }
    )
    .is_err());
}

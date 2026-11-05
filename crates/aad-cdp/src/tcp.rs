use crate::CdpError;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Keep one pending connection alive; cancellation drops its socket.
pub(crate) fn connect(
    address: SocketAddr,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
    _protocol: &str,
) -> Result<TcpStream, CdpError> {
    #[cfg(feature = "lifecycle-test-hooks")]
    let injection = RECORDER_START
        .with(|active| active.get())
        .then(|| std::env::var("AAD_TEST_PENDING_TCP").ok())
        .flatten()
        .and_then(|path| {
            let config = std::fs::read_to_string(&path).ok()?;
            let (protocol, port) = config.trim().split_once(' ')?;
            (protocol == _protocol).then(|| (path, port.parse::<u16>().unwrap()))
        });
    #[cfg(feature = "lifecycle-test-hooks")]
    let address = injection.as_ref().map_or(address, |(_, port)| {
        SocketAddr::from(([127, 0, 0, 1], *port))
    });
    let check = || {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(CdpError::connect("cancelled"));
        }
        deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| CdpError::connect("TCP deadline elapsed"))
    };
    check()?;
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(CdpError::io)?;
    socket.set_nonblocking(true).map_err(CdpError::io)?;
    if let Err(error) = socket.connect(&address.into()) {
        // EINPROGRESS on Linux/BSD/macOS and WSAEWOULDBLOCK on Windows.
        if error.kind() != std::io::ErrorKind::WouldBlock
            && !matches!(error.raw_os_error(), Some(115 | 36 | 10035))
        {
            return Err(CdpError::connect(error.to_string()));
        }
    }
    loop {
        let remaining = check()?;
        if let Some(error) = socket.take_error().map_err(CdpError::io)? {
            return Err(CdpError::connect(error.to_string()));
        }
        if socket.peer_addr().is_ok() {
            socket.set_nonblocking(false).map_err(CdpError::io)?;
            return Ok(socket.into());
        }
        #[cfg(feature = "lifecycle-test-hooks")]
        if let Some((path, _)) = &injection {
            std::fs::write(format!("{path}.pending"), "pending").unwrap();
        }
        std::thread::sleep(remaining.min(Duration::from_millis(20)));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn saturated_queue_cancels_http_and_websocket_within_one_second() {
        let listener = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        listener
            .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        listener.listen(0).unwrap();
        let address = listener.local_addr().unwrap().as_socket().unwrap();
        let _occupied = TcpStream::connect(address).unwrap();
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
        for websocket in [false, true] {
            let cancel = AtomicBool::new(false);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    if websocket {
                        crate::ws::WebSocket::connect_cancellable(
                            &format!("ws://{address}/devtools/page/test"),
                            deadline,
                            Some(&cancel),
                        )
                        .map(|_| ())
                    } else {
                        crate::http_get_cancellable(
                            address.port(),
                            "/json/list",
                            deadline,
                            Some(&cancel),
                        )
                        .map(|_| ())
                    }
                });
                std::thread::sleep(Duration::from_millis(100));
                let start = Instant::now();
                cancel.store(true, Ordering::SeqCst);
                let error = worker.join().unwrap().unwrap_err();
                assert!(error.to_string().contains("cancelled"), "{error}");
                assert!(start.elapsed() < Duration::from_secs(1));
            });
        }
    }
}

#[cfg(feature = "lifecycle-test-hooks")]
thread_local! {
    static RECORDER_START: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(feature = "lifecycle-test-hooks")]
pub(crate) struct RecorderStart;
#[cfg(feature = "lifecycle-test-hooks")]
impl RecorderStart {
    pub(crate) fn enter() -> Self {
        RECORDER_START.with(|active| active.set(true));
        Self
    }
}
#[cfg(feature = "lifecycle-test-hooks")]
impl Drop for RecorderStart {
    fn drop(&mut self) {
        RECORDER_START.with(|active| active.set(false));
    }
}

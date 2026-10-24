//! A minimal, text-only WebSocket/1.1 client (RFC 6455 client side).
//!
//! Just enough to talk to a local CDP endpoint: connect with the required key
//! handshake, send unmasked/masked text frames, and reassemble fragmented or
//! responses larger than 64 KiB. No TLS (CDP here is loopback plaintext only), no
//! extensions, no server role.

use base64::Engine;
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone)]
pub enum CdpError {
    Connect(String),
    Protocol(String),
    Io(String),
    BadUrl,
}

impl CdpError {
    pub fn bad_url() -> Self {
        CdpError::BadUrl
    }
    pub fn connect(message: impl Into<String>) -> Self {
        CdpError::Connect(message.into())
    }
    pub fn protocol(message: impl Into<String>) -> Self {
        CdpError::Protocol(message.into())
    }
    pub fn io(error: std::io::Error) -> Self {
        CdpError::Io(error.to_string())
    }
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Connect(message) => write!(formatter, "cdp connect: {message}"),
            CdpError::Protocol(message) => write!(formatter, "cdp protocol: {message}"),
            CdpError::Io(message) => write!(formatter, "cdp io: {message}"),
            CdpError::BadUrl => write!(formatter, "cdp: unsupported websocket url"),
        }
    }
}
impl std::error::Error for CdpError {}

/// One decoded WebSocket data frame (text or binary).
pub struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

impl Frame {
    pub fn is_text(&self) -> bool {
        self.opcode == 1
    }
    pub fn as_text(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }
}

pub struct WebSocket {
    stream: TcpStream,
    writer: Arc<Mutex<TcpStream>>,
}

const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

impl WebSocket {
    /// Connect to a `ws://host:port/path` URL.
    #[cfg(test)]
    pub fn connect_url(url: &str) -> Result<Self, CdpError> {
        Self::connect_until(url, Instant::now() + std::time::Duration::from_secs(10))
    }

    pub fn connect_until(url: &str, deadline: Instant) -> Result<Self, CdpError> {
        let (address, path) = local_endpoint(url)?;
        let host_port = address.to_string();
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| CdpError::connect("deadline elapsed"))?;
        let mut stream = TcpStream::connect_timeout(&address, timeout)
            .map_err(|error| CdpError::connect(format!("{host_port}: {error}")))?;
        stream.set_nonblocking(false).map_err(CdpError::io)?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .map_err(CdpError::io)?;

        // Fixed nonce is fine for a localhost client; servers only echo the
        // hash, they do not require unpredictability.
        let key = base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
        let accept = websocket_accept(&key);
        let handshake = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host_port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .write_all(handshake.as_bytes())
            .map_err(CdpError::io)?;

        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream
                .set_read_timeout(Some(
                    deadline
                        .checked_duration_since(Instant::now())
                        .ok_or_else(|| CdpError::connect("handshake timed out"))?,
                ))
                .map_err(CdpError::io)?;
            stream.read_exact(&mut byte).map_err(CdpError::io)?;
            response.push(byte[0]);
            if response.ends_with(b"\r\n\r\n") {
                break;
            }
            if response.len() > 16_384 {
                return Err(CdpError::protocol("handshake header too large"));
            }
        }
        let head = String::from_utf8_lossy(&response);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(CdpError::connect(format!(
                "bad handshake: {}",
                head.lines().next().unwrap_or("")
            )));
        }
        if !head.contains(&format!("Sec-WebSocket-Accept: {accept}"))
            && !head.contains(&format!("sec-websocket-accept: {accept}"))
        {
            return Err(CdpError::protocol("accept key mismatch"));
        }

        Self::from_stream(stream)
    }

    pub(crate) fn from_stream(stream: TcpStream) -> Result<Self, CdpError> {
        let writer = Arc::new(Mutex::new(stream.try_clone().map_err(CdpError::io)?));
        Ok(Self { stream, writer })
    }

    /// Exactly one clone may read. Writers (including pong) share a frame lock.
    pub fn reader(&self) -> Result<Self, CdpError> {
        Ok(Self {
            stream: self.stream.try_clone().map_err(CdpError::io)?,
            writer: self.writer.clone(),
        })
    }

    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    pub fn set_write_deadline(&self, deadline: Instant) -> Result<(), CdpError> {
        self.stream
            .set_write_timeout(Some(
                deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| CdpError::connect("deadline elapsed"))?,
            ))
            .map_err(CdpError::io)
    }

    /// Send a client text frame, masked per RFC 6455.
    pub fn send_text(&mut self, text: &str) -> Result<(), CdpError> {
        self.send_frame(1, text.as_bytes())
    }
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), CdpError> {
        let random = uuid::Uuid::new_v4();
        let mask: [u8; 4] = random.as_bytes()[..4].try_into().unwrap();
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        self.writer
            .lock()
            .unwrap()
            .write_all(&frame)
            .map_err(CdpError::io)
    }

    /// Receive one complete (possibly fragmented) message with a deadline.
    pub fn recv_until(&mut self, deadline: Instant) -> Result<Frame, CdpError> {
        let mut assembled: Option<(u8, Vec<u8>)> = None;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| CdpError::connect("cdp read timed out"))?;
            self.stream
                .set_read_timeout(Some(remaining))
                .map_err(CdpError::io)?;

            let frame = self.read_frame(deadline)?;
            match frame.opcode {
                0x8 => return Err(CdpError::protocol("connection closed by peer")),
                0x9 => {
                    self.send_frame(0xA, &frame.payload)?;
                    continue;
                }
                0xA => continue,
                _ => {}
            }

            if frame.fin {
                if let Some((opcode, mut payload)) = assembled.take() {
                    if frame.opcode != 0 || payload.len() + frame.payload.len() > MAX_MESSAGE_BYTES
                    {
                        return Err(CdpError::protocol("invalid or oversized continuation"));
                    }
                    payload.extend(frame.payload);
                    return Ok(Frame {
                        fin: true,
                        opcode,
                        payload,
                    });
                }
                if frame.opcode == 0 {
                    return Err(CdpError::protocol("unexpected continuation"));
                }
                return Ok(frame);
            } else {
                if (assembled.is_some() && frame.opcode != 0)
                    || (assembled.is_none() && !matches!(frame.opcode, 1 | 2))
                {
                    return Err(CdpError::protocol("invalid fragmentation sequence"));
                }
                let entry = assembled.get_or_insert_with(|| (frame.opcode, Vec::new()));
                if entry.1.len() + frame.payload.len() > MAX_MESSAGE_BYTES {
                    return Err(CdpError::protocol("message too large"));
                }
                entry.1.extend(frame.payload);
            }
        }
    }

    fn read_exact_until(
        &mut self,
        mut bytes: &mut [u8],
        deadline: Instant,
    ) -> Result<(), CdpError> {
        while !bytes.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| CdpError::connect("cdp read timed out"))?;
            self.stream
                .set_read_timeout(Some(remaining))
                .map_err(CdpError::io)?;
            let count = self.stream.read(bytes).map_err(CdpError::io)?;
            if count == 0 {
                return Err(CdpError::protocol("unexpected end of websocket frame"));
            }
            bytes = &mut bytes[count..];
        }
        Ok(())
    }
    fn read_frame(&mut self, deadline: Instant) -> Result<Frame, CdpError> {
        let mut head = [0u8; 2];
        self.read_exact_until(&mut head, deadline)?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0F;
        let masked = head[1] & 0x80 != 0;
        if head[0] & 0x70 != 0 || masked || !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10) {
            return Err(CdpError::protocol("invalid server frame"));
        }
        let mut len = (head[1] & 0x7F) as u64;
        if len == 126 {
            let mut ext = [0u8; 2];
            self.read_exact_until(&mut ext, deadline)?;
            len = u16::from_be_bytes(ext) as u64;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            self.read_exact_until(&mut ext, deadline)?;
            len = u64::from_be_bytes(ext);
        }
        // Cap a single message generously: CDP dumps can be large.
        if len > MAX_MESSAGE_BYTES as u64 {
            return Err(CdpError::protocol("frame too large"));
        }
        if opcode >= 8 && (!fin || len > 125) {
            return Err(CdpError::protocol("invalid control frame"));
        }
        let mut payload = vec![0u8; len as usize];
        self.read_exact_until(&mut payload, deadline)?;
        Ok(Frame {
            fin,
            opcode,
            payload,
        })
    }
}

/// Discovery JSON is untrusted. Resolve no DNS names and reject injection into
/// the HTTP request line, preserving the client's loopback-only boundary.
fn local_endpoint(url: &str) -> Result<(SocketAddr, &str), CdpError> {
    if url
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(CdpError::BadUrl);
    }
    let rest = url.strip_prefix("ws://").ok_or(CdpError::BadUrl)?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let authority = authority
        .strip_prefix("localhost:")
        .map(|port| format!("127.0.0.1:{port}"))
        .unwrap_or_else(|| authority.to_string());
    let address: SocketAddr = authority.parse().map_err(|_| CdpError::BadUrl)?;
    if !address.ip().is_loopback() || address.port() == 0 || path.contains('#') {
        return Err(CdpError::BadUrl);
    }
    Ok((address, path))
}

fn websocket_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receive(bytes: &[u8]) -> Result<Frame, CdpError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(bytes).unwrap();
        let mut client = WebSocket::from_stream(stream).unwrap();
        client.recv_until(Instant::now() + std::time::Duration::from_secs(1))
    }

    #[test]
    fn fragmented_cdp_response_keeps_its_text_opcode() {
        let frame = receive(&[0x01, 2, b'{', b'"', 0x80, 2, b'"', b'}']).unwrap();
        assert!(frame.is_text(), "CDP ignores non-text responses");
        assert_eq!(frame.as_text(), "{\"\"}");
    }

    #[test]
    fn invalid_continuation_sequences_are_rejected() {
        assert!(receive(&[0x80, 0]).is_err());
        assert!(receive(&[0x01, 0, 0x81, 0]).is_err());
    }

    #[test]
    fn a_trickling_frame_cannot_reset_the_absolute_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let writer = std::thread::spawn(move || {
            peer.write_all(&[0x81, 4]).unwrap();
            for byte in b"test" {
                std::thread::sleep(std::time::Duration::from_millis(25));
                if peer.write_all(&[*byte]).is_err() {
                    break;
                }
            }
        });
        let mut client = WebSocket::from_stream(stream).unwrap();
        let started = Instant::now();
        assert!(client
            .recv_until(started + std::time::Duration::from_millis(60))
            .is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(300));
        drop(client);
        writer.join().unwrap();
    }
    #[test]
    fn ping_is_answered_with_a_masked_pong_while_waiting_for_cdp() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        peer.write_all(&[0x89, 1, b'p', 0x81, 2, b'{', b'}'])
            .unwrap();
        let mut client = WebSocket::from_stream(stream).unwrap();
        assert_eq!(
            client
                .recv_until(Instant::now() + std::time::Duration::from_secs(1))
                .unwrap()
                .as_text(),
            "{}"
        );
        let mut reply = [0; 7];
        peer.read_exact(&mut reply).unwrap();
        assert_eq!(&reply[..2], &[0x8A, 0x81]);
        assert_eq!(reply[6] ^ reply[2], b'p');
    }

    #[test]
    fn discovery_cannot_redirect_the_client_outside_loopback() {
        for url in [
            "ws://192.0.2.1:9222/devtools/page/1",
            "ws://example.com:9222/",
            "ws://127.0.0.1.example.com:9222/",
            "ws://user@127.0.0.1:9222/",
            "ws://[::]:9222/",
            "wss://127.0.0.1:9222/",
            "ws://127.0.0.1:9222/\r\nInjected: header",
            "ws://127.0.0.1:9222/path with spaces",
        ] {
            assert!(
                matches!(WebSocket::connect_url(url), Err(CdpError::BadUrl)),
                "{url}"
            );
        }
    }

    #[test]
    fn explicit_loopback_addresses_and_localhost_are_supported() {
        for url in [
            "ws://127.0.0.1:9222/devtools/page/1?x=1",
            "ws://localhost:9222/devtools/page/1",
            "ws://[::1]:9222/devtools/page/1",
        ] {
            let (address, path) = local_endpoint(url).unwrap();
            assert!(address.ip().is_loopback());
            assert_eq!(address.port(), 9222);
            assert!(path.starts_with("/devtools/page/1"));
        }
    }
}

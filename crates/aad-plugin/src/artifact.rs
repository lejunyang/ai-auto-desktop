//! The artifact side channel.
//!
//! Large binary payloads — screenshots, documents, recordings — do not belong
//! on the NDJSON control stream: base64 inflates them by a third, and a single
//! oversized line would block every other message.  They travel instead over a
//! separate duplex byte stream in framed form.
//!
//! # Wire format (`AADF` v1)
//!
//! Each frame is a fixed 13-byte prefix followed by a JSON header and an
//! optional raw payload:
//!
//! ```text
//! "AADF" | version:u8 | header_len:u32 | payload_len:u32 | header | payload
//! ```
//!
//! All integers are big endian.  Every transfer is `*_open`, one or more
//! `*_chunk` frames, then `*_end` carrying the declared size and digest, so the
//! receiver can prove it reassembled exactly what the sender intended.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Read, Write};

pub const PROTOCOL: &str = "aad-artifact-socket-v1";
pub const MAGIC: &[u8; 4] = b"AADF";
pub const VERSION: u8 = 1;

pub const CHANNEL_FD_ENV: &str = "AAD_ARTIFACT_CHANNEL_FD";
pub const CHANNEL_PIPE_ENV: &str = "AAD_ARTIFACT_PIPE_NAME";
pub const CHANNEL_HOST_PID_ENV: &str = "AAD_ARTIFACT_HOST_PID";

pub const MAX_HEADER_BYTES: usize = 4096;
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

const PREFIX_BYTES: usize = 13;

/// Frames that begin a transfer.
const OPEN_TYPES: &[&str] = &["input_open", "output_open"];
/// Frames that carry payload bytes.
const CHUNK_TYPES: &[&str] = &["input_chunk", "output_chunk"];
/// Frames that complete a transfer and assert its size and digest.
const END_TYPES: &[&str] = &["input_end", "output_end"];
/// Frames that carry no payload and only mark progress.
const MARKER_TYPES: &[&str] = &["invocation_ready", "inputs_complete", "inputs_accepted"];
const COMPLETE_TYPE: &str = "invocation_complete";

/// A transport or protocol failure, safe to surface without leaking paths.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactError {
    pub code: String,
    pub message: String,
}

impl ArtifactError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new("ARTIFACT_IPC.INVALID_FRAME", message)
    }

    fn channel(message: impl Into<String>) -> Self {
        Self::new("ARTIFACT_IPC.CHANNEL_FAILED", message)
    }

    pub fn into_automation_error(self) -> aad_core::AutomationError {
        aad_core::AutomationError::new(self.code, self.message)
            .with_category("artifact")
            .with_effect("unknown")
    }
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ArtifactError {}

pub type Result<T> = std::result::Result<T, ArtifactError>;

/// One decoded frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub header: Map<String, Value>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn frame_type(&self) -> &str {
        self.header
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn request_id(&self) -> &str {
        self.header
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn slot(&self) -> Option<&str> {
        self.header.get("slot").and_then(Value::as_str)
    }
}

// ---------------------------------------------------------------------------
// Field validation
// ---------------------------------------------------------------------------

fn is_hex32(value: &str) -> bool {
    value.len() == 32
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
}

fn is_slot(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.len() <= 128
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A digest is `sha256:` followed by 64 lowercase hex digits.
fn is_digest(value: &str) -> bool {
    match value.strip_prefix("sha256:") {
        Some(hex) => {
            hex.len() == 64
                && hex
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        }
        None => false,
    }
}

fn is_error_code(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.starts_with(|c: char| c.is_ascii_uppercase())
                && part
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
}

/// Compute the digest of a payload in the protocol's canonical form.
pub fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

fn require_exact_fields(header: &Map<String, Value>, required: &[&str]) -> Result<()> {
    let present: BTreeSet<&str> = header.keys().map(String::as_str).collect();
    let wanted: BTreeSet<&str> = required.iter().copied().collect();
    if present != wanted {
        // Naming the difference makes a version mismatch obvious rather than
        // producing a vague "invalid frame".
        let missing: Vec<&str> = wanted.difference(&present).copied().collect();
        let unexpected: Vec<&str> = present.difference(&wanted).copied().collect();
        return Err(ArtifactError::invalid(format!(
            "artifact frame fields are wrong (missing: {missing:?}, unexpected: {unexpected:?})"
        )));
    }
    Ok(())
}

fn text<'a>(header: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    header
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ArtifactError::invalid(format!("artifact frame {key} must be a string")))
}

fn size_of(header: &Map<String, Value>) -> Result<u64> {
    let size = header
        .get("size_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| ArtifactError::invalid("artifact frame size_bytes must be an integer"))?;
    if size > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::new(
            "ARTIFACT_IPC.SIZE_LIMIT_EXCEEDED",
            "artifact exceeds the protocol size limit",
        ));
    }
    Ok(size)
}

/// Validate a header against the rules for its frame type.
pub fn validate_header(header: &Map<String, Value>, payload_len: usize) -> Result<()> {
    let frame_type = header
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ArtifactError::invalid("artifact frame type is missing"))?;

    let request_id = text(header, "request_id")?;
    if !is_hex32(request_id) {
        return Err(ArtifactError::invalid(
            "artifact frame request id is invalid",
        ));
    }

    let check_slot_and_token = |header: &Map<String, Value>| -> Result<()> {
        if !is_slot(text(header, "slot")?) {
            return Err(ArtifactError::invalid("artifact frame slot is invalid"));
        }
        if !is_hex32(text(header, "token")?) {
            return Err(ArtifactError::invalid("artifact frame token is invalid"));
        }
        Ok(())
    };

    if OPEN_TYPES.contains(&frame_type) {
        require_exact_fields(
            header,
            &[
                "type",
                "request_id",
                "slot",
                "token",
                "media_type",
                "size_bytes",
                "digest",
            ],
        )?;
        check_slot_and_token(header)?;
        let media_type = text(header, "media_type")?;
        if media_type.is_empty() || !media_type.contains('/') {
            return Err(ArtifactError::invalid("artifact media type is invalid"));
        }
        if !is_digest(text(header, "digest")?) {
            return Err(ArtifactError::invalid("artifact digest is invalid"));
        }
        size_of(header)?;
        if payload_len != 0 {
            return Err(ArtifactError::invalid(
                "artifact open frame must not carry a payload",
            ));
        }
    } else if CHUNK_TYPES.contains(&frame_type) {
        require_exact_fields(header, &["type", "request_id", "slot", "token"])?;
        check_slot_and_token(header)?;
        // An empty chunk would let a sender stall a transfer indefinitely.
        if payload_len == 0 || payload_len > MAX_FRAME_PAYLOAD_BYTES {
            return Err(ArtifactError::invalid(
                "artifact chunk payload size is invalid",
            ));
        }
    } else if END_TYPES.contains(&frame_type) {
        require_exact_fields(
            header,
            &[
                "type",
                "request_id",
                "slot",
                "token",
                "size_bytes",
                "digest",
            ],
        )?;
        check_slot_and_token(header)?;
        size_of(header)?;
        if !is_digest(text(header, "digest")?) {
            return Err(ArtifactError::invalid("artifact digest is invalid"));
        }
        if payload_len != 0 {
            return Err(ArtifactError::invalid(
                "artifact end frame must not carry a payload",
            ));
        }
    } else if MARKER_TYPES.contains(&frame_type) {
        require_exact_fields(header, &["type", "request_id"])?;
        if payload_len != 0 {
            return Err(ArtifactError::invalid(
                "artifact marker must not carry a payload",
            ));
        }
    } else if frame_type == COMPLETE_TYPE {
        let status = text(header, "status")?;
        if status == "error" {
            require_exact_fields(header, &["type", "request_id", "status", "error"])?;
            let error = header
                .get("error")
                .and_then(Value::as_object)
                .ok_or_else(|| ArtifactError::invalid("artifact error must be an object"))?;
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| ArtifactError::invalid("artifact error code is missing"))?;
            if !is_error_code(code) {
                return Err(ArtifactError::invalid("artifact error code is invalid"));
            }
        } else if status == "ok" {
            require_exact_fields(header, &["type", "request_id", "status"])?;
        } else {
            return Err(ArtifactError::invalid(
                "artifact completion status is invalid",
            ));
        }
        if payload_len != 0 {
            return Err(ArtifactError::invalid(
                "artifact completion must not carry a payload",
            ));
        }
    } else {
        return Err(ArtifactError::invalid(format!(
            "artifact frame type {frame_type:?} is not recognised"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Encode one validated frame.
pub fn encode(header: &Map<String, Value>, payload: &[u8]) -> Result<Vec<u8>> {
    validate_header(header, payload.len())?;

    // Canonical JSON: sorted keys and no incidental whitespace, so an
    // identical frame always produces identical bytes.
    let raw_header = canonical_json(&Value::Object(header.clone()));
    if raw_header.is_empty() || raw_header.len() > MAX_HEADER_BYTES {
        return Err(ArtifactError::new(
            "ARTIFACT_IPC.HEADER_LIMIT_EXCEEDED",
            "artifact frame header exceeds the protocol limit",
        ));
    }

    let mut frame = Vec::with_capacity(PREFIX_BYTES + raw_header.len() + payload.len());
    frame.extend_from_slice(MAGIC);
    frame.push(VERSION);
    frame.extend_from_slice(&(raw_header.len() as u32).to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(raw_header.as_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Serialise with sorted keys and no incidental whitespace.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let body: Vec<String> = sorted
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        Value::String((*key).clone()),
                        canonical_json(value)
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// Write one frame to a stream.
pub fn send_frame(
    writer: &mut impl Write,
    header: &Map<String, Value>,
    payload: &[u8],
) -> Result<()> {
    let frame = encode(header, payload)?;
    writer
        .write_all(&frame)
        .and_then(|()| writer.flush())
        .map_err(|error| ArtifactError::channel(format!("artifact channel write failed: {error}")))
}

/// Read exactly one frame from a stream.
pub fn receive_frame(reader: &mut impl Read) -> Result<Frame> {
    let mut prefix = [0u8; PREFIX_BYTES];
    read_exact(reader, &mut prefix)?;

    if &prefix[0..4] != MAGIC {
        return Err(ArtifactError::invalid("artifact frame magic is wrong"));
    }
    if prefix[4] != VERSION {
        return Err(ArtifactError::new(
            "ARTIFACT_IPC.VERSION_UNSUPPORTED",
            format!("artifact protocol version {} is not supported", prefix[4]),
        ));
    }
    let header_len = u32::from_be_bytes([prefix[5], prefix[6], prefix[7], prefix[8]]) as usize;
    let payload_len = u32::from_be_bytes([prefix[9], prefix[10], prefix[11], prefix[12]]) as usize;

    // Check the declared lengths before allocating: a hostile or corrupt
    // prefix must not be able to make us reserve gigabytes.
    if header_len == 0 || header_len > MAX_HEADER_BYTES {
        return Err(ArtifactError::new(
            "ARTIFACT_IPC.HEADER_LIMIT_EXCEEDED",
            "artifact frame header exceeds the protocol limit",
        ));
    }
    if payload_len > MAX_FRAME_PAYLOAD_BYTES {
        return Err(ArtifactError::new(
            "ARTIFACT_IPC.SIZE_LIMIT_EXCEEDED",
            "artifact frame payload exceeds the protocol limit",
        ));
    }

    let mut raw_header = vec![0u8; header_len];
    read_exact(reader, &mut raw_header)?;
    let mut payload = vec![0u8; payload_len];
    read_exact(reader, &mut payload)?;

    let header: Value = serde_json::from_slice(&raw_header)
        .map_err(|_| ArtifactError::invalid("artifact frame header is not valid JSON"))?;
    let header = header
        .as_object()
        .cloned()
        .ok_or_else(|| ArtifactError::invalid("artifact frame header must be an object"))?;

    validate_header(&header, payload.len())?;
    Ok(Frame { header, payload })
}

fn read_exact(reader: &mut impl Read, buffer: &mut [u8]) -> Result<()> {
    reader.read_exact(buffer).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            ArtifactError::new(
                "ARTIFACT_IPC.CHANNEL_CLOSED",
                "the artifact channel closed mid-frame",
            )
        } else {
            ArtifactError::channel(format!("artifact channel read failed: {error}"))
        }
    })
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

/// Header builders for one artifact transfer.
pub struct Transfer {
    pub request_id: String,
    pub slot: String,
    pub token: String,
    /// `input` when sending to a plugin, `output` when receiving from one.
    pub direction: &'static str,
}

impl Transfer {
    pub fn new(request_id: &str, slot: &str, token: &str, direction: &'static str) -> Self {
        Self {
            request_id: request_id.to_string(),
            slot: slot.to_string(),
            token: token.to_string(),
            direction,
        }
    }

    fn header(&self, suffix: &str) -> Map<String, Value> {
        let mut header = Map::new();
        header.insert("type".into(), json!(format!("{}_{suffix}", self.direction)));
        header.insert("request_id".into(), json!(self.request_id));
        header.insert("slot".into(), json!(self.slot));
        header.insert("token".into(), json!(self.token));
        header
    }

    pub fn open(&self, media_type: &str, payload: &[u8]) -> Map<String, Value> {
        let mut header = self.header("open");
        header.insert("media_type".into(), json!(media_type));
        header.insert("size_bytes".into(), json!(payload.len()));
        header.insert("digest".into(), json!(digest_of(payload)));
        header
    }

    pub fn chunk(&self) -> Map<String, Value> {
        self.header("chunk")
    }

    pub fn end(&self, payload: &[u8]) -> Map<String, Value> {
        let mut header = self.header("end");
        header.insert("size_bytes".into(), json!(payload.len()));
        header.insert("digest".into(), json!(digest_of(payload)));
        header
    }

    /// Send a whole artifact as open, chunks and end.
    pub fn send(&self, writer: &mut impl Write, media_type: &str, payload: &[u8]) -> Result<()> {
        if payload.len() as u64 > MAX_ARTIFACT_BYTES {
            return Err(ArtifactError::new(
                "ARTIFACT_IPC.SIZE_LIMIT_EXCEEDED",
                "artifact exceeds the protocol size limit",
            ));
        }
        send_frame(writer, &self.open(media_type, payload), &[])?;
        for chunk in payload.chunks(MAX_FRAME_PAYLOAD_BYTES) {
            send_frame(writer, &self.chunk(), chunk)?;
        }
        send_frame(writer, &self.end(payload), &[])?;
        Ok(())
    }
}

/// Reassembles one artifact from its frames, verifying size and digest.
pub struct Receiver {
    slot: String,
    token: String,
    media_type: String,
    declared_size: u64,
    declared_digest: String,
    bytes: Vec<u8>,
    finished: bool,
}

impl Receiver {
    /// Begin from an `*_open` frame.
    pub fn open(frame: &Frame) -> Result<Self> {
        if !OPEN_TYPES.contains(&frame.frame_type()) {
            return Err(ArtifactError::invalid("expected an artifact open frame"));
        }
        let header = &frame.header;
        Ok(Self {
            slot: text(header, "slot")?.to_string(),
            token: text(header, "token")?.to_string(),
            media_type: text(header, "media_type")?.to_string(),
            declared_size: size_of(header)?,
            declared_digest: text(header, "digest")?.to_string(),
            bytes: Vec::new(),
            finished: false,
        })
    }

    /// Accept a chunk or end frame; returns true once the transfer completes.
    pub fn accept(&mut self, frame: &Frame) -> Result<bool> {
        if self.finished {
            return Err(ArtifactError::invalid(
                "artifact frame arrived after the transfer ended",
            ));
        }
        // Slot and token must match, or two concurrent transfers could be
        // interleaved into one corrupt artifact.
        if frame.slot() != Some(self.slot.as_str()) {
            return Err(ArtifactError::invalid("artifact frame slot does not match"));
        }
        if frame.header.get("token").and_then(Value::as_str) != Some(self.token.as_str()) {
            return Err(ArtifactError::invalid(
                "artifact frame token does not match",
            ));
        }

        if CHUNK_TYPES.contains(&frame.frame_type()) {
            // Refuse to buffer more than was declared.
            if self.bytes.len() as u64 + frame.payload.len() as u64 > self.declared_size {
                return Err(ArtifactError::new(
                    "ARTIFACT_IPC.SIZE_MISMATCH",
                    "artifact payload exceeds its declared size",
                ));
            }
            self.bytes.extend_from_slice(&frame.payload);
            Ok(false)
        } else if END_TYPES.contains(&frame.frame_type()) {
            let declared_end_size = size_of(&frame.header)?;
            if declared_end_size != self.declared_size
                || self.bytes.len() as u64 != self.declared_size
            {
                return Err(ArtifactError::new(
                    "ARTIFACT_IPC.SIZE_MISMATCH",
                    "artifact payload size does not match its declaration",
                ));
            }
            // Verify against both declarations: a truncated or altered
            // transfer must never be handed on as if it were intact.
            let actual = digest_of(&self.bytes);
            if actual != self.declared_digest || actual != text(&frame.header, "digest")? {
                return Err(ArtifactError::new(
                    "ARTIFACT_IPC.DIGEST_MISMATCH",
                    "artifact digest does not match its contents",
                ));
            }
            self.finished = true;
            Ok(true)
        } else {
            Err(ArtifactError::invalid(
                "expected an artifact chunk or end frame",
            ))
        }
    }

    /// The verified artifact, available only once the transfer completed.
    pub fn finish(self) -> Result<Artifact> {
        if !self.finished {
            return Err(ArtifactError::invalid(
                "the artifact transfer is incomplete",
            ));
        }
        Ok(Artifact {
            slot: self.slot,
            media_type: self.media_type,
            digest: self.declared_digest,
            bytes: self.bytes,
        })
    }
}

/// A complete, digest-verified artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct Artifact {
    pub slot: String,
    pub media_type: String,
    pub digest: String,
    pub bytes: Vec<u8>,
}

impl Artifact {
    /// A reference describing the artifact without carrying its bytes.
    pub fn to_ref(&self) -> Value {
        json!({
            "slot": self.slot,
            "media_type": self.media_type,
            "size_bytes": self.bytes.len(),
            "digest": self.digest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_id() -> String {
        "a".repeat(32)
    }

    fn token() -> String {
        "b".repeat(32)
    }

    fn transfer(direction: &'static str) -> Transfer {
        Transfer::new(&request_id(), "screenshot", &token(), direction)
    }

    fn roundtrip(header: &Map<String, Value>, payload: &[u8]) -> Result<Frame> {
        let encoded = encode(header, payload)?;
        receive_frame(&mut std::io::Cursor::new(encoded))
    }

    #[test]
    fn a_frame_round_trips_through_the_wire_format() {
        let sender = transfer("output");
        let payload = b"some bytes";
        let frame = roundtrip(&sender.chunk(), payload).expect("the frame round-trips");

        assert_eq!(frame.frame_type(), "output_chunk");
        assert_eq!(frame.payload, payload);
        assert_eq!(frame.slot(), Some("screenshot"));
    }

    #[test]
    fn the_prefix_matches_the_documented_layout() {
        let sender = transfer("input");
        let encoded = encode(&sender.chunk(), b"xy").unwrap();

        assert_eq!(&encoded[0..4], MAGIC);
        assert_eq!(encoded[4], VERSION);
        let header_len = u32::from_be_bytes([encoded[5], encoded[6], encoded[7], encoded[8]]);
        let payload_len = u32::from_be_bytes([encoded[9], encoded[10], encoded[11], encoded[12]]);
        assert_eq!(payload_len, 2);
        assert_eq!(encoded.len(), PREFIX_BYTES + header_len as usize + 2);
    }

    #[test]
    fn a_whole_artifact_transfers_and_verifies() {
        let sender = transfer("output");
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();

        let mut stream = Vec::new();
        sender.send(&mut stream, "image/png", &payload).unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let mut receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();
        loop {
            let frame = receive_frame(&mut cursor).unwrap();
            if receiver.accept(&frame).unwrap() {
                break;
            }
        }
        let artifact = receiver.finish().unwrap();

        assert_eq!(artifact.bytes, payload);
        assert_eq!(artifact.media_type, "image/png");
        assert_eq!(artifact.digest, digest_of(&payload));
    }

    #[test]
    fn a_large_artifact_is_split_across_chunks() {
        let sender = transfer("output");
        let payload = vec![7u8; MAX_FRAME_PAYLOAD_BYTES * 2 + 17];

        let mut stream = Vec::new();
        sender
            .send(&mut stream, "application/octet-stream", &payload)
            .unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let mut receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();
        let mut chunks = 0;
        loop {
            let frame = receive_frame(&mut cursor).unwrap();
            if CHUNK_TYPES.contains(&frame.frame_type()) {
                chunks += 1;
                assert!(frame.payload.len() <= MAX_FRAME_PAYLOAD_BYTES);
            }
            if receiver.accept(&frame).unwrap() {
                break;
            }
        }

        assert_eq!(chunks, 3, "the payload must be split, not sent whole");
        assert_eq!(receiver.finish().unwrap().bytes.len(), payload.len());
    }

    #[test]
    fn a_corrupted_payload_is_rejected_by_its_digest() {
        let sender = transfer("output");
        let payload = b"the original bytes".to_vec();

        let mut stream = Vec::new();
        send_frame(&mut stream, &sender.open("text/plain", &payload), &[]).unwrap();
        // A different payload than the one the digest was computed over.
        send_frame(&mut stream, &sender.chunk(), b"the tampered bytes").unwrap();
        send_frame(&mut stream, &sender.end(&payload), &[]).unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let mut receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();
        let chunk = receive_frame(&mut cursor).unwrap();
        receiver.accept(&chunk).unwrap();
        let end = receive_frame(&mut cursor).unwrap();

        let error = receiver.accept(&end).unwrap_err();
        assert_eq!(error.code, "ARTIFACT_IPC.DIGEST_MISMATCH");
    }

    #[test]
    fn a_truncated_transfer_is_rejected_by_its_size() {
        let sender = transfer("output");
        let payload = b"twenty characters!!!".to_vec();

        let mut stream = Vec::new();
        send_frame(&mut stream, &sender.open("text/plain", &payload), &[]).unwrap();
        send_frame(&mut stream, &sender.chunk(), b"short").unwrap();
        send_frame(&mut stream, &sender.end(&payload), &[]).unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let mut receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();
        let chunk = receive_frame(&mut cursor).unwrap();
        receiver.accept(&chunk).unwrap();
        let end = receive_frame(&mut cursor).unwrap();

        assert_eq!(
            receiver.accept(&end).unwrap_err().code,
            "ARTIFACT_IPC.SIZE_MISMATCH"
        );
    }

    #[test]
    fn a_payload_longer_than_declared_is_refused_while_streaming() {
        // Detecting this at the end would mean buffering unbounded data first.
        let sender = transfer("output");
        let payload = b"tiny".to_vec();

        let mut stream = Vec::new();
        send_frame(&mut stream, &sender.open("text/plain", &payload), &[]).unwrap();
        send_frame(&mut stream, &sender.chunk(), b"far more than declared").unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let mut receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();
        let chunk = receive_frame(&mut cursor).unwrap();

        assert_eq!(
            receiver.accept(&chunk).unwrap_err().code,
            "ARTIFACT_IPC.SIZE_MISMATCH"
        );
    }

    #[test]
    fn an_incomplete_transfer_cannot_be_finished() {
        let sender = transfer("output");
        let payload = b"data".to_vec();
        let mut stream = Vec::new();
        send_frame(&mut stream, &sender.open("text/plain", &payload), &[]).unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();

        assert!(receiver.finish().is_err());
    }

    #[test]
    fn frames_from_another_transfer_cannot_be_interleaved() {
        let mine = transfer("output");
        let theirs = Transfer::new(&request_id(), "other_slot", &token(), "output");
        let payload = b"data".to_vec();

        let mut stream = Vec::new();
        send_frame(&mut stream, &mine.open("text/plain", &payload), &[]).unwrap();
        send_frame(&mut stream, &theirs.chunk(), b"not mine").unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let mut receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();
        let foreign = receive_frame(&mut cursor).unwrap();

        assert!(receiver.accept(&foreign).is_err());
    }

    #[test]
    fn a_frame_with_a_mismatched_token_is_rejected() {
        let mine = transfer("output");
        let impostor = Transfer::new(&request_id(), "screenshot", &"c".repeat(32), "output");
        let payload = b"data".to_vec();

        let mut stream = Vec::new();
        send_frame(&mut stream, &mine.open("text/plain", &payload), &[]).unwrap();
        send_frame(&mut stream, &impostor.chunk(), b"data").unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let mut receiver = Receiver::open(&receive_frame(&mut cursor).unwrap()).unwrap();
        let foreign = receive_frame(&mut cursor).unwrap();

        assert!(receiver.accept(&foreign).is_err());
    }

    #[test]
    fn wrong_magic_bytes_are_rejected() {
        let mut stream = encode(&transfer("output").chunk(), b"x").unwrap();
        stream[0] = b'X';

        let error = receive_frame(&mut std::io::Cursor::new(stream)).unwrap_err();
        assert_eq!(error.code, "ARTIFACT_IPC.INVALID_FRAME");
    }

    #[test]
    fn an_unsupported_version_is_named_in_the_error() {
        let mut stream = encode(&transfer("output").chunk(), b"x").unwrap();
        stream[4] = 99;

        let error = receive_frame(&mut std::io::Cursor::new(stream)).unwrap_err();
        assert_eq!(error.code, "ARTIFACT_IPC.VERSION_UNSUPPORTED");
        assert!(error.message.contains("99"), "{}", error.message);
    }

    #[test]
    fn a_stream_that_ends_mid_frame_is_reported_as_closed() {
        let stream = encode(&transfer("output").chunk(), b"payload").unwrap();
        let truncated = &stream[..stream.len() - 3];

        let error = receive_frame(&mut std::io::Cursor::new(truncated)).unwrap_err();
        assert_eq!(error.code, "ARTIFACT_IPC.CHANNEL_CLOSED");
    }

    #[test]
    fn an_oversized_declared_length_is_refused_before_allocating() {
        // A hostile prefix must not be able to make the reader reserve memory.
        let mut stream = Vec::new();
        stream.extend_from_slice(MAGIC);
        stream.push(VERSION);
        stream.extend_from_slice(&(64u32).to_be_bytes());
        stream.extend_from_slice(&(u32::MAX).to_be_bytes());

        let error = receive_frame(&mut std::io::Cursor::new(stream)).unwrap_err();
        assert_eq!(error.code, "ARTIFACT_IPC.SIZE_LIMIT_EXCEEDED");
    }

    #[test]
    fn an_oversized_header_is_refused() {
        let mut stream = Vec::new();
        stream.extend_from_slice(MAGIC);
        stream.push(VERSION);
        stream.extend_from_slice(&((MAX_HEADER_BYTES as u32) + 1).to_be_bytes());
        stream.extend_from_slice(&0u32.to_be_bytes());

        let error = receive_frame(&mut std::io::Cursor::new(stream)).unwrap_err();
        assert_eq!(error.code, "ARTIFACT_IPC.HEADER_LIMIT_EXCEEDED");
    }

    #[test]
    fn an_open_frame_must_not_carry_a_payload() {
        let sender = transfer("output");
        let header = sender.open("text/plain", b"data");

        assert!(encode(&header, b"unexpected").is_err());
    }

    #[test]
    fn a_chunk_frame_must_carry_a_payload() {
        // An empty chunk would let a sender stall a transfer forever.
        assert!(encode(&transfer("output").chunk(), b"").is_err());
    }

    #[test]
    fn unknown_or_missing_header_fields_are_rejected() {
        let sender = transfer("output");

        let mut extra = sender.chunk();
        extra.insert("surprise".into(), json!(1));
        assert!(encode(&extra, b"x").is_err());

        let mut missing = sender.chunk();
        missing.remove("token");
        assert!(encode(&missing, b"x").is_err());
    }

    #[test]
    fn malformed_identifiers_are_rejected() {
        let cases = [
            ("request_id", json!("too-short")),
            ("token", json!("NOTLOWERCASEHEX0123456789012345a")),
            ("slot", json!("9starts-with-a-digit")),
            ("slot", json!("has spaces")),
        ];
        for (field, value) in cases {
            let mut header = transfer("output").chunk();
            header.insert(field.into(), value.clone());
            assert!(
                encode(&header, b"x").is_err(),
                "{field}={value} should be rejected"
            );
        }
    }

    #[test]
    fn a_malformed_digest_is_rejected() {
        let sender = transfer("output");
        let mut header = sender.open("text/plain", b"data");
        header.insert("digest".into(), json!("md5:abc"));

        assert!(encode(&header, &[]).is_err());
        assert!(is_digest(&digest_of(b"data")));
    }

    #[test]
    fn an_unknown_frame_type_is_rejected() {
        let mut header = transfer("output").chunk();
        header.insert("type".into(), json!("output_teleport"));

        assert!(encode(&header, b"x").is_err());
    }

    #[test]
    fn a_completion_frame_carries_a_status_and_any_error() {
        let mut ok = Map::new();
        ok.insert("type".into(), json!(COMPLETE_TYPE));
        ok.insert("request_id".into(), json!(request_id()));
        ok.insert("status".into(), json!("ok"));
        assert!(encode(&ok, &[]).is_ok());

        let mut failed = ok.clone();
        failed.insert("status".into(), json!("error"));
        // An error status without an error object must not be accepted.
        assert!(encode(&failed, &[]).is_err());

        failed.insert(
            "error".into(),
            json!({"code": "PLUGIN.FAILED", "message": "no"}),
        );
        assert!(encode(&failed, &[]).is_ok());

        let mut bad_code = failed.clone();
        bad_code.insert(
            "error".into(),
            json!({"code": "lowercase", "message": "no"}),
        );
        assert!(encode(&bad_code, &[]).is_err());
    }

    #[test]
    fn an_artifact_reference_omits_the_bytes() {
        let artifact = Artifact {
            slot: "screenshot".into(),
            media_type: "image/png".into(),
            digest: digest_of(b"pixels"),
            bytes: b"pixels".to_vec(),
        };
        let reference = artifact.to_ref();

        assert_eq!(reference["size_bytes"], 6);
        assert_eq!(reference["digest"], artifact.digest);
        assert!(
            reference.get("bytes").is_none(),
            "bytes must not be inlined"
        );
    }

    #[test]
    fn headers_encode_canonically_regardless_of_insertion_order() {
        // Identical frames must produce identical bytes.
        let mut first = Map::new();
        first.insert("type".into(), json!("inputs_complete"));
        first.insert("request_id".into(), json!(request_id()));
        let mut second = Map::new();
        second.insert("request_id".into(), json!(request_id()));
        second.insert("type".into(), json!("inputs_complete"));

        assert_eq!(encode(&first, &[]).unwrap(), encode(&second, &[]).unwrap());
    }
}

//! Run-scoped, immutable image artifacts for built-in Rust providers.
//!
//! Public workflow values carry only an [`ArtifactRef`]. The bytes remain in a
//! bounded host-owned store and can only be resolved by providers participating
//! in the same run. This preserves the capability boundary without requiring a
//! second language runtime or exposing temporary filesystem paths.

use aad_core::AutomationError;
use image::io::Reader as ImageReader;
use image::{AnimationDecoder, ImageFormat};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const ARTIFACT_API_VERSION: &str = "ai-auto-desktop.dev/v1alpha1";
pub const ARTIFACT_KIND: &str = "ArtifactRef";
pub const DEFAULT_MAX_SIZE_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_PIXELS: u64 = 16_000_000;
pub const DEFAULT_MAX_DIMENSION: u32 = 20_000;
pub const DEFAULT_MAX_ARTIFACTS: usize = 128;
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
pub const DEFAULT_TTL: Duration = Duration::from_secs(3_600);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub digest: String,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: u64,
}

impl ArtifactRef {
    pub fn from_value(value: &Value) -> Result<Self, AutomationError> {
        let reference: Self = serde_json::from_value(value.clone()).map_err(|_| invalid_ref())?;
        reference.validate()?;
        Ok(reference)
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("ArtifactRef is always JSON serializable")
    }

    fn validate(&self) -> Result<(), AutomationError> {
        let valid_id = self
            .artifact_id
            .strip_prefix("art_")
            .is_some_and(|suffix| suffix.len() == 32 && suffix.chars().all(is_id_char));
        let valid_digest = self.digest.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() && !character.is_uppercase())
        });
        if self.api_version != ARTIFACT_API_VERSION
            || self.kind != ARTIFACT_KIND
            || !valid_id
            || !valid_digest
            || !supported_media_type(&self.media_type)
            || self.size_bytes == 0
            || self.size_bytes > DEFAULT_MAX_SIZE_BYTES as u64
        {
            return Err(invalid_ref());
        }
        Ok(())
    }
}

fn is_id_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '-'
}

#[derive(Clone)]
struct Record {
    reference: ArtifactRef,
    bytes: Vec<u8>,
    expires_at: Instant,
}

#[derive(Default)]
struct State {
    records: HashMap<String, Record>,
    total_bytes: usize,
    closed: bool,
}

/// A bounded in-memory artifact capability scope.
///
/// Keeping the immutable snapshot in memory avoids symlink and namespace races
/// uniformly on Windows, macOS and Linux. The store is deliberately ephemeral:
/// durable workflows reject artifact contracts until a restart-safe encrypted
/// store and taint policy are specified.
pub struct ArtifactStore {
    state: Mutex<State>,
    ttl: Duration,
    max_size_bytes: usize,
    max_pixels: u64,
    max_dimension: u32,
    max_artifacts: usize,
    max_total_bytes: usize,
}

impl std::fmt::Debug for ArtifactStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .state
            .lock()
            .map(|state| state.records.len())
            .unwrap_or(0);
        formatter
            .debug_struct("ArtifactStore")
            .field("artifacts", &count)
            .field("max_size_bytes", &self.max_size_bytes)
            .field("max_total_bytes", &self.max_total_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for ArtifactStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            ttl: DEFAULT_TTL,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_pixels: DEFAULT_MAX_PIXELS,
            max_dimension: DEFAULT_MAX_DIMENSION,
            max_artifacts: DEFAULT_MAX_ARTIFACTS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

impl ArtifactStore {
    pub fn import_bytes(
        &self,
        bytes: impl AsRef<[u8]>,
        requested_media_type: Option<&str>,
    ) -> Result<ArtifactRef, AutomationError> {
        let bytes = bytes.as_ref();
        if bytes.is_empty() {
            return Err(artifact_error(
                "ARTIFACT.INVALID_SOURCE",
                "artifact image must not be empty",
            ));
        }
        if bytes.len() > self.max_size_bytes {
            return Err(artifact_error(
                "ARTIFACT.SIZE_LIMIT_EXCEEDED",
                "artifact exceeds the configured byte limit",
            )
            .with_detail("maxSizeBytes", json!(self.max_size_bytes)));
        }
        let media_type = detect_media_type(bytes).ok_or_else(|| {
            artifact_error(
                "ARTIFACT.UNSUPPORTED_MEDIA_TYPE",
                "artifact is not a supported image format",
            )
        })?;
        if requested_media_type.is_some_and(|requested| requested != media_type) {
            return Err(artifact_error(
                "ARTIFACT.MEDIA_TYPE_MISMATCH",
                "artifact bytes do not match the requested media type",
            ));
        }
        self.validate_image(bytes, media_type)?;

        let reference = ArtifactRef {
            api_version: ARTIFACT_API_VERSION.into(),
            kind: ARTIFACT_KIND.into(),
            artifact_id: format!("art_{}", uuid::Uuid::new_v4().simple()),
            digest: digest_of(bytes),
            media_type: media_type.into(),
            size_bytes: bytes.len() as u64,
        };
        let mut state = self.state.lock().map_err(|_| {
            artifact_error("ARTIFACT.STORE_CLOSED", "artifact store is unavailable")
        })?;
        if state.closed {
            return Err(artifact_error(
                "ARTIFACT.STORE_CLOSED",
                "artifact execution scope is closed",
            ));
        }
        purge_expired(&mut state);
        if state.records.len() >= self.max_artifacts
            || state.total_bytes.saturating_add(bytes.len()) > self.max_total_bytes
        {
            return Err(artifact_error(
                "ARTIFACT.QUOTA_EXCEEDED",
                "artifact store quota is exhausted",
            ));
        }
        state.total_bytes += bytes.len();
        state.records.insert(
            reference.artifact_id.clone(),
            Record {
                reference: reference.clone(),
                bytes: bytes.to_vec(),
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok(reference)
    }

    pub fn resolve(&self, value: &Value) -> Result<Vec<u8>, AutomationError> {
        let reference = ArtifactRef::from_value(value)?;
        let mut state = self.state.lock().map_err(|_| {
            artifact_error("ARTIFACT.STORE_CLOSED", "artifact store is unavailable")
        })?;
        if state.closed {
            return Err(artifact_error(
                "ARTIFACT.STORE_CLOSED",
                "artifact execution scope is closed",
            ));
        }
        purge_expired(&mut state);
        let record = state.records.get(&reference.artifact_id).ok_or_else(|| {
            artifact_error(
                "ARTIFACT.SCOPE_MISMATCH",
                "artifact reference does not belong to this execution scope",
            )
        })?;
        if record.reference != reference || digest_of(&record.bytes) != reference.digest {
            return Err(artifact_error(
                "ARTIFACT.INTEGRITY_FAILED",
                "artifact reference or content integrity validation failed",
            ));
        }
        Ok(record.bytes.clone())
    }

    pub fn cleanup(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.records.clear();
            state.total_bytes = 0;
            state.closed = true;
        }
    }

    pub fn len(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.records.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn validate_image(&self, bytes: &[u8], media_type: &str) -> Result<(), AutomationError> {
        let format = image_format(media_type);
        reject_multiple_frames(bytes, format)?;
        let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
            .into_dimensions()
            .map_err(|_| {
                artifact_error(
                    "ARTIFACT.INVALID_IMAGE",
                    "artifact image failed structural validation",
                )
            })?;
        if width == 0 || height == 0 || width > self.max_dimension || height > self.max_dimension {
            return Err(artifact_error(
                "ARTIFACT.DIMENSION_LIMIT_EXCEEDED",
                "artifact image exceeds the configured dimension limit",
            )
            .with_detail("maxDimension", json!(self.max_dimension)));
        }
        if u64::from(width) * u64::from(height) > self.max_pixels {
            return Err(artifact_error(
                "ARTIFACT.PIXEL_LIMIT_EXCEEDED",
                "artifact image exceeds the configured pixel limit",
            )
            .with_detail("maxPixels", json!(self.max_pixels)));
        }
        image::load_from_memory_with_format(bytes, format).map_err(|_| {
            artifact_error(
                "ARTIFACT.INVALID_IMAGE",
                "artifact image failed structural validation",
            )
        })?;
        Ok(())
    }
}

fn reject_multiple_frames(bytes: &[u8], format: ImageFormat) -> Result<(), AutomationError> {
    let has_more_than_one = match format {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
            .ok()
            .and_then(|decoder| {
                decoder
                    .into_frames()
                    .take(2)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()
            })
            .is_some_and(|frames| frames.len() > 1),
        ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(bytes))
            .ok()
            .filter(|decoder| decoder.is_apng())
            .and_then(|decoder| {
                decoder
                    .apng()
                    .into_frames()
                    .take(2)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()
            })
            .is_some_and(|frames| frames.len() > 1),
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
            .ok()
            .is_some_and(|decoder| decoder.has_animation()),
        // The image crate decodes the first TIFF directory. Reject a second
        // directory by parsing the first IFD's next pointer directly.
        ImageFormat::Tiff => tiff_has_multiple_directories(bytes),
        _ => false,
    };
    if has_more_than_one {
        return Err(artifact_error(
            "ARTIFACT.MULTI_FRAME_UNSUPPORTED",
            "artifact image must contain exactly one frame",
        ));
    }
    Ok(())
}

fn tiff_has_multiple_directories(bytes: &[u8]) -> bool {
    let little = bytes.starts_with(b"II*\0");
    let big = bytes.starts_with(b"MM\0*");
    if !little && !big || bytes.len() < 8 {
        return false;
    }
    let read_u16 = |slice: &[u8]| {
        if little {
            u16::from_le_bytes([slice[0], slice[1]])
        } else {
            u16::from_be_bytes([slice[0], slice[1]])
        }
    };
    let read_u32 = |slice: &[u8]| {
        if little {
            u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
        } else {
            u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]])
        }
    };
    let offset = read_u32(&bytes[4..8]) as usize;
    if offset.checked_add(2).is_none_or(|end| end > bytes.len()) {
        return false;
    }
    let entries = read_u16(&bytes[offset..offset + 2]) as usize;
    let next_offset = offset
        .checked_add(2)
        .and_then(|value| value.checked_add(entries.saturating_mul(12)));
    next_offset
        .filter(|offset| offset.checked_add(4).is_some_and(|end| end <= bytes.len()))
        .is_some_and(|offset| read_u32(&bytes[offset..offset + 4]) != 0)
}

fn purge_expired(state: &mut State) {
    let now = Instant::now();
    let expired: Vec<String> = state
        .records
        .iter()
        .filter(|(_, record)| record.expires_at <= now)
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        if let Some(record) = state.records.remove(&id) {
            state.total_bytes = state.total_bytes.saturating_sub(record.bytes.len());
        }
    }
}

pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() >= 3
        && bytes[0] == b'P'
        && matches!(bytes[1], b'1'..=b'7')
        && bytes[2].is_ascii_whitespace()
    {
        Some("image/x-portable-anymap")
    } else {
        None
    }
}

fn image_format(media_type: &str) -> ImageFormat {
    match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/tiff" => ImageFormat::Tiff,
        "image/bmp" => ImageFormat::Bmp,
        "image/webp" => ImageFormat::WebP,
        "image/x-portable-anymap" => ImageFormat::Pnm,
        _ => unreachable!("unsupported media type was already rejected"),
    }
}

pub fn supported_media_type(value: &str) -> bool {
    matches!(
        value,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/tiff"
            | "image/bmp"
            | "image/webp"
            | "image/x-portable-anymap"
    )
}

fn digest_of(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("sha256:{:x}", digest.finalize())
}

fn invalid_ref() -> AutomationError {
    artifact_error(
        "ARTIFACT.INVALID_REF",
        "artifact reference does not match the closed v1alpha1 contract",
    )
}

fn artifact_error(code: &str, message: impl Into<String>) -> AutomationError {
    AutomationError::new(code, message)
        .with_category("artifact")
        .with_phase("artifact")
        .with_effect("not_applied")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn reference_is_closed_and_resolves_only_in_its_own_scope() {
        let store = ArtifactStore::default();
        let reference = store.import_bytes(PNG_1X1, Some("image/png")).unwrap();
        assert_eq!(store.resolve(&reference.to_value()).unwrap(), PNG_1X1);
        assert!(reference.to_value().get("path").is_none());

        let foreign = ArtifactStore::default();
        assert_eq!(
            foreign.resolve(&reference.to_value()).unwrap_err().code,
            "ARTIFACT.SCOPE_MISMATCH"
        );
    }

    #[test]
    fn invalid_and_mismatched_images_fail_closed() {
        let store = ArtifactStore::default();
        assert_eq!(
            store.import_bytes(b"not an image", None).unwrap_err().code,
            "ARTIFACT.UNSUPPORTED_MEDIA_TYPE"
        );
        assert_eq!(
            store
                .import_bytes(PNG_1X1, Some("image/jpeg"))
                .unwrap_err()
                .code,
            "ARTIFACT.MEDIA_TYPE_MISMATCH"
        );
    }

    #[test]
    fn reference_tampering_and_closed_scopes_fail_closed() {
        let store = ArtifactStore::default();
        let reference = store.import_bytes(PNG_1X1, Some("image/png")).unwrap();
        let mut digest = reference.to_value();
        digest["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
        assert_eq!(
            store.resolve(&digest).unwrap_err().code,
            "ARTIFACT.INTEGRITY_FAILED"
        );

        let mut extra = reference.to_value();
        extra["path"] = json!("/private/escape");
        assert_eq!(
            store.resolve(&extra).unwrap_err().code,
            "ARTIFACT.INVALID_REF"
        );

        store.cleanup();
        assert_eq!(
            store.resolve(&reference.to_value()).unwrap_err().code,
            "ARTIFACT.STORE_CLOSED"
        );
        assert_eq!(
            store
                .import_bytes(PNG_1X1, Some("image/png"))
                .unwrap_err()
                .code,
            "ARTIFACT.STORE_CLOSED"
        );
    }

    #[test]
    fn forged_oversized_size_is_not_a_valid_reference() {
        let store = ArtifactStore::default();
        let reference = store.import_bytes(PNG_1X1, Some("image/png")).unwrap();
        let mut forged = reference.to_value();
        forged["sizeBytes"] = json!(u64::MAX);
        assert_eq!(
            store.resolve(&forged).unwrap_err().code,
            "ARTIFACT.INVALID_REF"
        );
    }
}

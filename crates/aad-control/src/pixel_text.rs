//! Read text from a provider-verified PNG on a single request deadline.
use crate::{error, Result};
use aad_ocr::TextLayout;
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) fn read_capture(
    language: Option<&str>,
    timeout: Duration,
    layout: TextLayout,
    capture: impl FnOnce(Duration) -> Result<Value>,
) -> Result<Value> {
    use base64::Engine as _;
    let until = std::time::Instant::now() + timeout;
    let left = || {
        until
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| error("CONTROL.TIMEOUT", "request deadline elapsed"))
    };
    let engine = tesseract()?;
    let languages = match language {
        None => Vec::new(),
        Some(tag) => {
            let wanted = tesseract_language(tag);
            let available = engine.languages(left()?).map_err(ocr_unavailable)?;
            match wanted.filter(|code| available.iter().any(|have| have == code)) {
                Some(code) => vec![code.to_string()],
                None => {
                    return Err(error(
                        "DRIVER.OCR_UNAVAILABLE",
                        format!("no installed Tesseract language data reads {tag}"),
                    )
                    .with_detail("available_languages", json!(available)))
                }
            }
        }
    };
    let shot = capture(left()?)?;
    let png = shot["data"]
        .as_str()
        .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "capture returned no PNG"))?;
    let mut read = engine
        .recognize_png_layout(&png, &languages, left()?, layout)
        .map_err(ocr_unavailable)?;
    for key in ["format", "data", "width", "height", "clipped"] {
        read[key] = shot[key].clone();
    }
    Ok(read)
}

pub(crate) fn tesseract() -> Result<aad_ocr::OcrProvider> {
    let engine = aad_ocr::OcrProvider::new().map_err(ocr_unavailable)?;
    engine.engine_ready().map_err(ocr_unavailable)?;
    Ok(engine)
}

/// An engine that is missing or cannot be sandboxed is "no OCR here", the
/// same answer Windows gives for a missing language pack.
fn ocr_unavailable(cause: aad_core::AutomationError) -> aad_core::AutomationError {
    match cause.code.as_str() {
        "OCR.ENGINE_UNAVAILABLE" | "OCR.ENGINE_ISOLATION_UNAVAILABLE" => error(
            "DRIVER.OCR_UNAVAILABLE",
            "Tesseract OCR is not available on this host",
        )
        .with_detail("reason", json!(cause.message))
        .with_detail("engine", json!("tesseract")),
        "OCR.TIMEOUT" => error("CONTROL.TIMEOUT", cause.message.clone()).with_cause(cause),
        _ => error("DRIVER.OCR_FAILED", "Tesseract could not read the image").with_cause(cause),
    }
}

/// A BCP-47 tag to the Tesseract traineddata that reads it.
pub fn tesseract_language(tag: &str) -> Option<&'static str> {
    let lower = tag.to_ascii_lowercase();
    let primary = lower.split('-').next().unwrap_or_default();
    Some(match primary {
        "en" => "eng",
        "de" => "deu",
        "fr" => "fra",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "nl" => "nld",
        "ru" => "rus",
        "uk" => "ukr",
        "pl" => "pol",
        "tr" => "tur",
        "ja" => "jpn",
        "ko" => "kor",
        "ar" => "ara",
        "he" => "heb",
        "hi" => "hin",
        "zh" if lower.contains("hant") || lower.ends_with("-tw") || lower.ends_with("-hk") => {
            "chi_tra"
        }
        "zh" => "chi_sim",
        _ => return None,
    })
}

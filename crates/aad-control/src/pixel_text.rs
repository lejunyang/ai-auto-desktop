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
    let languages = languages(&engine, language, left()?)?;
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

fn languages(
    engine: &aad_ocr::OcrProvider,
    tag: Option<&str>,
    timeout: Duration,
) -> Result<Vec<String>> {
    let Some(tag) = tag else {
        return Ok(Vec::new());
    };
    let available = engine.languages(timeout).map_err(ocr_unavailable)?;
    tesseract_language(tag)
        .filter(|code| available.iter().any(|have| have == code))
        .map(|code| vec![code.to_owned()])
        .ok_or_else(|| {
            error(
                "DRIVER.OCR_UNAVAILABLE",
                format!("no installed Tesseract language data reads {tag}"),
            )
            .with_detail("available_languages", json!(available))
        })
}

/// OCR from the interiors of detected shapes. Masking preserves the original
/// dimensions, so the resulting word boxes use the same canvas fractions.
pub(crate) fn flowchart_labels(
    image: &image::RgbaImage,
    tag: Option<&str>,
    deadline: std::time::Instant,
) -> Result<Value> {
    use image::ImageEncoder;
    let left = || {
        deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| error("CONTROL.TIMEOUT", "canvas label recognition timed out"))
    };
    let engine = tesseract()?;
    let languages = languages(&engine, tag, left()?)?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ColorType::Rgba8,
        )
        .map_err(|_| {
            error(
                "OCR.IMAGE_UNSUPPORTED",
                "could not encode the flowchart label image",
            )
        })?;
    let mut read = engine
        .recognize_png_layout(&png, &languages, left()?, TextLayout::Sparse)
        .map_err(ocr_unavailable)?;
    read["width"] = json!(image.width());
    read["height"] = json!(image.height());
    Ok(crate::text_answer(&read, crate::browser::PROVIDER)["lines"].take())
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

//! Windows' built-in text recognition (Windows.Media.Ocr).
use aad_platform::{Error, ImageBox, Pixels, Recognition, TextSpan};
use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

type Result<T> = std::result::Result<T, Error>;

pub fn ocr_failed(what: &str, error: windows::core::Error) -> Error {
    Error::new("DRIVER.OCR_FAILED", format!("{what}: {error}"))
}

/// The OCR languages installed on this machine, by tag.
pub fn ocr_languages() -> Vec<String> {
    OcrEngine::AvailableRecognizerLanguages()
        .ok()
        .map(|languages| {
            languages
                .into_iter()
                .filter_map(|language| language.LanguageTag().ok())
                .map(|tag| tag.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Windows' built-in OCR (Windows.Media.Ocr): offline, no extra engine to
/// install, one recogniser per installed OCR language pack.
pub fn recognize(pixels: &Pixels, language: Option<&str>) -> Result<Recognition> {
    crate::com::ensure();
    let available = ocr_languages();
    let unavailable = |message: &str| {
        Error::new("DRIVER.OCR_UNAVAILABLE", message.to_string())
            .with_detail("available_languages", serde_json::json!(available))
    };
    let engine = match language {
        Some(tag) => {
            let language = Language::CreateLanguage(&HSTRING::from(tag))
                .map_err(|_| unavailable("the language tag is not one Windows knows"))?;
            if !OcrEngine::IsLanguageSupported(&language).unwrap_or(false) {
                return Err(unavailable(
                    "no OCR language pack for that language is installed",
                ));
            }
            OcrEngine::TryCreateFromLanguage(&language)
                .map_err(|_| unavailable("the OCR engine for that language could not start"))?
        }
        // The user's profile languages first; an account whose languages have
        // no OCR pack falls back to the first pack that is installed.
        None => match OcrEngine::TryCreateFromUserProfileLanguages() {
            Ok(engine) => engine,
            Err(_) => {
                let first = available
                    .first()
                    .ok_or_else(|| unavailable("no OCR language pack is installed"))?;
                let language = Language::CreateLanguage(&HSTRING::from(first.as_str()))
                    .map_err(|error| ocr_failed("OCR language", error))?;
                OcrEngine::TryCreateFromLanguage(&language)
                    .map_err(|_| unavailable("no OCR engine could start"))?
            }
        },
    };
    let limit = OcrEngine::MaxImageDimension().unwrap_or(10_000);
    if pixels.width > limit || pixels.height > limit {
        return Err(Error::new(
            "DRIVER.ACTION_UNSUPPORTED",
            format!("OCR reads images of at most {limit} pixels a side; read a smaller element"),
        ));
    }
    let bgra: Vec<u8> = pixels
        .rgba
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    let writer = DataWriter::new().map_err(|error| ocr_failed("image buffer", error))?;
    writer
        .WriteBytes(&bgra)
        .map_err(|error| ocr_failed("image buffer", error))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|error| ocr_failed("image buffer", error))?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        pixels.width as i32,
        pixels.height as i32,
    )
    .map_err(|error| ocr_failed("bitmap", error))?;
    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.get())
        .map_err(|error| ocr_failed("recognition", error))?;
    let span = |text: HSTRING, rect: windows::Foundation::Rect| TextSpan {
        text: text.to_string(),
        bounds: ImageBox {
            x: f64::from(rect.X),
            y: f64::from(rect.Y),
            width: f64::from(rect.Width),
            height: f64::from(rect.Height),
        },
    };
    let mut lines = Vec::new();
    for line in result
        .Lines()
        .map_err(|error| ocr_failed("recognition", error))?
    {
        let words: Vec<TextSpan> = line
            .Words()
            .map(|words| {
                words
                    .into_iter()
                    .filter_map(|word| Some(span(word.Text().ok()?, word.BoundingRect().ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        // A line's box is the union of its words'.
        let (mut left, mut top, mut right, mut bottom) = (f64::MAX, f64::MAX, 0.0f64, 0.0f64);
        for word in &words {
            left = left.min(word.bounds.x);
            top = top.min(word.bounds.y);
            right = right.max(word.bounds.x + word.bounds.width);
            bottom = bottom.max(word.bounds.y + word.bounds.height);
        }
        if words.is_empty() {
            continue;
        }
        let text = line.Text().map(|text| text.to_string()).unwrap_or_default();
        lines.push((
            TextSpan {
                text,
                bounds: ImageBox {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                },
            },
            words,
        ));
    }
    let language = engine
        .RecognizerLanguage()
        .and_then(|language| language.LanguageTag())
        .map(|tag| tag.to_string())
        .unwrap_or_default();
    Ok(Recognition {
        engine: "windows.media.ocr".into(),
        language,
        lines,
    })
}

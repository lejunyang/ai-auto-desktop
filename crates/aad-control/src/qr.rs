//! Decode QR evidence without returning or logging its challenge payload.
use crate::{error, Result};
use base64::Engine;
use image::{DynamicImage, GenericImageView, ImageOutputFormat};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Cursor;

impl crate::Session {
    pub(crate) fn watch_qr(
        &self,
        resource: &str,
        locator: &crate::Locator,
        expired: Option<&crate::Locator>,
        confirmed: Option<&crate::Locator>,
        previous: Option<&str>,
        context: &crate::Context,
    ) -> Result<Value> {
        if locator.is_empty()
            || expired.is_some_and(crate::Locator::is_empty)
            || confirmed.is_some_and(crate::Locator::is_empty)
            || previous.is_some_and(|v| v.len() > 256)
        {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "nonempty QR/state locators and bounded previous version required",
            ));
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(5).min(context.remaining()?);
        loop {
            context.remaining()?;
            let (id, observation) = self.observe_resource(resource, 2000, context)?;
            let matches = |locator: &crate::Locator| {
                self.find_in(&id, &observation, locator, crate::Expect::Optional, context)
            };
            for (state, condition) in [("confirmed", confirmed), ("expired", expired)] {
                if let Some(condition) = condition {
                    let evidence = matches(condition)?;
                    if evidence["found"] == true {
                        context.remaining()?;
                        return Ok(
                            json!({"event":if previous==Some(state){"unchanged"}else{state}, "state":state,
                            "version":state,"stop":state=="confirmed", "evidence":{"observation_id":id,"target":evidence["target"]}}),
                        );
                    }
                }
            }
            let found = matches(locator)?;
            let mut evidence = if found["found"] == true {
                let target: crate::Target = serde_json::from_value(found["target"].clone())
                    .map_err(|_| error("QR.BAD_TARGET", "missing capture target"))?;
                let shot = self.dispatch(&crate::Operation::Capture { target }, context)?;
                if shot["clipped"] == true {
                    return Err(error(
                        "QR.CLIPPED",
                        "capture is clipped; choose a smaller container",
                    ));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(shot["data"].as_str().unwrap_or_default())
                    .map_err(|_| error("QR.INVALID_IMAGE", "invalid capture encoding"))?;
                identify_cancellable(bytes, context)?
            } else {
                json!({"status":"not_found", "candidates":0})
            };
            context.remaining()?;
            let version = evidence["digest"]
                .as_str()
                .or_else(|| evidence["status"].as_str())
                .unwrap()
                .to_string();
            if previous != Some(&version) {
                evidence["event"] = json!(if evidence["status"] == "identified" {
                    "changed"
                } else {
                    evidence["status"].as_str().unwrap()
                });
                evidence["version"] = json!(version);
                evidence["stop"] = json!(false);
                return Ok(evidence);
            }
            if std::time::Instant::now() + std::time::Duration::from_millis(250) >= deadline {
                return Ok(json!({"event":"unchanged","version":version,"stop":false}));
            }
            for _ in 0..10 {
                context.remaining()?;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
}

/// Private CLI worker entry: PNG in, bounded JSON out. No files or decoded
/// payloads are retained. Embedders must ship the same-source aad executable.
pub fn worker() -> std::io::Result<()> {
    use std::io::{Read, Write};
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    #[cfg(feature = "qr-test-hooks")]
    if let Ok(marker) = std::env::var("AAD_TEST_QR_BARRIER") {
        std::fs::write(&marker, b"entered")?;
        while !std::path::Path::new(&format!("{marker}.release")).exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let value = match identify(&bytes) {
        Ok(value) => json!({"result":value}),
        Err(e) => json!({"error":{"code":e.code,"message":e.message}}),
    };
    std::io::stdout().write_all(value.to_string().as_bytes())
}

fn identify_cancellable(bytes: Vec<u8>, context: &crate::Context) -> Result<Value> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    if ACTIVE
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < 2).then_some(n + 1)
        })
        .is_err()
    {
        return Err(error("QR.BUSY", "two decoders are already active"));
    }
    struct Permit;
    impl Drop for Permit {
        fn drop(&mut self) {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _permit = Permit;
    context.remaining()?;
    let executable = std::env::current_exe()
        .map_err(|_| error("QR.WORKER_UNAVAILABLE", "cannot locate aad decoder"))?;
    let executable = executable.with_file_name(if cfg!(windows) { "aad.exe" } else { "aad" });
    let child = Command::new(executable)
        .arg("--aad-qr-decode")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            error(
                "QR.WORKER_UNAVAILABLE",
                "same-source aad decoder is required beside the host",
            )
        })?;
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(child);
    let mut input = child.0.stdin.take().unwrap();
    let mut output = child.0.stdout.take().unwrap();
    let sender = std::thread::spawn(move || input.write_all(&bytes));
    let receiver = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output
            .by_ref()
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    loop {
        // Killing the isolated process interrupts PNG preparation, grid search,
        // decoding and PNG encoding, including library code without checkpoints.
        context.remaining()?;
        if child
            .0
            .try_wait()
            .map_err(|_| error("QR.WORKER_FAILED", "decoder wait failed"))?
            .is_some()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    sender
        .join()
        .map_err(|_| error("QR.WORKER_FAILED", "decoder input failed"))?
        .map_err(|_| error("QR.WORKER_FAILED", "decoder input closed"))?;
    let bytes = receiver
        .join()
        .map_err(|_| error("QR.WORKER_FAILED", "decoder output failed"))?
        .map_err(|_| error("QR.WORKER_FAILED", "decoder output closed"))?;
    context.remaining()?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(error("QR.IMAGE_LIMIT", "decoder output exceeds limit"));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| error("QR.WORKER_FAILED", "invalid decoder response"))?;
    if let Some(failure) = value.get("error") {
        return Err(error(
            failure["code"].as_str().unwrap_or("QR.WORKER_FAILED"),
            failure["message"].as_str().unwrap_or("decoder failed"),
        ));
    }
    Ok(value["result"].clone())
}

pub fn identify(png: &[u8]) -> Result<Value> {
    if png.len() > 16 * 1024 * 1024 {
        return Err(error("QR.IMAGE_LIMIT", "image exceeds 16 MiB"));
    }
    let reader = image::io::Reader::new(Cursor::new(png))
        .with_guessed_format()
        .map_err(|_| error("QR.INVALID_IMAGE", "unrecognised image"))?;
    let (w, h) = reader
        .into_dimensions()
        .map_err(|_| error("QR.INVALID_IMAGE", "invalid image dimensions"))?;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > 16_000_000 {
        return Err(error("QR.IMAGE_LIMIT", "image exceeds 16 megapixels"));
    }
    let image = image::load_from_memory(png)
        .map_err(|_| error("QR.INVALID_IMAGE", "image decode failed"))?;
    let gray = image.to_luma8();
    let mut prepared =
        rqrr::PreparedImage::prepare_from_greyscale(w as usize, h as usize, |x, y| {
            gray.get_pixel(x as u32, y as u32)[0]
        });
    let grids = prepared.detect_grids();
    let valid: Vec<_> = grids.iter().filter(|grid| grid.decode().is_ok()).collect();
    if valid.len() != 1 {
        return Ok(
            json!({"status":if valid.is_empty(){"not_found"}else{"ambiguous"},"candidates":valid.len()}),
        );
    }
    let bounds = &valid[0].bounds;
    let left = bounds.iter().map(|p| p.x).min().unwrap().max(0) as u32;
    let top = bounds.iter().map(|p| p.y).min().unwrap().max(0) as u32;
    let right = (bounds.iter().map(|p| p.x).max().unwrap().max(0) as u32 + 1).min(w);
    let bottom = (bounds.iter().map(|p| p.y).max().unwrap().max(0) as u32 + 1).min(h);
    let crop = image.view(left, top, right - left, bottom - top).to_image();
    // A generous white border preserves a quiet zone without exposing nearby page text.
    let border = ((right - left).max(bottom - top) / 4).max(16);
    let mut padded = image::RgbaImage::from_pixel(
        crop.width() + 2 * border,
        crop.height() + 2 * border,
        image::Rgba([255, 255, 255, 255]),
    );
    image::imageops::overlay(&mut padded, &crop, border.into(), border.into());
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(padded)
        .write_to(&mut output, ImageOutputFormat::Png)
        .map_err(|_| error("QR.ENCODE_FAILED", "failed to encode QR evidence"))?;
    let bytes = output.into_inner();
    Ok(
        json!({"status":"identified","candidates":1,"mime_type":"image/png",
        "digest":format!("{:x}",Sha256::digest(&bytes)),
        "data":base64::engine::general_purpose::STANDARD.encode(bytes)}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn code(text: &str) -> image::RgbaImage {
        let qr = qrcode::QrCode::new(text).unwrap();
        let size = qr.width();
        image::RgbaImage::from_fn(((size + 8) * 8) as u32, ((size + 8) * 8) as u32, |x, y| {
            let (x, y) = (x as usize / 8, y as usize / 8);
            let dark = x >= 4
                && y >= 4
                && x < size + 4
                && y < size + 4
                && qr[(x - 4, y - 4)] == qrcode::Color::Dark;
            image::Rgba(if dark {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            })
        })
    }
    fn png(image: image::RgbaImage) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut out, ImageOutputFormat::Png)
            .unwrap();
        out.into_inner()
    }
    #[test]
    fn real_codes_round_trip_without_payload_and_reject_ambiguity() {
        let first = code("fixture-only-ticket-one");
        let bytes = png(first.clone());
        let result = identify(&bytes).unwrap();
        assert_eq!(result["status"], "identified");
        assert!(!result.to_string().contains("fixture-only-ticket"));
        let delivered = base64::engine::general_purpose::STANDARD
            .decode(result["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(identify(&delivered).unwrap()["status"], "identified");
        let image = image::load_from_memory(&delivered).unwrap().to_rgba8();
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 255, 255]);
        let second = png(code("fixture-only-ticket-two"));
        assert_ne!(result["digest"], identify(&second).unwrap()["digest"]);
        let mut pair = image::RgbaImage::from_pixel(
            first.width() * 2 + 32,
            first.height(),
            image::Rgba([255, 255, 255, 255]),
        );
        image::imageops::overlay(&mut pair, &first, 0, 0);
        image::imageops::overlay(&mut pair, &first, i64::from(first.width() + 32), 0);
        assert_eq!(identify(&png(pair)).unwrap()["status"], "ambiguous");
        if let Ok(root) = std::env::var("AAD_QR_FIXTURES") {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(std::path::Path::new(&root).join("one.png"), bytes).unwrap();
            std::fs::write(std::path::Path::new(&root).join("two.png"), second).unwrap();
        }
    }
}

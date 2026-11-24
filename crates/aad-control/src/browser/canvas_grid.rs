use super::*;
use base64::Engine;
use image::RgbaImage;
use sha2::{Digest, Sha256};

fn unproven() -> aad_core::AutomationError {
    error(
        "CONTROL.CANVAS_SCOPE_UNPROVEN",
        "cannot prove the screenshot contains exactly the visible canvas bitmap",
    )
}

pub(super) fn scope(client: &mut Client, context: &Ctx, args: &Value, mode: &str) -> Result<Value> {
    let expression = format!("{}({args},{})", include_str!("canvas_grid.js"), json!(mode));
    let result = run(
        client,
        context.session.as_deref(),
        "Runtime.evaluate",
        json!({
            "contextId":context.id,"expression":expression,"awaitPromise":true,"returnByValue":true
        }),
    )?;
    checked(evaluated(result)?)
}

impl BrowserBackend {
    pub(crate) fn visible_canvas(
        &self,
        snapshot: &Value,
        node: &Value,
        timeout: Duration,
    ) -> Result<(RgbaImage, Value)> {
        if node["tag"] != "canvas" || node["states"]["protected"] == true {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "choose one observed canvas",
            ));
        }
        self.with_capture_target(snapshot, node, timeout, capture_region)
    }
}

pub(super) fn capture_region(
    client: &mut Client,
    context: &Ctx,
    args: &Value,
) -> Result<(RgbaImage, Value)> {
    let rect = scope(client, context, args, "geometry")?;
    let (width, height) = (
        rect["rw"].as_u64().ok_or_else(unproven)?,
        rect["rh"].as_u64().ok_or_else(unproven)?,
    );
    if !(1..=2048).contains(&width) || !(1..=2048).contains(&height) || width * height > 2_000_000 {
        return Err(unproven());
    }
    // CDP screenshot clips may temporarily override another session's device
    // metrics. At non-unit scale read only the checked canvas backing region.
    let mut shot = if rect["sourceView"] == true {
        let mut reference = scope(client, context, args, "reference")?;
        if reference["rect"] != rect {
            return Err(unproven());
        }
        json!({"data":reference["data"].take()})
    } else {
        command(
            client,
            "Page.captureScreenshot",
            json!({"format":"png","captureBeyondViewport":false,
            "clip":{"x":rect["x"],"y":rect["y"],"width":width,"height":height,"scale":1}}),
        )?
    };
    let proof = scope(client, context, args, "proof")?;
    let image = decode(&shot["data"], width, height)?;
    if proof["rect"] != rect || proof["sha256"] != format!("{:x}", Sha256::digest(image.as_raw())) {
        return Err(unproven());
    }
    shot["format"] = json!("png");
    shot["width"] = json!(width);
    shot["height"] = json!(height);
    let region = &rect["region"];
    shot["clipped"] = json!(region["fw"].as_f64() != Some(1.) || region["fh"].as_f64() != Some(1.));
    shot["canvas_region"] = region.clone();
    shot["canvas_proof"] = proof;
    Ok((image, shot))
}

fn decode(encoded: &Value, width: u64, height: u64) -> Result<RgbaImage> {
    let encoded = encoded
        .as_str()
        .filter(|s| s.len() <= 12_000_000)
        .ok_or_else(unproven)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| unproven())?;
    let reader =
        || image::io::Reader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png);
    let dimensions = reader().into_dimensions().map_err(|_| unproven())?;
    if dimensions != (width as u32, height as u32) {
        return Err(unproven()
            .with_detail("actual_size", json!(dimensions))
            .with_detail("expected_size", json!([width, height])));
    }
    Ok(reader().decode().map_err(|_| unproven())?.into_rgba8())
}

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

fn scope(client: &mut Client, context: &Ctx, args: &Value, verify_pixels: bool) -> Result<Value> {
    let expression = format!("{}({args},{verify_pixels})", include_str!("canvas_grid.js"));
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
    ) -> Result<RgbaImage> {
        if node["tag"] != "canvas" || node["states"]["protected"] == true {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "choose one observed canvas",
            ));
        }
        self.with_capture_target(snapshot, node, timeout, |client, context, args| {
            let rect = scope(client, context, args, false)?;
            let (width, height) = (
                rect["width"].as_u64().ok_or_else(unproven)?,
                rect["height"].as_u64().ok_or_else(unproven)?,
            );
            if !(1..=2048).contains(&width)
                || !(1..=2048).contains(&height)
                || width * height > 2_000_000
            {
                return Err(unproven());
            }
            let shot = command(
                client,
                "Page.captureScreenshot",
                json!({"format":"png","captureBeyondViewport":false,
                "clip":{"x":rect["x"],"y":rect["y"],"width":width,"height":height,"scale":1}}),
            )?;
            let proof = scope(client, context, args, true)?;
            if proof["rect"] != rect {
                return Err(unproven());
            }
            let encoded = shot["data"]
                .as_str()
                .filter(|s| s.len() <= 12_000_000)
                .ok_or_else(unproven)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| unproven())?;
            let reader = || {
                image::io::Reader::with_format(
                    std::io::Cursor::new(&bytes),
                    image::ImageFormat::Png,
                )
            };
            if reader().into_dimensions().map_err(|_| unproven())? != (width as u32, height as u32)
            {
                return Err(unproven());
            }
            let image = reader().decode().map_err(|_| unproven())?.into_rgba8();
            if proof["sha256"] != format!("{:x}", Sha256::digest(image.as_raw())) {
                return Err(unproven());
            }
            Ok(image)
        })
    }
}

//! Read one declared equal-cell layout from a verified, visible canvas.
use crate::{browser, error, Context, Expect, Locator, Result, Session, Target};
use aad_ocr::OcrProvider;
use image::{imageops, DynamicImage, ImageOutputFormat, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Cursor;
use std::time::Duration;

const MAX_CELLS: usize = 64;
const MAX_CELL_CHARS: usize = 256;
const CONFIDENCE_THRESHOLD: f64 = 0.80;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub locator: Locator,
    pub rows: u32,
    pub columns: Vec<String>,
}

impl Session {
    pub(crate) fn read_canvas_grid(
        &self,
        resource: &str,
        spec: &Spec,
        context: &Context,
    ) -> Result<Value> {
        crate::table::validate_columns(&spec.columns)?;
        if spec.locator.is_empty() || !(1..=256).contains(&spec.rows) {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "choose a canvas locator and 1..256 rows across the full canvas",
            ));
        }
        if self.readable(resource)?.0.name() != browser::PROVIDER {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "canvas grid OCR requires a Chromium resource",
            ));
        }
        let (id, observation) = self.observe_resource(resource, crate::MAX_NODES, context)?;
        let found = self.find_in(&id, &observation, &spec.locator, Expect::One, context)?;
        let target: Target = serde_json::from_value(found["target"].clone()).map_err(|_| {
            error(
                "CONTROL.BAD_PROVIDER_OUTPUT",
                "missing observed canvas target",
            )
        })?;
        let (observation, node) = self.observed_node(&target)?;
        let browser = self.browser.clone().ok_or_else(|| {
            error(
                "CONTROL.ACTION_UNSUPPORTED",
                "Chromium capture is unavailable",
            )
        })?;
        let timeout = context.remaining()?;
        let (image, shot) = crate::capture_cancellable(context, move || {
            browser.visible_canvas(&observation.raw, &node, timeout)
        })?;
        context.remaining()?;
        let view = crate::canvas_objects::Region::read(&shot["canvas_region"])?;
        let mut result = read_cells(&image, view, spec, context)?;
        result["visible_region"] = json!(view);
        result["target"] = json!(target);
        Ok(result)
    }
}

fn read_cells(
    image: &RgbaImage,
    view: crate::canvas_objects::Region,
    spec: &Spec,
    context: &Context,
) -> Result<Value> {
    let (width, height) = image.dimensions();
    let columns = spec.columns.len() as u32;
    if f64::from(height) / (view.fh * f64::from(spec.rows)) < 4.0
        || f64::from(width) / (view.fw * f64::from(columns)) < 4.0
    {
        return Err(error(
            "CONTROL.INVALID_REQUEST",
            "each canvas cell needs at least four pixels in each dimension",
        ));
    }
    let declared = (columns * spec.rows) as usize;
    let ocr = OcrProvider::new()?;
    let mut cells = Vec::new();
    let mut reasons = Vec::new();
    if view.fw < 1. || view.fh < 1. {
        reasons.push("viewport_clip");
    }
    let mut uncertain_cells = 0;
    let span = |i: u32, count: u32, origin: f64, range: f64, pixels: u32| {
        let (start, end) = (
            f64::from(i) / f64::from(count),
            f64::from(i + 1) / f64::from(count),
        );
        if start + 1e-9 < origin || end > origin + range + 1e-9 {
            return None;
        }
        let at = |f: f64| {
            (((f - origin) / range * f64::from(pixels) + 1e-9)
                .floor()
                .clamp(0., f64::from(pixels))) as u32
        };
        Some((at(start), at(end) - at(start)))
    };
    for index in 0..declared {
        context.remaining()?;
        let row = index as u32 / columns;
        let col = index as u32 % columns;
        let Some((left, w)) = span(col, columns, view.fx, view.fw, width) else {
            continue;
        };
        let Some((top, h)) = span(row, spec.rows, view.fy, view.fh, height) else {
            continue;
        };
        if cells.len() == MAX_CELLS {
            reasons.push("cell_limit");
            break;
        }
        // The outer pixel of each cell is reserved for a thin grid rule.
        let crop = imageops::crop_imm(image, left + 1, top + 1, w - 2, h - 2).to_image();
        let mut text = String::new();
        let mut confidence: Option<f64> = None;
        if !crop.pixels().all(|pixel| pixel == crop.get_pixel(0, 0)) {
            let mut png = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(crop)
                .write_to(&mut png, ImageOutputFormat::Png)
                .map_err(|_| error("OCR.IMAGE_UNSUPPORTED", "cannot encode canvas cell"))?;
            let read = ocr.recognize_line_png(
                png.get_ref(),
                context.remaining()?.min(Duration::from_secs(8)),
            )?;
            for word in read["lines"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|line| line["words"].as_array().into_iter().flatten())
            {
                let value = word["text"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(value);
                let score = word["confidence"].as_f64().unwrap_or(0.0);
                confidence = Some(confidence.map_or(score, |prior| prior.min(score)));
            }
        }
        let truncated = text.chars().count() > MAX_CELL_CHARS;
        if truncated && !reasons.contains(&"cell_text_limit") {
            reasons.push("cell_text_limit");
        }
        let uncertain = truncated || confidence.is_none_or(|c| c < CONFIDENCE_THRESHOLD);
        uncertain_cells += usize::from(uncertain);
        cells.push(json!({"row":row+1,"column":spec.columns[col as usize],
            "bounds":{"left":left,"top":top,"width":w,"height":h},
            "text":(!text.is_empty()).then(||text.chars().take(MAX_CELL_CHARS).collect::<String>()),
            "confidence":confidence,"uncertain":uncertain,"truncated":truncated}));
    }
    context.remaining()?;
    Ok(
        json!({"scope":"visible_canvas","dataset_complete":null,"source":"tesseract",
        "width":width,"height":height,"rows":spec.rows,"columns":spec.columns,
        "declared_cells":declared,"returned_cells":cells.len(),"cells":cells,
        "confidence_threshold":CONFIDENCE_THRESHOLD,"uncertain_cells":uncertain_cells,
        "truncated":!reasons.is_empty(),"truncation_reasons":reasons}),
    )
}

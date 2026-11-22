use super::*;
use crate::canvas_objects::{Options, Query, Source};
use sha2::{Digest, Sha256};

pub(crate) struct Scene {
    pub(crate) public: Value,
    pub(super) guard: Guard,
}

pub(super) enum Guard {
    Editor(Value),
    Pixels(String),
}

fn editor(client: &mut Client, ctx: &Ctx, args: &Value) -> Result<Value> {
    checked(eval(
        client,
        ctx,
        format!("{}({args})", include_str!("editor_scene.js")),
    )?)
}

impl Guard {
    /// Re-read after pointer arrival, since hover handlers can redraw a canvas.
    pub(super) fn check(&self, client: &mut Client, ctx: &Ctx, args: &Value) -> Result<()> {
        let same = match self {
            Self::Editor(graph) => editor(client, ctx, args)? == *graph,
            Self::Pixels(hash) => canvas_grid::scope(client, ctx, args, true)?["sha256"] == *hash,
        };
        if same {
            Ok(())
        } else {
            Err(stale())
        }
    }
}

fn stale() -> AutomationError {
    error(
        "CONTROL.STALE_TARGET",
        "canvas objects changed; read the canvas again",
    )
}

impl BrowserBackend {
    pub(crate) fn read_objects(
        &self,
        snapshot: &Value,
        node: &Value,
        options: &Options,
        timeout: Duration,
    ) -> Result<Scene> {
        let deadline = Instant::now() + timeout;
        let left = || {
            deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| error("CONTROL.TIMEOUT", "canvas object recognition timed out"))
        };
        let source = options.source()?;
        if node["tag"] != "canvas" || node["states"]["protected"] == true {
            return Err(error(
                "CONTROL.ACTION_UNSUPPORTED",
                "choose one observed canvas",
            ));
        }
        if matches!(source, Source::Auto | Source::Editor) {
            let graph = self.with_capture_target(snapshot, node, left()?, editor)?;
            if !graph.is_null() {
                let mut scene = Scene::new(graph.clone(), "editor", Guard::Editor(graph));
                if options.preview {
                    let (_, shot) = self.visible_canvas(snapshot, node, left()?)?;
                    self.with_capture_target(snapshot, node, left()?, |c, ctx, args| {
                        scene.guard.check(c, ctx, args)
                    })?;
                    scene.preview(&shot)?;
                }
                return Ok(scene);
            }
            if source == Source::Editor {
                return Err(error(
                    "CONTROL.ACTION_UNSUPPORTED",
                    "this canvas has no published editor scene",
                ));
            }
        }
        let (image, shot) = self.visible_canvas(snapshot, node, left()?)?;
        let guard = Guard::Pixels(format!("{:x}", Sha256::digest(image.as_raw())));
        if source == Source::Vision {
            let graph =
                canvas_vision::detect(&shot, options.description.as_deref().unwrap(), left()?)?;
            let mut scene = Scene::new(graph, "vision", guard);
            if options.preview {
                scene.preview(&shot)?;
            }
            return Ok(scene);
        }
        let mut label_source = Value::Null;
        let mut graph =
            flowchart::detect(&image, |mask| {
                match crate::pixel_text::flowchart_labels(
                    mask,
                    options.language.as_deref(),
                    deadline,
                ) {
                    Ok(lines) => {
                        label_source = json!("tesseract");
                        Ok(lines)
                    }
                    Err(err)
                        if err.code == "DRIVER.OCR_UNAVAILABLE" && options.language.is_none() =>
                    {
                        label_source = json!("unavailable");
                        Ok(json!([]))
                    }
                    Err(err) => Err(err),
                }
            })?;
        left()?;
        graph["label_source"] = label_source;
        let mut scene = Scene::new(graph, "flowchart", guard);
        if options.preview {
            scene.preview(&shot)?;
        }
        Ok(scene)
    }
}

impl Scene {
    /// Render only numeric annotations and a previously verified PNG into the preview.
    fn preview(&mut self, shot: &Value) -> Result<()> {
        use std::fmt::Write as _;
        let bad = || error("CONTROL.BAD_PROVIDER_OUTPUT", "invalid canvas preview");
        let w = shot["width"].as_u64().ok_or_else(bad)?;
        let h = shot["height"].as_u64().ok_or_else(bad)?;
        let png = shot["data"].as_str().ok_or_else(bad)?;
        let mut svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}"><image width="{w}" height="{h}" href="data:image/png;base64,{png}"/>"#
        );
        for (index, obj) in self.public["objects"]
            .as_array_mut()
            .ok_or_else(bad)?
            .iter_mut()
            .enumerate()
        {
            let b = &obj["box"];
            let px =
                |key: &str, span: u64| b[key].as_f64().map(|f| f * span as f64).ok_or_else(bad);
            let (x, y, width, height) = (px("fx", w)?, px("fy", h)?, px("fw", w)?, px("fh", h)?);
            let n = index + 1;
            let tx = (x + 2.).min((w as f64 - 18.).max(0.));
            let ty = (y + 15.).min(h as f64);
            write!(svg, r##"<rect x="{x:.2}" y="{y:.2}" width="{width:.2}" height="{height:.2}" fill="none" stroke="#0068ff" stroke-width="2"/><text x="{tx:.2}" y="{ty:.2}" font-family="sans-serif" font-weight="bold" font-size="15" fill="#00308b" stroke="white" stroke-width="3" paint-order="stroke">{n}</text>"##).unwrap();
            obj["number"] = json!(n);
        }
        svg.push_str("</svg>");
        self.public["preview_svg"] = json!(svg);
        Ok(())
    }
    fn new(mut graph: Value, source: &str, guard: Guard) -> Self {
        graph["source"] = json!(source);
        let mut hash = Sha256::new();
        hash.update(graph.to_string());
        if let Guard::Pixels(pixels) = &guard {
            hash.update(pixels);
        }
        graph["digest"] = json!(format!("{:x}", hash.finalize()));
        Self {
            public: graph,
            guard,
        }
    }

    pub(super) fn click(&self, query: &Query) -> Result<Action> {
        let fields = [
            &query.id,
            &query.label,
            &query.kind,
            &query.after,
            &query.before,
        ];
        if fields.iter().any(|f| {
            f.as_ref()
                .is_some_and(|s| s.trim().is_empty() || s.len() > 1024)
        }) || (fields.iter().all(|f| f.is_none())
            && query.region.is_none()
            && self.public["source"] != "vision")
            || (query.id.is_some() && query.digest.is_none() && self.public["source"] != "editor")
        {
            return Err(error("CONTROL.INVALID_REQUEST", "choose an object by label, kind, relation, visual description, or an id and its read digest"));
        }
        if query
            .min_confidence
            .is_some_and(|score| !(0.0..=1.0).contains(&score) || self.public["source"] != "vision")
        {
            return Err(error(
                "CONTROL.INVALID_REQUEST",
                "min_confidence requires vision and a score from zero to one",
            ));
        }
        if query
            .digest
            .as_ref()
            .is_some_and(|d| self.public["digest"] != *d)
        {
            return Err(stale());
        }
        let objects = self.public["objects"]
            .as_array()
            .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "missing canvas objects"))?;
        let edges = self.public["edges"]
            .as_array()
            .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "missing canvas edges"))?;
        let labels: BTreeMap<_, _> = objects
            .iter()
            .filter_map(|o| Some((o["id"].as_str()?, o["label"].as_str()?)))
            .collect();
        let adjacent = |id: &Value, label: &Option<String>, near: &str, far: &str| {
            label.as_ref().is_none_or(|label| {
                edges.iter().any(|e| {
                    e["status"] == "confirmed"
                        && e[near] == *id
                        && e[far]
                            .as_str()
                            .and_then(|other| labels.get(other))
                            .is_some_and(|name| *name == label)
                })
            })
        };
        let matches: Vec<_> = objects
            .iter()
            .filter(|o| {
                query.id.as_ref().is_none_or(|s| o["id"] == *s)
                    && query.region.is_none_or(|r| r.contains(&o["box"]))
                    && query.min_confidence.is_none_or(|min| {
                        o["confidence"].as_f64().is_some_and(|score| score >= min)
                    })
                    && query.kind.as_ref().is_none_or(|s| o["kind"] == *s)
                    && query.label.as_ref().is_none_or(|s| o["label"] == *s)
                    && adjacent(&o["id"], &query.after, "to", "from")
                    && adjacent(&o["id"], &query.before, "from", "to")
            })
            .collect();
        let object = match matches.as_slice() {
            [] => return Err(error("CONTROL.NOT_FOUND", "no canvas object matched")),
            [object] => object,
            _ => {
                return Err(
                    error("CONTROL.AMBIGUOUS", "multiple canvas objects matched")
                        .with_detail("digest", self.public["digest"].clone())
                        .with_detail("candidates", json!(matches)),
                )
            }
        };
        let bounds = &object["box"];
        let number = |key: &str| {
            bounds[key]
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "invalid object bounds"))
        };
        let (x, y, w, h) = (number("fx")?, number("fy")?, number("fw")?, number("fh")?);
        if w <= 0.0 || h <= 0.0 || x + w > 1.00000001 || y + h > 1.00000001 {
            return Err(error(
                "CONTROL.BAD_PROVIDER_OUTPUT",
                "object falls outside the canvas",
            ));
        }
        let centre = crate::Fraction {
            fx: x + w / 2.0,
            fy: y + h / 2.0,
        };
        for other in objects.iter().filter(|other| other["id"] != object["id"]) {
            let b = &other["box"];
            let contains = |value: f64, start: &str, size: &str| {
                b[start]
                    .as_f64()
                    .zip(b[size].as_f64())
                    .is_some_and(|(s, n)| s <= value && value < s + n)
            };
            if contains(centre.fx, "fx", "fw") && contains(centre.fy, "fy", "fh") {
                return Err(error(
                    "CONTROL.OCCLUDED",
                    "another recognized object overlaps the selected object's centre",
                ));
            }
        }
        Ok(Action::Click {
            fraction: Some(centre),
            count: None,
            button: None,
            modifiers: Vec::new(),
            hold_ms: None,
        })
    }
}

use super::*;
use crate::canvas_objects::{Options, Query, Source};
use sha2::{Digest, Sha256};

pub(crate) struct Scene {
    pub(crate) public: Value,
    pub(super) guard: Guard,
}

pub(super) enum Guard {
    Editor(Value),
    Pixels(Value),
}

fn editor(client: &mut Client, ctx: &Ctx, args: &Value) -> Result<Value> {
    checked(eval(
        client,
        ctx,
        format!("{}({args})", include_str!("editor_scene.js")),
    )?)
}

impl Guard {
    /// Revalidate the observed scene before scrolling its retained canvas.
    pub(super) fn reveal(&self, client: &mut Client, ctx: &Ctx, args: &Value) -> Result<()> {
        self.check(client, ctx, args)?;
        let session = ctx.session.as_deref();
        let node = run(
            client,
            session,
            "Runtime.evaluate",
            json!({
                "contextId":ctx.id,"expression":format!("globalThis.__aadControlV1.element({args})"),
                "returnByValue":false
            }),
        )?;
        let object = node["result"]["objectId"]
            .as_str()
            .filter(|_| node["result"]["subtype"] == "node")
            .ok_or_else(stale)?;
        let result = (|| {
            let model = run(
                client,
                session,
                "DOM.getBoxModel",
                json!({"objectId":object}),
            )?;
            let offset = |axis: &str, fraction: &str| -> Result<f64> {
                let size = model["model"][axis]
                    .as_f64()
                    .filter(|v| *v >= 2.)
                    .ok_or_else(stale)?;
                let f = args["fraction"][fraction].as_f64().ok_or_else(stale)?;
                Ok((size * f - 0.5).clamp(0., size - 1.))
            };
            let rect =
                json!({"x":offset("width","fx")?,"y":offset("height","fy")?,"width":1,"height":1});
            ctx.commit_input(client)?;
            run(
                client,
                session,
                "DOM.scrollIntoViewIfNeeded",
                json!({"objectId":object,"rect":rect}),
            )?;
            Ok(())
        })();
        let _ = run(
            client,
            session,
            "Runtime.releaseObject",
            json!({"objectId":object}),
        );
        result
    }

    /// Re-read after pointer arrival, since hover handlers can redraw a canvas.
    pub(super) fn check(&self, client: &mut Client, ctx: &Ctx, args: &Value) -> Result<()> {
        let same = match self {
            Self::Editor(graph) => editor(client, ctx, args)? == *graph,
            Self::Pixels(proof) => {
                canvas_grid::capture_region(client, ctx, args)?.1["canvas_proof"] == *proof
            }
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
    pub(super) fn resolve_object_click(
        &self,
        snapshot: &Value,
        node: &Value,
        options: &Options,
        query: &Query,
        action: &crate::Context,
    ) -> Result<(Action, Guard)> {
        let read = || self.read_objects(snapshot, node, options, action.remaining()?);
        let mut scene = read()?;
        let click = match (scene.click(query), query.view_hint) {
            (Err(err), Some(hint))
                if err.code == "CONTROL.NOT_FOUND" && scene.public["clipped"] == true =>
            {
                let meta = &snapshot["metadata"];
                if self
                    .browser_input(Instant::now() + action.remaining()?)?
                    .instance
                    != meta["browser_instance_id"]
                {
                    return Err(stale());
                }
                let page_id = meta["page_id"].as_str().ok_or_else(stale)?;
                let frame_id = meta["frame_id"].as_str().ok_or_else(stale)?;
                self.with_page_state(page_id, action.remaining()?, |page| {
                    if let Some(dialog) = &page.dialog {
                        return Err(dialog_open(dialog));
                    }
                    let entry = self.locate(page, frame_id)?;
                    if !entry.main() || entry.frame["loaderId"] != meta["loader_id"] {
                        return Err(stale());
                    }
                    let id = meta["context_id"].as_i64().ok_or_else(stale)?;
                    let mut ctx = Ctx::new(entry.session, id);
                    let frames = frame_map(page)?;
                    ctx.input = Some(InputAccess::new(self.access.clone(), &frames, frame_id)?);
                    ctx.action = Some(action);
                    let args = json!({
                        "document_token": snapshot["document_token"],
                        "observation": snapshot["snapshot_id"],
                        "node_id": node["node_id"],
                        "action": "click_object",
                        "fraction": hint
                    });
                    refresh_closed_roots(&mut page.client, &ctx, Some(&args), true)?;
                    scene.guard.reveal(&mut page.client, &ctx, &args)
                })?;
                scene = read().map_err(|err| err.with_effect("contextual"))?;
                scene
                    .click(query)
                    .map_err(|err| err.with_effect("contextual"))?
            }
            (result, _) => result?,
        };
        Ok((click, scene.guard))
    }

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
        let guard = Guard::Pixels(shot["canvas_proof"].clone());
        if source == Source::Vision {
            let mut graph =
                canvas_vision::detect(&shot, options.description.as_deref().unwrap(), left()?)?;
            project_objects(&mut graph, &shot)?;
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
        project_objects(&mut graph, &shot)?;
        let mut scene = Scene::new(graph, "flowchart", guard);
        if options.preview {
            scene.preview(&shot)?;
        }
        Ok(scene)
    }
}

fn project_objects(graph: &mut Value, shot: &Value) -> Result<()> {
    use crate::canvas_objects::Region;
    let region = Region::read(&shot["canvas_region"])?;
    for object in graph["objects"]
        .as_array_mut()
        .ok_or_else(|| error("CONTROL.BAD_PROVIDER_OUTPUT", "missing canvas objects"))?
    {
        object["box"] = json!(region.project(Region::read(&object["box"])?));
    }
    graph["visible_region"] = shot["canvas_region"].clone();
    graph["clipped"] = shot["clipped"].clone();
    Ok(())
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
        let view = crate::canvas_objects::Region::read(&shot["canvas_region"])?;
        for (index, obj) in self.public["objects"]
            .as_array_mut()
            .ok_or_else(bad)?
            .iter_mut()
            .enumerate()
        {
            let b = &obj["box"];
            let obj_box = crate::canvas_objects::Region::read(b)?;
            let x = (obj_box.fx - view.fx) / view.fw * w as f64;
            let y = (obj_box.fy - view.fy) / view.fh * h as f64;
            let width = obj_box.fw / view.fw * w as f64;
            let height = obj_box.fh / view.fh * h as f64;
            if x + width <= 0. || y + height <= 0. || x >= w as f64 || y >= h as f64 {
                continue;
            }
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
            hash.update(pixels.to_string());
        }
        graph["digest"] = json!(format!("{:x}", hash.finalize()));
        Self {
            public: graph,
            guard,
        }
    }

    pub(super) fn click(&self, query: &Query) -> Result<Action> {
        if query.view_hint.is_some_and(|hint| {
            self.public["source"] != "flowchart"
                || query.digest.is_some()
                || query.id.is_some()
                || !(0.0..=1.0).contains(&hint.fx)
                || !(0.0..=1.0).contains(&hint.fy)
        }) {
            return Err(error("CONTROL.INVALID_REQUEST","view_hint requires a semantic flowchart query and canvas fractions from zero to one"));
        }
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
            input_timing: None,
            fraction: Some(centre),
            count: None,
            button: None,
            modifiers: Vec::new(),
            hold_ms: None,
        })
    }
}

use super::super::{
    canvas_objects::{Guard, Scene},
    flowchart,
};
use aad_browser::recording::{CanvasRecognition, Options, Recorder};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::{atomic::AtomicBool, Arc};
use std::time::{Duration, Instant};

pub fn start_recording(
    options: Options,
    cancel: Option<Arc<AtomicBool>>,
) -> aad_browser::recording::Result<Recorder> {
    Recorder::start_with_objects(
        options,
        cancel,
        Some(CanvasRecognition {
            editor_reader: include_str!("../editor_scene.js"),
            recognize,
        }),
    )
}

fn recognize(png: &str, evidence: &Value) -> Option<Value> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(png).ok()?;
    let reader =
        || image::io::Reader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png);
    let (width, height) = reader().into_dimensions().ok()?;
    if u64::from(width) != evidence["rw"].as_u64()?
        || u64::from(height) != evidence["rh"].as_u64()?
        || width > 2048
        || height > 2048
        || width * height > 2_000_000
    {
        return None;
    }
    let image = reader().decode().ok()?.into_rgba8();
    let deadline = Instant::now() + Duration::from_secs(3);
    let result = flowchart::detect(&image, |mask| {
        match crate::pixel_text::flowchart_labels(mask, None, deadline) {
            Err(error) if error.code == "DRIVER.OCR_UNAVAILABLE" => Ok(json!([])),
            other => other,
        }
    });
    let mut selector = query(
        result.ok()?,
        evidence["fx"].as_f64()?,
        evidence["fy"].as_f64()?,
        &format!("{:x}", Sha256::digest(image.as_raw())),
    )?;
    if let Some(hint) = evidence.get("view_hint") {
        if !crate::recording::valid_view_hint(hint) {
            return None;
        }
        selector["view_hint"] = hint.clone();
    }
    Some(selector)
}

fn query(mut graph: Value, fx: f64, fy: f64, hash: &str) -> Option<Value> {
    graph["source"] = json!("flowchart");
    let scene = Scene {
        public: graph,
        guard: Guard::Pixels(hash.into()),
    };
    let objects = scene.public["objects"].as_array()?;
    let contains = |object: &Value, fx: f64, fy: f64| {
        let bounds = &object["box"];
        let axis = |point, start: &str, size: &str| {
            bounds[start]
                .as_f64()
                .zip(bounds[size].as_f64())
                .is_some_and(|(start, size)| start <= point && point < start + size)
        };
        axis(fx, "fx", "fw") && axis(fy, "fy", "fh")
    };
    let mut hits = objects.iter().filter(|obj| contains(obj, fx, fy));
    let hit = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    let usable =
        |text: &str| !text.trim().is_empty() && text.len() <= 1024 && !text.contains("${{");
    let label = hit["label"].as_str().filter(|s| usable(s));
    let kind = hit["kind"].as_str().filter(|s| usable(s))?;
    let base = label.map_or_else(|| json!({"kind":kind}), |name| json!({"label":name}));
    let verified = |candidate: &Value| {
        serde_json::from_value(candidate.clone())
            .ok()
            .and_then(|query| scene.click(&query).ok())
            .is_some_and(|action| matches!(action, crate::Action::Click { fraction: Some(centre), .. } if contains(hit, centre.fx, centre.fy)))
    };
    let adjacent = |near: &str, far: &str| {
        scene.public["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|edge| edge["status"] == "confirmed" && edge[near] == hit["id"])
            .filter_map(|edge| {
                objects.iter().find(|obj| obj["id"] == edge[far])?["label"]
                    .as_str()
                    .filter(|s| usable(s))
            })
            .collect::<BTreeSet<_>>()
    };
    let after = adjacent("to", "from");
    let before = adjacent("from", "to");
    for base in
        std::iter::once(base.clone()).chain(label.map(|_| json!({"label":label,"kind":kind})))
    {
        if verified(&base) {
            return Some(base);
        }
        for (key, neighbors) in [("after", &after), ("before", &before)] {
            for neighbor in neighbors {
                let mut candidate = base.clone();
                candidate[key] = json!(neighbor);
                if verified(&candidate) {
                    return Some(candidate);
                }
            }
        }
        for (pred, succ) in after
            .iter()
            .flat_map(|pred| before.iter().map(move |succ| (pred, succ)))
            .take(128)
        {
            let mut candidate = base.clone();
            candidate["before"] = json!(succ);
            candidate["after"] = json!(pred);
            if verified(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_labels_require_distinguishing_confirmed_relations() {
        let objects: Vec<_> = ["REVIEW", "REVIEW", "REVIEW", "PAY", "SHIP"].into_iter().enumerate().map(|(index, label)| {
            json!({"id":index.to_string(), "label":label, "kind":"rectangle", "box":{"fx":0.2*index as f64,"fy":0.1,"fw":0.1,"fh":0.1}})
        }).collect();
        let edge = |from: &str, to: &str| json!({"from":from, "to":to, "status":"confirmed"});
        let mut graph = json!({"objects":objects, "edges":[edge("3","0"),edge("0","4"),edge("3","1"),edge("2","4")]});
        let at_first = |graph: &Value| query(graph.clone(), 0.05, 0.15, "frame");
        assert_eq!(
            at_first(&graph),
            Some(json!({"label":"REVIEW","after":"PAY","before":"SHIP"}))
        );
        graph["edges"][1]["status"] = json!("uncertain");
        assert_eq!(at_first(&graph), None);
        graph["edges"][1]["status"] = json!("confirmed");
        graph["edges"].as_array_mut().unwrap().push(edge("1", "4"));
        assert_eq!(at_first(&graph), None);
    }
}

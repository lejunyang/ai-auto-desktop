use super::{eval, Page};
use serde_json::{json, Value};

pub struct CanvasRecognition {
    pub editor_reader: &'static str,
    pub recognize: fn(&str, &Value) -> Option<Value>,
}

impl CanvasRecognition {
    pub(super) fn install(&self, script: String) -> String {
        script.replace(
            "/*AAD_CANVAS*/null",
            &include_str!("canvas.js").replace("/*AAD_EDITOR*/", self.editor_reader),
        )
    }

    pub(super) fn resolve(&self, page: &mut Page, session: &str, context: i64, action: &mut Value) {
        let evidence = action["canvas"].take();
        if let Some(
            reason @ ("unsupported_geometry"
            | "unresolved_hit"
            | "unverified_visibility"
            | "possible_overlay"
            | "visible_region_changed"
            | "translucent_pixels"
            | "capture_failed"),
        ) = evidence["reason"].as_str()
        {
            action["canvas_reason"] = json!(reason);
        }
        if !session.is_empty() || action["trusted"] != true || action["tag"] != "canvas" {
            return;
        }
        if evidence["source"] == "editor" {
            if let Some(id) = evidence["query"]["id"].as_str().filter(|id| id.len() <= 64) {
                action["canvas"] = json!({"source":"editor","query":{"id":id}});
            }
            return;
        }
        if evidence["source"] != "flowchart" {
            return;
        }
        let (Some(doc), Some(seq)) = (action["document_id"].as_str(), action["sequence"].as_u64())
        else {
            return;
        };
        let expression = format!("globalThis.__aadCdp?.canvasProof({}, {seq})", json!(doc));
        let proof = eval(&mut page.client, session, context, &expression).ok();
        if let Some(png) = proof
            .as_ref()
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 1024 * 1024)
        {
            if let Some(query) = (self.recognize)(png, &evidence) {
                action["canvas"] = json!({"source":"flowchart","query":query});
            } else {
                action["canvas_reason"] = json!("unresolved_hit");
            }
        } else {
            action["canvas_reason"] = json!("frame_unavailable");
        }
    }
}

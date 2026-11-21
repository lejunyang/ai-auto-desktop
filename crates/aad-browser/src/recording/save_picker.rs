use super::{Recorder, Result, Step};
use serde_json::{json, Value};

impl Recorder {
    // Only a browser-issued, handle-less chooser opening can corroborate the
    // API call. A page notification on its own cannot produce a save step.
    pub(super) fn save_chooser_opened(&mut self, page: &str, frame: &str) -> bool {
        let last = self.steps.iter_mut().rev().find(|s| {
            s.page_id == page
                && s.frame_id == frame
                && (s.kind == "action"
                    || s.kind.starts_with("save_picker")
                    || s.kind.starts_with("file_chooser"))
        });
        if let Some(call) =
            last.filter(|s| s.kind == "save_picker_pending" && s.action["opened"] == false)
        {
            call.action["opened"] = json!(true);
            return true;
        }
        false
    }

    pub(super) fn save_picker_event(
        &mut self,
        page: &str,
        frame: &str,
        event: &Value,
    ) -> Result<()> {
        let Some(call) = event["call"].as_str() else {
            return Ok(());
        };
        let Some(doc) = event["document_id"].as_str() else {
            return Ok(());
        };
        let same_frame = |s: &&Step| s.page_id == page && s.frame_id == frame;
        if event["phase"] == "start" {
            let trigger = event["trigger_sequence"].as_u64().and_then(|seq| {
                self.steps
                    .iter()
                    .rev()
                    .filter(same_frame)
                    .find(|s| {
                        s.kind == "action"
                            && s.action["document_id"] == doc
                            && s.action["sequence"] == seq
                            && s.action["trusted"] == true
                            && matches!(s.action["kind"].as_str(), Some("click" | "enter"))
                    })
                    .map(|s| s.id)
            });
            let enter = trigger.and_then(|id| {
                self.steps[id].action["enter_sequence"]
                    .as_u64()
                    .and_then(|seq| {
                        self.steps[..id]
                            .iter()
                            .rev()
                            .filter(same_frame)
                            .find(|s| {
                                s.kind == "action"
                                    && s.action["document_id"] == doc
                                    && s.action["sequence"] == seq
                                    && s.action["trusted"] == true
                                    && s.action["kind"] == "enter"
                            })
                            .map(|s| s.id)
                    })
            });
            if let Some(id) = trigger {
                if let Some(enter) = enter {
                    self.steps[enter].action["kind"] = json!("file_trigger");
                }
                let action = &mut self.steps[id].action;
                action["trigger"] = if enter.is_some() {
                    json!("enter")
                } else {
                    action["kind"].clone()
                };
                action["kind"] = json!("save_pending");
            }
            let gesture = trigger.map(|id| self.steps[id].action["trigger"].clone());
            self.new_step(page, frame, "save_picker_pending", json!({"document_id":doc,"call":call,"trigger_id":trigger,"enter_id":enter,"trigger":gesture,"opened":false}))?;
            return Ok(());
        }
        let Some(row) = self
            .steps
            .iter()
            .rev()
            .filter(same_frame)
            .find(|s| {
                s.kind == "save_picker_pending"
                    && s.action["document_id"] == doc
                    && s.action["call"] == call
            })
            .map(|s| s.id)
        else {
            return Ok(());
        };
        let checkpoint = &mut self.steps[row];
        let outcome = match event["phase"].as_str() {
            Some("selected") if checkpoint.action["opened"] == true => "save_as",
            Some("cancel") => "save_cancel",
            Some("selected" | "failed") => "save_failed",
            _ => return Ok(()),
        };
        checkpoint.kind = "save_picker_finished".into();
        checkpoint.action["outcome"] = json!(outcome);
        self.storage.append(
            "evidence/actions.jsonl",
            &serde_json::to_value(&*checkpoint)?,
        )?;
        if let Some(id) = checkpoint.action["trigger_id"].as_u64() {
            self.steps[id as usize].action["kind"] = json!(outcome);
        } else {
            self.gap("save_picker_trigger_unobserved", json!({"step_id":row}))?;
        }
        if outcome == "save_failed" {
            self.gap("save_picker_selection_unconfirmed", json!({"step_id":row}))?;
        }
        Ok(())
    }
}

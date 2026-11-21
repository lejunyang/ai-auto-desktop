use super::{native::Window, Result};
use aad_control::{
    recording::{from_browser_steps, from_events},
    steps::{App, Step},
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Clone, PartialEq, Eq)]
pub enum Source {
    Native(usize, Window),
    Portable(usize),
    Browser(String, String), // frame, document
    Boundary,
}

#[derive(Clone, serde::Serialize)]
pub enum ReplaySource {
    Browser,
    Linux(usize, Window),
    Portable(usize),
}

struct Event {
    at: u64,
    source: Source,
    value: Value,
}

#[derive(Default)]
pub struct Timeline {
    events: Vec<Event>,
    pub gaps: Vec<Value>,
    pub filtered: BTreeMap<&'static str, usize>,
}

impl Timeline {
    pub fn push(&mut self, at: u64, source: Source, value: Value) -> Result<()> {
        if self.events.len() >= 20_000 {
            return Err("mixed recording event budget reached (20000)".into());
        }
        self.events.push(Event { at, source, value });
        Ok(())
    }

    pub fn gap(&mut self, at: u64, mut gap: Value) -> Result<()> {
        gap["at_ms"] = json!(at);
        self.gaps.push(gap);
        self.push(at, Source::Boundary, Value::Null)
    }

    pub fn filter(&mut self, at: u64, reason: &'static str) -> Result<()> {
        *self.filtered.entry(reason).or_default() += 1;
        self.push(at, Source::Boundary, Value::Null)
    }

    pub fn read_browser(&mut self, dir: &Path, page: &str) -> Result<()> {
        // edit_update holds each individual input, even when the web-only
        // walkthrough has already collapsed the whole sequence to a final value.
        let mut frames = HashMap::new();
        let mut seen = HashSet::new();
        let mut pending_changes = HashMap::new();
        let mut triggers: HashMap<u64, usize> = HashMap::new();
        for line in BufReader::new(File::open(dir.join("evidence/actions.jsonl"))?).lines() {
            let row: Value = serde_json::from_str(&line?)?;
            let id = row["id"].as_u64().ok_or("missing browser checkpoint id")?;
            let frame = match row["kind"].as_str() {
                Some("file_chooser_pending" | "file_chooser_without_dom_input") => {
                    if row["page_id"] != page {
                        return Err("unexpected browser chooser page".into());
                    }
                    if let Some(index) = row["action"]["enter_id"]
                        .as_u64()
                        .and_then(|id| triggers.remove(&id))
                    {
                        self.events[index].value["kind"] = json!("file_trigger");
                    }
                    if let Some(index) = row["action"]["trigger_id"]
                        .as_u64()
                        .and_then(|id| triggers.remove(&id))
                    {
                        self.events[index].value["kind"] =
                            json!(if row["action"]["uncertain"] == true {
                                "file_trigger_uncertain"
                            } else {
                                "file_trigger"
                            });
                    }
                    continue;
                }
                Some("action") => {
                    if row["page_id"] != page {
                        return Err("unexpected browser page in evidence".into());
                    }
                    let frame = row["frame_id"]
                        .as_str()
                        .ok_or("missing browser frame")?
                        .to_string();
                    frames.insert(id, frame.clone());
                    frame
                }
                Some("edit_update") => frames
                    .get(&id)
                    .ok_or("browser edit without its checkpoint")?
                    .clone(),
                _ => continue,
            };
            let action = &row["action"];
            let at = action["at_ms"]
                .as_u64()
                .ok_or("missing browser action timestamp")?;
            if action["trusted"] != true {
                self.filter(at, "untrusted_dom")?;
                continue;
            }
            let doc = action["document_id"]
                .as_str()
                .ok_or("missing browser document id")?;
            let seq = action["sequence"]
                .as_u64()
                .ok_or("missing browser event sequence")?;
            if !seen.insert((doc.to_string(), seq)) {
                *self.filtered.entry("duplicate_browser_event").or_default() += 1;
                continue;
            }
            if action["kind"] == "input" {
                let key = (
                    doc.to_string(),
                    action["node_id"]
                        .as_str()
                        .ok_or("missing DOM node id")?
                        .to_string(),
                );
                let state = (action["value"].clone(), action["checked"].clone());
                match action["dom_event"].as_str() {
                    Some("input") => {
                        pending_changes.insert(key, state);
                    }
                    Some("change") if pending_changes.remove(&key) == Some(state) => {
                        *self.filtered.entry("redundant_dom_change").or_default() += 1;
                        continue;
                    }
                    _ => (),
                }
            }
            if action["kind"] == "file_cancel"
                && action["type"] == "file"
                && action["tag"] == "input"
                && action["dom_event"] == "cancel"
            {
                self.filter(at, "file_selection_cancelled_or_unchanged")?;
                continue;
            }
            if action["tag"] == "select" || action["type"] == "hidden" {
                self.gap(at, json!({"source":"browser","reason":"unsupported_input_type","type":action["type"]}))?;
                continue;
            }
            if matches!(action["kind"].as_str(), Some("click" | "enter")) {
                triggers.insert(id, self.events.len());
            }
            self.push(at, Source::Browser(frame, doc.into()), action.clone())?;
        }
        match File::open(dir.join("evidence/health.jsonl")) {
            Ok(file) => {
                for line in BufReader::new(file).lines() {
                    let row: Value = serde_json::from_str(&line?)?;
                    self.gap(
                        row["at_ms"]
                            .as_u64()
                            .ok_or("missing browser gap timestamp")?,
                        json!({"source":"browser","reason":row["kind"],"detail":row["detail"]}),
                    )?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    pub fn steps(
        &mut self,
        start: u64,
        stop: u64,
        precision: u64,
    ) -> Result<(Vec<Step>, Vec<ReplaySource>)> {
        self.events.retain(|e| (start..=stop).contains(&e.at));
        // Stable for each source, native before browser at an identical tick.
        self.events
            .sort_by_key(|e| (e.at, matches!(e.source, Source::Browser(..))));
        let mut steps = Vec::new();
        let mut windows = Vec::new();
        let mut next = 1;
        let mut previous: Option<&Event> = None;
        for event in self
            .events
            .iter()
            .filter(|e| !matches!(e.source, Source::Boundary))
        {
            if previous.is_some_and(|p| p.source != event.source && event.at - p.at <= precision) {
                self.gaps.push(json!({"source":"mixed","reason":"order_within_clock_precision","at_ms":event.at}));
            }
            previous = Some(event);
        }
        for run in self.events.chunk_by(|a, b| a.source == b.source) {
            let (batch, window) = match &run[0].source {
                Source::Boundary => continue,
                Source::Portable(index) => {
                    for event in run {
                        let mut step: Step = serde_json::from_value(event.value.clone())?;
                        step.id = format!("rec{next}");
                        next += 1;
                        windows.push(ReplaySource::Portable(*index));
                        steps.push(step);
                    }
                    continue;
                }
                Source::Native(index, window) => {
                    let events: Vec<_> = run.iter().map(|e| e.value.clone()).collect();
                    let app = App {
                        provider: Some(super::native::PROVIDER.into()),
                        ..Default::default()
                    };
                    (
                        from_events(&app, &events, &mut next),
                        ReplaySource::Linux(*index, window.clone()),
                    )
                }
                Source::Browser(..) => {
                    let mut rows = String::new();
                    let mut previous: Option<&Value> = None;
                    for event in run {
                        let action = &event.value;
                        // Checkbox/radio input + change report the same checked
                        // state. A second click or a resource switch breaks the pair.
                        if matches!(action["type"].as_str(), Some("checkbox" | "radio"))
                            && action["kind"] == "input"
                            && previous.is_some_and(|p| {
                                p["kind"] == "input"
                                    && p["node_id"] == action["node_id"]
                                    && p["checked"] == action["checked"]
                            })
                        {
                            continue;
                        }
                        rows.push_str(&json!({"kind":"action","action":action}).to_string());
                        rows.push('\n');
                        previous = Some(action);
                    }
                    (from_browser_steps(&rows, &mut next), ReplaySource::Browser)
                }
            };
            for skipped in batch.skipped {
                self.gaps.push(json!({"source":if matches!(window, ReplaySource::Browser) {"browser"} else {"native"},"at_ms":run[0].at,"skipped":skipped}));
            }
            windows.extend(std::iter::repeat_n(window, batch.steps.len()));
            steps.extend(batch.steps);
        }
        Ok((steps, windows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trusted_file_outcomes_replay_and_chooser_openers_do_not_survive_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("evidence")).unwrap();
        let row = |id, kind, trusted| json!({"id":id,"kind":"action","page_id":"page","frame_id":"frame","action":{"kind":kind,"type":if matches!(kind,"click"|"enter") {"button"} else {"file"},"at_ms":id,"sequence":id,"node_id":"a","document_id":"doc","trusted":trusted,"dom_event":if kind=="file_cancel" {"cancel"} else {"input"},"url":"https://example.test/","tag":"input","hidden":true,"control":{"role":"button","name":"Attachment"}}});
        let rows = [
            row(0, "enter", true),
            row(1, "click", true),
            row(2, "file_activation", false),
            json!({"id":3,"kind":"file_chooser_pending","page_id":"page","frame_id":"frame","action":{"trigger_id":1,"enter_id":0,"uncertain":false}}),
            row(4, "upload", false),
            row(5, "upload", true),
            row(6, "file_cancel", true),
        ];
        let data = rows
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("evidence/actions.jsonl"), data).unwrap();
        let mut timeline = Timeline::default();
        timeline.read_browser(dir.path(), "page").unwrap();
        let (steps, sources) = timeline.steps(0, 10, 0).unwrap();
        assert!(timeline.gaps.is_empty());
        assert_eq!(timeline.filtered["untrusted_dom"], 2);
        assert_eq!(
            timeline.filtered["file_selection_cancelled_or_unchanged"],
            1
        );
        assert_eq!(sources.len(), 1);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].action, Some(json!({"type":"upload"})));
        assert_eq!(steps[0].input.as_ref().unwrap().name, "rec1_files");
    }
}

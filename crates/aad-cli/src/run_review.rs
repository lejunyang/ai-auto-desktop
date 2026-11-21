use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

pub fn review(path: &Path) -> Result<Value, String> {
    let mut reader = BufReader::new(File::open(path).map_err(|err| err.to_string())?);
    if reader
        .fill_buf()
        .map_err(|err| err.to_string())?
        .starts_with(b"SQLite format 3\0")
    {
        aad_runtime::recovery::review(path).map_err(|err| err.message)
    } else {
        inspect(reader)
    }
}

fn inspect(mut input: impl BufRead) -> Result<Value, String> {
    let mut sequence = 0u64;
    let mut run = Value::Null;
    let mut digest = Value::Null;
    let mut last = Value::Null;
    let mut action = Value::Null;
    let mut failed = Value::Null;
    let mut terminal = None;
    let mut torn = false;
    let mut line = Vec::new();
    let mut slowest = Vec::<(u64, Value)>::with_capacity(6);
    loop {
        line.clear();
        let size = input
            .by_ref()
            .take(4 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|err| err.to_string())?;
        if size == 0 {
            break;
        }
        if size > 4 * 1024 * 1024 {
            return Err("journal event exceeds size limit".into());
        }
        if line.last() != Some(&b'\n') {
            torn = true;
            break;
        }
        sequence += 1;
        let event: Value = serde_json::from_slice(&line)
            .map_err(|_| format!("invalid event at line {sequence}"))?;
        let payload = &event["payload"];
        if event["kind"] != "RunEvent" || event["seq"] != sequence || terminal.is_some() {
            return Err(format!("invalid journal sequence at line {sequence}"));
        }
        if sequence == 1 {
            if event["type"] != "run.started"
                || !event["runId"].is_string()
                || !payload["plan_digest"].is_string()
            {
                return Err("missing run header".into());
            }
            run = event["runId"].clone();
            digest = payload["plan_digest"].clone();
        } else if event["runId"] != run {
            return Err("mixed run IDs".into());
        }
        if event["type"] == "step.finished" {
            if let (Some(elapsed), Some(id), Some(status)) = (
                payload["elapsed_ms"].as_u64(),
                payload["id"].as_str(),
                payload["status"].as_str(),
            ) {
                // Keep earlier steps first on ties. A loop may finish the
                // same step repeatedly, so retain the original event number.
                let index = slowest.partition_point(|(ms, _)| *ms >= elapsed);
                if index < 5 {
                    slowest.insert(
                        index,
                        (
                            elapsed,
                            json!({"step":id,"status":status,
                        "elapsed_ms":elapsed,"event":sequence}),
                        ),
                    );
                    slowest.truncate(5);
                }
            }
        }
        match event["type"].as_str() {
            Some("action.started") => {
                action = json!({"step":payload["id"],"uses":payload["uses"],"effect":"unknown"})
            }
            Some("action.finished") => action = Value::Null,
            Some("action.failed") => {
                action = Value::Null;
                failed = json!({"step":payload["id"],"code":payload["error"]["code"],"effect":payload["error"]["effect"]});
            }
            Some("step.finished") if payload["status"] == "succeeded" => {
                last = payload["id"].clone()
            }
            Some("run.finished") => terminal = Some(payload["status"].clone()),
            Some("run.failed") => terminal = Some(json!("failed")),
            _ => {}
        }
    }
    if sequence == 0 {
        return Err("no complete journal header".into());
    }
    let slowest_steps: Vec<_> = slowest.into_iter().map(|(_, step)| step).collect();
    Ok(
        json!({"run_id":run,"plan_digest":digest,"status":terminal.unwrap_or(json!("no_final_event")),
        "last_completed_step":last,"unconfirmed_action":action,"last_action_error":failed,
        "last_complete_event":sequence,"partial_final_line":torn,"slowest_steps":slowest_steps}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tolerates_only_a_torn_final_line_and_keeps_unknown_input_visible() {
        let header = r#"{"kind":"RunEvent","seq":1,"type":"run.started","runId":"r","payload":{"plan_digest":"digest"}}"#;
        let enter = r#"{"kind":"RunEvent","seq":2,"type":"action.started","runId":"r","payload":{"id":"submit","uses":"control.session.act@1"}}"#;
        let log = format!("{header}\n{enter}\n{{");
        let result = inspect(log.as_bytes()).unwrap();
        assert_eq!(result["unconfirmed_action"]["step"], "submit");
        assert_eq!(result["partial_final_line"], true);
        assert_eq!(result["status"], "no_final_event");
        assert!(inspect(log.replace("\"seq\":2", "\"seq\":3").as_bytes()).is_err());
        assert!(inspect(format!("{log}\n").as_bytes()).is_err());
    }
}

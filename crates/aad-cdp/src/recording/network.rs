use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "authorization",
        "cookie",
        "setcookie",
        "password",
        "passwd",
        "secret",
        "token",
        "ticket",
        "credential",
        "apikey",
        "otp",
        "verificationcode",
    ]
    .iter()
    .any(|part| key.contains(part))
}

/// Opt-in only. Unknown opaque bodies are omitted rather than claiming to be safe.
pub fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if sensitive(key) || key == "postDataEntries" || key == "urlFragment" {
                    *value = json!("[REDACTED]");
                } else if key == "url" || key == "documentURL" || key == "href" {
                    if let Some(text) = value.as_str() {
                        *value = json!(redact_url(text));
                    }
                } else if key == "postData"
                    || key == "body"
                    || key == "payloadData"
                    || key == "data" && value.is_string()
                {
                    if let Some(text) = value.as_str() {
                        if let Ok(mut parsed) = serde_json::from_str::<Value>(text) {
                            redact(&mut parsed);
                            *value = json!(parsed.to_string());
                        } else {
                            *value = json!("[REDACTED OPAQUE BODY]");
                        }
                    } else {
                        redact(value);
                    }
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact(item)
            }
        }
        _ => {}
    }
}

/// The address with credentials, fragment and sensitive query values removed.
pub fn redact_url(text: &str) -> String {
    let Ok(mut url) = url::Url::parse(text) else {
        return "[REDACTED URL]".into();
    };
    let pairs: Vec<_> = url
        .query_pairs()
        .map(|(k, v)| {
            (
                k.to_string(),
                if sensitive(&k) {
                    "[REDACTED]".into()
                } else {
                    v.to_string()
                },
            )
        })
        .collect();
    if url.query().is_some() {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

pub fn truncate(text: &str, limit: usize) -> (&str, bool) {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}

pub fn route(text: &str) -> String {
    url::Url::parse(text)
        .map(|u| format!("{}{}", u.origin().ascii_serialization(), u.path()))
        .unwrap_or_else(|_| text.to_string())
}

pub fn important(kind: &str, status: u64, url: &str) -> bool {
    matches!(
        kind,
        "Document" | "XHR" | "Fetch" | "Other" | "WebSocket" | "EventSource"
    ) || status >= 400
        || url.to_ascii_lowercase().contains("qr")
}

/// A small reading aid; complete captured values remain in network evidence.
pub fn business_summary(body: &Value) -> Value {
    let parsed = body
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let Some(parsed) = parsed else {
        return Value::Null;
    };
    let mut result = serde_json::Map::new();
    for (prefix, value) in [("", &parsed), ("data.", &parsed["data"])] {
        if let Some(fields) = value.as_object() {
            for key in [
                "status",
                "state",
                "code",
                "message",
                "id",
                "qr_id",
                "expires_in",
                "order_id",
                "name",
            ] {
                if let Some(value) = fields.get(key) {
                    if value.is_string() || value.is_number() || value.is_boolean() {
                        let short = if let Some(s) = value.as_str() {
                            json!(truncate(s, 160).0)
                        } else {
                            value.clone()
                        };
                        result.insert(format!("{prefix}{key}"), short);
                    }
                }
            }
        }
    }
    Value::Object(result)
}

/// Fold only exact request/body/status repetitions. Different business states
/// (including two different HTTP 200 payloads) must remain separate evidence.
pub fn fingerprint(record: &Value) -> String {
    let mut headers = record["response"]["headers"].clone();
    if let Some(headers) = headers.as_object_mut() {
        headers.retain(|key, _| {
            !matches!(key.to_ascii_lowercase().as_str(), "date" | "server-timing")
        });
    }
    let semantic = json!([
        record["page_id"],
        record["session"],
        record["frame_id"],
        record["step_id"],
        record["request"],
        record["response"]["status"],
        headers,
        record["body"],
        record["error"]
    ]);
    format!("{:x}", Sha256::digest(semantic.to_string().as_bytes()))
}

#[derive(Default)]
pub struct Groups {
    pub records: BTreeMap<String, Value>,
    last: BTreeMap<String, String>,
    sequence: u64,
    pub bytes: usize,
}
impl Groups {
    pub fn new() -> Self {
        Self {
            records: BTreeMap::new(),
            last: BTreeMap::new(),
            sequence: 0,
            bytes: 0,
        }
    }
    pub fn add(&mut self, mut record: Value) {
        let stream = json!([
            record["page_id"],
            record["session"],
            record["frame_id"],
            record["step_id"],
            record["request"]["method"],
            record["request"]["url"]
        ])
        .to_string();
        let previous_key = self.last.get(&stream).cloned();
        let repeated = previous_key
            .as_ref()
            .and_then(|key| self.records.get(key))
            .is_some_and(|previous| fingerprint(previous) == fingerprint(&record));
        if repeated {
            let previous = self
                .records
                .get_mut(previous_key.as_ref().unwrap())
                .unwrap();
            previous["count"] = json!(previous["count"].as_u64().unwrap_or(1) + 1);
            previous["last_ms"] = record["finished_ms"].clone();
            previous["last_request_id"] = record["request_id"].clone();
        } else {
            record["count"] = json!(1);
            record["last_ms"] = record["finished_ms"].clone();
            self.sequence += 1;
            let key = self.sequence.to_string();
            self.bytes += record.to_string().len();
            self.last.insert(stream, key.clone());
            self.records.insert(key, record);
        }
    }
    pub fn take(&mut self) -> BTreeMap<String, Value> {
        self.last.clear();
        self.bytes = 0;
        std::mem::take(&mut self.records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_redaction_removes_secrets_without_losing_business_fields() {
        let original = json!({"request":{"url":"https://example.com/?token=raw-token&order=123","headers":{"Cookie":"sid=raw-cookie","Authorization":"raw-auth"},"postData":"{\"password\":\"raw-password\",\"name\":\"alice\"}"},"body":"{\"token\":\"raw-body\",\"status\":\"confirmed\"}"});
        let mut filtered = original.clone();
        redact(&mut filtered);
        assert!(original.to_string().contains("raw-cookie"));
        for secret in [
            "raw-token",
            "raw-cookie",
            "raw-auth",
            "raw-password",
            "raw-body",
        ] {
            assert!(!filtered.to_string().contains(secret));
        }
        assert!(filtered.to_string().contains("confirmed"));
        assert!(filtered.to_string().contains("alice"));
    }
    #[test]
    fn repeated_polling_folds_but_business_transition_does_not() {
        let record = json!({"request":{"method":"GET","url":"https://example.com/status"},"response":{"status":200},"body":"waiting"});
        let mut groups = Groups::new();
        groups.add(record.clone());
        groups.add(record.clone());
        let mut confirmed = record.clone();
        confirmed["body"] = json!("confirmed");
        groups.add(confirmed);
        assert_eq!(groups.records.len(), 2);
        assert!(groups.records.values().any(|r| r["count"] == 2));
        groups.add(record);
        assert_eq!(
            groups.records.len(),
            3,
            "a return to waiting is a new transition, not an earlier duplicate"
        );
    }
}

use super::{failure, Page, Result};
use aad_control::browser::export::ExpectedFile;
use aad_control::{Action, Expect, Locator, Operation, Target, View};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Named {
    role: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Spec {
    task_id: String,
    scope: Option<Named>,
    trigger: Named,
    #[serde(default)]
    required_names: Vec<String>,
    expected: ExpectedFile,
}

impl Named {
    fn locator(&self) -> Locator {
        Locator {
            role: Some(self.role.clone()),
            name: Some(self.name.clone()),
            ..Default::default()
        }
    }
}

impl Page {
    pub(super) fn download_export(&mut self, spec: Spec) -> Result<Value> {
        if !self.session.policy().allow_input {
            return Err(failure(
                "POLICY.DENIED",
                "downloadExport needs permission to click the observed task control",
            ));
        }
        let valid = |text: &str| {
            !text.trim().is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
        };
        let scope = spec.scope.unwrap_or_else(|| Named {
            role: "row".into(),
            name: spec.task_id.clone(),
        });
        if !self.chromium
            || !valid(&spec.task_id)
            || !valid(&scope.name)
            || !valid(&scope.role)
            || !matches!(spec.trigger.role.as_str(), "button" | "link")
            || !valid(&spec.trigger.name)
            || spec.required_names.len() > 16
            || spec.required_names.iter().any(|name| !valid(name))
        {
            return Err(failure("BROWSER.EXPORT_INVALID","supply a Chromium task ID, named scope, named button/link and at most 16 exact condition names"));
        }
        spec.expected.validate().map_err(|err| err.to_json())?;
        let remaining = |stage: &str| {
            if self.stop.is_cancelled() {
                return Err(failure("CONTROL.CANCELLED", "export cancelled"));
            }
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                Err(failure("BROWSER.EXPORT_TIMEOUT", stage))
            } else {
                Ok(left.min(Duration::from_secs(2)))
            }
        };
        self.session
            .download_cursor(remaining("waiting for task")?)
            .map_err(|err| err.to_json())?;
        let find = |seen: &Value, locator: Locator| {
            self.request(Operation::Find {
                observation_id: seen["observation_id"].as_str().unwrap_or_default().into(),
                locator: Box::new(locator),
                expect: Expect::Optional,
            })
        };
        let mut interval = Duration::from_millis(125);
        let (frame, target) = loop {
            remaining(
                "task ID, conditions or ready download control not observed before deadline",
            )?;
            let seen = self.observe(aad_control::ViewMode::Nodes)?;
            let row = find(&seen, scope.locator())?;
            if row["found"] == true {
                let row: Target = serde_json::from_value(row["target"].clone())
                    .map_err(|err| failure("BROWSER.BAD_RESULT", err))?;
                let seen = self.request(Operation::Observe {
                    resource_id: self.resource_id.clone(),
                    max_nodes: aad_control::MAX_NODES,
                    view: View {
                        scope_target: Some(row),
                        ..Default::default()
                    },
                })?;
                let mut matches = true;
                for name in std::iter::once(&spec.task_id).chain(&spec.required_names) {
                    if scope.name != *name
                        && find(
                            &seen,
                            Locator {
                                name: Some(name.clone()),
                                ..Default::default()
                            },
                        )?["found"]
                            != true
                    {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    let found = find(&seen, spec.trigger.locator())?;
                    if found["found"] == true && found["node"]["states"]["enabled"] == true {
                        let frame = seen["metadata"]["frame_id"]
                            .as_str()
                            .ok_or_else(|| {
                                failure("BROWSER.BAD_RESULT", "no observed Chromium frame")
                            })?
                            .to_owned();
                        let target: Target = serde_json::from_value(found["target"].clone())
                            .map_err(|err| failure("BROWSER.BAD_RESULT", err))?;
                        break (frame, target);
                    }
                }
            }
            std::thread::sleep(interval.min(remaining("waiting for ready task")?));
            interval = (interval * 2).min(Duration::from_secs(1));
        };
        let cursor = self
            .session
            .download_cursor(remaining("before download click")?)
            .map_err(|err| err.to_json())?;
        cursor.require_idle().map_err(|err| err.to_json())?;
        self.request(Operation::Act {
            target,
            action: Action::click(),
            verify: None,
        })?;
        self.writes += 1;
        loop {
            let left = remaining(
                "no verified completed download before deadline; the trigger was clicked",
            )?;
            let file = self
                .session
                .poll_export(&cursor, &frame, &spec.expected, left, || {
                    if self.stop.is_cancelled() || Instant::now() >= self.deadline {
                        Err(aad_control::error(
                            "BROWSER.EXPORT_INTERRUPTED",
                            "file verification interrupted",
                        ))
                    } else {
                        Ok(())
                    }
                })
                .map_err(|err| err.to_json())?;
            if let Some(file) = file {
                return Ok(
                    json!({"verified":true,"task_id":spec.task_id,"scope":{"role":scope.role,"name":scope.name},
                    "matched_names":spec.required_names,"file":file}),
                );
            }
            std::thread::sleep(Duration::from_millis(75).min(left));
        }
    }
}

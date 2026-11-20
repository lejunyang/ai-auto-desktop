//! Opt-in retention and dual-observation input. Public callers still use the
//! ordinary observed Target contract; this is a provider-private dispatch hook.
use super::*;

pub(crate) const CROSS_DRAG_SEGMENTS: usize = 12;
const MAX_RETAINED: usize = 16;
const RECORD_TTL: Duration = Duration::from_secs(300);

pub struct DragBetweenEndpoint<'a> {
    pub target: &'a NativeRef,
    pub window: &'a WindowAnchor,
    pub point: (i32, i32),
    pub process_id: u32,
}

pub struct DragBetweenRequest<'a> {
    pub source: DragBetweenEndpoint<'a>,
    pub destination: DragBetweenEndpoint<'a>,
    pub button: u8,
    pub modifiers: &'a [String],
    pub hold_ms: Option<u16>,
    pub deadline: Instant,
}

fn same_application(a: &Record, b: &Record) -> bool {
    ["bus_name", "object_path", "process_id"]
        .iter()
        .all(|key| a.public["application"][key] == b.public["application"][key])
}

impl DriverState {
    fn prune(&mut self) {
        self.retained.retain(|r| r.created.elapsed() < RECORD_TTL);
    }

    pub(super) fn publish(&mut self, record: Record, retain: bool) {
        if retain {
            self.prune();
            self.retained.retain(|r| !same_application(r, &record));
            self.retained.push_back(record.clone());
            while self.retained.len() > MAX_RETAINED {
                self.retained.pop_front();
            }
        }
        self.current = Some(record);
    }

    pub(super) fn invalidate_all(&mut self) {
        if let Some(record) = self.current.as_mut() {
            record.valid = false;
        }
        for record in &mut self.retained {
            record.valid = false;
        }
    }

    pub(super) fn retained_record(&self, snapshot: &str, revision: u64) -> Option<&Record> {
        self.retained.iter().find(|r| {
            r.valid
                && r.created.elapsed() < RECORD_TTL
                && r.public["snapshot_id"] == snapshot
                && r.public["revision"] == revision
        })
    }

    pub(super) fn scope_record(&self, scope: &str, retain: bool) -> Option<&Record> {
        let matches = |r: &&Record| {
            r.scope.as_ref().is_some_and(|s| s.id == scope)
                && (!retain || r.created.elapsed() < RECORD_TTL)
        };
        if retain {
            self.retained.iter().find(matches)
        } else {
            self.current.as_ref().filter(matches)
        }
    }
}

pub(super) fn enabled_manifest() -> Value {
    let mut doc = manifest_document();
    let mut contract = doc["actions"]["drag"].clone();
    contract["description"] = json!(
        "分别重新验证两个保留资源的观察和窗口，在有界直线路径上拖拽；不支持跨资源自定义路径。"
    );
    contract["input_schema"]["properties"]
        .as_object_mut()
        .expect("built-in schema")
        .remove("path");
    doc["actions"]["drag_between"] = contract;
    doc
}

struct PreparedEndpoint {
    record: Record,
    id: String,
    native: NativeRef,
    window: WindowAnchor,
    bounds: Bounds,
    point: (i32, i32),
    process_id: u32,
}

impl AtspiProvider {
    pub(super) fn private_validator(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            manifest: self.manifest.clone(),
            generation: self.generation.clone(),
            state: Arc::new(Mutex::new(DriverState::default())),
            retain_resources: self.retain_resources,
            invocation_gate: Arc::new(Mutex::new(())),
            cancellation: self.cancellation.clone(),
        }
    }

    fn prepare_endpoint(
        &self,
        original: &Record,
        args: &Map<String, Value>,
        fraction: Option<(f64, f64)>,
        deadline: Instant,
    ) -> Result<PreparedEndpoint, AutomationError> {
        remaining(deadline, false)?;
        if let Some(token) = &self.cancellation {
            token.check()?;
        }
        let fresh = self.capture_scoped(
            &original.application_selector,
            original.max_depth,
            original.max_nodes,
            original.scope.clone(),
            deadline,
        )?;
        let (id, node, native) = Self::revalidate_target(args, original, &fresh, deadline)?;
        if node["states"]["protected"] != false || node["provenance"]["value_redacted"] != false {
            return Err(driver_error(
                "DRIVER.PROTECTED_ELEMENT",
                "drag endpoint is not proven unprotected",
            ));
        }
        if !["enabled", "visible", "showing", "sensitive"]
            .iter()
            .all(|name| node["states"][name] == true)
            || !node["actions"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|a| a == "drag"))
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "drag endpoint cannot receive pointer input",
            ));
        }
        let bounds = parse_bounds(node.get("bounds"))?;
        let window = input_window(&fresh, &id)
            .ok_or_else(|| stale("drag endpoint has no observed window"))?;
        let point = point_in(bounds, fraction)?;
        let visibility = self.path_occlusion_record(&fresh, &id, deadline)?;
        let (visible, visible_id) = visibility
            .as_ref()
            .map(|(r, id)| (r, id.as_str()))
            .unwrap_or((&fresh, id.as_str()));
        self.validate_path_point(visible, visible_id, point, deadline)?;
        let process_id = process_id(&node, &fresh)?;
        Ok(PreparedEndpoint {
            record: fresh,
            id,
            native,
            window,
            bounds,
            point,
            process_id,
        })
    }

    pub(super) fn drag_between(
        &self,
        args: &Map<String, Value>,
        deadline: Instant,
    ) -> Result<Value, AutomationError> {
        if !self.retain_resources {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "cross-resource drag is unavailable",
            ));
        }
        only_keys(
            args,
            &[
                "target",
                "locator",
                "to",
                "to_locator",
                "fraction",
                "to_fraction",
                "button",
                "modifiers",
                "hold_ms",
            ],
        )?;
        let button = match args.get("button") {
            None | Some(Value::Null) => 1,
            Some(Value::String(b)) if b == "left" => 1,
            Some(Value::String(b)) if b == "middle" => 2,
            Some(Value::String(b)) if b == "right" => 3,
            _ => return Err(invalid("invalid drag button")),
        };
        let modifiers = parse_modifiers(args.get("modifiers"))?;
        let hold_ms = parse_hold_ms(args.get("hold_ms"))?;
        let fraction = parse_fraction(args.get("fraction"))?;
        let to_fraction = parse_fraction(args.get("to_fraction"))?;
        let source_args = Map::from_iter([
            (
                "target".into(),
                args.get("target").cloned().unwrap_or(Value::Null),
            ),
            (
                "locator".into(),
                args.get("locator").cloned().unwrap_or(Value::Null),
            ),
        ]);
        let destination_args = Map::from_iter([
            (
                "target".into(),
                args.get("to").cloned().unwrap_or(Value::Null),
            ),
            (
                "locator".into(),
                args.get("to_locator").cloned().unwrap_or(Value::Null),
            ),
        ]);
        // Acquire both original records before any read-only refresh can replace
        // a provider record. Each endpoint retains its own locator/scope.
        let source_original = self.observed_record(&source_args)?;
        let destination_original = self.observed_record(&destination_args)?;
        Self::revalidate_target(&source_args, &source_original, &source_original, deadline)?;
        Self::revalidate_target(
            &destination_args,
            &destination_original,
            &destination_original,
            deadline,
        )?;
        let validator = self.private_validator();
        let source =
            validator.prepare_endpoint(&source_original, &source_args, fraction, deadline)?;
        let destination = validator.prepare_endpoint(
            &destination_original,
            &destination_args,
            to_fraction,
            deadline,
        )?;
        let baselines = [
            (&source, &source_original, &source_args, fraction),
            (
                &destination,
                &destination_original,
                &destination_args,
                to_fraction,
            ),
        ]
        .into_iter()
        .map(|(prepared, original, args, fraction)| {
            (
                original.clone(),
                args.clone(),
                fraction,
                prepared.native.clone(),
                prepared.window.clone(),
                prepared.bounds,
                prepared.point,
                prepared.process_id,
            )
        })
        .collect::<Vec<_>>();
        let validate: DragPathValidation = Arc::new(move |index| {
            if index > CROSS_DRAG_SEGMENTS + 3 {
                return Err(invalid("unexpected cross-resource segment"));
            }
            for (original, args, fraction, native, window, bounds, point, process) in &baselines {
                let current = validator.prepare_endpoint(original, args, *fraction, deadline)?;
                if current.native != *native
                    || current.window.native != window.native
                    || current.window.bounds != window.bounds
                    || current.bounds != *bounds
                    || current.point != *point
                    || current.process_id != *process
                {
                    return Err(stale("drag endpoint identity or geometry changed"));
                }
            }
            remaining(deadline, false)?;
            if let Some(token) = &validator.cancellation {
                token.check()?;
            }
            Ok(())
        });
        if let Some(token) = &self.cancellation {
            token.check()?;
        }
        remaining(deadline, false)?;
        let result = self.backend.drag_between(
            DragBetweenRequest {
                source: DragBetweenEndpoint {
                    target: &source.native,
                    window: &source.window,
                    point: source.point,
                    process_id: source.process_id,
                },
                destination: DragBetweenEndpoint {
                    target: &destination.native,
                    window: &destination.window,
                    point: destination.point,
                    process_id: destination.process_id,
                },
                button,
                modifiers: &modifiers,
                hold_ms,
                deadline,
            },
            validate,
        )?;
        remaining(deadline, true)?;
        Ok(
            json!({"ok":true,"action":"drag_between","resolved":target(&source.record,&source.id),"backend_result":result}),
        )
    }
}

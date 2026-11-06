//! Dual-window delivery uses a dedicated helper command. Single-window drag
//! never receives an exemption from its original PID/window ownership checks.
use super::*;
use crate::cross_resource::CROSS_DRAG_SEGMENTS;
use crate::{DragBetweenEndpoint, DragBetweenRequest, DragPathValidation};

impl LinuxBackend {
    fn cross_window_id(
        &self,
        end: &DragBetweenEndpoint<'_>,
        path: &Path,
        deadline: Instant,
        require_focus: bool,
    ) -> Result<u64, AutomationError> {
        self.check_cancelled(false)?;
        if end.target.bus_name != end.window.native.bus_name
            || self.process_id(&end.target.bus_name) != Some(end.process_id)
        {
            return Err(super::super::stale(
                "drag endpoint window process is unproven",
            ));
        }
        let mut args = vec![
            "focus-info".into(),
            "--expected-pid".into(),
            end.process_id.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        append_window_args(&mut args, end.window, None);
        let output = self.run_helper(
            path,
            &args,
            None,
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )?;
        let proof = parse_one_json_object(&output.stdout, "cross-resource window")?;
        let id = proof["window"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= u32::MAX as u64);
        if !output.status.success()
            || proof["ok"] != true
            || proof["window_bound"] != true
            || proof["expected_pid"] != end.process_id
            || id.is_none()
            || (require_focus && proof["focused"] != true)
        {
            return Err(super::super::stale(
                "drag endpoint window or modifier focus is unproven",
            ));
        }
        Ok(id.expect("validated id"))
    }

    pub(super) fn run_drag_between(
        &self,
        request: DragBetweenRequest<'_>,
        validate: DragPathValidation,
    ) -> Result<Value, AutomationError> {
        let DragBetweenRequest {
            source,
            destination,
            button,
            modifiers,
            hold_ms,
            deadline,
        } = request;
        self.check_cancelled(false)?;
        let support = self.probe_input(deadline)?;
        for feature in [
            "anchored-drag-between",
            "cooperative-cancel",
            "window-focus",
        ] {
            support.require(feature)?;
        }
        if !modifiers.is_empty() {
            support.require("pointer-modifiers")?;
        }
        if hold_ms.is_some() {
            support.require("bounded-hold")?;
        }
        let source_id =
            self.cross_window_id(&source, &support.path, deadline, !modifiers.is_empty())?;
        let destination_id = self.cross_window_id(&destination, &support.path, deadline, false)?;
        let dx = (i64::from(destination.point.0) - i64::from(source.point.0)).abs();
        let dy = (i64::from(destination.point.1) - i64::from(source.point.1)).abs();
        if dx.max(dy) > 4096 {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "cross-resource straight route exceeds 4096 pixels",
            ));
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut args = vec![
            "drag-between".into(),
            "--button".into(),
            button.to_string(),
            "--path-token".into(),
            nonce.clone(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        for (prefix, end, id) in [
            ("source", &source, source_id),
            ("destination", &destination, destination_id),
        ] {
            let bounds = end.window.bounds;
            for (name, value) in [
                ("pid", end.process_id.to_string()),
                ("x", end.point.0.to_string()),
                ("y", end.point.1.to_string()),
                ("window-id", id.to_string()),
                ("window-x", bounds.x.to_string()),
                ("window-y", bounds.y.to_string()),
                ("window-width", bounds.width.to_string()),
                ("window-height", bounds.height.to_string()),
            ] {
                args.extend([format!("--{prefix}-{name}"), value]);
            }
        }
        if !modifiers.is_empty() {
            args.extend(["--modifiers".into(), modifiers.join(",")]);
        }
        if let Some(ms) = hold_ms {
            args.extend(["--hold-ms".into(), ms.to_string()]);
        }
        let output = drag_path::run(
            &support.path,
            &args,
            &nonce,
            CROSS_DRAG_SEGMENTS + 3,
            deadline,
            self.cancellation.as_ref(),
            self.path_validation_busy.clone(),
            validate,
        )?;
        let input = parse_input_helper(
            output,
            "drag_between",
            (CROSS_DRAG_SEGMENTS + 3 + modifiers.len() * 2) as u64,
            source.process_id,
        )?;
        Ok(
            json!({"native_interface":"dual AT-SPI revalidation -> XTEST","input":input,"synthetic_input":true,
            "segments_revalidated":CROSS_DRAG_SEGMENTS+4,"source_process_id":source.process_id,"destination_process_id":destination.process_id}),
        )
    }
}

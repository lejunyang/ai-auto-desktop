use super::*;
use crate::atspi::{DragPathRequest, DragPathValidation};

impl LinuxBackend {
    pub(super) fn run_drag_path(
        &self,
        request: DragPathRequest<'_>,
        validate: DragPathValidation,
    ) -> Result<Value, AutomationError> {
        let DragPathRequest {
            target,
            window,
            points,
            button,
            modifiers,
            hold_ms,
            process_id,
            deadline,
        } = request;
        self.check_cancelled(false)?;
        if !(3..=34).contains(&points.len()) {
            return Err(super::super::invalid(
                "drag path must have 1..32 intermediate anchors",
            ));
        }
        let support = self.probe_input(deadline)?;
        for feature in [
            "drag",
            "anchored-drag-path",
            "cooperative-cancel",
            "window-focus",
        ] {
            support.require(feature)?;
        }
        if !modifiers.is_empty() {
            support.require("pointer-modifiers")?;
            support.require_keyboard_route()?;
        }
        if hold_ms.is_some() {
            support.require("bounded-hold")?;
        }
        let window = window.ok_or_else(|| {
            driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "drag path requires an observed top-level window",
            )
        })?;
        if window.native.bus_name != target.bus_name
            || self.process_id(&target.bus_name) != Some(process_id)
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "drag window process is not proven",
            ));
        }
        // Resolve the exact observed window without activating it or requiring
        // keyboard focus for an unmodified pointer gesture.
        let mut lookup = vec![
            "focus-info".into(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        append_window_args(&mut lookup, window, None);
        let proof = self.run_helper(
            &support.path,
            &lookup,
            None,
            deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )?;
        let proof_value = parse_one_json_object(&proof.stdout, "drag window")?;
        let window_id = proof_value
            .get("window")
            .and_then(Value::as_u64)
            .filter(|id| *id > 0 && *id <= u32::MAX as u64);
        if !proof.status.success()
            || proof_value.get("ok") != Some(&json!(true))
            || proof_value.get("window_bound") != Some(&json!(true))
            || proof_value.get("expected_pid") != Some(&json!(process_id))
            || window_id.is_none()
            || (!modifiers.is_empty() && proof_value.get("focused") != Some(&json!(true)))
        {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "drag window identity or modifier focus is not proven",
            ));
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let last = points[points.len() - 1];
        let path = points[1..points.len() - 1]
            .iter()
            .map(|(x, y)| format!("{x},{y}"))
            .collect::<Vec<_>>()
            .join(";");
        let mut args = vec![
            "drag".into(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--x".into(),
            points[0].0.to_string(),
            "--y".into(),
            points[0].1.to_string(),
            "--to-x".into(),
            last.0.to_string(),
            "--to-y".into(),
            last.1.to_string(),
            "--button".into(),
            button.to_string(),
            "--path".into(),
            path,
            "--path-token".into(),
            nonce.clone(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(deadline)?.to_string(),
        ];
        append_window_args(&mut args, window, window_id);
        if !modifiers.is_empty() {
            args.extend(["--modifiers".into(), modifiers.join(",")]);
        }
        if let Some(ms) = hold_ms {
            args.extend(["--hold-ms".into(), ms.to_string()]);
        }
        let output = anchored_helper::run_drag(
            &support.path,
            &args,
            &nonce,
            points.len(),
            deadline,
            self.cancellation.as_ref(),
            self.path_validation_busy.clone(),
            validate,
        )?;
        let input = parse_input_helper(
            output,
            "drag_path",
            (points.len() + 2 + modifiers.len() * 2) as u64,
            process_id,
        )?;
        Ok(
            json!({"native_interface": "AT-SPI segment revalidation -> XTEST", "input": input,
            "synthetic_input": true, "path_anchors": points.len() - 2, "segments_revalidated": points.len() + 1}),
        )
    }
}

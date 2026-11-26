//! Dual-window delivery uses a dedicated helper command. Single-window drag
//! never receives an exemption from its original PID/window ownership checks.
use super::*;
use crate::atspi::cross_resource::CROSS_DRAG_SEGMENTS;
use crate::atspi::{DragBetweenRequest, DragPathValidation};

impl LinuxBackend {
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
            GEOMETRY_FEATURE,
        ] {
            support.require(feature)?;
        }
        if !modifiers.is_empty() {
            support.require("pointer-modifiers")?;
        }
        if hold_ms.is_some() {
            support.require("bounded-hold")?;
        }
        let source_binding = self.verify_window_with_helper(
            source.target,
            source.window,
            source.process_id,
            deadline,
            &support.path,
            !modifiers.is_empty(),
        )?;
        let destination_binding = self.verify_window_with_helper(
            destination.target,
            destination.window,
            destination.process_id,
            deadline,
            &support.path,
            false,
        )?;
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
        for (prefix, end, binding) in [
            ("source", &source, source_binding),
            ("destination", &destination, destination_binding),
        ] {
            let bounds = end.window.bounds;
            for (name, value) in [
                ("pid", end.process_id.to_string()),
                ("x", end.point.0.to_string()),
                ("y", end.point.1.to_string()),
                ("window-id", binding.client.to_string()),
                ("window-proof", binding.proof.to_string()),
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

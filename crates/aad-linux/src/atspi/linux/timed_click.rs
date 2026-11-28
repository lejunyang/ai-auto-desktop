use super::*;
use crate::atspi::{timed_click::unsupported, TimedClickRequest};
use aad_platform::input_timing::{plan_click, ClickGeometry, CLICK_PLANNER_VERSION};
use std::fmt::Write as _;

const FEATURE: &str = "timed-click-v1";
const CLEANUP: Duration = Duration::from_millis(250);
const HOLD_RECHECK_MS: u32 = 125;

fn require_features(has: impl Fn(&str) -> bool) -> Result<(), AutomationError> {
    [FEATURE, GEOMETRY_FEATURE, "cooperative-cancel"]
        .into_iter()
        .find(|feature| !has(feature))
        .map_or(Ok(()), |feature| Err(missing_helper_feature(feature)))
}

/// Apply the same capability rules to the observation Control will dispatch.
pub fn check_x11_timed_click_support(helper: &Value) -> Result<(), AutomationError> {
    if helper["ready"] == true {
        let features = helper["features"].as_array();
        return require_features(|name| {
            features.is_some_and(|list| list.iter().any(|f| f == name))
        });
    }
    if helper["code"] == "DRIVER.HELPER_INCOMPATIBLE" {
        let mut error = incompatible_helper("XTest helper handshake is incompatible");
        if let Some(message) = helper["message"].as_str() {
            error.message = message.into();
        }
        return Err(error);
    }
    Err(unsupported())
}

impl LinuxBackend {
    pub(super) fn timed_click_helper(&self, deadline: Instant) -> Result<PathBuf, AutomationError> {
        if crate::wayland_portal::is_wayland_session() {
            return Err(unsupported());
        }
        let support = self.probe_input(deadline)?;
        require_features(|feature| support.has(feature))?;
        Ok(support.path)
    }

    pub(super) fn run_timed_click(
        &self,
        request: TimedClickRequest<'_>,
    ) -> Result<Value, AutomationError> {
        let TimedClickRequest {
            target,
            window,
            point,
            window_bounds,
            target_bounds,
            hold_ms,
            timing,
            process_id,
            deadline,
            validate,
        } = request;
        self.check_cancelled(false)?;
        let path = self.timed_click_helper(deadline)?;
        let binding =
            self.verify_window_with_helper(target, window, process_id, deadline, &path, true)?;
        let dispatch_deadline = deadline - CLEANUP;
        remaining(dispatch_deadline, false)?;
        let mut args = vec![
            "timed-click-origin".into(),
            "--expected-pid".into(),
            process_id.to_string(),
            "--deadline-monotonic-ns".into(),
            monotonic_deadline_ns(dispatch_deadline)?.to_string(),
        ];
        append_window_args(&mut args, "--window-", window, Some(binding));
        let output = self.run_helper(
            &path,
            &args,
            None,
            dispatch_deadline,
            XTEST_OUTPUT_LIMIT,
            XTEST_OUTPUT_LIMIT,
        )?;
        if !output.status.success() {
            return parse_input_helper(output, "timed_click_origin", 0, process_id);
        }
        let origin: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
            driver_error(
                "DRIVER.ACTION_FAILED",
                "XTest helper returned no pointer origin",
            )
        })?;
        let coordinate = |field| {
            origin[field]
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| driver_error("DRIVER.ACTION_FAILED", "invalid XTest pointer origin"))
        };
        let start = (coordinate("x")?, coordinate("y")?);
        let seed = timing
            .seed
            .unwrap_or_else(|| uuid::Uuid::new_v4().as_u128() as u32);
        let mut geometry = ClickGeometry {
            start,
            target: point,
            window: window_bounds,
            hit: target_bounds,
            allow_overshoot: true,
        };
        let plan_for = |geometry| {
            plan_click(geometry, seed, timing.speed, hold_ms).ok_or_else(|| {
                driver_error(
                    "DRIVER.POINT_NOT_PROVEN",
                    "timed click start and destination must be inside the observed X11 window",
                )
            })
        };
        let mut plan = plan_for(geometry)?;
        let mut overshoot = plan
            .moves
            .iter()
            .filter(|step| step.overshoot)
            .map(|step| step.point)
            .collect::<Vec<_>>();
        if !overshoot.is_empty() {
            match validate(&overshoot, false) {
                Ok(()) => {}
                Err(error) if error.code == "DRIVER.POINT_NOT_PROVEN" => {
                    geometry.allow_overshoot = false;
                    plan = plan_for(geometry)?;
                    overshoot.clear();
                }
                Err(error) => return Err(error),
            }
        }
        let receipt = json!({
            "planner_version": CLICK_PLANNER_VERSION, "seed": seed, "speed": timing.speed,
            "movement_ms": plan.movement_ms, "settle_ms": plan.settle_ms, "hold_ms": plan.hold_ms,
            "moves": plan.moves.len(), "overshoot": !overshoot.is_empty(),
        });
        if remaining(dispatch_deadline, false)?
            < Duration::from_millis(u64::from(plan.duration_ms()) + 50)
        {
            return Err(driver_error(
                "DRIVER.TIMEOUT",
                "the timed click cannot finish within the input deadline",
            )
            .with_detail("dispatch_started", json!(false))
            .with_detail("input_timing", receipt));
        }
        let (sx, sy) = start;
        let (x, y) = point;
        let (left, top) = target_bounds.min;
        let (right, bottom) = target_bounds.max;
        let mut payload = format!(
            "{sx} {sy} {x} {y} {left} {top} {right} {bottom} {} {} {}\n",
            plan.settle_ms,
            plan.hold_ms,
            plan.moves.len()
        );
        for step in &plan.moves {
            let (x, y) = step.point;
            writeln!(
                payload,
                "{} {x} {y} {}",
                step.at_ms,
                u8::from(step.overshoot)
            )
            .unwrap();
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        args[0] = "timed-click".into();
        args.extend(["--anchor-token".into(), nonce.clone()]);
        let before_press = 2 + usize::from(!overshoot.is_empty());
        let hold_checks = (plan.hold_ms.saturating_sub(1) / HOLD_RECHECK_MS) as usize;
        let handshakes = before_press + hold_checks + 1;
        let validate_phase = Arc::new(move |index| {
            if index == 1 && !overshoot.is_empty() {
                validate(&overshoot, false)
            } else {
                validate(&[point], index >= before_press)
            }
        });
        let input = anchored_helper::run(anchored_helper::Request {
            path: &path,
            args: &args,
            nonce: &nonce,
            payload: Some(payload.as_bytes()),
            handshakes,
            poll: Duration::from_millis(1),
            cleanup_grace: CLEANUP,
            deadline: dispatch_deadline,
            token: self.cancellation.as_ref(),
            validation: anchored_helper::Validation::Worker {
                busy: self.path_validation_busy.clone(),
                validate: validate_phase,
            },
        })
        .and_then(|output| {
            parse_input_helper(
                output,
                "timed_click",
                (plan.moves.len() + 2) as u64,
                process_id,
            )
        })
        .map_err(|error| error.with_detail("input_timing", receipt.clone()))?;
        Ok(json!({
            "native_interface": "AT-SPI hit revalidation -> XTEST", "synthetic_input": true,
            "input": input, "input_timing": receipt,
        }))
    }
}

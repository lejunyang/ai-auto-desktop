use super::*;
use aad_platform::input_timing::{ClickBounds, MAX_CLICK_SPEED, MIN_CLICK_SPEED};

pub(super) fn unsupported() -> AutomationError {
    driver_error(
        "DRIVER.ACTION_UNSUPPORTED",
        "timed clicks require the Linux AT-SPI X11 backend",
    )
}

#[derive(Clone, Copy)]
pub struct TimedClickOptions {
    pub(super) seed: Option<u32>,
    pub(super) speed: f64,
}

impl TimedClickOptions {
    pub(super) fn parse(value: &Value) -> Result<Self, AutomationError> {
        let object = value
            .as_object()
            .ok_or_else(|| invalid("input_timing must be an object"))?;
        only_keys(object, &["seed", "speed"])?;
        let seed = object
            .get("seed")
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| invalid("input_timing.seed must be a 32-bit unsigned integer"))
            })
            .transpose()?;
        let speed = object
            .get("speed")
            .map(|value| {
                value
                    .as_f64()
                    .filter(|speed| (MIN_CLICK_SPEED..=MAX_CLICK_SPEED).contains(speed))
                    .ok_or_else(|| invalid("input_timing.speed must be in 0.5..2"))
            })
            .transpose()?
            .unwrap_or(1.0);
        Ok(Self { seed, speed })
    }
}

type ClickPoint = (i32, i32);

pub type TimedClickValidation =
    Arc<dyn Fn(&[ClickPoint], bool) -> Result<(), AutomationError> + Send + Sync>;

pub struct TimedClickRequest<'a> {
    pub(super) target: &'a NativeRef,
    pub(super) window: &'a WindowAnchor,
    pub(super) point: ClickPoint,
    pub(super) window_bounds: ClickBounds,
    pub(super) target_bounds: ClickBounds,
    pub(super) hold_ms: Option<u16>,
    pub(super) timing: TimedClickOptions,
    pub(super) process_id: u32,
    pub(super) deadline: Instant,
    pub(super) validate: TimedClickValidation,
}

pub(super) struct ObservedClick<'a> {
    pub(super) args: &'a Map<String, Value>,
    pub(super) fresh: &'a Record,
    pub(super) id: &'a str,
    pub(super) node: &'a Value,
    pub(super) target: &'a NativeRef,
    pub(super) window: Option<&'a WindowAnchor>,
    pub(super) point: ClickPoint,
    pub(super) hold_ms: Option<u16>,
    pub(super) timing: TimedClickOptions,
    pub(super) deadline: Instant,
}

fn inclusive_bounds(bounds: Bounds) -> Result<ClickBounds, AutomationError> {
    let max = |start: i32, size| {
        i32::checked_sub(size, 1)
            .filter(|size| *size >= 0)
            .and_then(|offset| start.checked_add(offset))
            .ok_or_else(|| invalid("timed click requires positive, finite bounds"))
    };
    Ok(ClickBounds {
        min: (bounds.x, bounds.y),
        max: (max(bounds.x, bounds.width)?, max(bounds.y, bounds.height)?),
    })
}

fn enabled_button(node: &Value) -> bool {
    matches!(node["role"].as_str(), Some("push_button" | "button"))
        && node["states"]["protected"] != true
        && node["provenance"]["value_redacted"] != true
        && node["states"]["enabled"] == true
        && node["states"]["sensitive"] == true
        && node["states"]["visible"] == true
        && node["states"]["showing"] == true
        && node["actions"]
            .as_array()
            .is_some_and(|actions| actions.iter().any(|a| a == "pointer_gesture"))
}

fn focused_element(record: &Record, window: &WindowAnchor) -> Option<NativeRef> {
    let nodes = record.public["nodes"].as_array()?;
    nodes.iter().find_map(|node| {
        let id = node["node_id"].as_str()?;
        (node["states"]["focused"] == true
            && input_window(record, id).is_some_and(|w| w.native == window.native))
        .then(|| record.handles[id].clone())
    })
}

impl AtspiProvider {
    pub(super) fn dispatch_timed_click(
        &self,
        observed: ObservedClick<'_>,
    ) -> Result<Value, AutomationError> {
        let ObservedClick {
            args,
            fresh,
            id,
            node,
            target,
            window,
            point,
            hold_ms,
            timing,
            deadline,
        } = observed;
        let deadline = deadline.min(Instant::now() + Duration::from_secs(7));
        let window = window
            .filter(|window| window.x11.is_some())
            .ok_or_else(|| {
                driver_error(
                    "DRIVER.ACTION_UNSUPPORTED",
                    "timed click requires a proven X11 window",
                )
            })?;
        if !enabled_button(node) {
            return Err(driver_error(
                "DRIVER.ACTION_UNSUPPORTED",
                "timed click requires a visible, enabled button",
            ));
        }
        let bounds = parse_bounds(node.get("bounds"))?;
        let window_bounds = inclusive_bounds(window.bounds)?;
        let target_bounds = inclusive_bounds(bounds)?;
        let process_id = process_id(node, fresh)?;
        let original = fresh.clone();
        let original_id = id.to_string();
        let mut original_args = args.clone();
        original_args.insert("target".into(), super::target(fresh, id));
        let expected_window = window.clone();
        // Read privately so a timed action cannot replace the public snapshot
        // during a phase check or from a late worker after cancellation.
        let validator = self.private_validator();
        let before = validator.private_window_record(fresh, id, "timed click", deadline)?;
        let before = before.as_ref().map_or(fresh, |(record, _)| record);
        let original_focus = focused_element(before, window);
        let validate = Arc::new(move |points: &[ClickPoint], after_press: bool| {
            if let Some(token) = &validator.cancellation {
                token.check()?;
            }
            let now = validator.capture_scoped_with_window(
                &original.application_selector,
                original.max_depth,
                original.max_nodes,
                original.scope.clone(),
                deadline,
                Some(&expected_window),
            )?;
            let (id, node, _) = Self::revalidate_target(&original_args, &original, &now, deadline)?;
            revalidate_x11_window(&original, &original_id, &now, &id)?;
            if parse_bounds(node.get("bounds"))? != bounds || !enabled_button(&node) {
                return Err(stale("timed click button geometry or input state changed"));
            }
            let expanded = validator.private_window_record(&now, &id, "timed click", deadline)?;
            let (visibility, id) = expanded
                .as_ref()
                .map_or((&now, id.as_str()), |(record, id)| (record, id.as_str()));
            let focus = focused_element(visibility, &expected_window);
            let expected = if after_press && node["states"]["focusable"] == true {
                visibility.handles.get(id)
            } else {
                original_focus.as_ref()
            };
            if focus.as_ref() != expected {
                return Err(driver_error(
                    "DRIVER.FOCUS_LOST",
                    "focus moved away from the timed click's observed element",
                ));
            }
            for point in points {
                validator.validate_path_point(
                    visibility,
                    id,
                    *point,
                    Some(&expected_window.native),
                    deadline,
                )?;
            }
            remaining(deadline, false)?;
            Ok(())
        });
        self.backend.timed_click(TimedClickRequest {
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
        })
    }
}

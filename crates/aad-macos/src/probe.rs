//! The macOS part of `aad probe`: read-only checks of what desktop
//! automation needs here. Nothing is asked for, injected or captured.
use aad_platform::probe::{Check, State};
use serde_json::json;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

pub fn run() -> Vec<Check> {
    let accessibility = unsafe { AXIsProcessTrusted() };
    let capture = unsafe { CGPreflightScreenCaptureAccess() };
    vec![
        Check::new(
            "macos.accessibility",
            if accessibility {
                State::Available
            } else {
                State::Unavailable
            },
            if accessibility {
                "Accessibility trust is granted to this process identity."
            } else {
                "Accessibility trust is not granted to this process identity."
            },
            json!({"preflight_completed": true, "authorized": accessibility, "prompt_requested": false}),
        ),
        Check::new(
            "macos.screen_capture",
            if capture {
                State::Available
            } else {
                State::Unavailable
            },
            if capture {
                "Screen Capture permission is granted to this process identity."
            } else {
                "Screen Capture permission is not granted to this process identity."
            },
            json!({"preflight_completed": true, "authorized": capture, "prompt_requested": false, "capture_attempted": false}),
        ),
    ]
}

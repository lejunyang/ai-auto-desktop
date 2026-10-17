//! Real signed-helper transport regression, without desktop writes or TCC.
#![cfg(target_os = "macos")]

use aad_runtime::Provider;
use serde_json::json;
use std::time::Duration;

#[test]
fn small_requests_reply_without_closing_the_input_pipe() {
    let provider = aad_macos_ax::native_provider().unwrap();
    // Construction handshakes over the same open pipe. A second request also
    // proves the helper did not need EOF to dispatch its first frame.
    for _ in 0..2 {
        let result = provider
            .invoke(
                "desktop.macos_ax.list_apps@1",
                json!({}),
                Some(Duration::from_secs(10)),
            )
            .expect("signed helper must respond with stdin still open");
        assert!(result["apps"].is_array(), "{result}");
    }
}

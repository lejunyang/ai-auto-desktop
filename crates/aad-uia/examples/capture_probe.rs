// Does the capture session work on a real window?
//
// The unit tests cover coalescing and the buffer, but nothing in them proves a
// subscription is ever established or that a real click reaches it -- and that
// is precisely where the two spikes found surprises.
//
// Watches a window, reports what it captured, and says which mechanisms were
// installed so partial coverage is visible rather than looking like silence.

use std::time::{Duration, Instant};

use aad_uia::capture::{coalesce, recordable};

fn main() {
    let needle = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: capture_probe <window id> [seconds]");
        std::process::exit(2);
    });
    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(15);

    let session = match aad_uia::windows::CaptureSession::start(&needle, 512) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("could not start capturing: {error:?}");
            std::process::exit(1);
        }
    };
    println!("WATCHING sources={:?}", session.sources);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut collected = Vec::new();
    let mut dropped_total = 0;
    while Instant::now() < deadline {
        let (events, dropped) = session.drain(128);
        collected.extend(events);
        dropped_total += dropped;
        std::thread::sleep(Duration::from_millis(150));
    }
    let (events, dropped) = session.drain(512);
    collected.extend(events);
    dropped_total += dropped;

    println!(
        "\n=== 原始事件 {} 个（丢弃 {}）===",
        collected.len(),
        dropped_total
    );
    for event in collected.iter().take(24) {
        let who = event
            .node
            .as_ref()
            .map(|node| format!("{} {:?}", node.role, node.name.as_deref().unwrap_or("")))
            .unwrap_or_else(|| "<无法定位>".into());
        println!(
            "  [{}] {} via {} -> {}",
            event.sequence,
            event.kind.as_str(),
            event.source,
            who
        );
    }

    let steps = coalesce(recordable(collected));
    println!("\n=== 归并后会录成 {} 步 ===", steps.len());
    for step in &steps {
        let who = step
            .node
            .as_ref()
            .map(|node| {
                format!(
                    "{} {:?} value={:?}",
                    node.role,
                    node.name.as_deref().unwrap_or(""),
                    node.value.as_deref().unwrap_or("")
                )
            })
            .unwrap_or_else(|| "<无法定位>".into());
        println!("  {} -> {}", step.kind.action().unwrap_or("?"), who);
    }
}

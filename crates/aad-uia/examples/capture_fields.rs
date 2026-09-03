// What does a captured event give us to build a step from?
//
// A recorded step needs a locator, and `Locator::synthesize` needs the whole
// node list to judge uniqueness -- but a captured event carries one element,
// resolved at the moment it happened. So either capture takes a snapshot per
// event (expensive, and racy: the UI has already moved on), or it matches the
// captured element back into a snapshot taken at start.
//
// Which is viable depends on what the callback actually populates. If the
// captured node carries automation_id and class_name, matching it back into a
// snapshot is straightforward; if it is mostly empty, neither approach works
// and events would have to be resolved differently.
//
// So: capture events, then dump every field of each captured element, rather
// than designing against an assumption.

use std::time::{Duration, Instant};

fn main() {
    let window = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: capture_fields <window id> [seconds]");
        std::process::exit(2);
    });
    let seconds: u64 = std::env::args()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(12);

    let session = match aad_uia::windows::CaptureSession::start(&window, 512) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("could not start capturing: {error:?}");
            std::process::exit(1);
        }
    };
    println!("WATCHING sources={:?}", session.sources);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut collected = Vec::new();
    while Instant::now() < deadline {
        let (events, _) = session.drain(128);
        collected.extend(events);
        std::thread::sleep(Duration::from_millis(150));
    }
    let (events, _) = session.drain(512);
    collected.extend(events);

    let steps = aad_uia::capture::coalesce(aad_uia::capture::recordable(collected));
    println!("\n=== {} 步，每步的元素字段 ===", steps.len());
    for step in &steps {
        match &step.node {
            None => println!("  {} -> <无法定位>", step.kind.as_str()),
            Some(node) => {
                println!("  {} via {}", step.kind.as_str(), step.source);
                println!("      role          = {:?}", node.role);
                println!("      name          = {:?}", node.name);
                println!("      value         = {:?}", node.value);
                println!("      automation_id = {:?}", node.automation_id);
                println!("      class_name    = {:?}", node.class_name);
                println!("      framework_id  = {:?}", node.framework_id);
                println!("      actions       = {:?}", node.actions);
            }
        }
    }
}

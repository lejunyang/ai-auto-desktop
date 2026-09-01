//! A durable runner that can be killed, for testing crash recovery.
//!
//! Exists so a test can start a real run in a real process, kill that process
//! without warning, and then recover from nothing but the journal on disk.
//! Writing the crash state by hand only proves the recovery code does what its
//! author expected; killing a process proves the file is genuinely sufficient.
//!
//! Usage: `crash_runner <journal-path> <descriptor-path> <run-id>`
//!
//! It prints each completed step to stdout and flushes, so the parent knows when
//! real progress has been committed and can kill at a meaningful moment.

use std::io::Write;

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let [_, journal_path, descriptor_path, run_id] = arguments.as_slice() else {
        eprintln!("usage: crash_runner <journal> <descriptor> <run-id>");
        std::process::exit(2);
    };

    let raw = std::fs::read_to_string(descriptor_path).expect("read descriptor");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse descriptor");
    let descriptor =
        aad_core::compiler::compile_descriptor(value, None).expect("compile descriptor");

    let journal = aad_runtime::durable::JournalStore::open(journal_path).expect("open journal");
    let executor = aad_runtime::durable_exec::DurableExecutor::new(journal);

    // A short lease TTL so the parent does not have to wait long for the
    // abandoned lease to lapse after the kill.
    let options = aad_runtime::durable_exec::DurableOptions::default()
        .with_owner_id("crash-runner")
        .with_lease_ttl_seconds(1.0);

    println!("started");
    let _ = std::io::stdout().flush();

    match executor.start(&descriptor, Some(run_id), options) {
        Ok(outcome) => {
            println!("finished {}", outcome.run.status.as_str());
        }
        Err(error) => {
            println!("failed {}", error.code);
        }
    }
    let _ = std::io::stdout().flush();
}

//! Save and reopen a recording through the same code the GUI commands call.
//!
//! The Tauri commands are thin wrappers around this module, but "the unit tests
//! pass" is not the same claim as "a recording written to the real store can be
//! listed and read back". This exercises the actual filesystem in the actual
//! default location, then puts it back as it was.

use aad_gui::recordings;
use serde_json::json;

/// The document the GUI produces for a one-step recording, verbatim.
fn recorded() -> serde_json::Value {
    json!({
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Recording",
        "metadata": {"name": "integration-check"},
        "steps": [{
            "id": "step_1",
            "action": "set_value",
            "locator": {"role": "Edit", "name": "Body"},
            "summary": "role=Edit name=\"Body\"",
            "window": {"class_name": "Notepad", "process_name": "notepad.exe"},
            "window_title": "notes.txt - Notepad",
            "argument": "written by the recording",
            "enabled": true
        }]
    })
}

#[test]
fn a_recording_survives_the_real_store() {
    let name = "aad-integration-check";
    let directory = recordings::recordings_dir();
    println!("store: {}", directory.display());

    let recording_path = recordings::save_recording(name, &recorded()).expect("saving must work");
    let workflow_path = recordings::save_workflow(name, &json!({"kind": "Workflow"}))
        .expect("saving the workflow must work");
    println!("wrote: {}", recording_path.display());

    // Reading it back is the claim that matters; a written file that cannot be
    // reopened is not persistence.
    let reloaded = recordings::load_recording(&recording_path).expect("reopening must work");
    assert_eq!(
        reloaded,
        recorded(),
        "the round trip must not alter anything"
    );

    // And it has to be discoverable, or the Open dialog shows nothing.
    let listed = recordings::list_recordings().expect("listing must work");
    let found = listed
        .iter()
        .find(|entry| entry.name == name)
        .expect("the saved recording must appear in the list");
    assert!(found.path.ends_with(recordings::RECORDING_SUFFIX));
    assert!(found.modified.is_some(), "the list needs a time to sort by");
    println!("listed as: {} ({} total)", found.name, listed.len());

    // Leave the user's store as it was found.
    std::fs::remove_file(&recording_path).ok();
    std::fs::remove_file(&workflow_path).ok();
    assert!(
        !recordings::list_recordings()
            .unwrap()
            .iter()
            .any(|entry| entry.name == name),
        "the test must clean up after itself"
    );
}

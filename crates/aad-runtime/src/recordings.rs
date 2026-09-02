//! Saving and reopening recordings.
//!
//! Two files, deliberately distinct:
//!
//! * `<name>.recording.json` — the editable source, `kind: Recording`. This is
//!   what a person reopens to adjust steps.
//! * `<name>.workflow.json` — the compiled, runnable form, `kind: Workflow`,
//!   accepted by `aad run`.
//!
//! Compilation is one-way. A workflow can express far more than a recording can
//! represent, so reconstructing a recording from one would quietly lose the parts
//! a person edits. Keeping the source means editing never degrades.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Recordings hold locators, not references, so they stay valid between runs.
pub const RECORDING_SUFFIX: &str = ".recording.json";
pub const WORKFLOW_SUFFIX: &str = ".workflow.json";

/// Refuse anything larger than this rather than loading it into the UI.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// A name long enough to be descriptive, short enough for any filesystem.
const MAX_NAME_CHARS: usize = 96;

#[derive(Debug)]
pub struct StoreError {
    pub code: &'static str,
    pub message: String,
}

impl StoreError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// Where recordings live. Overridable so tests never touch a real home.
pub fn recordings_dir() -> PathBuf {
    if let Some(configured) = std::env::var_os("AAD_RECORDINGS_DIR") {
        return PathBuf::from(configured);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".ai-auto-desktop").join("recordings")
}

/// Check that a name is safe to turn into a filename.
///
/// A name reaches this function from the UI, so it is untrusted input that ends
/// up in a path. Rejecting separators and parent references keeps a recording
/// called `../../autorun` from being written outside the store.
fn validate_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::new("STORE.NAME_INVALID", "a recording needs a name"));
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(StoreError::new(
            "STORE.NAME_INVALID",
            format!("a name may be at most {MAX_NAME_CHARS} characters"),
        ));
    }
    // Anything that could change which directory is written to, plus the
    // characters Windows forbids in filenames outright.
    const FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'];
    if let Some(bad) = trimmed.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(StoreError::new(
            "STORE.NAME_INVALID",
            format!("a name may not contain {bad:?}"),
        ));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(StoreError::new(
            "STORE.NAME_INVALID",
            "a name may not contain control characters",
        ));
    }
    // `.` and `..` are directory references, not names.
    if trimmed.chars().all(|c| c == '.') {
        return Err(StoreError::new("STORE.NAME_INVALID", "a name must have some text in it"));
    }
    // A trailing dot or space is silently stripped by Windows, which would make
    // the saved file's name differ from the one shown in the UI.
    if trimmed.ends_with('.') {
        return Err(StoreError::new("STORE.NAME_INVALID", "a name may not end with a dot"));
    }
    Ok(trimmed)
}

/// Write `document` as the editable recording called `name`.
pub fn save_recording(name: &str, document: &Value) -> Result<PathBuf> {
    write_document(name, RECORDING_SUFFIX, document)
}

/// Write `descriptor` as the runnable workflow called `name`.
pub fn save_workflow(name: &str, descriptor: &Value) -> Result<PathBuf> {
    write_document(name, WORKFLOW_SUFFIX, descriptor)
}

/// Where the runnable workflow called `name` lives.
///
/// Goes through the same validation as saving, so a name that could escape the
/// store is refused here too and a caller cannot reach a file that `save_workflow`
/// would never have written.
pub fn workflow_path(name: &str) -> Result<PathBuf> {
    let name = validate_name(name)?;
    Ok(recordings_dir().join(format!("{name}{WORKFLOW_SUFFIX}")))
}

fn write_document(name: &str, suffix: &str, document: &Value) -> Result<PathBuf> {
    let name = validate_name(name)?;
    let directory = recordings_dir();
    fs::create_dir_all(&directory).map_err(|error| {
        StoreError::new(
            "STORE.WRITE_FAILED",
            format!("cannot create {}: {error}", directory.display()),
        )
    })?;

    let path = directory.join(format!("{name}{suffix}"));
    // Pretty-printed because these files are read and edited by hand.
    let mut serialized = serde_json::to_string_pretty(document)
        .map_err(|error| StoreError::new("STORE.WRITE_FAILED", error.to_string()))?;
    serialized.push('\n');

    // Write to a sibling then rename, so an interrupted save cannot leave a
    // half-written file where a valid recording used to be.
    let staging = directory.join(format!(".{name}{suffix}.partial"));
    fs::write(&staging, serialized.as_bytes()).map_err(|error| {
        StoreError::new("STORE.WRITE_FAILED", format!("cannot write {}: {error}", staging.display()))
    })?;
    fs::rename(&staging, &path).map_err(|error| {
        let _ = fs::remove_file(&staging);
        StoreError::new(
            "STORE.WRITE_FAILED",
            format!("cannot replace {}: {error}", path.display()),
        )
    })?;
    Ok(path)
}

/// Read a saved recording back.
///
/// Only accepts paths inside the store: a path also arrives from the UI, and
/// following an arbitrary one would turn "open a recording" into "read any file
/// on this machine and show it".
pub fn load_recording(path: &Path) -> Result<Value> {
    let directory = recordings_dir();
    // Compare canonical paths so that `store/../store/x` and a symlink out of
    // the store are both judged by where they actually lead.
    let resolved = fs::canonicalize(path).map_err(|error| {
        StoreError::new(
            "STORE.NOT_FOUND",
            format!("cannot open {}: {error}", path.display()),
        )
    })?;
    let root = fs::canonicalize(&directory).map_err(|error| {
        StoreError::new(
            "STORE.NOT_FOUND",
            format!("cannot open {}: {error}", directory.display()),
        )
    })?;
    if !resolved.starts_with(&root) {
        return Err(StoreError::new(
            "STORE.PATH_REFUSED",
            "only files inside the recordings folder can be opened",
        ));
    }

    let metadata = fs::metadata(&resolved)
        .map_err(|error| StoreError::new("STORE.NOT_FOUND", error.to_string()))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(StoreError::new(
            "STORE.FILE_TOO_LARGE",
            format!("{} is larger than {MAX_FILE_BYTES} bytes", resolved.display()),
        ));
    }

    let text = fs::read_to_string(&resolved)
        .map_err(|error| StoreError::new("STORE.READ_FAILED", error.to_string()))?;
    // A byte-order mark is invisible but breaks a JSON parser, and editors on
    // Windows add one without asking.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).map_err(|error| {
        StoreError::new(
            "STORE.FILE_INVALID",
            format!("{} is not valid JSON: {error}", resolved.display()),
        )
    })
}

/// One saved recording, as shown in the open list.
#[derive(Debug, serde::Serialize)]
pub struct SavedRecording {
    pub name: String,
    pub path: String,
    /// Seconds since the epoch, for ordering. Absent when unavailable.
    pub modified: Option<u64>,
}

/// List the saved recordings, most recently changed first.
///
/// A missing store is an empty list, not an error: nothing has been saved yet.
/// Only the editable sources are listed; compiled workflows are outputs, and
/// reopening one would not restore an editable session. To list what can be
/// *run*, use [`list_workflows`].
pub fn list_recordings() -> Result<Vec<SavedRecording>> {
    list_by_suffix(RECORDING_SUFFIX)
}

/// List the saved workflows, most recently changed first.
///
/// Distinct from [`list_recordings`] on purpose. That lists the editable sources
/// the desktop app reopens; this lists the compiled workflows that can actually
/// be run, which is what a caller wanting to *execute* something by name needs.
/// A recording with no compiled workflow beside it would be a misleading offer.
pub fn list_workflows() -> Result<Vec<SavedRecording>> {
    list_by_suffix(WORKFLOW_SUFFIX)
}

fn list_by_suffix(suffix: &str) -> Result<Vec<SavedRecording>> {
    let directory = recordings_dir();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(StoreError::new(
                "STORE.READ_FAILED",
                format!("cannot list {}: {error}", directory.display()),
            ))
        }
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(name) = filename.strip_suffix(suffix) else {
            continue;
        };
        // A leading dot marks an interrupted save's staging file.
        if name.is_empty() || filename.starts_with('.') {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs());
        found.push(SavedRecording {
            name: name.to_string(),
            path: path.to_string_lossy().to_string(),
            modified,
        });
    }

    found.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, MutexGuard};

    /// The store is chosen by an environment variable, which is process-wide, so
    /// tests that set it must not overlap.
    static GUARD: Mutex<()> = Mutex::new(());

    struct Sandbox {
        directory: PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl Sandbox {
        fn new(label: &str) -> Self {
            let lock = GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let directory = std::env::temp_dir().join(format!("aad-store-{label}"));
            let _ = fs::remove_dir_all(&directory);
            fs::create_dir_all(&directory).unwrap();
            std::env::set_var("AAD_RECORDINGS_DIR", &directory);
            Self { directory, _lock: lock }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            std::env::remove_var("AAD_RECORDINGS_DIR");
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn recording(name: &str) -> Value {
        json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Recording",
            "metadata": {"name": name},
            "steps": [{
                "id": "step_1",
                "action": "invoke",
                "locator": {"role": "Button", "name": "Save"},
                "summary": "Button \"Save\"",
                "window": {"title": "Notepad", "process_name": "notepad.exe", "class_name": "Notepad"},
                "enabled": true,
            }],
        })
    }

    #[test]
    fn a_saved_recording_can_be_read_back_unchanged() {
        let _sandbox = Sandbox::new("roundtrip");
        let original = recording("demo");

        let path = save_recording("demo", &original).expect("must save");
        let reloaded = load_recording(&path).expect("must reload");

        assert_eq!(reloaded, original, "a round trip must not alter the recording");
    }

    #[test]
    fn saving_twice_replaces_rather_than_duplicates() {
        let _sandbox = Sandbox::new("replace");
        save_recording("demo", &recording("demo")).unwrap();

        let mut updated = recording("demo");
        updated["steps"] = json!([]);
        let path = save_recording("demo", &updated).unwrap();

        assert_eq!(load_recording(&path).unwrap()["steps"], json!([]));
        assert_eq!(list_recordings().unwrap().len(), 1, "one name, one file");
    }

    #[test]
    fn a_name_cannot_escape_the_store() {
        let _sandbox = Sandbox::new("escape");
        // These would otherwise write outside the recordings folder.
        for hostile in ["../escaped", "..\\escaped", "sub/dir", "C:\\absolute"] {
            let error = save_recording(hostile, &recording("x"))
                .expect_err(&format!("{hostile:?} must be refused"));
            assert_eq!(error.code, "STORE.NAME_INVALID", "for {hostile:?}");
        }
    }

    #[test]
    fn an_empty_or_dotted_name_is_refused() {
        let _sandbox = Sandbox::new("blank");
        for hostile in ["", "   ", ".", "..", "trailing."] {
            let error = save_recording(hostile, &recording("x"))
                .expect_err(&format!("{hostile:?} must be refused"));
            assert_eq!(error.code, "STORE.NAME_INVALID", "for {hostile:?}");
        }
    }

    #[test]
    fn reading_outside_the_store_is_refused() {
        let sandbox = Sandbox::new("outside");
        // A real file that exists, just not one the UI should be able to read.
        let outsider = sandbox.directory.parent().unwrap().join("aad-outsider.json");
        fs::write(&outsider, "{\"secret\":true}").unwrap();

        let error = load_recording(&outsider).expect_err("must refuse a path outside the store");

        assert_eq!(error.code, "STORE.PATH_REFUSED");
        let _ = fs::remove_file(&outsider);
    }

    #[test]
    fn traversal_through_the_store_is_refused() {
        let sandbox = Sandbox::new("traverse");
        let outsider = sandbox.directory.parent().unwrap().join("aad-traversed.json");
        fs::write(&outsider, "{}").unwrap();

        // Starts inside the store but does not stay there.
        let path = sandbox.directory.join("..").join("aad-traversed.json");
        let error = load_recording(&path).expect_err("must judge where the path leads");

        assert_eq!(error.code, "STORE.PATH_REFUSED");
        let _ = fs::remove_file(&outsider);
    }

    #[test]
    fn a_corrupt_file_is_reported_rather_than_loaded_as_nothing() {
        let _sandbox = Sandbox::new("corrupt");
        let path = save_recording("broken", &recording("broken")).unwrap();
        fs::write(&path, "{ this is not json").unwrap();

        let error = load_recording(&path).expect_err("truncated JSON must be an error");

        assert_eq!(error.code, "STORE.FILE_INVALID");
    }

    #[test]
    fn a_byte_order_mark_does_not_break_loading() {
        // Windows editors add one invisibly, and it is not valid JSON.
        let _sandbox = Sandbox::new("bom");
        let path = save_recording("bom", &recording("bom")).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("\u{feff}{text}")).unwrap();

        let loaded = load_recording(&path).expect("a BOM must be tolerated");

        assert_eq!(loaded["kind"], "Recording");
    }

    #[test]
    fn listing_shows_only_editable_recordings() {
        let _sandbox = Sandbox::new("listing");
        save_recording("alpha", &recording("alpha")).unwrap();
        // A compiled workflow is an output; reopening one restores nothing.
        save_workflow("alpha", &json!({"kind": "Workflow"})).unwrap();

        let listed = list_recordings().unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "alpha");
        assert!(listed[0].path.ends_with(RECORDING_SUFFIX));
    }

    #[test]
    fn listing_an_absent_store_is_empty_not_an_error() {
        let sandbox = Sandbox::new("absent");
        fs::remove_dir_all(&sandbox.directory).unwrap();

        assert!(list_recordings().expect("nothing saved yet is not a failure").is_empty());
    }

    #[test]
    fn listing_puts_the_most_recent_first() {
        let _sandbox = Sandbox::new("ordering");
        save_recording("older", &recording("older")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        save_recording("newer", &recording("newer")).unwrap();

        let listed = list_recordings().unwrap();

        assert_eq!(listed[0].name, "newer", "the one just worked on comes first");
    }

    #[test]
    fn an_interrupted_save_leaves_no_partial_file_in_the_listing() {
        let _sandbox = Sandbox::new("partial");
        save_recording("real", &recording("real")).unwrap();
        // Imitate the staging file a crashed save would leave behind.
        fs::write(recordings_dir().join(".real.recording.json.partial"), "{").unwrap();

        let listed = list_recordings().unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "real");
    }

    #[test]
    fn a_workflow_is_saved_beside_its_recording_under_a_distinct_name() {
        // The two must not collide: saving the runnable form cannot overwrite
        // the editable source it was compiled from.
        let _sandbox = Sandbox::new("distinct");

        let recording_path = save_recording("both", &recording("both")).unwrap();
        let workflow_path = save_workflow("both", &json!({"kind": "Workflow"})).unwrap();

        assert_ne!(recording_path, workflow_path);
        assert_eq!(load_recording(&recording_path).unwrap()["kind"], "Recording");
    }

    #[test]
    fn workflow_path_agrees_with_where_saving_puts_the_file() {
        // Two ways of naming the same file. If they ever disagree, a caller
        // looking a workflow up by name would miss one that was really saved.
        let _sandbox = Sandbox::new("path-agrees");

        let saved = save_workflow("checkout", &json!({"kind": "Workflow"})).unwrap();
        let looked_up = workflow_path("checkout").unwrap();

        assert_eq!(saved, looked_up);
        assert!(looked_up.exists());
    }

    #[test]
    fn a_workflow_name_that_would_escape_the_store_is_refused() {
        // The name reaches this through a tool call, so it is untrusted input
        // that becomes a path. It must be refused rather than resolved.
        let _sandbox = Sandbox::new("path-escape");

        for name in ["../outside", "..\\outside", "nested/name", "nested\\name", "..", ""] {
            let refused = workflow_path(name);
            assert!(
                refused.is_err(),
                "{name:?} must not resolve to a path"
            );
            assert_eq!(refused.unwrap_err().code, "STORE.NAME_INVALID");
        }
    }

    #[test]
    fn listing_workflows_and_listing_recordings_answer_different_questions() {
        // A recording is editable source; a workflow is what can be run. Mixing
        // them would offer a caller something it cannot execute, or hide
        // something it can.
        let _sandbox = Sandbox::new("two-listings");

        save_recording("draft", &recording("draft")).unwrap();
        save_workflow("runnable", &json!({"kind": "Workflow"})).unwrap();

        let recordings: Vec<String> =
            list_recordings().unwrap().into_iter().map(|item| item.name).collect();
        let workflows: Vec<String> =
            list_workflows().unwrap().into_iter().map(|item| item.name).collect();

        assert_eq!(recordings, vec!["draft".to_string()]);
        assert_eq!(workflows, vec!["runnable".to_string()]);
    }

    #[test]
    fn an_interrupted_workflow_save_is_not_offered_as_runnable() {
        // Staging files start with a dot. Listing one would hand out a path to
        // a half-written file.
        let _sandbox = Sandbox::new("partial-workflow");

        save_workflow("real", &json!({"kind": "Workflow"})).unwrap();
        fs::write(
            recordings_dir().join(format!(".ghost{WORKFLOW_SUFFIX}.partial")),
            b"{",
        )
        .unwrap();
        fs::write(recordings_dir().join(format!(".ghost{WORKFLOW_SUFFIX}")), b"{").unwrap();

        let names: Vec<String> =
            list_workflows().unwrap().into_iter().map(|item| item.name).collect();

        assert_eq!(names, vec!["real".to_string()]);
    }
}

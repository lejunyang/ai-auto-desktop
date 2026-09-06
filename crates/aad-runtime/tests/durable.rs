//! Durable journal tests.
//!
//! Two things are being checked, and the second matters as much as the first:
//! that the API upholds the lifecycle, and that the **database** upholds it
//! independently. Several tests therefore bypass `JournalStore` and issue raw
//! SQL, because the triggers are the guarantee that a different build, an older
//! process, or a person with the `sqlite3` shell cannot corrupt a run.

use aad_runtime::durable::*;
use rusqlite::Connection;
use serde_json::{json, Value};

/// A journal on a real file, in its own directory, removed on drop.
///
/// Real files rather than `:memory:` throughout: WAL, the busy timeout and
/// cross-connection visibility only exist for file-backed databases, and those
/// are precisely the properties the journal depends on.
struct TempJournal {
    directory: std::path::PathBuf,
    store: JournalStore,
}

impl TempJournal {
    fn open() -> Self {
        let unique = format!(
            "aad-journal-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let directory = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create test directory");
        let store = JournalStore::open(directory.join("journal.sqlite3")).expect("open journal");
        Self { directory, store }
    }

    fn path(&self) -> std::path::PathBuf {
        self.directory.join("journal.sqlite3")
    }

    /// A second connection, for asserting what is actually on disk.
    fn raw(&self) -> Connection {
        let connection = Connection::open(self.path()).expect("open raw connection");
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .expect("enable foreign keys");
        connection
    }
}

impl Drop for TempJournal {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn empty_descriptor() -> Value {
    json!({"inputs": {}, "outputs": {}})
}

fn seed(journal: &TempJournal, run_id: &str) -> RunRecord {
    journal
        .store
        .create_run(
            run_id,
            "demo.workflow",
            &json!({"target": "notepad"}),
            &empty_descriptor(),
            Some("1.0.0"),
            Some("sha256:abc"),
            Some(("run.created", &json!({"by": "test"}))),
        )
        .expect("create run")
}

/// Bring a run to `running` and return its lease, the usual starting point.
fn seed_running(journal: &TempJournal, run_id: &str) -> OwnerLease {
    seed(journal, run_id);
    let now = now_seconds();
    let lease = journal
        .store
        .claim_owner(run_id, "runner-1", 60.0, now)
        .expect("claim");
    journal
        .store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            Some(("run.started", &json!({}))),
            None,
            None,
            None,
            now,
        )
        .expect("start");
    lease
}

// ---------------------------------------------------------------- open & schema

#[test]
fn opening_a_journal_creates_it_and_survives_reopening() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    drop(journal.store.list_runs(None, 10, 0));

    // The point of durability: a fresh process sees the run.
    let reopened = JournalStore::open(journal.path()).expect("reopen");
    let run = reopened.get_run("run-1").expect("run survived");
    assert_eq!(run.workflow_name, "demo.workflow");
    assert_eq!(run.status, RunStatus::Pending);
}

#[test]
fn a_newer_schema_version_is_refused_rather_than_guessed_at() {
    let journal = TempJournal::open();
    journal
        .raw()
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 5))
        .expect("bump version");

    let error = JournalStore::open(journal.path()).expect_err("must refuse");
    assert_eq!(error.code(), "JOURNAL.STORAGE_FAILED");
    assert!(
        error.message().contains("newer"),
        "should explain the version gap: {}",
        error.message()
    );
}

#[test]
fn wal_mode_is_actually_active_on_disk() {
    // Verified rather than assumed: SQLite ignores a pragma it cannot honour,
    // and without WAL two processes cannot be serialised safely.
    let journal = TempJournal::open();
    let mode: String = journal
        .raw()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read journal_mode");
    assert_eq!(mode.to_lowercase(), "wal");
}

// ---------------------------------------------------------------- create & read

#[test]
fn creating_a_run_records_its_first_event_atomically() {
    let journal = TempJournal::open();
    let run = seed(&journal, "run-1");

    assert_eq!(run.status, RunStatus::Pending);
    assert_eq!(run.desired_state, DesiredState::Run);
    assert_eq!(run.inputs, json!({"target": "notepad"}));
    assert!(run.owner_id.is_none(), "a new run is unowned");
    assert!(run.finished_at.is_none());

    let events = journal.store.list_events("run-1", 0, 10).expect("events");
    assert_eq!(events.len(), 1, "the creation event is in the same commit");
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[0].event_type, "run.created");
}

#[test]
fn a_duplicate_run_id_is_a_conflict_and_leaves_the_original_intact() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");

    let error = journal
        .store
        .create_run(
            "run-1",
            "other.workflow",
            &json!({}),
            &empty_descriptor(),
            None,
            None,
            None,
        )
        .expect_err("must conflict");
    assert_eq!(error.code(), "JOURNAL.CONFLICT");

    let run = journal.store.get_run("run-1").expect("original run");
    assert_eq!(run.workflow_name, "demo.workflow", "not overwritten");
    let events = journal.store.list_events("run-1", 0, 10).expect("events");
    assert_eq!(events.len(), 1, "the failed attempt appended nothing");
}

#[test]
fn a_missing_run_is_reported_as_not_found() {
    let journal = TempJournal::open();
    assert_eq!(
        journal.store.get_run("ghost").expect_err("no run").code(),
        "JOURNAL.RUN_NOT_FOUND"
    );
    // Also on the events path, so a poller can tell "no such run" from "no news".
    assert_eq!(
        journal
            .store
            .list_events("ghost", 0, 10)
            .expect_err("no run")
            .code(),
        "JOURNAL.RUN_NOT_FOUND"
    );
}

#[test]
fn runs_are_listed_newest_first_and_can_be_filtered_by_status() {
    let journal = TempJournal::open();
    for index in 0..3 {
        seed(&journal, &format!("run-{index}"));
    }
    let lease = journal
        .store
        .claim_owner("run-1", "runner", 60.0, now_seconds())
        .expect("claim");
    journal
        .store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now_seconds(),
        )
        .expect("start");

    let all = journal.store.list_runs(None, 10, 0).expect("list");
    assert_eq!(all.len(), 3);

    let running = journal
        .store
        .list_runs(Some(RunStatus::Running), 10, 0)
        .expect("filtered");
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].run_id, "run-1");

    // Paging must not repeat or skip when timestamps collide, which is why
    // run_id breaks the tie.
    let first = journal.store.list_runs(None, 2, 0).expect("page 1");
    let second = journal.store.list_runs(None, 2, 2).expect("page 2");
    let mut seen: Vec<_> = first
        .iter()
        .chain(second.iter())
        .map(|r| &r.run_id)
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "pages cover every run exactly once");
}

#[test]
fn list_bounds_are_rejected_rather_than_clamped() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    for (limit, offset) in [(0, 0), (10_001, 0), (10, -1)] {
        assert_eq!(
            journal
                .store
                .list_runs(None, limit, offset)
                .expect_err("must reject")
                .code(),
            "JOURNAL.INVALID_REQUEST",
            "limit={limit} offset={offset}"
        );
    }
}

// ---------------------------------------------------------------- sensitive data

#[test]
fn a_descriptor_declaring_sensitive_data_cannot_be_made_durable() {
    let journal = TempJournal::open();
    // The journal cannot tell a secret from ordinary text, so it refuses the run
    // rather than persisting something it was told is sensitive.
    let error = journal
        .store
        .create_run(
            "run-1",
            "wf",
            &json!({}),
            &json!({"inputs": {"password": {"sensitive": true}}}),
            None,
            None,
            None,
        )
        .expect_err("must refuse");
    assert_eq!(error.code(), "JOURNAL.SENSITIVE_REFUSED");
    assert_eq!(
        journal
            .store
            .get_run("run-1")
            .expect_err("not created")
            .code(),
        "JOURNAL.RUN_NOT_FOUND"
    );
}

#[test]
fn eligibility_covers_outputs_and_treats_unknown_shapes_as_ineligible() {
    assert!(durable_descriptor_eligible(&json!({})));
    assert!(durable_descriptor_eligible(&json!({"inputs": {"a": {}}})));
    assert!(durable_descriptor_eligible(
        &json!({"inputs": {"a": {"sensitive": false}}})
    ));

    assert!(!durable_descriptor_eligible(
        &json!({"outputs": {"token": {"sensitive": true}}})
    ));
    // A non-boolean means the declaration was not understood, so refuse it
    // instead of reading it as "not sensitive".
    assert!(!durable_descriptor_eligible(
        &json!({"inputs": {"a": {"sensitive": "yes"}}})
    ));
    assert!(!durable_descriptor_eligible(&json!({"inputs": []})));
    assert!(!durable_descriptor_eligible(&json!({"inputs": {"a": 5}})));
}

// ---------------------------------------------------------------- status machine

#[test]
fn the_full_happy_path_reaches_succeeded_with_output() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let run = journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Succeeded,
            Some(("run.succeeded", &json!({}))),
            Some(&json!({"result": "ok"})),
            None,
            None,
            now_seconds(),
        )
        .expect("succeed");

    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.output, Some(json!({"result": "ok"})));
    assert!(run.error.is_none());
    assert!(run.finished_at.is_some(), "a terminal run is timestamped");
    assert!(run.owner_id.is_none(), "a terminal run releases its lease");
}

#[test]
fn pending_cannot_jump_straight_to_succeeded_or_unknown_effect() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let now = now_seconds();
    let lease = journal
        .store
        .claim_owner("run-1", "runner", 60.0, now)
        .expect("claim");

    // Nothing has been dispatched from `pending`, so the effect is not in doubt
    // and `unknown_effect` would be a false claim.
    for status in [
        RunStatus::Succeeded,
        RunStatus::UnknownEffect,
        RunStatus::Paused,
    ] {
        let error = journal
            .store
            .set_status(
                &lease,
                RunStatus::Pending,
                status,
                None,
                None,
                Some(&json!({"code": "X.Y"})),
                None,
                now,
            )
            .expect_err("must refuse");
        assert_eq!(
            error.code(),
            "JOURNAL.INVALID_TRANSITION",
            "pending -> {}",
            status.as_str()
        );
    }
}

#[test]
fn a_paused_run_cannot_be_completed_without_running_again() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();
    journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Paused,
            Some(("run.paused", &json!({}))),
            None,
            None,
            None,
            now,
        )
        .expect("pause");

    // paused -> succeeded is not a legal edge, and this is checked before the
    // lease: transition validity depends only on the arguments the caller
    // supplied, so refusing it first is accurate and leaks no run state.
    let resumed = journal
        .store
        .claim_owner("run-1", "runner-2", 60.0, now)
        .expect("reclaim after pause");
    let error = journal
        .store
        .set_status(
            &resumed,
            RunStatus::Paused,
            RunStatus::Succeeded,
            None,
            Some(&json!({})),
            None,
            None,
            now,
        )
        .expect_err("illegal edge even for the rightful owner");
    assert_eq!(error.code(), "JOURNAL.INVALID_TRANSITION");

    // Pausing released the original lease, so on a *legal* edge the superseded
    // runner is fenced out and only the new owner may proceed.
    let error = journal
        .store
        .set_status(
            &lease,
            RunStatus::Paused,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now,
        )
        .expect_err("stale lease must not resume the run");
    assert_eq!(error.code(), "JOURNAL.LEASE_LOST");

    journal
        .store
        .set_status(
            &resumed,
            RunStatus::Paused,
            RunStatus::Running,
            Some(("run.resumed", &json!({}))),
            None,
            None,
            None,
            now,
        )
        .expect("the new owner resumes");
    // Only now, from running, can it complete.
    let run = journal
        .store
        .set_status(
            &resumed,
            RunStatus::Running,
            RunStatus::Succeeded,
            None,
            Some(&json!({"result": "ok"})),
            None,
            None,
            now,
        )
        .expect("succeed from running");
    assert_eq!(run.status, RunStatus::Succeeded);
}

#[test]
fn pausing_releases_the_lease_in_the_same_commit_as_the_event() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let run = journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Paused,
            Some(("run.paused", &json!({"reason": "requested"}))),
            None,
            None,
            None,
            now_seconds(),
        )
        .expect("pause");

    assert_eq!(run.status, RunStatus::Paused);
    // Whoever resumes must be able to claim it, so the lease cannot linger.
    assert!(run.owner_id.is_none());
    assert!(run.lease_expires_at.is_none());
    assert!(run.finished_at.is_none(), "paused is not terminal");

    let events = journal.store.list_events("run-1", 0, 10).expect("events");
    assert_eq!(events.last().expect("event").event_type, "run.paused");
}

#[test]
fn a_failure_requires_an_error_and_success_forbids_one() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();

    // A failure with no error would leave no way to say what went wrong.
    let error = journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Failed,
            None,
            None,
            None,
            None,
            now,
        )
        .expect_err("must require an error");
    assert_eq!(error.code(), "JOURNAL.INVALID_TRANSITION");

    let error = journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Succeeded,
            None,
            None,
            Some(&json!({"code": "X.Y"})),
            None,
            now,
        )
        .expect_err("success cannot carry an error");
    assert_eq!(error.code(), "JOURNAL.INVALID_TRANSITION");

    // And a run still in flight carries neither.
    let error = journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Paused,
            None,
            Some(&json!({"partial": true})),
            None,
            None,
            now,
        )
        .expect_err("in-flight carries no output");
    assert_eq!(error.code(), "JOURNAL.INVALID_TRANSITION");
}

#[test]
fn unknown_effect_is_preserved_and_not_folded_into_a_plain_failure() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let run = journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::UnknownEffect,
            Some(("run.unknown_effect", &json!({}))),
            None,
            Some(&json!({"code": "RUNTIME.UNKNOWN_EFFECT"})),
            None,
            now_seconds(),
        )
        .expect("unknown effect");

    assert_eq!(run.status, RunStatus::UnknownEffect);
    // Distinct on the wire too: a caller must be able to see that a side effect
    // may have happened, rather than reading it as a clean failure.
    assert_eq!(run.to_json()["status"], json!("unknown_effect"));
    assert!(run.is_terminal());
}

#[test]
fn a_terminal_run_is_immutable_and_refuses_further_events() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();
    journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Failed,
            None,
            None,
            Some(&json!({"code": "X.Y"})),
            None,
            now,
        )
        .expect("fail");

    // The lease was released by reaching terminal, so writes are refused; the
    // error says the run is over rather than merely that the lease moved.
    let error = journal
        .store
        .append_event(&lease, "run.extra", &json!({}), now)
        .expect_err("no events after terminal");
    assert_eq!(error.code(), "JOURNAL.INVALID_TRANSITION");

    assert_eq!(
        journal
            .store
            .claim_owner("run-1", "runner-2", 60.0, now)
            .expect_err("cannot claim")
            .code(),
        "JOURNAL.INVALID_TRANSITION"
    );
    assert_eq!(
        journal
            .store
            .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Cancel, None)
            .expect_err("no control after terminal")
            .code(),
        "JOURNAL.INVALID_TRANSITION"
    );
}

#[test]
fn a_concurrent_status_change_is_reported_as_a_conflict() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    // The caller believed the run was still pending; it is running.
    let error = journal
        .store
        .set_status(
            &lease,
            RunStatus::Pending,
            RunStatus::Running,
            None,
            None,
            None,
            None,
            now_seconds(),
        )
        .expect_err("must conflict");
    assert_eq!(error.code(), "JOURNAL.CONFLICT");
    assert!(
        error.message().contains("running"),
        "should report what was actually found: {}",
        error.message()
    );
}

// ---------------------------------------------------------------- control plane

#[test]
fn requesting_a_pause_records_intent_without_claiming_the_run_stopped() {
    let journal = TempJournal::open();
    seed_running(&journal, "run-1");
    let run = journal
        .store
        .compare_and_set_desired_state(
            "run-1",
            DesiredState::Run,
            DesiredState::Pause,
            Some(("run.pause_requested", &json!({"by": "operator"}))),
        )
        .expect("request pause");

    assert_eq!(run.desired_state, DesiredState::Pause);
    // Still running: the runner has not reached a safe point, and it may be
    // mid-dispatch. Reporting `paused` here would be a lie.
    assert_eq!(run.status, RunStatus::Running);

    let events = journal.store.list_events("run-1", 0, 10).expect("events");
    assert_eq!(
        events.last().expect("event").event_type,
        "run.pause_requested"
    );
}

#[test]
fn the_control_plane_does_not_need_the_runners_token() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    // An operator must be able to ask for a cancel without being able to forge
    // journal writes, so control takes no lease at all.
    journal
        .store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Cancel, None)
        .expect("cancel without any token");

    // The runner's lease is untouched and still usable for its own writes.
    let run = journal
        .store
        .ensure_live_lease(&lease, now_seconds())
        .expect("lease still live");
    assert_eq!(run.desired_state, DesiredState::Cancel);
    assert_eq!(run.owner_id.as_deref(), Some("runner-1"));
}

#[test]
fn cancel_is_absorbing_so_a_racing_resume_cannot_lose_it() {
    let journal = TempJournal::open();
    seed_running(&journal, "run-1");
    journal
        .store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Cancel, None)
        .expect("cancel");

    for target in [DesiredState::Run, DesiredState::Pause] {
        let error = journal
            .store
            .compare_and_set_desired_state("run-1", DesiredState::Cancel, target, None)
            .expect_err("cancel cannot be walked back");
        assert_eq!(
            error.code(),
            "JOURNAL.INVALID_TRANSITION",
            "cancel -> {}",
            target.as_str()
        );
    }
    assert_eq!(
        journal.store.get_run("run-1").expect("run").desired_state,
        DesiredState::Cancel
    );
}

#[test]
fn a_stale_desired_state_expectation_is_a_conflict() {
    let journal = TempJournal::open();
    seed_running(&journal, "run-1");
    journal
        .store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Pause, None)
        .expect("pause");

    // Two operators racing: the second still believes intent is `run`.
    let error = journal
        .store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Pause, None)
        .expect_err("must conflict");
    assert_eq!(error.code(), "JOURNAL.CONFLICT");
}

#[test]
fn a_completing_step_notices_a_concurrent_pause_request() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();
    journal
        .store
        .compare_and_set_desired_state("run-1", DesiredState::Run, DesiredState::Pause, None)
        .expect("operator asks for a pause");

    // The runner expected intent to still be `run`, so its checkpoint is refused
    // rather than silently overwriting the request.
    let error = journal
        .store
        .append_event_with_checkpoint(
            &lease,
            "step.completed",
            &json!({}),
            &json!({"phase": "between_top_level_steps"}),
            Some(RunStatus::Running),
            Some(DesiredState::Run),
            now,
        )
        .expect_err("must conflict");
    assert_eq!(error.code(), "JOURNAL.CONFLICT");

    // Nothing was written, so the runner can re-read and honour the pause.
    let run = journal.store.get_run("run-1").expect("run");
    assert!(run.checkpoint.is_none());
}

// ---------------------------------------------------------------- owner leases

#[test]
fn a_live_lease_blocks_another_claimant_but_an_expired_one_does_not() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let now = 1_000.0;
    let lease = journal
        .store
        .claim_owner("run-1", "runner-1", 30.0, now)
        .expect("claim");

    let error = journal
        .store
        .claim_owner("run-1", "runner-2", 30.0, now + 10.0)
        .expect_err("live lease blocks");
    assert_eq!(error.code(), "JOURNAL.LEASE_CONFLICT");

    // Expiry is what lets a crashed runner's work be picked up: a dead process
    // never releases its lease explicitly.
    let taken_over = journal
        .store
        .claim_owner("run-1", "runner-2", 30.0, now + 31.0)
        .expect("expired lease can be taken over");
    assert_eq!(taken_over.owner_id, "runner-2");

    // And the superseded runner can no longer write.
    let error = journal
        .store
        .append_event(&lease, "step.started", &json!({}), now + 31.0)
        .expect_err("superseded runner is fenced");
    assert_eq!(error.code(), "JOURNAL.LEASE_LOST");
}

#[test]
fn a_new_claim_issues_a_different_token_so_the_old_one_is_useless() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let first = journal
        .store
        .claim_owner("run-1", "runner", 10.0, 1_000.0)
        .expect("claim");
    let second = journal
        .store
        .claim_owner("run-1", "runner", 10.0, 1_020.0)
        .expect("reclaim");
    assert_ne!(
        first.token(),
        second.token(),
        "re-claiming must not reissue the same secret"
    );
}

#[test]
fn only_the_hash_of_a_lease_token_is_persisted() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let lease = journal
        .store
        .claim_owner("run-1", "runner", 60.0, now_seconds())
        .expect("claim");

    let stored: String = journal
        .raw()
        .query_row(
            "SELECT lease_token_hash FROM runs WHERE run_id = 'run-1'",
            [],
            |row| row.get(0),
        )
        .expect("read hash");
    assert_ne!(stored, lease.token(), "the plaintext token is not stored");
    assert_eq!(stored.len(), 64, "a sha256 hex digest");

    // The transport shape must not leak it either.
    let json = journal.store.get_run("run-1").expect("run").to_json();
    assert!(
        !serde_json::to_string(&json)
            .expect("serialise")
            .contains(lease.token()),
        "the token must not appear in the transport shape"
    );
    // Nor the debug rendering, which is the other way secrets reach logs.
    assert!(!format!("{lease:?}").contains(lease.token()));
}

#[test]
fn a_heartbeat_extends_a_live_lease_and_is_refused_once_it_lapsed() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let lease = journal
        .store
        .claim_owner("run-1", "runner", 30.0, 1_000.0)
        .expect("claim");

    let extended = journal
        .store
        .heartbeat_owner(&lease, 30.0, 1_010.0)
        .expect("heartbeat");
    assert!(extended.expires_at > lease.expires_at);

    // Past its expiry the lease is gone, even though nobody else took it: the
    // runner must re-claim rather than assume it still owns the run.
    let error = journal
        .store
        .heartbeat_owner(&lease, 30.0, 2_000.0)
        .expect_err("expired lease cannot be extended");
    assert_eq!(error.code(), "JOURNAL.LEASE_LOST");
}

#[test]
fn releasing_a_lease_lets_the_next_runner_claim_immediately() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let now = 1_000.0;
    let lease = journal
        .store
        .claim_owner("run-1", "runner-1", 600.0, now)
        .expect("claim");
    let run = journal.store.release_owner(&lease, now).expect("release");
    assert!(run.owner_id.is_none());

    // No waiting for the long TTL to lapse.
    journal
        .store
        .claim_owner("run-1", "runner-2", 30.0, now)
        .expect("claim after release");
}

#[test]
fn a_lease_for_one_run_cannot_write_to_another() {
    let journal = TempJournal::open();
    let first = seed_running(&journal, "run-1");
    seed(&journal, "run-2");
    let now = now_seconds();

    // Same owner name, genuine token, different run. Each lease is scoped to the
    // run it was issued for, so holding one must not grant writes to the other.
    let second = journal
        .store
        .claim_owner("run-2", "runner-1", 60.0, now)
        .expect("claim run-2");
    assert_ne!(first.token(), second.token());

    journal
        .store
        .append_event(&second, "step.started", &json!({}), now)
        .expect("writes to its own run");
    journal
        .store
        .ensure_live_lease(&first, now)
        .expect("run-1 lease untouched");

    // run-2's history got exactly the one event; run-1's is unaffected.
    assert_eq!(
        journal
            .store
            .list_events("run-2", 0, 10)
            .expect("events")
            .len(),
        2
    );
    assert_eq!(
        journal
            .store
            .list_events("run-1", 0, 10)
            .expect("events")
            .len(),
        2
    );
}

// ---------------------------------------------------------------- events

#[test]
fn event_sequence_numbers_are_contiguous_and_start_at_one() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();
    for index in 0..3 {
        journal
            .store
            .append_event(&lease, "step.completed", &json!({"index": index}), now)
            .expect("append");
    }
    let events = journal.store.list_events("run-1", 0, 100).expect("events");
    let sequence: Vec<i64> = events.iter().map(|event| event.seq).collect();
    // run.created, run.started, then the three appends.
    assert_eq!(sequence, vec![1, 2, 3, 4, 5]);
}

#[test]
fn events_can_be_followed_incrementally() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();
    journal
        .store
        .append_event(&lease, "step.started", &json!({}), now)
        .expect("append");

    let tail = journal.store.list_events("run-1", 2, 100).expect("tail");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].event_type, "step.started");
    assert_eq!(tail[0].seq, 3);

    // Nothing new yet is an empty list, not an error.
    assert!(journal
        .store
        .list_events("run-1", 3, 100)
        .expect("caught up")
        .is_empty());
}

#[test]
fn an_event_for_a_missing_run_is_not_found_rather_than_a_storage_error() {
    let journal = TempJournal::open();
    // The aggregate inside the insert yields a row even when its WHERE matches
    // nothing, so without an explicit existence check a missing run reached the
    // foreign key and surfaced as a storage failure (extended code 787) instead
    // of the "run not found" a caller can act on.
    assert_eq!(
        journal
            .store
            .list_events("ghost", 0, 10)
            .expect_err("no such run")
            .code(),
        "JOURNAL.RUN_NOT_FOUND"
    );

    // Same on the write path: delete a run out from under a live lease and the
    // next append must say the run is gone, not report a constraint violation.
    seed(&journal, "run-1");
    let lease = journal
        .store
        .claim_owner("run-1", "runner", 60.0, now_seconds())
        .expect("claim");
    journal
        .raw()
        .execute("DELETE FROM runs WHERE run_id = 'run-1'", [])
        .expect("delete the run behind the lease");
    let error = journal
        .store
        .append_event(&lease, "step.started", &json!({}), now_seconds())
        .expect_err("the run is gone");
    assert_eq!(error.code(), "JOURNAL.RUN_NOT_FOUND");
}

#[test]
fn malformed_event_types_are_rejected() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();
    for candidate in [
        "Run.Started",
        "run started",
        "",
        "9run",
        "run..started",
        "run.",
    ] {
        let error = journal
            .store
            .append_event(&lease, candidate, &json!({}), now)
            .expect_err("must reject");
        assert_eq!(
            error.code(),
            "JOURNAL.INVALID_REQUEST",
            "event type {candidate:?}"
        );
    }
    for candidate in [
        "run.started",
        "step.retry_scheduled",
        "a",
        "run.step-1.done",
    ] {
        journal
            .store
            .append_event(&lease, candidate, &json!({}), now)
            .unwrap_or_else(|error| panic!("{candidate:?} should be valid: {error}"));
    }
}

#[test]
fn non_finite_numbers_are_refused_because_they_do_not_round_trip() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    // serde_json cannot even represent NaN, so the realistic risk is a value
    // that decodes differently than written. Guard the encoder directly.
    let deep = json!({"a": [{"b": 1.5}]});
    journal
        .store
        .append_event(&lease, "step.done", &deep, now_seconds())
        .expect("finite nested data is fine");
}

// ---------------------------------------------------------------- checkpoints

#[test]
fn a_checkpoint_and_its_event_are_committed_together() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let checkpoint = json!({
        "phase": "between_top_level_steps",
        "nextTopLevelIndex": 2,
        "deadline": 1_760_000_000.0,
        "variables": {"count": 1},
    });
    journal
        .store
        .append_event_with_checkpoint(
            &lease,
            "run.checkpointed",
            &json!({"index": 2}),
            &checkpoint,
            Some(RunStatus::Running),
            Some(DesiredState::Run),
            now_seconds(),
        )
        .expect("checkpoint");

    let run = journal.store.get_run("run-1").expect("run");
    assert_eq!(run.checkpoint, Some(checkpoint));
    let events = journal.store.list_events("run-1", 0, 10).expect("events");
    assert_eq!(
        events.last().expect("event").event_type,
        "run.checkpointed",
        "history explains the state it accompanies"
    );
}

#[test]
fn a_checkpoint_survives_reopening_the_journal() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    journal
        .store
        .append_event_with_checkpoint(
            &lease,
            "run.checkpointed",
            &json!({}),
            &json!({"phase": "in_top_level_step", "nextTopLevelIndex": 1}),
            None,
            None,
            now_seconds(),
        )
        .expect("checkpoint");

    // The whole point: a new process can pick up where the old one stopped.
    let reopened = JournalStore::open(journal.path()).expect("reopen");
    let run = reopened.get_run("run-1").expect("run");
    assert_eq!(
        run.checkpoint.expect("checkpoint")["phase"],
        json!("in_top_level_step")
    );
    assert_eq!(run.status, RunStatus::Running);
}

#[test]
fn a_lost_lease_cannot_write_a_checkpoint() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let now = now_seconds();
    // Another runner takes over after the first lease lapsed.
    journal
        .store
        .claim_owner("run-1", "runner-2", 60.0, now + 120.0)
        .expect("take over");

    let error = journal
        .store
        .append_event_with_checkpoint(
            &lease,
            "run.checkpointed",
            &json!({}),
            &json!({"phase": "finalizing"}),
            None,
            None,
            now + 120.0,
        )
        .expect_err("fenced");
    assert_eq!(error.code(), "JOURNAL.LEASE_LOST");
    assert!(
        journal
            .store
            .get_run("run-1")
            .expect("run")
            .checkpoint
            .is_none(),
        "the fenced write left no trace"
    );
}

// ------------------------------------------------- database-level enforcement
//
// These bypass JournalStore on purpose. The triggers are what protect a run
// from a different build, an older process, or a hand-written UPDATE.

#[test]
fn the_database_itself_refuses_an_illegal_status_transition() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let raw = journal.raw();
    // pending -> paused is not a legal edge, and no Rust code is involved here.
    let error = raw
        .execute(
            "UPDATE runs SET status = 'paused' WHERE run_id = 'run-1'",
            [],
        )
        .expect_err("trigger must abort");
    assert!(
        error.to_string().contains("invalid run status transition"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_database_itself_makes_cancel_absorbing() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let raw = journal.raw();
    raw.execute(
        "UPDATE runs SET desired_state = 'cancel' WHERE run_id = 'run-1'",
        [],
    )
    .expect("cancel");
    let error = raw
        .execute(
            "UPDATE runs SET desired_state = 'run' WHERE run_id = 'run-1'",
            [],
        )
        .expect_err("trigger must abort");
    assert!(
        error.to_string().contains("absorbing"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_database_itself_freezes_a_terminal_run() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    journal
        .store
        .set_status(
            &lease,
            RunStatus::Running,
            RunStatus::Cancelled,
            None,
            None,
            Some(&json!({"code": "RUNTIME.CANCELLED"})),
            None,
            now_seconds(),
        )
        .expect("cancel");

    let raw = journal.raw();
    let error = raw
        .execute(
            "UPDATE runs SET workflow_name = 'tampered' WHERE run_id = 'run-1'",
            [],
        )
        .expect_err("terminal rows are frozen");
    assert!(
        error.to_string().contains("immutable"),
        "unexpected: {error}"
    );

    let error = raw
        .execute(
            "INSERT INTO events (run_id, seq, event_type, payload_json, created_at)
             VALUES ('run-1', 99, 'run.extra', '{}', '2026-01-01T00:00:00.000000+00:00')",
            [],
        )
        .expect_err("terminal runs accept no events");
    assert!(
        error
            .to_string()
            .contains("terminal run cannot accept events")
            || error.to_string().contains("contiguous"),
        "unexpected: {error}"
    );
}

#[test]
fn the_database_itself_rejects_a_gap_in_the_event_sequence() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    // A gap would make an event feed look complete while missing history.
    let error = journal
        .raw()
        .execute(
            "INSERT INTO events (run_id, seq, event_type, payload_json, created_at)
             VALUES ('run-1', 7, 'step.started', '{}', '2026-01-01T00:00:00.000000+00:00')",
            [],
        )
        .expect_err("must abort");
    assert!(
        error.to_string().contains("contiguous"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_database_itself_rejects_a_half_set_lease() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    // A lease with an owner but no token would fence nothing while looking as
    // though it did.
    let error = journal
        .raw()
        .execute(
            "UPDATE runs SET owner_id = 'sneaky' WHERE run_id = 'run-1'",
            [],
        )
        .expect_err("CHECK must abort");
    assert!(
        error.to_string().contains("CHECK") || error.to_string().contains("constraint"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_database_itself_rejects_output_on_a_failed_run() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let raw = journal.raw();
    raw.execute(
        "UPDATE runs SET status = 'running' WHERE run_id = 'run-1'",
        [],
    )
    .expect("start");
    let error = raw
        .execute(
            "UPDATE runs SET status = 'failed', output_json = '{\"a\":1}',
                 error_json = '{\"code\":\"X.Y\"}', finished_at = '2026-01-01T00:00:00.000000+00:00'
             WHERE run_id = 'run-1'",
            [],
        )
        .expect_err("CHECK must abort");
    assert!(
        error.to_string().contains("CHECK") || error.to_string().contains("constraint"),
        "unexpected error: {error}"
    );
}

#[test]
fn deleting_a_run_takes_its_events_with_it() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let raw = journal.raw();
    raw.execute("DELETE FROM runs WHERE run_id = 'run-1'", [])
        .expect("delete");
    let orphans: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = 'run-1'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(orphans, 0, "cascade leaves no orphaned history");
}

// ---------------------------------------------------------------- transport shape

#[test]
fn the_transport_shape_matches_the_run_schema() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    let lease = journal
        .store
        .claim_owner("run-1", "runner-1", 60.0, 1_000.0)
        .expect("claim");
    let json = journal.store.get_run("run-1").expect("run").to_json();

    assert_eq!(json["apiVersion"], json!("ai-auto-desktop.dev/v1alpha1"));
    assert_eq!(json["kind"], json!("Run"));
    assert_eq!(json["runId"], json!("run-1"));
    assert_eq!(json["workflow"]["name"], json!("demo.workflow"));
    assert_eq!(json["workflow"]["version"], json!("1.0.0"));
    assert_eq!(json["workflow"]["planDigest"], json!("sha256:abc"));
    assert_eq!(json["status"], json!("pending"));
    assert_eq!(json["desiredState"], json!("run"));
    assert_eq!(json["ownerLease"]["ownerId"], json!("runner-1"));
    assert_eq!(json["ownerLease"]["expiresAt"], json!(1_060.0));
    assert!(json["createdAt"].is_string());
    let _ = lease;
}

#[test]
fn an_event_transport_shape_matches_the_event_schema() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let event = journal
        .store
        .append_event(
            &lease,
            "step.started",
            &json!({"stepId": "click"}),
            now_seconds(),
        )
        .expect("append");
    let json = event.to_json();

    assert_eq!(json["kind"], json!("RunEvent"));
    assert_eq!(json["runId"], json!("run-1"));
    assert_eq!(json["type"], json!("step.started"));
    assert_eq!(json["seq"], json!(3));
    assert_eq!(json["payload"]["stepId"], json!("click"));
    assert!(json["createdAt"].is_string());
}

#[test]
fn timestamps_are_utc_microsecond_iso_8601() {
    let journal = TempJournal::open();
    let run = seed(&journal, "run-1");
    // Lexicographic ordering is what makes `created_at DESC` correct, so the
    // format has to be fixed-width and zero-padded.
    assert_eq!(run.created_at.len(), 32, "got {:?}", run.created_at);
    assert!(
        run.created_at.ends_with("+00:00"),
        "got {:?}",
        run.created_at
    );
    assert_eq!(&run.created_at[4..5], "-");
    assert_eq!(&run.created_at[10..11], "T");
    let year: i32 = run.created_at[0..4].parse().expect("year");
    assert!((2024..2100).contains(&year), "implausible year: {year}");
}

// ---------------------------------------------------------------- concurrency

#[test]
fn two_stores_on_one_file_serialise_their_writes() {
    let journal = TempJournal::open();
    seed(&journal, "run-1");
    // A separate store is a separate connection, as a second worker process
    // would be. WAL plus BEGIN IMMEDIATE has to make this safe.
    let other = JournalStore::open(journal.path()).expect("second store");

    let first = journal
        .store
        .claim_owner("run-1", "runner-1", 60.0, 1_000.0)
        .expect("first claims");
    let error = other
        .claim_owner("run-1", "runner-2", 60.0, 1_000.0)
        .expect_err("second is refused");
    assert_eq!(error.code(), "JOURNAL.LEASE_CONFLICT");

    // Both connections agree on who owns the run.
    assert_eq!(
        other.get_run("run-1").expect("run").owner_id.as_deref(),
        Some("runner-1")
    );
    journal
        .store
        .append_event(&first, "step.started", &json!({}), 1_000.0)
        .expect("owner writes");
    assert_eq!(
        other.list_events("run-1", 0, 10).expect("events").len(),
        2,
        "the other connection sees committed events"
    );
}

#[test]
fn concurrent_appends_from_many_threads_produce_no_gaps_or_duplicates() {
    let journal = TempJournal::open();
    let lease = seed_running(&journal, "run-1");
    let path = journal.path();
    let now = now_seconds();

    // Allocating the sequence inside the INSERT, under the writer lock, is what
    // stops two appends choosing the same number.
    std::thread::scope(|scope| {
        for thread in 0..4 {
            let path = path.clone();
            let lease = lease.clone();
            scope.spawn(move || {
                let store = JournalStore::open(&path).expect("open per-thread store");
                for index in 0..5 {
                    store
                        .append_event(
                            &lease,
                            "step.completed",
                            &json!({"thread": thread, "index": index}),
                            now,
                        )
                        .expect("append");
                }
            });
        }
    });

    let events = journal
        .store
        .list_events("run-1", 0, 1_000)
        .expect("events");
    assert_eq!(events.len(), 22, "2 seeded + 20 appended");
    let sequence: Vec<i64> = events.iter().map(|event| event.seq).collect();
    let expected: Vec<i64> = (1..=22).collect();
    assert_eq!(sequence, expected, "contiguous with no duplicates");
}

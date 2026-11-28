//! SQLite-backed durable journal for long-running workflow runs.
//!
//! Ported from the Python `journal.py`, keeping its invariants rather than its
//! shape. The design point worth restating: **the database enforces the state
//! machine, not just this code**. Triggers and CHECK constraints reject an
//! invalid transition even when the caller is a different build, an older
//! process, or someone poking at the file with the `sqlite3` shell. Rust-side
//! validation exists to give good errors, not as the only line of defence.
//!
//! Three separations carry the correctness:
//!
//! * **`status` vs `desired_state`.** `status` is what the runner has actually
//!   reached; `desired_state` is what the operator asked for. A pause request
//!   only sets intent — claiming the run is paused before the runner reached a
//!   safe point would be a lie, and the run may still be mid-dispatch.
//! * **Control plane vs owner lease.** Changing intent never needs the runner's
//!   token: an operator must be able to request a cancel without being able to
//!   forge journal writes. The lease only fences *writes* to status, events and
//!   checkpoints, so a superseded runner cannot corrupt the record.
//! * **Terminal is immutable.** Once a run reaches a terminal status, nothing
//!   may modify the row or append events — enforced by trigger.
//!
//! `cancel` is absorbing: once requested it cannot be walked back to `run` or
//! `pause`, so a cancel cannot be lost to a racing resume.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{
    params, Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

mod writers;
use writers::Writers;

#[cfg(all(test, target_os = "linux"))]
mod forked_writer_tests;

/// The run store used when none is named.
///
/// One per user, beside the saved workflows in `~/.ai-auto-desktop`, so a run
/// started by one front end -- `aad start`, or `run_workflow` over MCP -- is
/// visible to `status`, `resume` and the rest from any other. `AAD_RUN_STORE`
/// overrides it, which is also what keeps tests away from a real home.
pub fn default_store_path() -> PathBuf {
    if let Some(configured) = std::env::var_os("AAD_RUN_STORE") {
        return PathBuf::from(configured);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".ai-auto-desktop").join("runs.sqlite3")
}

/// Bumped only with a migration; a newer file is refused rather than guessed at.
pub const SCHEMA_VERSION: i64 = 1;

/// How long a writer waits for a competing transaction before giving up.
pub const DEFAULT_BUSY_TIMEOUT_MS: u32 = 30_000;

/// Where a run has actually got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Pending,
    Running,
    Paused,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    UnknownEffect,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::UnknownEffect => "unknown_effect",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "timed_out" => Self::TimedOut,
            "cancelled" => Self::Cancelled,
            "unknown_effect" => Self::UnknownEffect,
            other => {
                return Err(JournalError::invalid(format!(
                    "invalid run status: {other:?}"
                )))
            }
        })
    }

    /// Whether this status admits no further change.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::TimedOut | Self::Cancelled | Self::UnknownEffect
        )
    }

    /// The statuses reachable from here.
    ///
    /// Deliberately narrow. Notably `pending` cannot reach `unknown_effect`:
    /// nothing has been dispatched yet, so the effect is not in doubt.
    fn allows(self, next: Self) -> bool {
        match self {
            Self::Pending => matches!(
                next,
                Self::Running | Self::Failed | Self::TimedOut | Self::Cancelled
            ),
            Self::Running => matches!(
                next,
                Self::Paused
                    | Self::Succeeded
                    | Self::Failed
                    | Self::TimedOut
                    | Self::Cancelled
                    | Self::UnknownEffect
            ),
            Self::Paused => matches!(
                next,
                Self::Running
                    | Self::Failed
                    | Self::TimedOut
                    | Self::Cancelled
                    | Self::UnknownEffect
            ),
            _ => false,
        }
    }
}

/// What the operator wants to happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesiredState {
    Run,
    Pause,
    Cancel,
}

impl DesiredState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Pause => "pause",
            Self::Cancel => "cancel",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "run" => Self::Run,
            "pause" => Self::Pause,
            "cancel" => Self::Cancel,
            other => {
                return Err(JournalError::invalid(format!(
                    "invalid desired state: {other:?}"
                )))
            }
        })
    }

    /// `cancel` is absorbing, so a cancel can never be undone by a later resume.
    fn allows(self, next: Self) -> bool {
        match self {
            Self::Run => matches!(next, Self::Pause | Self::Cancel),
            Self::Pause => matches!(next, Self::Run | Self::Cancel),
            Self::Cancel => false,
        }
    }
}

/// What went wrong, in the categories callers actually branch on.
#[derive(Debug)]
pub enum JournalError {
    /// No such run.
    NotFound(String),
    /// Compare-and-set saw different state; the caller should re-read.
    Conflict(String),
    /// A live lease is held by somebody else.
    LeaseConflict(String),
    /// This lease no longer owns the run, so its writes are refused.
    LeaseLost(String),
    /// The requested transition violates the lifecycle.
    InvalidTransition(String),
    /// The caller marked data sensitive, and sensitive data is never persisted.
    Sensitive(String),
    /// Bad argument.
    Invalid(String),
    /// The storage layer failed.
    Storage(String),
}

impl JournalError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// A stable code, so callers and the CLI can branch without parsing prose.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "JOURNAL.RUN_NOT_FOUND",
            Self::Conflict(_) => "JOURNAL.CONFLICT",
            Self::LeaseConflict(_) => "JOURNAL.LEASE_CONFLICT",
            Self::LeaseLost(_) => "JOURNAL.LEASE_LOST",
            Self::InvalidTransition(_) => "JOURNAL.INVALID_TRANSITION",
            Self::Sensitive(_) => "JOURNAL.SENSITIVE_REFUSED",
            Self::Invalid(_) => "JOURNAL.INVALID_REQUEST",
            Self::Storage(_) => "JOURNAL.STORAGE_FAILED",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m)
            | Self::Conflict(m)
            | Self::LeaseConflict(m)
            | Self::LeaseLost(m)
            | Self::InvalidTransition(m)
            | Self::Sensitive(m)
            | Self::Invalid(m)
            | Self::Storage(m) => m,
        }
    }
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for JournalError {}

impl From<rusqlite::Error> for JournalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, JournalError>;

/// One persisted run.
#[derive(Clone, Debug)]
pub struct RunRecord {
    pub run_id: String,
    pub workflow_name: String,
    pub workflow_version: Option<String>,
    pub plan_digest: Option<String>,
    pub status: RunStatus,
    pub desired_state: DesiredState,
    pub inputs: Value,
    pub output: Option<Value>,
    pub error: Option<Value>,
    pub checkpoint: Option<Value>,
    pub owner_id: Option<String>,
    pub lease_expires_at: Option<f64>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

impl RunRecord {
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// The transport shape from `run.schema.json`.
    ///
    /// The lease token is never included — only the owner id and expiry, which
    /// are enough to explain who holds the run without handing over the ability
    /// to write as them.
    pub fn to_json(&self) -> Value {
        let mut workflow = serde_json::Map::new();
        workflow.insert("name".into(), Value::String(self.workflow_name.clone()));
        if let Some(version) = &self.workflow_version {
            workflow.insert("version".into(), Value::String(version.clone()));
        }
        if let Some(digest) = &self.plan_digest {
            workflow.insert("planDigest".into(), Value::String(digest.clone()));
        }
        serde_json::json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Run",
            "runId": self.run_id,
            "workflow": Value::Object(workflow),
            "status": self.status.as_str(),
            "desiredState": self.desired_state.as_str(),
            "inputs": self.inputs,
            "output": self.output,
            "error": self.error,
            "checkpoint": self.checkpoint,
            "ownerLease": self.owner_id.as_ref().map(|owner| serde_json::json!({
                "ownerId": owner,
                "expiresAt": self.lease_expires_at,
            })),
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "finishedAt": self.finished_at,
        })
    }
}

/// One appended event.
#[derive(Clone, Debug)]
pub struct EventRecord {
    pub run_id: String,
    pub seq: i64,
    pub event_type: String,
    pub payload: Value,
    pub created_at: String,
}

impl EventRecord {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "RunEvent",
            "runId": self.run_id,
            "seq": self.seq,
            "type": self.event_type,
            "payload": self.payload,
            "createdAt": self.created_at,
        })
    }
}

/// When a lease predicate is evaluated. Live callers sample the clock only
/// after acquiring the SQLite writer lock. An explicit timestamp lets callers
/// with their own clock evaluate an operation at a fixed instant.
#[derive(Clone, Copy, Debug)]
pub enum LeaseTime {
    Current,
    At(f64),
}

impl From<f64> for LeaseTime {
    fn from(seconds: f64) -> Self {
        Self::At(seconds)
    }
}

impl LeaseTime {
    fn seconds(self) -> f64 {
        match self {
            Self::Current => now_seconds(),
            Self::At(seconds) => seconds,
        }
    }
}

/// Proof that this process owns a run.
///
/// The token is a bearer secret: whoever holds it can write to the run. It is
/// deliberately kept out of `Debug` so it cannot reach a log by accident.
#[derive(Clone)]
pub struct OwnerLease {
    pub run_id: String,
    pub owner_id: String,
    token: String,
    pub expires_at: f64,
}

impl OwnerLease {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Debug for OwnerLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerLease")
            .field("run_id", &self.run_id)
            .field("owner_id", &self.owner_id)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

const TERMINAL_SQL: &str = "'succeeded','failed','timed_out','cancelled','unknown_effect'";

/// A run's declared inputs and outputs must all be non-sensitive to be durable.
///
/// Conservative on purpose: the journal cannot tell a secret from ordinary text,
/// so rather than guess it refuses the whole run when anything is marked
/// sensitive. A future secret broker can substitute references before this point.
pub fn durable_descriptor_eligible(descriptor: &Value) -> bool {
    for section in ["inputs", "outputs"] {
        let Some(value) = descriptor.get(section) else {
            // An absent section is fine: nothing is declared, so nothing leaks.
            continue;
        };
        let Some(entries) = value.as_object() else {
            return false;
        };
        for definition in entries.values() {
            let Some(definition) = definition.as_object() else {
                return false;
            };
            match definition.get("sensitive") {
                None => {}
                Some(Value::Bool(false)) => {}
                // Both `true` and a non-boolean are refused: an unexpected type
                // means the declaration was not understood.
                _ => return false,
            }
        }
    }
    true
}

/// The durable journal. One connection, so one owning thread.
///
/// Concurrent workers open their own store against the same path; WAL plus
/// `BEGIN IMMEDIATE` serialises their writes.
pub struct JournalStore {
    connection: Connection,
    writers: Arc<Writers>,
    busy_timeout: Duration,
    pub path: PathBuf,
}

impl std::fmt::Debug for JournalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalStore")
            .field("path", &self.path)
            .finish()
    }
}

impl JournalStore {
    /// Open or create a journal, refusing anything that cannot be trusted.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_timeout(path, DEFAULT_BUSY_TIMEOUT_MS)
    }

    pub fn open_with_timeout(path: impl AsRef<Path>, busy_timeout_ms: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                JournalError::Storage(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        let connection = Connection::open(&path)?;
        // SQLite resolves URI and relative filenames. Use the underlying file
        // for admission, independently of the name a caller used to open it.
        let filename = connection.path().map(Path::new).unwrap_or(&path);
        let writers = Writers::for_file(
            &filename
                .canonicalize()
                .unwrap_or_else(|_| filename.to_owned()),
        );
        let busy_timeout = Duration::from_millis(busy_timeout_ms.min(i32::MAX as u32) as u64);
        let store = Self {
            connection,
            writers,
            busy_timeout,
            path,
        };
        store.configure().map_err(|err| match err {
            JournalError::Storage(message) => {
                JournalError::Storage(format!("configure journal: {message}"))
            }
            other => other,
        })?;
        store.migrate().map_err(|err| match err {
            JournalError::Storage(message) => {
                JournalError::Storage(format!("open journal schema: {message}"))
            }
            other => other,
        })?;
        Ok(store)
    }

    pub(crate) fn heartbeat_connection(&self, ttl_seconds: f64) -> Result<Self> {
        let wait_ms = self.busy_timeout.as_millis() as u32;
        Self::open_with_timeout(&self.path, wait_ms.min((ttl_seconds * 500.0) as u32))
    }

    /// Apply and then *verify* the pragmas the journal depends on.
    ///
    /// Verification is the point. SQLite silently ignores a pragma it cannot
    /// honour, and a journal running without WAL or without `synchronous=FULL`
    /// looks fine until a crash loses the last commit. Refusing to open is far
    /// better than durability that is quietly absent.
    fn configure(&self) -> Result<()> {
        let mode: String = self.retry_busy(Instant::now() + self.busy_timeout, || {
            self.connection
                .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")?;
            let mode: String = self
                .connection
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
            if mode.eq_ignore_ascii_case("wal") {
                Ok(mode)
            } else {
                self.connection
                    .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            }
        })?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(JournalError::Storage(format!(
                "SQLite refused WAL mode and reported {mode:?}; concurrent runners \
                 could not be serialised safely"
            )));
        }
        let foreign_keys: i64 = self
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        if foreign_keys != 1 {
            return Err(JournalError::Storage(
                "SQLite foreign key enforcement is unavailable".into(),
            ));
        }
        let synchronous: i64 = self
            .connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        if synchronous != 2 {
            return Err(JournalError::Storage(
                "SQLite synchronous=FULL is unavailable, so a crash could lose \
                 the last committed transaction"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Create the schema, re-checking the version under the writer lock so that
    /// several processes racing to open a new journal is safe.
    fn migrate(&self) -> Result<()> {
        // Opening an existing journal to read progress can share a WAL snapshot
        // with a running writer. Only migrations require the writer lock.
        // A peer may finish the migration and immediately start writing events.
        // While waiting for the writer, look for its committed schema on every
        // attempt. A newly ready reader can then open alongside those writes.
        let pending = self.retry_busy(Instant::now() + self.busy_timeout, || {
            let version: i64 = self
                .connection
                .query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version == SCHEMA_VERSION {
                return Ok(None);
            }
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate).map(Some)
        })?;
        let Some(transaction) = pending else {
            return Ok(());
        };
        let current: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current > SCHEMA_VERSION {
            return Err(JournalError::Storage(format!(
                "journal schema {current} is newer than the supported {SCHEMA_VERSION}"
            )));
        }
        if current < SCHEMA_VERSION {
            transaction.execute_batch(&schema_ddl())?;
            transaction.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Begin an immediate transaction, taking the writer lock up front.
    ///
    /// `IMMEDIATE` rather than deferred: a read-then-write sequence that upgrades
    /// midway can fail with `SQLITE_BUSY` after its reads, which is exactly the
    /// race every compare-and-set here is trying to avoid.
    fn write<T>(&self, body: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let deadline = Instant::now() + self.busy_timeout;
        let _turn = self.writers.enter(deadline)?;
        let transaction = {
            let _reservation = self.writers.reserve(deadline)?;
            self.retry_busy(deadline, || {
                Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
            })?
        };
        let result = body(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    /// WAL setup can return BUSY without invoking SQLite's busy handler. During
    /// lock acquisition use short polls so external contenders get frequent
    /// chances between commits. Never replay the body of a writer transaction.
    fn retry_busy<T>(
        &self,
        deadline: Instant,
        mut attempt: impl FnMut() -> rusqlite::Result<T>,
    ) -> Result<T> {
        self.connection.busy_timeout(Duration::ZERO)?;
        // Contending processes can otherwise all retry at the same instant
        // throughout WAL creation or a slow FULL-synchronous commit.
        let mut delay_ms = 1;
        let phase_ms = u64::from(std::process::id() % 5);
        let result = loop {
            let result = attempt();
            if !matches!(&result, Err(e) if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy))
            {
                break result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break result;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(delay_ms + phase_ms)));
            delay_ms = (delay_ms * 2).min(24);
        };
        self.connection.busy_timeout(self.busy_timeout)?;
        Ok(result?)
    }

    /// Create a run, plus its first event, atomically.
    ///
    /// The event is in the same transaction so a run can never exist without the
    /// record of why it was created.
    #[allow(clippy::too_many_arguments)]
    pub fn create_run(
        &self,
        run_id: &str,
        workflow_name: &str,
        inputs: &Value,
        descriptor: &Value,
        workflow_version: Option<&str>,
        plan_digest: Option<&str>,
        first_event: Option<(&str, &Value)>,
    ) -> Result<RunRecord> {
        if !durable_descriptor_eligible(descriptor) {
            return Err(JournalError::Sensitive(
                "durable runs do not accept descriptors with sensitive inputs or outputs".into(),
            ));
        }
        bounded(run_id, "run_id", 256)?;
        bounded(workflow_name, "workflow_name", 192)?;
        if let Some(version) = workflow_version {
            bounded(version, "workflow_version", 128)?;
        }
        if let Some(digest) = plan_digest {
            bounded(digest, "plan_digest", 256)?;
        }
        let inputs_json = encode_json(inputs, "inputs")?;
        let now = utc_now();

        self.write(|transaction| {
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO runs (
                 run_id, workflow_name, workflow_version, plan_digest,
                 status, desired_state, inputs_json, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, 'pending', 'run', ?5, ?6, ?6)",
                params![
                    run_id,
                    workflow_name,
                    workflow_version,
                    plan_digest,
                    inputs_json,
                    now
                ],
            )?;
            if inserted != 1 {
                return Err(JournalError::Conflict(format!(
                    "run already exists: {run_id}"
                )));
            }
            if let Some((event_type, payload)) = first_event {
                append_event(transaction, run_id, event_type, payload)?;
            }
            read_run(transaction, run_id)
        })
    }

    pub fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        self.connection
            .query_row(
                "SELECT * FROM runs WHERE run_id = ?1",
                params![run_id],
                run_from_row,
            )
            .optional()?
            .ok_or_else(|| JournalError::NotFound(format!("run not found: {run_id}")))
    }

    /// Runs, newest first.
    pub fn list_runs(
        &self,
        status: Option<RunStatus>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RunRecord>> {
        if !(1..=10_000).contains(&limit) {
            return Err(JournalError::invalid("limit must be between 1 and 10000"));
        }
        if offset < 0 {
            return Err(JournalError::invalid("offset must be non-negative"));
        }
        // `run_id` breaks ties so paging cannot repeat or skip a row when two
        // runs share a timestamp.
        let order = "ORDER BY created_at DESC, run_id DESC LIMIT ?1 OFFSET ?2";
        let mut found = Vec::new();
        match status {
            Some(status) => {
                let mut statement = self
                    .connection
                    .prepare(&format!("SELECT * FROM runs WHERE status = ?3 {order}"))?;
                let rows =
                    statement.query_map(params![limit, offset, status.as_str()], run_from_row)?;
                for row in rows {
                    found.push(row?);
                }
            }
            None => {
                let mut statement = self
                    .connection
                    .prepare(&format!("SELECT * FROM runs {order}"))?;
                let rows = statement.query_map(params![limit, offset], run_from_row)?;
                for row in rows {
                    found.push(row?);
                }
            }
        }
        Ok(found)
    }

    /// Record operator intent, with its control event, in one transaction.
    ///
    /// Takes no lease: an operator must be able to ask for a pause or cancel
    /// without holding the runner's bearer token. This only records the wish —
    /// the runner acts on it at its next safe point.
    pub fn compare_and_set_desired_state(
        &self,
        run_id: &str,
        expected: DesiredState,
        desired: DesiredState,
        event: Option<(&str, &Value)>,
    ) -> Result<RunRecord> {
        if !expected.allows(desired) {
            return Err(JournalError::InvalidTransition(format!(
                "invalid desired state transition: {} -> {}",
                expected.as_str(),
                desired.as_str()
            )));
        }
        self.write(|transaction| {
            let changed = transaction.execute(
                &format!(
                    "UPDATE runs SET desired_state = ?1, updated_at = ?2
                 WHERE run_id = ?3 AND desired_state = ?4
                   AND status NOT IN ({TERMINAL_SQL})"
                ),
                params![desired.as_str(), utc_now(), run_id, expected.as_str()],
            )?;
            if changed != 1 {
                let run = read_run(transaction, run_id)?;
                return Err(if run.is_terminal() {
                    JournalError::InvalidTransition(format!(
                        "terminal run {run_id} cannot change desired state"
                    ))
                } else {
                    JournalError::Conflict(format!(
                        "desired state changed concurrently: expected {}, found {}",
                        expected.as_str(),
                        run.desired_state.as_str()
                    ))
                });
            }
            if let Some((event_type, payload)) = event {
                append_event(transaction, run_id, event_type, payload)?;
            }
            read_run(transaction, run_id)
        })
    }

    /// Take ownership of an unowned or expired run.
    ///
    /// Expiry is what lets a crashed runner's work be picked up: the lease is
    /// never explicitly released by a process that died, so a later claimant
    /// takes over once it lapses. A live lease held by someone else is refused.
    pub fn claim_owner(
        &self,
        run_id: &str,
        owner_id: &str,
        ttl_seconds: f64,
        now: impl Into<LeaseTime>,
    ) -> Result<OwnerLease> {
        self.claim_run(run_id, owner_id, ttl_seconds, now, None)
            .map(|(lease, _)| lease)
    }

    pub(crate) fn claim_run(
        &self,
        run_id: &str,
        owner_id: &str,
        ttl_seconds: f64,
        now: impl Into<LeaseTime>,
        expected: Option<RunStatus>,
    ) -> Result<(OwnerLease, RunRecord)> {
        bounded(owner_id, "owner_id", 256)?;
        if !(ttl_seconds.is_finite() && ttl_seconds > 0.0) {
            return Err(JournalError::invalid(
                "ttl_seconds must be a positive finite number",
            ));
        }
        let token = new_token();
        self.write(|transaction| {
            let now = now.into().seconds();
            let expires_at = lease_expiry(now, ttl_seconds)?;
            let claimed = transaction.execute(
                &format!(
                    "UPDATE runs
                 SET owner_id = ?1, lease_token_hash = ?2, lease_expires_at = ?3,
                     updated_at = ?4
                 WHERE run_id = ?5
                   AND status NOT IN ({TERMINAL_SQL})
                   AND (owner_id IS NULL OR lease_expires_at <= ?6)"
                ),
                params![
                    owner_id,
                    token_hash(&token),
                    expires_at,
                    utc_now(),
                    run_id,
                    now
                ],
            )?;
            if claimed != 1 {
                let run = read_run(transaction, run_id)?;
                return Err(if run.is_terminal() {
                    JournalError::InvalidTransition(format!(
                        "terminal run {run_id} cannot be claimed ({})",
                        run.status.as_str()
                    ))
                } else {
                    JournalError::LeaseConflict(format!(
                        "run {run_id} has a live lease owned by {:?}",
                        run.owner_id.unwrap_or_default()
                    ))
                });
            }
            let run = read_run(transaction, run_id)?;
            if let Some(status) = expected {
                if run.status != status {
                    return Err(JournalError::Conflict(format!(
                        "run status changed concurrently: expected {}, found {}",
                        status.as_str(),
                        run.status.as_str()
                    )));
                }
            }
            Ok((
                OwnerLease {
                    run_id: run_id.to_string(),
                    owner_id: owner_id.to_string(),
                    token,
                    expires_at,
                },
                run,
            ))
        })
    }

    /// Extend a lease that is still genuinely held.
    pub fn heartbeat_owner(
        &self,
        lease: &OwnerLease,
        ttl_seconds: f64,
        now: impl Into<LeaseTime>,
    ) -> Result<OwnerLease> {
        self.with_owner(lease, now, |owned| owned.heartbeat(ttl_seconds))
    }

    /// Give up a lease so another runner can claim the run immediately.
    pub fn release_owner(
        &self,
        lease: &OwnerLease,
        now: impl Into<LeaseTime>,
    ) -> Result<RunRecord> {
        self.write(|transaction| {
            let now = now.into().seconds();
            let released = transaction.execute(
                "UPDATE runs
             SET owner_id = NULL, lease_token_hash = NULL, lease_expires_at = NULL,
                 updated_at = ?1
             WHERE run_id = ?2 AND owner_id = ?3 AND lease_token_hash = ?4
               AND lease_expires_at > ?5",
                params![
                    utc_now(),
                    lease.run_id,
                    lease.owner_id,
                    token_hash(&lease.token),
                    now
                ],
            )?;
            if released != 1 {
                return Err(lease_lost(transaction, &lease.run_id));
            }
            read_run(transaction, &lease.run_id)
        })
    }

    /// Append an event under a live lease.
    pub fn append_event(
        &self,
        lease: &OwnerLease,
        event_type: &str,
        payload: &Value,
        now: impl Into<LeaseTime>,
    ) -> Result<EventRecord> {
        self.write(|transaction| {
            require_live_lease(transaction, lease, now.into().seconds())?;
            append_event(transaction, &lease.run_id, event_type, payload)
        })
    }

    /// Replace the checkpoint and append its event atomically.
    ///
    /// Atomic because a checkpoint without its event, or the reverse, would make
    /// the history disagree with the state it is supposed to explain. The
    /// optional expectations let a caller assert that neither the run's status
    /// nor the operator's intent moved while it was working, which is how a
    /// completing step notices a concurrent pause instead of overwriting it.
    #[allow(clippy::too_many_arguments)]
    pub fn append_event_with_checkpoint(
        &self,
        lease: &OwnerLease,
        event_type: &str,
        payload: &Value,
        checkpoint: &Value,
        expected_status: Option<RunStatus>,
        expected_desired_state: Option<DesiredState>,
        now: impl Into<LeaseTime>,
    ) -> Result<EventRecord> {
        self.with_owner(lease, now, |owned| {
            owned.expect(expected_status, expected_desired_state)?;
            owned.checkpoint(event_type, payload, checkpoint)
        })
    }

    pub(crate) fn with_owner<T>(
        &self,
        lease: &OwnerLease,
        now: impl Into<LeaseTime>,
        body: impl FnOnce(&OwnedRun<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write(|transaction| {
            let now = now.into().seconds();
            require_live_lease(transaction, lease, now)?;
            body(&OwnedRun {
                transaction,
                lease,
                now,
            })
        })
    }

    /// Move a run to a new status, fenced by the lease and by the expected value.
    ///
    /// Reaching `paused` or any terminal status also releases the lease, in the
    /// same transaction: a paused run must be claimable by whoever resumes it,
    /// and a finished run must not appear to be owned by a process that has
    /// stopped caring about it.
    #[allow(clippy::too_many_arguments)]
    pub fn set_status(
        &self,
        lease: &OwnerLease,
        expected: RunStatus,
        status: RunStatus,
        event: Option<(&str, &Value)>,
        output: Option<&Value>,
        error: Option<&Value>,
        expected_desired_state: Option<DesiredState>,
        now: impl Into<LeaseTime>,
    ) -> Result<RunRecord> {
        validate_status_transition(expected, status, output, error)?;
        self.with_owner(lease, now, |owned| {
            owned.expect(Some(expected), expected_desired_state)?;
            owned.set_status(expected, status, event, output, error)
        })
    }

    /// Events in order, for following or replaying a run's history.
    pub fn list_events(
        &self,
        run_id: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<EventRecord>> {
        if after_seq < 0 {
            return Err(JournalError::invalid("after_seq must be non-negative"));
        }
        if !(1..=10_000).contains(&limit) {
            return Err(JournalError::invalid("limit must be between 1 and 10000"));
        }
        // Distinguish "no such run" from "no new events", which callers polling
        // for progress need to tell apart.
        self.get_run(run_id)?;
        let mut statement = self.connection.prepare(
            "SELECT run_id, seq, event_type, payload_json, created_at
             FROM events WHERE run_id = ?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = statement.query_map(params![run_id, after_seq, limit], |row| {
            Ok(EventRecord {
                run_id: row.get(0)?,
                seq: row.get(1)?,
                event_type: row.get(2)?,
                payload: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or(Value::Null),
                created_at: row.get(4)?,
            })
        })?;
        let mut found = Vec::new();
        for row in rows {
            found.push(row?);
        }
        Ok(found)
    }

    /// The most recent events, oldest first.
    ///
    /// For reporting where a run has got to without reading its whole history,
    /// which for a long run can be far larger than the answer needs.
    pub fn list_recent_events(&self, run_id: &str, limit: i64) -> Result<Vec<EventRecord>> {
        if !(1..=10_000).contains(&limit) {
            return Err(JournalError::invalid("limit must be between 1 and 10000"));
        }
        self.get_run(run_id)?;
        let mut statement = self.connection.prepare(
            "SELECT run_id, seq, event_type, payload_json, created_at
             FROM events WHERE run_id = ?1
             ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![run_id, limit], |row| {
            Ok(EventRecord {
                run_id: row.get(0)?,
                seq: row.get(1)?,
                event_type: row.get(2)?,
                payload: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or(Value::Null),
                created_at: row.get(4)?,
            })
        })?;
        let mut found = Vec::new();
        for row in rows {
            found.push(row?);
        }
        found.reverse();
        Ok(found)
    }

    /// Confirm a lease is still live, for use at a safe point before continuing.
    pub fn ensure_live_lease(
        &self,
        lease: &OwnerLease,
        now: impl Into<LeaseTime>,
    ) -> Result<RunRecord> {
        self.write(|transaction| {
            let now = now.into().seconds();
            require_live_lease(transaction, lease, now)?;
            read_run(transaction, &lease.run_id)
        })
    }
}

/// Operations on a run after validating its bearer token under BEGIN IMMEDIATE.
pub(crate) struct OwnedRun<'a> {
    transaction: &'a Transaction<'a>,
    lease: &'a OwnerLease,
    now: f64,
}

impl OwnedRun<'_> {
    pub(crate) fn heartbeat(&self, ttl_seconds: f64) -> Result<OwnerLease> {
        let Self {
            transaction,
            lease,
            now,
        } = *self;
        if !(ttl_seconds.is_finite() && ttl_seconds > 0.0) {
            return Err(JournalError::invalid(
                "ttl_seconds must be a positive finite number",
            ));
        }
        let expires_at = lease_expiry(now, ttl_seconds)?;
        let extended = transaction.execute(
            &format!(
                "UPDATE runs SET lease_expires_at = ?1, updated_at = ?2
                 WHERE run_id = ?3 AND owner_id = ?4 AND lease_token_hash = ?5
                   AND lease_expires_at > ?6
                   AND status NOT IN ({TERMINAL_SQL})"
            ),
            params![
                expires_at,
                utc_now(),
                lease.run_id,
                lease.owner_id,
                token_hash(&lease.token),
                now
            ],
        )?;
        if extended != 1 {
            return Err(lease_lost(transaction, &lease.run_id));
        }
        Ok(OwnerLease {
            run_id: lease.run_id.clone(),
            owner_id: lease.owner_id.clone(),
            token: lease.token.clone(),
            expires_at,
        })
    }

    pub(crate) fn clear_pause(&self) -> Result<()> {
        let changed = self.transaction.execute(
            "UPDATE runs SET desired_state = 'run', updated_at = ?1 WHERE run_id = ?2 AND desired_state = 'pause'",
            params![utc_now(), self.lease.run_id],
        )?;
        if changed != 1 {
            return Err(JournalError::Conflict(
                "resume requires a standing pause".into(),
            ));
        }
        append_event(
            self.transaction,
            &self.lease.run_id,
            "run.resume_requested",
            &serde_json::json!({"fromDesiredState": "pause", "toDesiredState": "run"}),
        )?;
        Ok(())
    }

    pub(crate) fn record(&self) -> Result<RunRecord> {
        read_run(self.transaction, &self.lease.run_id)
    }

    pub(crate) fn expect(
        &self,
        status: Option<RunStatus>,
        desired: Option<DesiredState>,
    ) -> Result<DesiredState> {
        require_expected_state(self.transaction, &self.lease.run_id, status, desired)
    }

    pub(crate) fn checkpoint(
        &self,
        event_type: &str,
        payload: &Value,
        checkpoint: &Value,
    ) -> Result<EventRecord> {
        let checkpoint_json = encode_json(checkpoint, "checkpoint")?;
        let Self {
            transaction,
            lease,
            now,
        } = *self;
        let event = append_event(transaction, &lease.run_id, event_type, payload)?;
        let saved = transaction.execute(
            &format!(
                "UPDATE runs SET checkpoint_json = ?1, updated_at = ?2
                 WHERE run_id = ?3 AND owner_id = ?4 AND lease_token_hash = ?5
                   AND lease_expires_at > ?6
                   AND status NOT IN ({TERMINAL_SQL})"
            ),
            params![
                checkpoint_json,
                utc_now(),
                lease.run_id,
                lease.owner_id,
                token_hash(&lease.token),
                now
            ],
        )?;
        if saved != 1 {
            return Err(lease_lost(transaction, &lease.run_id));
        }
        Ok(event)
    }

    pub(crate) fn set_status(
        &self,
        expected: RunStatus,
        status: RunStatus,
        event: Option<(&str, &Value)>,
        output: Option<&Value>,
        error: Option<&Value>,
    ) -> Result<RunRecord> {
        let Self {
            transaction,
            lease,
            now,
        } = *self;
        validate_status_transition(expected, status, output, error)?;
        let output_json = output.map(|v| encode_json(v, "output")).transpose()?;
        let error_json = error.map(|v| encode_json(v, "error")).transpose()?;
        let releases_lease = status == RunStatus::Paused || status.is_terminal();
        let finished_at = status.is_terminal().then(utc_now);

        if let Some((event_type, payload)) = event {
            append_event(transaction, &lease.run_id, event_type, payload)?;
        }
        let updated = transaction.execute(
            &format!(
                "UPDATE runs
                 SET status = ?1, output_json = ?2, error_json = ?3, finished_at = ?4,
                     updated_at = ?5,
                     owner_id = CASE WHEN ?6 THEN NULL ELSE owner_id END,
                     lease_token_hash = CASE WHEN ?6 THEN NULL ELSE lease_token_hash END,
                     lease_expires_at = CASE WHEN ?6 THEN NULL ELSE lease_expires_at END
                 WHERE run_id = ?7 AND status = ?8 AND owner_id = ?9
                   AND lease_token_hash = ?10 AND lease_expires_at > ?11
                   AND status NOT IN ({TERMINAL_SQL})"
            ),
            params![
                status.as_str(),
                output_json,
                error_json,
                finished_at,
                utc_now(),
                releases_lease,
                lease.run_id,
                expected.as_str(),
                lease.owner_id,
                token_hash(&lease.token),
                now
            ],
        )?;
        if updated != 1 {
            let run = read_run(transaction, &lease.run_id)?;
            return Err(if run.is_terminal() {
                JournalError::InvalidTransition(format!(
                    "terminal run {} is immutable ({})",
                    lease.run_id,
                    run.status.as_str()
                ))
            } else {
                JournalError::Conflict(format!(
                    "run status changed concurrently: expected {}, found {}",
                    expected.as_str(),
                    run.status.as_str()
                ))
            });
        }
        read_run(transaction, &lease.run_id)
    }
}

fn lease_expiry(now: f64, ttl: f64) -> Result<f64> {
    let expires_at = now + ttl;
    if !expires_at.is_finite() || expires_at <= now {
        return Err(JournalError::invalid(
            "lease expiry must be finite and after now",
        ));
    }
    Ok(expires_at)
}

/// The schema, with the lifecycle enforced by the database itself.
fn schema_ddl() -> String {
    format!(
        "
CREATE TABLE runs (
    run_id TEXT PRIMARY KEY,
    workflow_name TEXT NOT NULL,
    workflow_version TEXT,
    plan_digest TEXT,
    status TEXT NOT NULL CHECK (status IN (
        'pending','running','paused','succeeded','failed',
        'timed_out','cancelled','unknown_effect'
    )),
    desired_state TEXT NOT NULL CHECK (desired_state IN ('run','pause','cancel')),
    inputs_json TEXT NOT NULL,
    output_json TEXT,
    error_json TEXT,
    checkpoint_json TEXT,
    owner_id TEXT,
    lease_token_hash TEXT,
    lease_expires_at REAL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    finished_at TEXT,
    -- A lease is all three columns or none of them; a half-set lease would
    -- fence nothing while looking like it did.
    CHECK ((owner_id IS NULL) = (lease_token_hash IS NULL)),
    CHECK ((owner_id IS NULL) = (lease_expires_at IS NULL)),
    CHECK (length(run_id) BETWEEN 1 AND 256),
    CHECK (length(workflow_name) BETWEEN 1 AND 192),
    CHECK (workflow_version IS NULL OR length(workflow_version) BETWEEN 1 AND 128),
    CHECK (plan_digest IS NULL OR length(plan_digest) BETWEEN 1 AND 256),
    CHECK (owner_id IS NULL OR length(owner_id) BETWEEN 1 AND 256),
    -- Output belongs only to success, an error only to failure, and neither to
    -- a run still in flight.
    CHECK (
        (status NOT IN ({TERMINAL_SQL}) AND output_json IS NULL AND error_json IS NULL)
        OR (status = 'succeeded' AND error_json IS NULL)
        OR (status IN ('failed','timed_out','cancelled','unknown_effect')
            AND output_json IS NULL AND error_json IS NOT NULL)
    ),
    CHECK (
        (status IN ({TERMINAL_SQL}) AND finished_at IS NOT NULL)
        OR (status NOT IN ({TERMINAL_SQL}) AND finished_at IS NULL)
    )
);

CREATE TABLE events (
    run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    seq INTEGER NOT NULL CHECK (seq >= 1),
    event_type TEXT NOT NULL CHECK (length(event_type) BETWEEN 1 AND 192),
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
) WITHOUT ROWID;

CREATE INDEX runs_created_at_idx ON runs(created_at DESC, run_id DESC);
CREATE INDEX runs_status_created_at_idx ON runs(status, created_at DESC, run_id DESC);

-- The state machine, enforced for every writer including a future one that
-- forgets to check.
CREATE TRIGGER runs_status_transition_valid
BEFORE UPDATE OF status ON runs
WHEN NEW.status != OLD.status AND NOT (
    (OLD.status = 'pending' AND NEW.status IN ('running','failed','timed_out','cancelled')) OR
    (OLD.status = 'running' AND NEW.status IN (
        'paused','succeeded','failed','timed_out','cancelled','unknown_effect')) OR
    (OLD.status = 'paused' AND NEW.status IN (
        'running','failed','timed_out','cancelled','unknown_effect'))
)
BEGIN
    SELECT RAISE(ABORT, 'invalid run status transition');
END;

-- A cancel cannot be walked back, so a racing resume cannot lose it.
CREATE TRIGGER runs_cancel_intent_absorbing
BEFORE UPDATE OF desired_state ON runs
WHEN OLD.desired_state = 'cancel' AND NEW.desired_state != 'cancel'
BEGIN
    SELECT RAISE(ABORT, 'cancel desired state is absorbing');
END;

CREATE TRIGGER runs_terminal_row_immutable
BEFORE UPDATE ON runs
WHEN OLD.status IN ({TERMINAL_SQL})
BEGIN
    SELECT RAISE(ABORT, 'terminal run is immutable');
END;

CREATE TRIGGER events_reject_terminal_run
BEFORE INSERT ON events
WHEN EXISTS (SELECT 1 FROM runs WHERE run_id = NEW.run_id AND status IN ({TERMINAL_SQL}))
BEGIN
    SELECT RAISE(ABORT, 'terminal run cannot accept events');
END;

-- Gaps would make an event feed look complete while missing history.
CREATE TRIGGER events_contiguous_sequence
BEFORE INSERT ON events
WHEN NEW.seq != COALESCE((SELECT MAX(seq) + 1 FROM events WHERE run_id = NEW.run_id), 1)
BEGIN
    SELECT RAISE(ABORT, 'event sequence must be contiguous');
END;
"
    )
}

/// Insert the next event for a run, allocating its sequence number in SQL.
///
/// `MAX(seq) + 1` is computed inside the statement, under the writer lock, so
/// two concurrent appends cannot choose the same number.
fn append_event(
    transaction: &Transaction<'_>,
    run_id: &str,
    event_type: &str,
    payload: &Value,
) -> Result<EventRecord> {
    bounded(event_type, "event_type", 192)?;
    if !is_event_type(event_type) {
        return Err(JournalError::invalid(
            "event_type must be a lowercase dot, dash, or underscore qualified name",
        ));
    }
    let payload_json = encode_json(payload, "event payload")?;
    let created_at = utc_now();

    // Check the run exists first. An aggregate SELECT yields a row even when its
    // WHERE matches nothing, so folding this into the INSERT below would let a
    // missing run reach the foreign key and surface as a storage failure instead
    // of the "run not found" the caller can actually act on. Verified: extended
    // code 787, zero rows inserted.
    let exists: Option<i64> = transaction
        .query_row(
            "SELECT 1 FROM runs WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(JournalError::NotFound(format!("run not found: {run_id}")));
    }

    // The sequence is still allocated inside the statement, under the writer
    // lock, so two concurrent appends cannot pick the same number.
    let seq: i64 = transaction.query_row(
        "INSERT INTO events (run_id, seq, event_type, payload_json, created_at)
         SELECT ?1, COALESCE(MAX(seq), 0) + 1, ?2, ?3, ?4 FROM events
         WHERE run_id = ?1
         RETURNING seq",
        params![run_id, event_type, payload_json, created_at],
        |row| row.get(0),
    )?;
    Ok(EventRecord {
        run_id: run_id.to_string(),
        seq,
        event_type: event_type.to_string(),
        payload: payload.clone(),
        created_at,
    })
}

fn require_live_lease(transaction: &Transaction<'_>, lease: &OwnerLease, now: f64) -> Result<()> {
    let held: Option<i64> = transaction
        .query_row(
            &format!(
                "SELECT 1 FROM runs
                 WHERE run_id = ?1 AND owner_id = ?2 AND lease_token_hash = ?3
                   AND lease_expires_at > ?4 AND status NOT IN ({TERMINAL_SQL})"
            ),
            params![lease.run_id, lease.owner_id, token_hash(&lease.token), now],
            |row| row.get(0),
        )
        .optional()?;
    if held.is_none() {
        return Err(lease_lost(transaction, &lease.run_id));
    }
    Ok(())
}

fn require_expected_state(
    transaction: &Transaction<'_>,
    run_id: &str,
    status: Option<RunStatus>,
    desired_state: Option<DesiredState>,
) -> Result<DesiredState> {
    let row: Option<(String, String)> = transaction
        .query_row(
            "SELECT status, desired_state FROM runs WHERE run_id = ?1",
            params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((actual_status, actual_desired)) = row else {
        return Err(JournalError::NotFound(format!("run not found: {run_id}")));
    };
    if let Some(expected) = status {
        if actual_status != expected.as_str() {
            return Err(JournalError::Conflict(format!(
                "run status changed concurrently: expected {}, found {actual_status}",
                expected.as_str()
            )));
        }
    }
    if let Some(expected) = desired_state {
        if actual_desired != expected.as_str() {
            return Err(JournalError::Conflict(format!(
                "desired state changed concurrently: expected {}, found {actual_desired}",
                expected.as_str()
            )));
        }
    }
    DesiredState::parse(&actual_desired)
}

/// Distinguish a lost lease from an immutable terminal run.
///
/// Both refuse the write, but they mean different things: one says "somebody
/// else owns this now", the other "this run is over".
fn lease_lost(transaction: &Transaction<'_>, run_id: &str) -> JournalError {
    match read_run(transaction, run_id) {
        Ok(run) if run.is_terminal() => JournalError::InvalidTransition(format!(
            "terminal run {run_id} is immutable ({})",
            run.status.as_str()
        )),
        Ok(_) => JournalError::LeaseLost(format!("owner lease is no longer held for run {run_id}")),
        Err(error) => error,
    }
}

fn read_run(transaction: &Transaction<'_>, run_id: &str) -> Result<RunRecord> {
    transaction
        .query_row(
            "SELECT * FROM runs WHERE run_id = ?1",
            params![run_id],
            run_from_row,
        )
        .optional()?
        .ok_or_else(|| JournalError::NotFound(format!("run not found: {run_id}")))
}

fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    let decode = |value: Option<String>| -> Option<Value> {
        value.and_then(|text| serde_json::from_str(&text).ok())
    };
    Ok(RunRecord {
        run_id: row.get("run_id")?,
        workflow_name: row.get("workflow_name")?,
        workflow_version: row.get("workflow_version")?,
        plan_digest: row.get("plan_digest")?,
        status: RunStatus::parse(&row.get::<_, String>("status")?)
            .unwrap_or(RunStatus::UnknownEffect),
        desired_state: DesiredState::parse(&row.get::<_, String>("desired_state")?)
            .unwrap_or(DesiredState::Cancel),
        inputs: decode(Some(row.get("inputs_json")?)).unwrap_or(Value::Null),
        output: decode(row.get("output_json")?),
        error: decode(row.get("error_json")?),
        checkpoint: decode(row.get("checkpoint_json")?),
        owner_id: row.get("owner_id")?,
        lease_expires_at: row.get("lease_expires_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        finished_at: row.get("finished_at")?,
    })
}

fn validate_status_transition(
    expected: RunStatus,
    status: RunStatus,
    output: Option<&Value>,
    error: Option<&Value>,
) -> Result<()> {
    if !expected.allows(status) {
        return Err(JournalError::InvalidTransition(format!(
            "invalid run status transition: {} -> {}",
            expected.as_str(),
            status.as_str()
        )));
    }
    if !status.is_terminal() && (output.is_some() || error.is_some()) {
        return Err(JournalError::InvalidTransition(
            "output and error may only be committed with a terminal status".into(),
        ));
    }
    if status == RunStatus::Succeeded && error.is_some() {
        return Err(JournalError::InvalidTransition(
            "a succeeded run cannot contain an error".into(),
        ));
    }
    if status.is_terminal() && status != RunStatus::Succeeded {
        // A failure without an error would leave no way to say what went wrong.
        if output.is_some() || error.is_none() {
            return Err(JournalError::InvalidTransition(format!(
                "{} requires an error and cannot contain output",
                status.as_str()
            )));
        }
    }
    Ok(())
}

/// Serialise canonically, refusing anything that would not survive a round trip.
///
/// Sorted keys and no NaN/Infinity: a value that decodes differently than it was
/// written would make a resumed run diverge from the one that was checkpointed.
fn encode_json(value: &Value, field: &str) -> Result<String> {
    fn finite(value: &Value) -> bool {
        match value {
            Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
            Value::Array(items) => items.iter().all(finite),
            Value::Object(entries) => entries.values().all(finite),
            _ => true,
        }
    }
    if !finite(value) {
        return Err(JournalError::invalid(format!(
            "{field} must be finite JSON data"
        )));
    }
    // BTreeMap sorts keys, so the same data always produces the same bytes.
    let sorted: std::collections::BTreeMap<String, Value>;
    let canonical = match value {
        Value::Object(entries) => {
            sorted = entries
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            serde_json::to_string(&sorted)
        }
        other => serde_json::to_string(other),
    };
    canonical.map_err(|error| JournalError::invalid(format!("{field}: {error}")))
}

fn bounded(value: &str, field: &str, maximum: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(JournalError::invalid(format!(
            "{field} must be a non-empty string"
        )));
    }
    if value.chars().count() > maximum {
        return Err(JournalError::invalid(format!(
            "{field} must not exceed {maximum} characters"
        )));
    }
    Ok(())
}

/// `lowercase.segments-with_separators`, so event types stay greppable.
fn is_event_type(value: &str) -> bool {
    let mut segments = value.split(['.', '_', '-']);
    let first = segments.next().unwrap_or("");
    let valid = |segment: &str, alpha_first: bool| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && (!alpha_first || segment.starts_with(|c: char| c.is_ascii_lowercase()))
    };
    valid(first, true) && segments.all(|segment| valid(segment, false))
}

/// Only the hash is stored, so a leaked journal file does not hand over the
/// ability to write as the current owner.
fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn new_token() -> String {
    // Two v4 UUIDs: 256 bits from the OS random source, which is what makes the
    // token unguessable by a process that does not hold it.
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn utc_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs() as i64;
    let micros = now.subsec_micros();
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}+00:00",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

/// Days since the epoch to a calendar date (Howard Hinnant's civil_from_days).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Seconds since the epoch, for lease arithmetic.
pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

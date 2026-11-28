//! Reproduce a fork between writer admission and SQLite's BEGIN IMMEDIATE.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, Receiver};

use fs2::FileExt;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use serde_json::json;

use super::*;

// A separate test process prevents a stalled child from keeping descriptors
// from other tests open. Each scenario is also traced independently.
fn in_isolated_process() -> bool {
    const CHILD_TEST: &str = "AAD_TEST_INHERITED_WRITER";
    let thread = std::thread::current();
    let test = thread.name().expect("libtest names each test thread");
    assert!(test.starts_with("durable::forked_writer_tests::"));
    if std::env::var(CHILD_TEST).as_deref() == Ok(test) {
        return true;
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_TEST, test)
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed in isolation");
    false
}

struct TempJournal(PathBuf);

impl TempJournal {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("aad-fork-writer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn path(&self) -> PathBuf {
        self.0.join("journal.sqlite3")
    }

    fn open(&self) -> JournalStore {
        JournalStore::open_with_timeout(self.path(), 25).unwrap()
    }

    fn admission_file(&self) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.0.join("journal.sqlite3.aad-writer.lock"))
            .unwrap()
    }

    fn open_seeded(&self) -> (JournalStore, OwnerLease) {
        let store = self.open();
        store
            .create_run("run-1", "fixture", &json!({}), &json!({}), None, None, None)
            .unwrap();
        let lease = store.claim_owner("run-1", "worker", 60.0, 1.0).unwrap();
        (store, lease)
    }

    fn history(&self) -> Vec<(i64, String, Value)> {
        self.open()
            .list_events("run-1", 0, 10)
            .unwrap()
            .into_iter()
            .map(|event| (event.seq, event.event_type, event.payload))
            .collect()
    }
}

impl Drop for TempJournal {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct PausedChild {
    gate: UnixStream,
    pid: Option<libc::pid_t>,
}

impl PausedChild {
    fn fork() -> Self {
        let (gate, child_end) = UnixStream::pair().unwrap();
        gate.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let parent_fd = gate.as_raw_fd();
        let child_fd = child_end.as_raw_fd();
        let program = c"/bin/true";
        let argv = [program.as_ptr(), std::ptr::null()];
        // SAFETY: The child uses only async-signal-safe syscalls until exec.
        // Close its copy of the parent's socket so losing the parent gives EOF.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let mut byte = 0u8;
            unsafe {
                libc::close(parent_fd);
                if libc::write(child_fd, b"F".as_ptr().cast(), 1) == 1
                    && libc::read(child_fd, std::ptr::from_mut(&mut byte).cast(), 1) == 0
                {
                    libc::execv(program.as_ptr(), argv.as_ptr());
                }
                libc::_exit(1);
            }
        }
        assert!(pid > 0, "fork failed: {}", io::Error::last_os_error());
        drop(child_end);
        let mut child = Self {
            gate,
            pid: Some(pid),
        };
        let mut ready = [0];
        child.gate.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"F");
        child
    }

    fn release_and_wait(&mut self) -> io::Result<ExitStatus> {
        let _ = self.gate.shutdown(Shutdown::Write);
        let pid = self.pid.take().unwrap();
        let mut status = 0;
        loop {
            // SAFETY: pid belongs to this child and status is a valid out-pointer.
            if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
                return Ok(ExitStatus::from_raw(status));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for PausedChild {
    fn drop(&mut self) {
        if self.pid.is_some() {
            let _ = self.release_and_wait();
        }
    }
}

fn pause_child_at_begin(
    store: &JournalStore,
    admission: File,
    after_fork: impl FnOnce() + Send + 'static,
) -> Receiver<PausedChild> {
    let (sender, receiver) = mpsc::channel();
    let mut after_fork = Some(after_fork);
    // The authorizer runs while SQLite prepares BEGIN, after admission and
    // before it tries to obtain the SQLite write lock.
    store
        .connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if let AuthAction::Transaction {
                operation: TransactionOperation::Begin,
            } = context.action
            {
                if let Some(after_fork) = after_fork.take() {
                    let error = FileExt::try_lock_exclusive(&admission).unwrap_err();
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                    let child = PausedChild::fork();
                    after_fork();
                    sender.send(child).unwrap();
                }
            }
            Authorization::Allow
        }))
        .unwrap();
    receiver
}

#[test]
fn releases_inherited_admission_before_transaction_body() {
    if !in_isolated_process() {
        return;
    }
    let journal = TempJournal::new();
    let (store, lease) = journal.open_seeded();
    let locker = Connection::open(journal.path()).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let child = pause_child_at_begin(&store, journal.admission_file(), move || {
        locker.execute_batch("ROLLBACK").unwrap();
    });
    let sqlite_probe = Connection::open(journal.path()).unwrap();
    sqlite_probe.busy_timeout(Duration::ZERO).unwrap();
    let admission_probe = journal.admission_file();
    store
        .write(|transaction| {
            FileExt::try_lock_exclusive(&admission_probe)
                .expect("admission should be free while the transaction is active");
            FileExt::unlock(&admission_probe).unwrap();
            let busy = sqlite_probe.execute_batch("BEGIN IMMEDIATE").unwrap_err();
            assert_eq!(busy.sqlite_error_code(), Some(ErrorCode::DatabaseBusy));
            append_event(transaction, "run-1", "worker.tick", &json!("first"))
        })
        .unwrap();
    let mut child = child.try_recv().unwrap();
    store
        .append_event(&lease, "worker.tick", &json!("next"), 2.0)
        .unwrap();
    assert_eq!(
        journal.history(),
        [
            (1, "worker.tick".into(), json!("first")),
            (2, "worker.tick".into(), json!("next")),
        ]
    );
    assert!(child.release_and_wait().unwrap().success());
}

#[test]
fn retries_after_rollback_while_child_still_holds_inherited_handle() {
    if !in_isolated_process() {
        return;
    }
    let journal = TempJournal::new();
    let (store, lease) = journal.open_seeded();
    let locker = Connection::open(journal.path()).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let child = pause_child_at_begin(&store, journal.admission_file(), || {});
    let error = store
        .append_event(&lease, "worker.tick", &json!("timed out"), 2.0)
        .unwrap_err();
    assert_eq!(error.code(), "JOURNAL.STORAGE_FAILED");
    assert_eq!(error.message(), "database is locked");
    let mut child = child.try_recv().unwrap();
    assert!(journal.history().is_empty());
    locker.execute_batch("ROLLBACK").unwrap();
    store
        .append_event(&lease, "worker.tick", &json!("retry"), 2.0)
        .expect("immediate retry should acquire admission after rollback");
    assert_eq!(
        journal.history(),
        [(1, "worker.tick".into(), json!("retry"))]
    );
    assert!(child.release_and_wait().unwrap().success());
}

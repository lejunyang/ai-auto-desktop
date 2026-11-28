use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use super::{JournalError, Result};

/// SQLite has a single writer but its busy handler does not queue waiters.
/// Give local writers a turn in arrival order and coordinate lock acquisition
/// across processes. SQLite still enforces all transaction boundaries.
pub(super) struct Writers {
    queue: Mutex<VecDeque<Arc<Condvar>>>,
    admission_path: PathBuf,
}

impl Writers {
    pub(super) fn for_file(path: &Path) -> Arc<Self> {
        static FILES: LazyLock<Mutex<HashMap<PathBuf, Weak<Writers>>>> =
            LazyLock::new(Mutex::default);
        let mut files = FILES.lock().unwrap();
        if let Some(writers) = files.get(path).and_then(Weak::upgrade) {
            return writers;
        }
        files.retain(|_, writers| writers.strong_count() != 0);
        let mut admission_path = path.as_os_str().to_owned();
        admission_path.push(".aad-writer.lock");
        let writers = Arc::new(Self {
            queue: Mutex::default(),
            admission_path: admission_path.into(),
        });
        files.insert(path.to_owned(), Arc::downgrade(&writers));
        writers
    }

    /// Only the process waiting to begin the next write holds this lock. The
    /// current transaction runs without it, so a waiter can reserve the next
    /// turn while SQLite is syncing a commit. Keep the inode across calls.
    pub(super) fn reserve(&self, deadline: Instant) -> Result<Option<Reservation>> {
        if std::fs::symlink_metadata(&self.admission_path)
            .is_ok_and(|meta| !meta.is_file() || meta.len() != 0)
        {
            return Ok(None);
        }
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.admission_path)
        {
            Ok(file) => file,
            // A pre-existing WAL can be writable even when this process cannot
            // create a sibling file. SQLite still serializes those writers.
            Err(_) => return Ok(None),
        };
        let pause = Duration::from_millis(2 + u64::from(std::process::id() % 5));
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Some(Reservation(file))),
                Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => (),
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return Ok(None),
                Err(e) => {
                    return Err(JournalError::Storage(format!(
                        "reserve journal writer: {e}"
                    )))
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(JournalError::Storage(
                    "timed out reserving the next journal writer".into(),
                ));
            }
            std::thread::sleep(pause.min(remaining));
        }
    }

    pub(super) fn enter(&self, deadline: Instant) -> Result<Turn<'_>> {
        let mut queue = self.queue.lock().unwrap();
        let turn = Arc::new(Condvar::new());
        queue.push_back(turn.clone());
        while !queue.front().is_some_and(|head| Arc::ptr_eq(head, &turn)) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                queue.retain(|waiting| !Arc::ptr_eq(waiting, &turn));
                return Err(JournalError::Storage(
                    "timed out waiting for the journal writer".into(),
                ));
            }
            queue = turn.wait_timeout(queue, remaining).unwrap().0;
        }
        Ok(Turn(self))
    }
}

pub(super) struct Reservation(File);

impl Drop for Reservation {
    fn drop(&mut self) {
        // A child can inherit the open file description until exec, so closing
        // this handle alone may leave the lock held.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub(super) struct Turn<'a>(&'a Writers);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.pop_front();
        if let Some(next) = queue.front() {
            next.notify_one();
        }
    }
}

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::Instant;

use super::{JournalError, Result};

/// SQLite has a single writer but its busy handler does not queue waiters.
/// Give connections in this process a turn in arrival order. Other processes
/// still arbitrate with us through SQLite's file lock.
#[derive(Default)]
pub(super) struct Writers {
    queue: Mutex<VecDeque<Arc<Condvar>>>,
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
        let writers = Arc::new(Self::default());
        files.insert(path.to_owned(), Arc::downgrade(&writers));
        writers
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

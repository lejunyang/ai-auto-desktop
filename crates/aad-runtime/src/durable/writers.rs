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
    queue: Mutex<VecDeque<u64>>,
    changed: Condvar,
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
        let ticket = queue.back().map_or(0, |last| last.wrapping_add(1));
        queue.push_back(ticket);
        while queue.front() != Some(&ticket) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                queue.retain(|&waiting| waiting != ticket);
                return Err(JournalError::Storage(
                    "timed out waiting for the journal writer".into(),
                ));
            }
            queue = self.changed.wait_timeout(queue, remaining).unwrap().0;
        }
        Ok(Turn(self))
    }
}

pub(super) struct Turn<'a>(&'a Writers);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.queue.lock().unwrap().pop_front();
        self.0.changed.notify_all();
    }
}

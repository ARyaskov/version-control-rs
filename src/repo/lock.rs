//! Exclusive repository lock shared by processes and threads.
//!
//! Every mutating operation (and crash recovery) runs under an OS file lock on
//! `.vcrs/lock`, so a second `vcrs` process or a concurrent HTTP request can
//! never observe — let alone roll back — a half-finished commit. Separate open
//! handles conflict even inside one process, which also serializes the HTTP
//! server's worker threads. The lock is re-entrant per thread so public
//! operations can call each other freely.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::Result;

use super::VCRS_DIR;

thread_local! {
    static HELD: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());
}

/// Guard for the repository lock; released when dropped.
#[derive(Debug)]
pub struct RepoLock {
    root: PathBuf,
    file: Option<File>,
}

impl RepoLock {
    /// Block until this thread holds the exclusive lock for `root`.
    pub fn acquire(root: &Path) -> Result<Self> {
        let nested = HELD.with(|held| {
            let mut held = held.borrow_mut();
            match held.get_mut(root) {
                Some(depth) => {
                    *depth += 1;
                    true
                }
                None => false,
            }
        });
        if nested {
            return Ok(Self {
                root: root.to_path_buf(),
                file: None,
            });
        }

        let dir = root.join(VCRS_DIR);
        std::fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join("lock"))?;
        file.lock()?;
        HELD.with(|held| held.borrow_mut().insert(root.to_path_buf(), 1));
        Ok(Self {
            root: root.to_path_buf(),
            file: Some(file),
        })
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(depth) = held.get_mut(&self.root) {
                *depth -= 1;
                if *depth == 0 {
                    held.remove(&self.root);
                }
            }
        });
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn reentrant_on_one_thread_exclusive_across_threads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let outer = RepoLock::acquire(&root).unwrap();
        let inner = RepoLock::acquire(&root).unwrap();

        let entered = Arc::new(AtomicBool::new(false));
        let t = {
            let root = root.clone();
            let entered = entered.clone();
            std::thread::spawn(move || {
                let _g = RepoLock::acquire(&root).unwrap();
                entered.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!entered.load(Ordering::SeqCst), "other thread must wait");
        drop(inner);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!entered.load(Ordering::SeqCst), "still held by outer guard");
        drop(outer);
        t.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
    }
}

#![allow(dead_code)] // Shared by multiple test binaries with different helper subsets.

use std::ffi::OsString;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

// Process environment and current directory are shared by tests in one binary.
// Hold this lock for every test that mutates either part of process state.
static PROCESS_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_current_dir() -> MutexGuard<'static, ()> {
    PROCESS_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn lock_environment() -> MutexGuard<'static, ()> {
    PROCESS_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) struct CurrentDirGuard {
    previous: PathBuf,
    _lock: Option<MutexGuard<'static, ()>>,
}

impl CurrentDirGuard {
    pub(crate) fn hold() -> Self {
        let lock = lock_current_dir();
        let previous = std::env::current_dir().expect("read current directory");
        Self {
            previous,
            _lock: Some(lock),
        }
    }

    pub(crate) fn enter(path: &Path) -> Self {
        let guard = Self::hold();
        guard.set(path);
        guard
    }

    pub(crate) fn set(&self, path: &Path) {
        std::env::set_current_dir(path).expect("set temporary workspace");
    }

    pub(crate) fn enter_under_lock(path: &Path) -> Self {
        let previous = std::env::current_dir().expect("read current directory");
        std::env::set_current_dir(path).expect("set temporary workspace");
        Self {
            previous,
            _lock: None,
        }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).expect("restore current directory");
    }
}

pub(crate) fn with_current_dir<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    let _guard = CurrentDirGuard::enter(path);
    operation()
}

pub(crate) fn with_current_dir_under_lock<T>(path: &Path, operation: impl FnOnce() -> T) -> T {
    let _guard = CurrentDirGuard::enter_under_lock(path);
    operation()
}

pub(crate) struct RestoreEnvironmentVariable {
    name: &'static str,
    value: Option<OsString>,
}

impl RestoreEnvironmentVariable {
    pub(crate) fn remove(name: &'static str) -> Self {
        let value = std::env::var_os(name);
        std::env::remove_var(name);
        Self { name, value }
    }

    pub(crate) fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self {
            name,
            value: previous,
        }
    }
}

impl Drop for RestoreEnvironmentVariable {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            std::env::set_var(self.name, value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

pub(crate) struct TempDir {
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl TempDir {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self::with_prefix("krypper-test-")
    }

    pub(crate) fn with_prefix(prefix: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("create temporary test directory");
        let path = directory.path().to_path_buf();
        Self {
            path,
            _directory: directory,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[allow(dead_code)]
    pub(crate) fn write_file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).expect("write temporary test file");
        path
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

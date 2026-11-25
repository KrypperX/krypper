//! Host-level execution lease and cleanup recovery state.

use super::process::bounded_host;
use crate::host::{effective_uid, host_command};
use crate::{SandboxOutcome, SandboxRunResult};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use uuid::Uuid;

static ACTIVE: Mutex<()> = Mutex::new(());
static UNCERTAIN_CLEANUP: AtomicBool = AtomicBool::new(false);

// One host-wide lease survives caller crashes. A later process must establish that the
// previous unit and its cgroup are empty before it may run another profile.
pub(super) struct HostLease {
    _lock: File,
    receipt: PathBuf,
}

#[derive(Debug)]
pub(super) enum HostLeaseError {
    Unavailable(io::Error),
    CleanupUnconfirmed(io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviousUnitState {
    NotLoaded,
    Stopped,
}

impl HostLease {
    pub(super) fn acquire() -> Result<Self, HostLeaseError> {
        let uid = effective_uid();
        let directory = PathBuf::from(format!("/run/user/{uid}"));
        let metadata = fs::symlink_metadata(&directory).map_err(HostLeaseError::Unavailable)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(HostLeaseError::Unavailable(io::Error::other(
                "untrusted runtime directory",
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("krypper-offline-cargo.lock"))
            .map_err(HostLeaseError::Unavailable)?;
        let metadata = file.metadata().map_err(HostLeaseError::Unavailable)?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.len() > 4096
        {
            return Err(HostLeaseError::Unavailable(io::Error::other(
                "untrusted execution lease",
            )));
        }
        // SAFETY: `file` owns a live descriptor for the duration of the call, and `flock`
        // takes only that descriptor and an integer operation.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(HostLeaseError::Unavailable(io::Error::other(
                "another offline profile is active",
            )));
        }
        let mut lease = Self {
            _lock: file,
            receipt: directory.join("krypper-offline-cargo.receipt"),
        };
        lease.recover_previous()?;
        Ok(lease)
    }

    fn recover_previous(&mut self) -> Result<(), HostLeaseError> {
        let uid = effective_uid();
        let mut previous = String::new();
        let receipt = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.receipt)
        {
            Ok(mut receipt) => {
                let metadata = receipt
                    .metadata()
                    .map_err(HostLeaseError::CleanupUnconfirmed)?;
                if !metadata.is_file()
                    || metadata.uid() != uid
                    || metadata.nlink() != 1
                    || metadata.len() > 4096
                {
                    return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                        "untrusted cleanup receipt",
                    )));
                }
                (&mut receipt)
                    .take(4097)
                    .read_to_string(&mut previous)
                    .map_err(HostLeaseError::CleanupUnconfirmed)?;
                receipt
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(HostLeaseError::CleanupUnconfirmed(error)),
        };
        if previous.is_empty() {
            return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                "incomplete cleanup receipt",
            )));
        }

        let mut lines = previous.lines();
        let unit = lines.next().unwrap_or("");
        let id = unit
            .strip_prefix("krypper-test-")
            .and_then(|s| s.strip_suffix(".service"));
        if id.and_then(|s| Uuid::parse_str(s).ok()).is_none() {
            return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                "unconfirmed execution lease",
            )));
        }
        let group = lines.next();
        if lines.next().is_some() {
            return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                "invalid cleanup receipt",
            )));
        }

        let state = bounded_host(host_command("/usr/bin/systemctl").args([
            "--user",
            "show",
            unit,
            "--property=LoadState,ActiveState",
        ]))
        .map_err(HostLeaseError::CleanupUnconfirmed)?;
        parse_previous_unit_state(&state).map_err(HostLeaseError::CleanupUnconfirmed)?;
        if let Some(group) = group {
            let group = Path::new(group);
            if !group.starts_with("/sys/fs/cgroup")
                || group.file_name().and_then(|s| s.to_str()) != Some(unit)
                || group
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                    "untrusted cleanup identity",
                )));
            }
            if group.exists()
                && !fs::read_to_string(group.join("cgroup.events"))
                    .map_err(HostLeaseError::CleanupUnconfirmed)?
                    .lines()
                    .any(|l| l == "populated 0")
            {
                return Err(HostLeaseError::CleanupUnconfirmed(io::Error::other(
                    "previous cgroup is not empty",
                )));
            }
        }
        drop(receipt);
        self.clear_receipt()
            .map_err(HostLeaseError::CleanupUnconfirmed)
    }
    pub(super) fn record(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Err(io::Error::other("cleanup receipt cannot be empty"));
        }
        let directory = receipt_parent(&self.receipt)?;
        let temporary = directory.join(format!(".krypper-cargo-{}", Uuid::new_v4()));
        let update = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.receipt)
        })();
        if update.is_err() {
            let _ = fs::remove_file(temporary);
        }
        update?;
        File::open(directory)?.sync_all()
    }

    pub(super) fn clear_receipt(&mut self) -> io::Result<()> {
        let directory = receipt_parent(&self.receipt)?;
        match fs::remove_file(&self.receipt) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        File::open(directory)?.sync_all()
    }
}

fn receipt_parent(receipt: &Path) -> io::Result<&Path> {
    receipt
        .parent()
        .ok_or_else(|| io::Error::other("invalid cleanup receipt path"))
}

fn parse_previous_unit_state(output: &str) -> io::Result<PreviousUnitState> {
    let mut load_state = None;
    let mut active_state = None;
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Err(io::Error::other("invalid previous systemd unit state"));
        };
        match key {
            "LoadState" if load_state.replace(value).is_none() => {}
            "ActiveState" if active_state.replace(value).is_none() => {}
            _ => return Err(io::Error::other("invalid previous systemd unit state")),
        }
    }

    match (load_state, active_state) {
        (Some("not-found"), Some("inactive")) => Ok(PreviousUnitState::NotLoaded),
        (Some("loaded"), Some("inactive" | "failed")) => Ok(PreviousUnitState::Stopped),
        _ => Err(io::Error::other("previous process cleanup is unconfirmed")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActiveLockError {
    Busy,
    Poisoned,
}

fn try_lock(lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>, ActiveLockError> {
    match lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err(ActiveLockError::Busy),
        Err(TryLockError::Poisoned(_)) => Err(ActiveLockError::Poisoned),
    }
}

pub(super) fn try_active_lock() -> Result<MutexGuard<'static, ()>, ActiveLockError> {
    try_lock(&ACTIVE)
}

pub(super) fn is_cleanup_uncertain() -> bool {
    UNCERTAIN_CLEANUP.load(Ordering::SeqCst)
}

pub(super) fn mark_cleanup_uncertain() {
    UNCERTAIN_CLEANUP.store(true, Ordering::SeqCst);
}

pub(super) fn hold_for_recovery(result: &mut SandboxRunResult, diagnostic: impl Into<String>) {
    result.outcome = SandboxOutcome::OutcomeUnknown;
    result.cleanup_confirmed = false;
    result.recovery_hold = true;
    result.diagnostic = Some(diagnostic.into());
}

#[cfg(test)]
mod path_invariant_tests {
    use super::{receipt_parent, HostLease};
    use std::path::{Path, PathBuf};

    #[test]
    fn cleanup_receipt_requires_a_parent_directory() {
        assert!(receipt_parent(Path::new("krypper.receipt")).is_ok());
        assert!(receipt_parent(Path::new("")).is_err());
    }

    #[test]
    fn clearing_rejects_a_receipt_without_a_parent_instead_of_panicking() {
        let mut lease = HostLease {
            _lock: std::fs::File::open("/dev/null").expect("open inert lock fixture"),
            receipt: PathBuf::new(),
        };

        assert!(lease.clear_receipt().is_err());
    }

    #[test]
    fn recording_rejects_empty_receipt_content() {
        let mut lease = HostLease {
            _lock: std::fs::File::open("/dev/null").expect("open inert lock fixture"),
            receipt: PathBuf::new(),
        };

        assert!(lease.record("").is_err());
    }
}

#[cfg(test)]
mod recovery_state_tests {
    use super::{parse_previous_unit_state, try_lock, ActiveLockError, PreviousUnitState};
    use std::sync::Mutex;

    #[test]
    fn previous_unit_state_distinguishes_not_loaded_from_unknown() {
        assert_eq!(
            parse_previous_unit_state("LoadState=not-found\nActiveState=inactive\n").unwrap(),
            PreviousUnitState::NotLoaded
        );
        assert_eq!(
            parse_previous_unit_state("LoadState=loaded\nActiveState=inactive\n").unwrap(),
            PreviousUnitState::Stopped
        );
        assert_eq!(
            parse_previous_unit_state("LoadState=loaded\nActiveState=failed\n").unwrap(),
            PreviousUnitState::Stopped
        );

        for unknown in [
            "LoadState=error\nActiveState=inactive\n",
            "LoadState=not-found\nActiveState=active\n",
            "LoadState=loaded\nActiveState=active\n",
            "LoadState=loaded\nActiveState=activating\n",
            "ActiveState=inactive\n",
            "LoadState=loaded\nActiveState=inactive\nExtra=value\n",
        ] {
            assert!(
                parse_previous_unit_state(unknown).is_err(),
                "unrecognized state must retain the recovery hold: {unknown:?}"
            );
        }
    }

    #[test]
    fn active_lock_reports_busy_and_poisoned_as_different_states() {
        let busy = Mutex::new(());
        let _guard = busy.lock().unwrap();
        assert!(matches!(try_lock(&busy), Err(ActiveLockError::Busy)));

        let poisoned = Mutex::new(());
        std::thread::scope(|scope| {
            let join = scope.spawn(|| {
                let _guard = poisoned.lock().unwrap();
                panic!("simulate a supervisor panic while holding the lock");
            });
            assert!(join.join().is_err());
        });
        assert!(matches!(
            try_lock(&poisoned),
            Err(ActiveLockError::Poisoned)
        ));
    }
}

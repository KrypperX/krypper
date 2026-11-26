//! TaskSession-owned grants and canonical binding for the fixed offline Cargo operation.

use crate::tools::process::{OfflineCargoPolicy, ProcessPolicyFacts};
use crate::tools::FileIdentity;
use krypper_sandbox::{
    NetworkRequirement, PinnedResourceAccess, PinnedResourceId, ProcessTreeRequirement, RunLimits,
    SandboxExecutable, SyscallRequirement,
};
use ring::digest::{digest, SHA256};
use std::ffi::OsStr;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub(super) const MAX_SCOPED_GRANT_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const PROCESS_INTENT_VERSION: u32 = 1;
const PROCESS_INTENT_DOMAIN: &[u8] = b"Krypper.ProcessIntent\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GrantMode {
    Standing,
    OneShot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GrantScope {
    Session,
    ExactFilter,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GrantExpiry {
    deadline: Instant,
    unix_seconds: u64,
    unix_nanos: u32,
}

impl GrantExpiry {
    pub(super) fn from_lifetime(lifetime: Duration) -> Result<Self, &'static str> {
        if lifetime.is_zero() || lifetime > MAX_SCOPED_GRANT_LIFETIME {
            return Err("grant expiry must be greater than zero and at most 24 hours");
        }
        let now = Instant::now();
        let deadline = now
            .checked_add(lifetime)
            .ok_or("grant expiry is out of range")?;
        let wall_expiry = SystemTime::now()
            .checked_add(lifetime)
            .ok_or("grant expiry is out of range")?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "grant expiry is out of range")?;
        Ok(Self {
            deadline,
            unix_seconds: wall_expiry.as_secs(),
            unix_nanos: wall_expiry.subsec_nanos(),
        })
    }

    pub(super) fn is_expired(self) -> bool {
        Instant::now() >= self.deadline
    }
}

#[derive(Clone, Debug)]
pub(super) struct ProcessGrant {
    pub(super) task_id: Uuid,
    pub(super) grant_id: Uuid,
    pub(super) epoch: u64,
    pub(super) mode: GrantMode,
    pub(super) scope: GrantScope,
    pub(super) exact_filter: Option<String>,
    pub(super) expiry: Option<GrantExpiry>,
    pub(super) policy: Arc<OfflineCargoPolicy>,
    pub(super) consumed: bool,
    pub(super) approved_digest: Option<[u8; 32]>,
}

impl ProcessGrant {
    pub(super) fn allows_filter(&self, filter: Option<&str>) -> bool {
        match self.scope {
            GrantScope::Session => true,
            GrantScope::ExactFilter => self.exact_filter.as_deref() == filter,
        }
    }

    pub(super) fn expired(&self) -> bool {
        self.expiry.is_some_and(GrantExpiry::is_expired)
    }
}

#[derive(Debug, Default)]
pub(super) struct ProcessGrantState {
    pub(super) epoch: u64,
    pub(super) current: Option<ProcessGrant>,
}

impl ProcessGrantState {
    pub(super) fn next_epoch(&mut self) -> Result<u64, &'static str> {
        let next = self
            .epoch
            .checked_add(1)
            .ok_or("process grant epoch exhausted")?;
        self.epoch = next;
        Ok(next)
    }

    pub(super) fn candidate_epoch(&mut self) -> Result<u64, &'static str> {
        let Some(next) = self.epoch.checked_add(1) else {
            if let Some(previous) = self.current.take() {
                previous
                    .policy
                    .cancellation()
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            return Err("process grant epoch exhausted");
        };
        Ok(next)
    }

    pub(super) fn replace_at_epoch(
        &mut self,
        next_epoch: u64,
        grant: ProcessGrant,
    ) -> Result<(), &'static str> {
        if self.epoch.checked_add(1) != Some(next_epoch) || grant.epoch != next_epoch {
            return Err("process grant changed while authorizing");
        }
        if let Some(previous) = self.current.replace(grant) {
            previous
                .policy
                .cancellation()
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.epoch = next_epoch;
        Ok(())
    }
}

pub(super) enum ProcessStartError<E> {
    Denied,
    Spawn(E),
}

/// Runs the effect while the caller holds the TaskSession grant-state mutex.
pub(super) fn start_if_authorized<T, E>(
    state: &mut ProcessGrantState,
    expected: &ProcessGrant,
    expected_digest: [u8; 32],
    current_digest: Option<[u8; 32]>,
    spawn: impl FnOnce() -> Result<T, E>,
) -> Result<T, ProcessStartError<E>> {
    let Some(current) = state.current.as_ref() else {
        return Err(ProcessStartError::Denied);
    };
    if state.epoch != expected.epoch
        || current.task_id != expected.task_id
        || current.grant_id != expected.grant_id
        || current.epoch != expected.epoch
        || current.mode != expected.mode
        || current.scope != expected.scope
        || current.exact_filter != expected.exact_filter
        || current.expiry != expected.expiry
        || current.approved_digest != expected.approved_digest
        || current.consumed
        || current.expired()
        || !Arc::ptr_eq(&current.policy, &expected.policy)
        || current_digest != Some(expected_digest)
        || (current.mode == GrantMode::OneShot && current.approved_digest != Some(expected_digest))
        || current
            .policy
            .cancellation()
            .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err(ProcessStartError::Denied);
    }
    if current.mode == GrantMode::OneShot {
        let Some(current) = state.current.as_mut() else {
            return Err(ProcessStartError::Denied);
        };
        current.consumed = true;
    }
    spawn().map_err(ProcessStartError::Spawn)
}

#[derive(Clone, Debug)]
pub(super) struct ProcessIntent {
    pub(super) version: u32,
    pub(super) operation: String,
    pub(super) task_id: Uuid,
    pub(super) session_id: Uuid,
    pub(super) grant_id: Uuid,
    pub(super) grant_epoch: u64,
    pub(super) mode: GrantMode,
    pub(super) scope: GrantScope,
    pub(super) filter: Option<String>,
    pub(super) expiry: Option<(u64, u32)>,
    pub(super) workspace_identity: FileIdentity,
    pub(super) workspace_revision: u64,
    pub(super) facts: ProcessPolicyFacts,
}

impl ProcessIntent {
    pub(super) fn for_grant(
        grant: &ProcessGrant,
        session_id: Uuid,
        filter: Option<&str>,
        workspace_identity: FileIdentity,
        workspace_revision: u64,
        facts: ProcessPolicyFacts,
    ) -> Self {
        Self {
            version: PROCESS_INTENT_VERSION,
            operation: "offline_cargo_test".to_owned(),
            task_id: grant.task_id,
            session_id,
            grant_id: grant.grant_id,
            grant_epoch: grant.epoch,
            mode: grant.mode,
            scope: grant.scope,
            filter: filter.map(str::to_owned),
            expiry: grant
                .expiry
                .map(|expiry| (expiry.unix_seconds, expiry.unix_nanos)),
            workspace_identity,
            workspace_revision,
            facts,
        }
    }

    pub(super) fn digest(&self) -> Result<[u8; 32], &'static str> {
        if self.version != PROCESS_INTENT_VERSION {
            return Err("unsupported process intent version");
        }
        let requirements = &self.facts.requirements;
        if self.operation != "offline_cargo_test"
            || self.facts.profile_id != "offline_cargo_test"
            || self.facts.executable != "/toolchain/bin/cargo"
            || self.facts.profile_version != requirements.version()
            || self.facts.arguments != requirements.arguments()
            || self.facts.working_directory != requirements.working_directory()
            || self.facts.environment_names != requirements.environment_names()
            || self
                .facts
                .environment
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .ne(krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT
                    .iter()
                    .copied())
            || self.facts.profile_limits != requirements.profile_limits()
            || requirements
                != &krypper_sandbox::ValidatedExecutionRequirements::offline_cargo_test()
            || self
                .facts
                .effective_limits
                .validate_against(self.facts.profile_limits)
                .is_err()
        {
            return Err("process intent is outside the fixed offline Cargo contract");
        }
        let mut encoder = CanonicalEncoder::new(PROCESS_INTENT_DOMAIN);
        encoder.u32(1, self.version);
        encoder.string(2, &self.operation);
        encoder.bytes(3, self.task_id.as_bytes());
        encoder.bytes(4, self.session_id.as_bytes());
        encoder.bytes(5, self.grant_id.as_bytes());
        encoder.u64(6, self.grant_epoch);
        encoder.u8(7, mode_tag(self.mode));
        encoder.u8(8, scope_tag(self.scope));
        encoder.option_string(9, self.filter.as_deref());
        encoder.option_expiry(10, self.expiry);

        encoder.u32(11, self.facts.profile_version);
        encoder.string(12, &self.facts.profile_id);
        encoder.string(13, &self.facts.executable);
        encoder.string_sequence(14, &self.facts.arguments)?;
        encoder.string(15, &self.facts.working_directory);
        encoder.string_sequence(16, &self.facts.environment_names)?;
        encoder.profile_limits(17, self.facts.profile_limits)?;
        encoder.run_limits(18, self.facts.effective_limits)?;

        let requirements = &self.facts.requirements;
        encoder.u32(19, requirements.version());
        encoder.string(20, requirements.profile_id());
        encoder.u8(21, executable_tag(requirements.executable()));
        encoder.string_sequence(22, requirements.arguments())?;
        encoder.string(23, requirements.working_directory());
        encoder.resources(24, requirements.resources())?;
        encoder.string_sequence(25, requirements.environment_names())?;
        encoder.profile_limits(26, requirements.profile_limits())?;
        encoder.u8(27, network_tag(requirements.network()));
        encoder.u8(28, syscall_tag(requirements.syscall()));
        encoder.u8(29, process_tree_tag(requirements.process_tree()));

        encoder.identity(30, Some(self.workspace_identity));
        encoder.identity(31, self.facts.toolchain_identity);
        encoder.identity(32, self.facts.registry_identity);
        encoder.u64(33, self.workspace_revision);
        encoder.string_pair_sequence(34, &self.facts.environment)?;
        let mut effective_argv = self.facts.arguments.clone();
        if let Some(filter) = &self.filter {
            effective_argv.push("--".to_owned());
            effective_argv.push(filter.clone());
        }
        encoder.string_sequence(35, &effective_argv)?;
        let digest = digest(&SHA256, &encoder.0);
        digest
            .as_ref()
            .try_into()
            .map_err(|_| "SHA-256 returned an invalid digest length")
    }
}

/// Confirm the fixed payload tail and the payload cwd/environment encoded in the Sandbox command.
/// The helper's other arguments remain owned by the closed Sandbox implementation.
pub(super) fn launch_command_matches_intent(command: &Command, intent: &ProcessIntent) -> bool {
    if command.get_program() != OsStr::new("/usr/bin/bwrap") || command.get_current_dir().is_some()
    {
        return false;
    }
    let args = command.get_args().collect::<Vec<_>>();
    let count_arg = |value: &OsStr| args.iter().filter(|arg| *arg == &value).count();
    if count_arg(OsStr::new("--clearenv")) != 1
        || count_arg(OsStr::new("--")) != 1 + usize::from(intent.filter.is_some())
        || args
            .windows(2)
            .filter(|pair| pair[0] == OsStr::new("--chdir"))
            .count()
            != 1
        || !args
            .windows(2)
            .any(|pair| pair[0] == OsStr::new("--chdir") && pair[1] == OsStr::new("/workspace"))
    {
        return false;
    }
    let actual_environment = args
        .windows(3)
        .filter(|triple| triple[0] == OsStr::new("--setenv"))
        .map(|triple| (triple[1], triple[2]))
        .collect::<Vec<_>>();
    if actual_environment.len() != krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT.len()
        || actual_environment
            .iter()
            .zip(krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT)
            .any(|((name, value), (expected_name, expected_value))| {
                *name != OsStr::new(expected_name) || *value != OsStr::new(expected_value)
            })
    {
        return false;
    }

    let mut expected_tail = vec!["--".to_owned(), intent.facts.executable.clone()];
    expected_tail.extend(intent.facts.arguments.iter().cloned());
    if let Some(filter) = &intent.filter {
        expected_tail.extend(["--".to_owned(), filter.clone()]);
    }
    args.len() >= expected_tail.len()
        && args[args.len() - expected_tail.len()..]
            .iter()
            .zip(expected_tail.iter())
            .all(|(actual, expected)| *actual == OsStr::new(expected))
}

fn mode_tag(mode: GrantMode) -> u8 {
    match mode {
        GrantMode::Standing => 1,
        GrantMode::OneShot => 2,
    }
}

fn scope_tag(scope: GrantScope) -> u8 {
    match scope {
        GrantScope::Session => 1,
        GrantScope::ExactFilter => 2,
    }
}

fn executable_tag(executable: SandboxExecutable) -> u8 {
    match executable {
        SandboxExecutable::Cargo => 1,
        SandboxExecutable::Rustc => 2,
    }
}

fn network_tag(network: NetworkRequirement) -> u8 {
    match network {
        NetworkRequirement::DenyAll => 1,
    }
}

fn syscall_tag(syscall: SyscallRequirement) -> u8 {
    match syscall {
        SyscallRequirement::OfflineCargoTestV1 => 1,
    }
}

fn process_tree_tag(tree: ProcessTreeRequirement) -> u8 {
    match tree {
        ProcessTreeRequirement::EphemeralTree => 1,
    }
}

#[derive(Default)]
struct CanonicalEncoder(Vec<u8>);

impl CanonicalEncoder {
    fn new(domain: &[u8]) -> Self {
        Self(domain.to_vec())
    }

    fn field(&mut self, tag: u16, bytes: &[u8]) {
        self.0.extend_from_slice(&tag.to_be_bytes());
        self.0
            .extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        self.0.extend_from_slice(bytes);
    }

    fn u8(&mut self, tag: u16, value: u8) {
        self.field(tag, &[value]);
    }

    fn bytes(&mut self, tag: u16, value: &[u8]) {
        self.field(tag, value);
    }

    fn u32(&mut self, tag: u16, value: u32) {
        self.field(tag, &value.to_be_bytes());
    }

    fn u64(&mut self, tag: u16, value: u64) {
        self.field(tag, &value.to_be_bytes());
    }

    fn string(&mut self, tag: u16, value: &str) {
        self.field(tag, value.as_bytes());
    }

    fn option_string(&mut self, tag: u16, value: Option<&str>) {
        match value {
            Some(value) => {
                let mut encoded = vec![1];
                encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
                encoded.extend_from_slice(value.as_bytes());
                self.field(tag, &encoded);
            }
            None => self.field(tag, &[0]),
        }
    }

    fn option_expiry(&mut self, tag: u16, value: Option<(u64, u32)>) {
        match value {
            Some((seconds, nanos)) => {
                let mut encoded = vec![1];
                encoded.extend_from_slice(&seconds.to_be_bytes());
                encoded.extend_from_slice(&nanos.to_be_bytes());
                self.field(tag, &encoded);
            }
            None => self.field(tag, &[0]),
        }
    }

    fn string_sequence(&mut self, tag: u16, values: &[String]) -> Result<(), &'static str> {
        let count = u32::try_from(values.len()).map_err(|_| "too many policy values")?;
        let mut encoded = count.to_be_bytes().to_vec();
        for value in values {
            let length = u64::try_from(value.len()).map_err(|_| "policy value is too large")?;
            encoded.extend_from_slice(&length.to_be_bytes());
            encoded.extend_from_slice(value.as_bytes());
        }
        self.field(tag, &encoded);
        Ok(())
    }

    fn string_pair_sequence(
        &mut self,
        tag: u16,
        values: &[(String, String)],
    ) -> Result<(), &'static str> {
        let count = u32::try_from(values.len()).map_err(|_| "too many environment values")?;
        let mut encoded = count.to_be_bytes().to_vec();
        for (name, value) in values {
            let name_length =
                u64::try_from(name.len()).map_err(|_| "environment name is too large")?;
            let value_length =
                u64::try_from(value.len()).map_err(|_| "environment value is too large")?;
            encoded.extend_from_slice(&name_length.to_be_bytes());
            encoded.extend_from_slice(name.as_bytes());
            encoded.extend_from_slice(&value_length.to_be_bytes());
            encoded.extend_from_slice(value.as_bytes());
        }
        self.field(tag, &encoded);
        Ok(())
    }

    fn profile_limits(
        &mut self,
        tag: u16,
        limits: krypper_sandbox::ProfileLimits,
    ) -> Result<(), &'static str> {
        let output = u64::try_from(limits.output_bytes).map_err(|_| "output limit is too large")?;
        let mut encoded = Vec::with_capacity(40);
        encoded.extend_from_slice(&limits.wall_secs.to_be_bytes());
        encoded.extend_from_slice(&limits.cpu_ms.to_be_bytes());
        encoded.extend_from_slice(&limits.memory_bytes.to_be_bytes());
        encoded.extend_from_slice(&output.to_be_bytes());
        encoded.extend_from_slice(&limits.processes.to_be_bytes());
        self.field(tag, &encoded);
        Ok(())
    }

    fn run_limits(&mut self, tag: u16, limits: RunLimits) -> Result<(), &'static str> {
        let output = u64::try_from(limits.output_bytes).map_err(|_| "output limit is too large")?;
        let mut encoded = Vec::with_capacity(40);
        encoded.extend_from_slice(&limits.wall_millis.to_be_bytes());
        encoded.extend_from_slice(&limits.cpu_budget_usec.to_be_bytes());
        encoded.extend_from_slice(&limits.memory_bytes.to_be_bytes());
        encoded.extend_from_slice(&output.to_be_bytes());
        encoded.extend_from_slice(&limits.processes.to_be_bytes());
        self.field(tag, &encoded);
        Ok(())
    }

    fn resources(
        &mut self,
        tag: u16,
        resources: &[krypper_sandbox::PinnedResourceRequirement],
    ) -> Result<(), &'static str> {
        let count = u32::try_from(resources.len()).map_err(|_| "too many policy resources")?;
        let mut encoded = count.to_be_bytes().to_vec();
        for resource in resources {
            encoded.push(match resource.id {
                PinnedResourceId::Workspace => 1,
                PinnedResourceId::Toolchain => 2,
                PinnedResourceId::CargoRegistry => 3,
            });
            encoded.push(match resource.access {
                PinnedResourceAccess::ReadOnly => 1,
                PinnedResourceAccess::ReadWrite => 2,
            });
        }
        self.field(tag, &encoded);
        Ok(())
    }

    fn identity(&mut self, tag: u16, identity: Option<FileIdentity>) {
        match identity {
            Some(identity) => {
                let mut encoded = vec![1];
                encoded.extend_from_slice(&identity.volume.to_be_bytes());
                encoded.extend_from_slice(&identity.file.to_be_bytes());
                self.field(tag, &encoded);
            }
            None => self.field(tag, &[0]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        start_if_authorized, GrantExpiry, GrantMode, GrantScope, ProcessGrant, ProcessGrantState,
        ProcessIntent, ProcessStartError, MAX_SCOPED_GRANT_LIFETIME, PROCESS_INTENT_VERSION,
    };
    use crate::test_support::TempDir;
    use crate::tools::process::{OfflineCargoPolicy, ProcessPolicyFacts};
    use crate::tools::FileIdentity;
    use krypper_sandbox::ValidatedExecutionRequirements;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn intent() -> ProcessIntent {
        let requirements = ValidatedExecutionRequirements::offline_cargo_test();
        let limits = requirements.profile_limits();
        ProcessIntent {
            version: PROCESS_INTENT_VERSION,
            operation: "offline_cargo_test".into(),
            task_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(2),
            grant_id: Uuid::from_u128(3),
            grant_epoch: 4,
            mode: GrantMode::Standing,
            scope: GrantScope::ExactFilter,
            filter: Some("focused_case".into()),
            expiry: Some((1_800_000_000, 123_456_789)),
            workspace_identity: FileIdentity {
                volume: 10,
                file: 11,
            },
            workspace_revision: 12,
            facts: ProcessPolicyFacts {
                profile_version: 2,
                profile_id: "offline_cargo_test".into(),
                executable: "/toolchain/bin/cargo".into(),
                arguments: requirements.arguments().to_vec(),
                working_directory: requirements.working_directory().into(),
                environment_names: requirements.environment_names().to_vec(),
                environment: krypper_sandbox::OFFLINE_CARGO_TEST_ENVIRONMENT
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                profile_limits: limits,
                effective_limits: limits.to_run_limits().unwrap(),
                requirements,
                toolchain_identity: Some(FileIdentity {
                    volume: 20,
                    file: 21,
                }),
                registry_identity: Some(FileIdentity {
                    volume: 30,
                    file: 31,
                }),
            },
        }
    }

    fn policy() -> Arc<OfflineCargoPolicy> {
        let directory = TempDir::with_prefix("process-grant-policy-");
        let toolchain = directory.path().join("toolchain");
        let registry = directory.path().join("registry");
        fs::create_dir_all(toolchain.join("bin")).unwrap();
        fs::create_dir_all(&registry).unwrap();
        fs::write(toolchain.join("bin/cargo"), b"\x7fELF").unwrap();
        fs::write(toolchain.join("bin/rustc"), b"\x7fELF").unwrap();
        Arc::new(OfflineCargoPolicy::new(&toolchain, &registry).unwrap())
    }

    fn grant(
        mode: GrantMode,
        scope: GrantScope,
        filter: Option<&str>,
        expiry: Option<GrantExpiry>,
    ) -> (ProcessGrant, [u8; 32]) {
        let mut grant = ProcessGrant {
            task_id: Uuid::from_u128(1),
            grant_id: Uuid::from_u128(2),
            epoch: 3,
            mode,
            scope,
            exact_filter: filter.map(str::to_owned),
            expiry,
            policy: policy(),
            consumed: false,
            approved_digest: None,
        };
        let intent = ProcessIntent::for_grant(
            &grant,
            Uuid::from_u128(4),
            filter,
            FileIdentity {
                volume: 10,
                file: 11,
            },
            12,
            grant.policy.authorization_facts().unwrap(),
        );
        let digest = intent.digest().unwrap();
        if mode == GrantMode::OneShot {
            grant.approved_digest = Some(digest);
        }
        (grant, digest)
    }

    #[test]
    fn canonical_intent_digest_is_stable_and_rejects_unknown_versions() {
        let original = intent();
        assert_eq!(
            original.digest().unwrap(),
            original.clone().digest().unwrap()
        );
        let mut unknown = original;
        unknown.version += 1;
        assert_eq!(unknown.digest(), Err("unsupported process intent version"));
    }

    #[test]
    fn canonical_digest_changes_when_any_authority_binding_changes() {
        let original = intent();
        let digest = original.digest().unwrap();
        let mut changed = original.clone();
        changed.filter = Some("other_case".into());
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.task_id = Uuid::from_u128(101);
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.session_id = Uuid::from_u128(102);
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.grant_id = Uuid::from_u128(103);
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.expiry = Some((1_800_000_001, 123_456_789));
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.workspace_revision += 1;
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.grant_epoch += 1;
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.facts.toolchain_identity = Some(FileIdentity {
            volume: 99,
            file: 21,
        });
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.workspace_identity.file += 1;
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.facts.registry_identity = Some(FileIdentity {
            volume: 30,
            file: 99,
        });
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.facts.effective_limits.wall_millis -= 1;
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.scope = GrantScope::Session;
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.mode = GrantMode::OneShot;
        assert_ne!(changed.digest().unwrap(), digest);
        let mut changed = original.clone();
        changed.facts.working_directory = "other".into();
        assert!(changed.digest().is_err());
        let mut changed = original.clone();
        changed.facts.executable = "/toolchain/bin/rustc".into();
        assert!(changed.digest().is_err());
        let mut changed = original.clone();
        changed.facts.arguments[0] = "check".into();
        assert!(changed.digest().is_err());
        let mut changed = original;
        changed.operation = "other".into();
        assert!(changed.digest().is_err());
    }

    #[test]
    fn scoped_and_expiring_grants_reject_other_filters_and_invalid_lifetimes() {
        let expiry = GrantExpiry::from_lifetime(Duration::from_secs(1)).unwrap();
        let (scoped_grant, _) = grant(
            GrantMode::Standing,
            GrantScope::ExactFilter,
            Some("selected"),
            Some(expiry),
        );
        assert!(scoped_grant.allows_filter(Some("selected")));
        assert!(!scoped_grant.allows_filter(Some("other")));
        assert!(!scoped_grant.allows_filter(None));
        assert!(GrantExpiry::from_lifetime(Duration::ZERO).is_err());
        assert!(
            GrantExpiry::from_lifetime(MAX_SCOPED_GRANT_LIFETIME + Duration::from_nanos(1))
                .is_err()
        );

        let expired = GrantExpiry {
            deadline: Instant::now() - Duration::from_secs(1),
            unix_seconds: 1,
            unix_nanos: 0,
        };
        let (grant, digest) = grant(
            GrantMode::Standing,
            GrantScope::ExactFilter,
            Some("selected"),
            Some(expired),
        );
        let mut state = ProcessGrantState {
            epoch: grant.epoch,
            current: Some(grant.clone()),
        };
        let result =
            start_if_authorized(&mut state, &grant, digest, Some(digest), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(ProcessStartError::Denied)));
    }

    #[test]
    fn stale_grant_candidate_cannot_replace_a_newer_epoch() {
        let mut state = ProcessGrantState::default();
        let first_epoch = state.candidate_epoch().unwrap();
        let stale_epoch = state.candidate_epoch().unwrap();
        let (mut first, _) = grant(GrantMode::Standing, GrantScope::Session, None, None);
        first.epoch = first_epoch;
        first.grant_id = Uuid::from_u128(10);
        let (mut stale, _) = grant(GrantMode::Standing, GrantScope::Session, None, None);
        stale.epoch = stale_epoch;
        stale.grant_id = Uuid::from_u128(11);

        state.replace_at_epoch(first_epoch, first).unwrap();
        assert_eq!(
            state.replace_at_epoch(stale_epoch, stale),
            Err("process grant changed while authorizing")
        );
        assert_eq!(state.epoch, first_epoch);
        assert_eq!(
            state.current.as_ref().unwrap().grant_id,
            Uuid::from_u128(10)
        );
    }

    #[test]
    fn one_shot_consumes_at_the_gate_even_when_spawn_fails_and_cannot_replay() {
        let (grant, digest) = grant(
            GrantMode::OneShot,
            GrantScope::ExactFilter,
            Some("selected"),
            Some(GrantExpiry::from_lifetime(Duration::from_secs(60)).unwrap()),
        );
        let mut state = ProcessGrantState {
            epoch: grant.epoch,
            current: Some(grant.clone()),
        };
        let failed_spawn = start_if_authorized(&mut state, &grant, digest, Some(digest), || {
            Err::<(), _>("spawn failed")
        });
        assert!(matches!(
            failed_spawn,
            Err(ProcessStartError::Spawn("spawn failed"))
        ));
        assert!(state.current.as_ref().unwrap().consumed);
        let replay =
            start_if_authorized(&mut state, &grant, digest, Some(digest), || Ok::<_, ()>(()));
        assert!(matches!(replay, Err(ProcessStartError::Denied)));
    }

    #[test]
    fn changed_revision_and_revocation_at_the_gate_deny_start() {
        let (grant, digest) = grant(
            GrantMode::OneShot,
            GrantScope::ExactFilter,
            Some("selected"),
            Some(GrantExpiry::from_lifetime(Duration::from_secs(60)).unwrap()),
        );
        let mut state = ProcessGrantState {
            epoch: grant.epoch,
            current: Some(grant.clone()),
        };
        let mut changed_intent = ProcessIntent::for_grant(
            &grant,
            Uuid::from_u128(4),
            Some("selected"),
            FileIdentity {
                volume: 10,
                file: 11,
            },
            12,
            grant.policy.authorization_facts().unwrap(),
        );
        assert_eq!(changed_intent.digest().unwrap(), digest);
        changed_intent.workspace_revision += 1;
        let changed_digest = changed_intent.digest().unwrap();
        let changed = start_if_authorized(&mut state, &grant, digest, Some(changed_digest), || {
            Ok::<_, ()>(())
        });
        assert!(matches!(changed, Err(ProcessStartError::Denied)));
        assert!(!state.current.as_ref().unwrap().consumed);

        state.current = None;
        state.epoch += 1;
        let revoked =
            start_if_authorized(&mut state, &grant, digest, Some(digest), || Ok::<_, ()>(()));
        assert!(matches!(revoked, Err(ProcessStartError::Denied)));
    }

    #[test]
    fn concurrent_one_shot_claims_start_at_most_once() {
        let (grant, digest) = grant(
            GrantMode::OneShot,
            GrantScope::ExactFilter,
            Some("selected"),
            Some(GrantExpiry::from_lifetime(Duration::from_secs(60)).unwrap()),
        );
        let state = Arc::new(Mutex::new(ProcessGrantState {
            epoch: grant.epoch,
            current: Some(grant.clone()),
        }));
        let launches = Arc::new(AtomicUsize::new(0));
        let entered_spawn = Arc::new(Barrier::new(2));
        let release_spawn = Arc::new(Barrier::new(2));
        let first = {
            let state = Arc::clone(&state);
            let launches = Arc::clone(&launches);
            let entered_spawn = Arc::clone(&entered_spawn);
            let release_spawn = Arc::clone(&release_spawn);
            let grant = grant.clone();
            thread::spawn(move || {
                let mut state = state.lock().unwrap();
                start_if_authorized(&mut state, &grant, digest, Some(digest), || {
                    entered_spawn.wait();
                    release_spawn.wait();
                    launches.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(())
                })
            })
        };
        entered_spawn.wait();
        let (attempting, attempted) = mpsc::channel();
        let second = {
            let state = Arc::clone(&state);
            let launches = Arc::clone(&launches);
            let grant = grant.clone();
            thread::spawn(move || {
                attempting.send(()).unwrap();
                let mut state = state.lock().unwrap();
                start_if_authorized(&mut state, &grant, digest, Some(digest), || {
                    launches.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(())
                })
            })
        };
        attempted.recv().unwrap();
        release_spawn.wait();
        assert!(first.join().unwrap().is_ok());
        assert!(matches!(
            second.join().unwrap(),
            Err(ProcessStartError::Denied)
        ));
        assert_eq!(launches.load(Ordering::SeqCst), 1);
    }
}

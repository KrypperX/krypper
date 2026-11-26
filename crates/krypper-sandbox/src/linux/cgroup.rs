//! Linux cgroup resource controls and supervisor lifecycle.

use super::process::bounded_host;
use super::recovery::mark_cleanup_uncertain;
use crate::host::host_command;
use crate::{RunLimits, SandboxControl, SandboxIsolationEvidence, SandboxResource};
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(super) fn record_cgroup_limit_events(group: &Path, events: &mut Vec<SandboxResource>) {
    for (name, field, resource) in [
        ("memory.events", "max", SandboxResource::Memory),
        ("pids.events", "max", SandboxResource::Processes),
    ] {
        if fs::read_to_string(group.join(name)).is_ok_and(|contents| {
            contents.lines().any(|line| {
                line.split_once(' ').is_some_and(|(key, value)| {
                    key == field && value.parse::<u64>().is_ok_and(|count| count > 0)
                })
            })
        }) {
            record_event(events, resource);
        }
    }
}

pub(super) fn cpu_usage_usec(group: &Path) -> io::Result<u64> {
    fs::read_to_string(group.join("cpu.stat"))?
        .lines()
        .find_map(|line| {
            line.strip_prefix("usage_usec ")
                .and_then(|value| value.parse::<u64>().ok())
        })
        .ok_or_else(|| io::Error::other("CPU accounting unavailable"))
}

pub(super) struct Supervisor {
    unit: String,
    group: PathBuf,
    cleaned: bool,
    limits: RunLimits,
}
impl Supervisor {
    pub(super) fn create(
        id: Uuid,
        limits: RunLimits,
        evidence: &mut SandboxIsolationEvidence,
    ) -> io::Result<Self> {
        let unit = format!("krypper-test-{}.service", id.simple());
        let memory = format!("MemoryMax={}", limits.memory_bytes);
        let tasks = format!("TasksMax={}", limits.processes);
        let runtime_secs = limits.wall_millis.div_ceil(1_000).saturating_add(15);
        let runtime = format!("RuntimeMaxSec={runtime_secs}");
        let keepalive = runtime_secs.to_string();
        // Only a trusted inert keeper starts here. No workspace code runs until verification.
        let mut command = host_command("/usr/bin/systemd-run");
        append_supervisor_args(
            &mut command,
            &unit,
            &memory,
            &tasks,
            &runtime,
            &keepalive,
            evidence,
        );
        let creation = bounded_host(&mut command);
        let mut supervisor = Self {
            unit,
            group: PathBuf::new(),
            cleaned: false,
            limits,
        };
        creation?;
        let group = bounded_host(host_command("/usr/bin/systemctl").args([
            "--user",
            "show",
            &supervisor.unit,
            "--property=ControlGroup",
            "--value",
        ]))?;
        supervisor.group = control_group_path(Path::new(group.trim()))?;
        supervisor.verify()?;
        let membership = fs::read_to_string("/proc/thread-self/cgroup")?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| io::Error::other("cgroup v2 membership unavailable"))?;
        let current = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        let common = supervisor
            .group
            .ancestors()
            .find(|ancestor| current.starts_with(ancestor))
            .ok_or_else(|| io::Error::other("cgroup attachment ancestry unavailable"))?;
        OpenOptions::new()
            .write(true)
            .open(common.join("cgroup.procs"))
            .map_err(|_| {
                io::Error::other(
                    "caller must run inside the writable delegated user-manager cgroup subtree",
                )
            })?;
        Ok(supervisor)
    }
    pub(super) fn verify(&self) -> io::Result<()> {
        let read = |name| fs::read_to_string(self.group.join(name));
        let memory = read("memory.max")?.trim().parse::<u64>().ok();
        let tasks = read("pids.max")?.trim().parse::<u64>().ok();
        let cpu = read("cpu.max")?;
        let fields = cpu
            .split_whitespace()
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>();
        if memory != Some(self.limits.memory_bytes)
            || tasks != Some(self.limits.processes)
            || read("memory.swap.max")?.trim() != "0"
            || !matches!(fields, Ok(ref v) if v.len() == 2 && v[0] > 0 && v[1] > 0 && v[0] <= v[1])
        {
            return Err(io::Error::other(
                "required hard resource controls unavailable",
            ));
        }
        // Both attach and whole-tree kill must be delegated before launch.
        OpenOptions::new()
            .write(true)
            .open(self.group.join("cgroup.procs"))?;
        OpenOptions::new()
            .write(true)
            .open(self.group.join("cgroup.kill"))?;
        Ok(())
    }
    pub(super) fn clean(&mut self) -> bool {
        if self.cleaned {
            return true;
        }
        if !self.group.as_os_str().is_empty() && self.group.exists() {
            // The manager can still terminate the unit when the caller's cgroup mount is
            // read-only. Success still requires independent empty-tree readback below.
            let _ = fs::write(self.group.join("cgroup.kill"), b"1");
        }
        let _ =
            bounded_host(host_command("/usr/bin/systemctl").args(["--user", "stop", &self.unit]));
        let inactive = bounded_host(host_command("/usr/bin/systemctl").args([
            "--user",
            "show",
            &self.unit,
            "--property=ActiveState",
            "--value",
        ]))
        .is_ok_and(|s| matches!(s.trim(), "inactive" | "failed"));
        let start = Instant::now();
        while self.group.exists() && start.elapsed() < Duration::from_secs(3) {
            if fs::read_to_string(self.group.join("cgroup.events"))
                .is_ok_and(|s| s.lines().any(|l| l == "populated 0"))
            {
                self.cleaned = inactive;
                return self.cleaned;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // A removed cgroup cannot contain live processes. Also require manager acknowledgement.
        self.cleaned = inactive && !self.group.exists();
        self.cleaned
    }

    pub(super) fn unit(&self) -> &str {
        &self.unit
    }

    pub(super) fn group(&self) -> &Path {
        &self.group
    }

    #[cfg(test)]
    pub(super) fn test_fixture(group: PathBuf, limits: RunLimits) -> Self {
        Self {
            unit: "unused".into(),
            group,
            cleaned: true,
            limits,
        }
    }
}

fn append_supervisor_args(
    command: &mut std::process::Command,
    unit: &str,
    memory: &str,
    tasks: &str,
    runtime: &str,
    keepalive: &str,
    evidence: &mut SandboxIsolationEvidence,
) {
    command.args([
        "--user",
        "--quiet",
        "--collect",
        "--unit",
        unit,
        "-p",
        "Delegate=yes",
    ]);
    command.args(["-p", memory]);
    evidence.request(SandboxControl::MemoryLimit);
    command.args(["-p", "MemorySwapMax=0"]);
    evidence.request(SandboxControl::SwapLimit);
    command.args(["-p", tasks]);
    evidence.request(SandboxControl::ProcessLimit);
    command.args(["-p", "CPUQuota=100%"]);
    evidence.request(SandboxControl::CpuLimit);
    command.args([
        "-p",
        runtime,
        "-p",
        "KillMode=control-group",
        "-p",
        "TimeoutStopSec=1",
        "-p",
        "SendSIGKILL=yes",
        "--",
        "/usr/bin/sleep",
        keepalive,
    ]);
    evidence.request(SandboxControl::WallClockLimit);
    evidence.request(SandboxControl::CgroupCleanup);
}

fn control_group_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(io::Error::other("invalid cgroup identity"));
    }
    let relative = path
        .strip_prefix("/")
        .map_err(|_| io::Error::other("invalid cgroup identity"))?;
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

pub(super) fn record_event(events: &mut Vec<SandboxResource>, resource: SandboxResource) {
    if events.contains(&resource) {
        return;
    }
    events.push(resource);
    events.sort_by_key(|resource| match resource {
        SandboxResource::Processes => 0,
        SandboxResource::Memory => 1,
        SandboxResource::Cpu => 2,
        SandboxResource::FileSize => 3,
    });
}

#[cfg(test)]
mod path_invariant_tests {
    use super::control_group_path;
    use std::path::Path;

    #[test]
    fn control_group_paths_must_be_absolute_and_contained() {
        assert_eq!(
            control_group_path(Path::new("/user.slice/krypper-test.service"))
                .expect("validated absolute cgroup path"),
            Path::new("/sys/fs/cgroup/user.slice/krypper-test.service")
        );
        assert!(control_group_path(Path::new("user.slice/krypper-test.service")).is_err());
        assert!(control_group_path(Path::new("/user.slice/../other.service")).is_err());
    }
}

#[cfg(test)]
mod readback_tests {
    use super::{append_supervisor_args, Supervisor};
    use crate::{
        RunLimits, SandboxBackend, SandboxControl, SandboxEvidenceStatus, SandboxIsolationEvidence,
    };
    use std::mem::ManuallyDrop;
    use std::process::Command;
    use uuid::Uuid;

    #[test]
    fn cgroup_configuration_arguments_have_matching_requested_evidence() {
        let mut command = Command::new("systemd-run");
        let mut evidence = SandboxIsolationEvidence::new(Some(SandboxBackend::Bubblewrap));
        append_supervisor_args(
            &mut command,
            "krypper-test.fixture.service",
            "MemoryMax=1024",
            "TasksMax=4",
            "RuntimeMaxSec=30",
            "30",
            &mut evidence,
        );
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        for (control, argument) in [
            (SandboxControl::MemoryLimit, "MemoryMax=1024"),
            (SandboxControl::SwapLimit, "MemorySwapMax=0"),
            (SandboxControl::ProcessLimit, "TasksMax=4"),
            (SandboxControl::CpuLimit, "CPUQuota=100%"),
            (SandboxControl::WallClockLimit, "RuntimeMaxSec=30"),
            (SandboxControl::CgroupCleanup, "KillMode=control-group"),
        ] {
            assert!(args.iter().any(|value| value == argument));
            assert!(evidence.controls().iter().any(|record| {
                record.control == control && record.status == SandboxEvidenceStatus::Requested
            }));
        }
        assert!(!evidence
            .controls()
            .iter()
            .any(|record| record.status == SandboxEvidenceStatus::Observed));
    }

    #[test]
    fn cgroup_readback_rejects_missing_and_unlimited_controls() {
        let path = std::env::temp_dir().join(format!("krypper-sandbox-cgroup-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let limits = RunLimits::offline_cargo_test();
        let supervisor = ManuallyDrop::new(Supervisor {
            unit: "unused".into(),
            group: path.clone(),
            cleaned: true,
            limits,
        });
        assert!(supervisor.verify().is_err());
        for (name, value) in [
            ("memory.max", limits.memory_bytes.to_string()),
            ("memory.swap.max", "0".into()),
            ("pids.max", limits.processes.to_string()),
            ("cpu.max", "100000 100000".into()),
            ("cgroup.procs", "".into()),
            ("cgroup.kill", "".into()),
        ] {
            std::fs::write(path.join(name), value).unwrap();
        }
        assert!(supervisor.verify().is_ok());
        for name in ["memory.max", "pids.max", "cpu.max"] {
            let original = std::fs::read(path.join(name)).unwrap();
            std::fs::write(path.join(name), "max").unwrap();
            assert!(supervisor.verify().is_err(), "{name} must fail closed");
            std::fs::write(path.join(name), original).unwrap();
        }
        std::fs::remove_dir_all(path).unwrap();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if !self.clean() {
            mark_cleanup_uncertain();
        }
    }
}

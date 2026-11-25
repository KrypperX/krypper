// Shared by several test and example targets; each target uses a subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::Builder;

pub struct TempRoot {
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl TempRoot {
    pub fn new(label: &str) -> Result<Self, String> {
        let directory = Builder::new()
            .prefix(&format!("krypper-aqb-{label}-"))
            .tempdir()
            .map_err(|error| format!("create fixture root: {error}"))?;
        let path = directory.path().to_path_buf();
        Ok(Self {
            path,
            _directory: directory,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn run_command(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("run command: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| format!("run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| format!("run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

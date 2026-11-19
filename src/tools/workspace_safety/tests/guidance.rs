use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use super::super::read::capture_workspace_root_at;
use super::NEXT_TEMP;

fn guidance_workspace() -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "krypper-guidance-read-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let workspace = base.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    (base, workspace)
}

#[test]
fn root_agents_read_is_bounded_and_does_not_load_nested_files() {
    let (base, workspace) = guidance_workspace();
    fs::create_dir_all(workspace.join("nested")).unwrap();
    fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
    fs::write(workspace.join("nested/AGENTS.md"), "nested guidance").unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    assert_eq!(
        super::read_root_agents_md_at(&root).unwrap().as_deref(),
        Some("root guidance")
    );
    fs::remove_file(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(super::read_root_agents_md_at(&root).unwrap(), None);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn root_agents_read_rejects_invalid_utf8_and_oversized_files() {
    let (base, workspace) = guidance_workspace();
    let root = capture_workspace_root_at(&workspace).unwrap();

    fs::write(workspace.join("AGENTS.md"), [0xff]).unwrap();
    assert_eq!(
        super::read_root_agents_md_at(&root),
        Err(super::RootGuidanceReadError::InvalidUtf8)
    );

    fs::write(workspace.join("AGENTS.md"), vec![b'x'; 16 * 1024 + 1]).unwrap();
    assert_eq!(
        super::read_root_agents_md_at(&root),
        Err(super::RootGuidanceReadError::TooLarge)
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn root_agents_read_rejects_symlinks_without_reading_outside_canary() {
    use std::os::unix::fs::symlink;

    let (base, workspace) = guidance_workspace();
    let outside = base.join("outside.txt");
    fs::write(&outside, "outside canary").unwrap();
    symlink(&outside, workspace.join("AGENTS.md")).unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    assert_eq!(
        super::read_root_agents_md_at(&root),
        Err(super::RootGuidanceReadError::Symlink)
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn root_agents_read_reports_read_failures_and_changes_during_read() {
    use std::io::{self, Read};

    let (base, workspace) = guidance_workspace();
    let agents = workspace.join("AGENTS.md");
    fs::write(&agents, "stable guidance").unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    assert_eq!(
        super::read_root_agents_md_at_with_reader(&root, |_| {
            Err(io::Error::other("injected read failure"))
        }),
        Err(super::RootGuidanceReadError::ReadFailed)
    );
    assert_eq!(
        super::read_root_agents_md_at_with_reader(&root, |file| {
            fs::write(&agents, "changed guidance")?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }),
        Err(super::RootGuidanceReadError::ChangedDuringRead)
    );
    fs::remove_dir_all(base).unwrap();
}

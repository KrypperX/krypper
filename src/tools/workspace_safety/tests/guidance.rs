use super::super::read::capture_workspace_root_at;
use super::temp_dir;
use std::fs;
use std::path::PathBuf;

fn guidance_workspace() -> (crate::test_support::TempDir, PathBuf) {
    let base = temp_dir("krypper-guidance-read-");
    let workspace = base.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    (base, workspace)
}

#[test]
fn root_agents_read_is_bounded_and_does_not_load_nested_files() {
    let (_base, workspace) = guidance_workspace();
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
}

#[test]
fn root_agents_read_rejects_invalid_utf8_and_oversized_files() {
    let (_base, workspace) = guidance_workspace();
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
}

#[test]
fn root_agents_read_rejects_symlinks_without_reading_outside_canary() {
    use std::os::unix::fs::symlink;

    let (base, workspace) = guidance_workspace();
    let outside = base.path().join("outside.txt");
    fs::write(&outside, "outside canary").unwrap();
    symlink(&outside, workspace.join("AGENTS.md")).unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    assert_eq!(
        super::read_root_agents_md_at(&root),
        Err(super::RootGuidanceReadError::Symlink)
    );
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
}

#[test]
fn root_agents_read_reports_read_failures_and_changes_during_read() {
    use std::io::{self, Read};

    let (_base, workspace) = guidance_workspace();
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
}

#[test]
fn project_guidance_is_root_to_target_ordered_and_excludes_siblings() {
    let (_base, workspace) = guidance_workspace();
    fs::create_dir_all(workspace.join("src/deep")).unwrap();
    fs::create_dir_all(workspace.join("tests")).unwrap();
    fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
    fs::write(workspace.join("src/AGENTS.md"), "src guidance").unwrap();
    fs::write(workspace.join("src/deep/AGENTS.md"), "deep guidance").unwrap();
    fs::write(workspace.join("tests/AGENTS.md"), "sibling guidance").unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    let nested =
        super::read_project_guidance_at(&root, &[PathBuf::from("src/deep/generated.rs")]).unwrap();
    assert_eq!(
        nested.as_deref(),
        Some(
            "root guidance\n\n--- AGENTS.md: src/AGENTS.md ---\nsrc guidance\n\n--- AGENTS.md: src/deep/AGENTS.md ---\ndeep guidance"
        )
    );
    assert_eq!(
        super::read_project_guidance_at(&root, &[PathBuf::from("note.txt")]).unwrap(),
        Some("root guidance".to_owned())
    );
    fs::remove_file(workspace.join("AGENTS.md")).unwrap();
    assert_eq!(
        super::read_project_guidance_at(&root, &[PathBuf::from("note.txt")]).unwrap(),
        None
    );
}

#[test]
fn project_guidance_rejects_out_of_workspace_targets_in_a_mixed_plan() {
    let (base, workspace) = guidance_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
    let outside = base.path().join("outside.txt");
    fs::write(&outside, "outside canary").unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    let result = super::read_project_guidance_at(
        &root,
        &[
            PathBuf::from("src/generated.rs"),
            PathBuf::from("../outside.txt"),
        ],
    );

    assert_eq!(result, Err(super::RootGuidanceReadError::InvalidTargetPath));
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
}

#[test]
fn project_guidance_rejects_symlinked_edit_targets_without_reading_canaries() {
    use std::os::unix::fs::symlink;

    let (base, workspace) = guidance_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
    let outside_file = base.path().join("outside.txt");
    fs::write(&outside_file, "outside file canary").unwrap();
    symlink(&outside_file, workspace.join("src/linked.rs")).unwrap();
    let outside_directory = base.path().join("outside-directory");
    fs::create_dir(&outside_directory).unwrap();
    let outside_canary = outside_directory.join("canary.txt");
    fs::write(&outside_canary, "outside directory canary").unwrap();
    symlink(&outside_directory, workspace.join("linked-directory")).unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    let file_target = super::read_project_guidance_at(
        &root,
        &[
            PathBuf::from("src/generated.rs"),
            PathBuf::from("src/linked.rs"),
        ],
    );
    let parent_target =
        super::read_project_guidance_at(&root, &[PathBuf::from("linked-directory/generated.rs")]);

    assert_eq!(file_target, Err(super::RootGuidanceReadError::Symlink));
    assert_eq!(parent_target, Err(super::RootGuidanceReadError::Symlink));
    assert_eq!(
        fs::read_to_string(outside_file).unwrap(),
        "outside file canary"
    );
    assert_eq!(
        fs::read_to_string(outside_canary).unwrap(),
        "outside directory canary"
    );
}

#[test]
fn project_guidance_rejects_non_directory_edit_target_parents() {
    let (_base, workspace) = guidance_workspace();
    fs::write(workspace.join("AGENTS.md"), "root guidance").unwrap();
    fs::write(workspace.join("blocked"), "not a directory").unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    let result = super::read_project_guidance_at(&root, &[PathBuf::from("blocked/generated.rs")]);

    assert_eq!(result, Err(super::RootGuidanceReadError::InvalidTargetPath));
}

#[test]
fn nested_guidance_changed_during_a_safe_read_is_rejected() {
    use std::io::Read;

    let (base, workspace) = guidance_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    let agents = workspace.join("src/AGENTS.md");
    fs::write(&agents, "stable guidance").unwrap();
    let outside = base.path().join("outside.txt");
    fs::write(&outside, "outside canary").unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();

    let result = super::read_project_guidance_at_with_reader(
        &root,
        &[PathBuf::from("src/generated.rs")],
        |file| {
            fs::write(&agents, "changed guidance")?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        },
    );

    assert_eq!(result, Err(super::RootGuidanceReadError::ChangedDuringRead));
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
}

#[test]
fn nested_guidance_created_after_absence_checks_is_rejected() {
    let (base, workspace) = guidance_workspace();
    fs::create_dir_all(workspace.join("src")).unwrap();
    let outside = base.path().join("outside.txt");
    fs::write(&outside, "outside canary").unwrap();
    let root = capture_workspace_root_at(&workspace).unwrap();
    let mut injected = false;

    let result = super::read_project_guidance_at_with_reader_and_hook(
        &root,
        &[PathBuf::from("src/generated.rs")],
        |_| Ok(Vec::new()),
        |directory| {
            if directory == [std::ffi::OsString::from("src")] {
                fs::write(workspace.join("src/AGENTS.md"), "late nested guidance")?;
                injected = true;
            }
            Ok(())
        },
    );

    assert!(
        injected,
        "the guidance file must be created after the absent read"
    );
    assert_eq!(result, Err(super::RootGuidanceReadError::ChangedDuringRead));
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside canary");
}

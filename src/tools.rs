pub mod capability_policy;
pub(crate) mod extension_authorization;
pub mod extension_contract;
// The MCP transport is not wired into the CLI or harness yet; it is exercised by its tests.
#[allow(dead_code, unused_imports)]
pub mod mcp_transport;
pub(crate) mod permission_policy;
pub mod process;
pub mod process_profile;
pub(crate) mod registry;
// Skill loading is not wired into the CLI yet; it is exercised by its tests.
#[allow(dead_code, unused_imports)]
pub(crate) mod skills;
mod workspace_safety;

pub(crate) use workspace_safety::{
    apply_patch_file_at_with_revision, capture_workspace_root, capture_workspace_root_at,
    capture_workspace_root_at_with_error, create_directory_at, create_text_file,
    create_text_file_at, encode_workspace_path, list_directory, list_task_directory_names_at,
    preview_replace_text, preview_workspace_patch, read_project_guidance_at,
    read_root_agents_md_at, read_text_file, read_text_file_at, replace_text_file_at_with_revision,
    search_text_files_at, search_tree_text_files_at, text_revision, CliError, FileIdentity,
    PatchEvidence, RootGuidanceReadError, WorkspaceRoot, MAX_PATCH_BYTES,
};

#[cfg(test)]
mod tests {
    use super::workspace_safety::replace_text_file_at;
    use super::{
        capture_workspace_root_at, create_text_file_at, read_text_file_at, search_text_files_at,
    };
    use crate::test_support::TempDir;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn workspace_safety_operations_keep_changes_inside_the_captured_root() {
        let directory = TempDir::with_prefix("krypper-tools-workspace-safety-");
        let workspace = directory.path().join("workspace");
        let outside = directory.path().join("outside.txt");
        fs::create_dir(&workspace).unwrap();
        fs::write(&outside, "outside text").unwrap();
        fs::write(workspace.join("note.txt"), "before exact match after").unwrap();
        let root = capture_workspace_root_at(&workspace).unwrap();

        create_text_file_at(&root, OsStr::new("created.txt"), "created text").unwrap();
        assert_eq!(
            read_text_file_at(&root, OsStr::new("created.txt")).unwrap(),
            "created text"
        );
        replace_text_file_at(&root, OsStr::new("note.txt"), "exact match", "new text").unwrap();
        assert_eq!(
            read_text_file_at(&root, OsStr::new("note.txt")).unwrap(),
            "before new text after"
        );
        assert_eq!(
            search_text_files_at(&root, OsStr::new("."), "new text").unwrap(),
            [(PathBuf::from("note.txt"), 1)]
        );

        let error =
            create_text_file_at(&root, OsStr::new("../outside.txt"), "changed").unwrap_err();
        assert_eq!(
            error.message(),
            "path must stay inside the current directory"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside text");
    }
}

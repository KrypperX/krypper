pub mod capability_policy;
pub mod extension_contract;
pub mod mcp_transport;
pub mod process;
pub mod process_profile;
pub(crate) mod registry;
mod workspace_safety;

pub(crate) use workspace_safety::{
    apply_patch_file_at, capture_workspace_root, capture_workspace_root_at, create_directory_at,
    create_text_file, create_text_file_at, list_directory, list_task_directory_names_at,
    read_project_guidance_at, read_root_agents_md_at, read_text_file, read_text_file_at,
    replace_text_file_at, search_text_files_at, search_tree_text_files_at, PatchEvidence,
    RootGuidanceReadError, WorkspaceRoot, MAX_PATCH_BYTES,
};

#[cfg(test)]
mod tests {
    use super::{
        capture_workspace_root_at, create_text_file_at, read_text_file_at, replace_text_file_at,
        search_text_files_at,
    };
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn workspace_safety_operations_keep_changes_inside_the_captured_root() {
        let path = std::env::temp_dir().join(format!(
            "krypper-tools-workspace-safety-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = path.join("workspace");
        let outside = path.join("outside.txt");
        fs::create_dir(&path).unwrap();
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
        fs::remove_dir_all(path).unwrap();
    }
}

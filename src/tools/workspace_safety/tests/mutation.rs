use super::super::{
    apply_patch_file_at, create_directory_at, create_text_file_at, read_text_file_at,
    replace_text_file_at,
    types::{CliError, DirHandle, WorkspaceRoot},
};
use super::temp_dir;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

fn workspace_root(path: &Path) -> WorkspaceRoot {
    WorkspaceRoot {
        descriptor: DirHandle::from_file(fs::File::open(path).unwrap()),
        path: fs::canonicalize(path).unwrap(),
    }
}

#[test]
fn mutations_reject_git_and_krypper_metadata_but_preserve_reads_and_normal_paths() {
    let directory = temp_dir("krypper-mutation-policy-");
    let path = directory.path();
    fs::create_dir_all(path.join(".git/hooks")).unwrap();
    fs::create_dir_all(path.join(".krypper/skills")).unwrap();
    fs::create_dir_all(path.join(".gitignore")).unwrap();
    fs::write(path.join(".git/config"), "protected\n").unwrap();
    fs::write(path.join(".git/hooks/pre-commit"), "protected\n").unwrap();
    fs::write(path.join(".krypper/skills/example.json"), "protected\n").unwrap();
    symlink(".git", path.join("git-alias")).unwrap();
    symlink(".krypper", path.join("krypper-alias")).unwrap();
    let root = workspace_root(path);

    let protected_files = [
        ".git/config",
        ".git/hooks/pre-commit",
        ".krypper/skills/example.json",
    ];
    for protected in protected_files {
        let contents = fs::read_to_string(path.join(protected)).unwrap();
        let error = create_text_file_at(&root, OsStr::new(protected), "overwrite")
            .expect_err("metadata files cannot be created or replaced");
        assert!(matches!(&error, CliError::WorkspaceMetadataMutationDenied));
        assert_eq!(
            error.message(),
            "workspace metadata paths cannot be modified"
        );
        assert!(
            create_directory_at(&root, OsStr::new(protected)).is_err(),
            "metadata paths cannot become directories: {protected}"
        );
        assert!(
            replace_text_file_at(&root, OsStr::new(protected), "protected", "changed").is_err(),
            "metadata paths cannot be replaced: {protected}"
        );
        let patch =
            format!("--- a/{protected}\n+++ b/{protected}\n@@ -1 +1 @@\n-protected\n+changed\n");
        assert!(
            apply_patch_file_at(&root, OsStr::new(protected), &patch).is_err(),
            "metadata paths cannot be patched: {protected}"
        );
        assert_eq!(fs::read_to_string(path.join(protected)).unwrap(), contents);
    }

    for target in [
        ".git/new.txt",
        ".git/hooks/new.txt",
        ".krypper/skills/new.json",
    ] {
        assert!(create_text_file_at(&root, OsStr::new(target), "new").is_err());
        assert!(create_directory_at(&root, OsStr::new(target)).is_err());
    }

    for alias in ["git-alias/config", "git-alias/hooks/pre-commit"] {
        assert!(create_text_file_at(&root, OsStr::new(alias), "overwrite").is_err());
        assert!(create_directory_at(&root, OsStr::new(alias)).is_err());
        assert!(replace_text_file_at(&root, OsStr::new(alias), "protected", "changed").is_err());
        let patch = format!("--- a/{alias}\n+++ b/{alias}\n@@ -1 +1 @@\n-protected\n+changed\n");
        assert!(apply_patch_file_at(&root, OsStr::new(alias), &patch).is_err());
    }
    assert!(create_text_file_at(&root, OsStr::new("krypper-alias/new.json"), "new").is_err());
    assert!(create_text_file_at(&root, OsStr::new("git-alias/new.json"), "new").is_err());
    assert!(create_directory_at(&root, OsStr::new("krypper-alias/new-dir")).is_err());
    assert!(create_directory_at(&root, OsStr::new("git-alias/new-dir")).is_err());

    assert_eq!(
        read_text_file_at(&root, OsStr::new(".git/config")).unwrap(),
        "protected\n"
    );
    create_text_file_at(&root, OsStr::new(".gitignore/x.git"), "allowed").unwrap();
    assert_eq!(
        fs::read_to_string(PathBuf::from(&path).join(".gitignore/x.git")).unwrap(),
        "allowed"
    );
    create_text_file_at(&root, OsStr::new("normal.txt"), "allowed").unwrap();
}

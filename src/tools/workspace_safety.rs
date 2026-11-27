//! Workspace containment boundary for reads, writes, patches and searches.
//!
//! Each submodule owns one responsibility: POSIX syscalls, containment decisions,
//! path resolution, reading, listing, creation and atomic replacement. This file is
//! a facade so every existing `crate::tools::workspace_safety::*` path keeps working.
//! Model-controlled mutations reject `.git` and `.krypper` path components;
//! reads retain their existing containment and symlink policy.

mod create;
mod listing;
mod patch;
mod paths;
mod read;
mod replace;
mod search;
mod syscalls;
mod types;

#[cfg(all(test, unix))]
mod tests;

pub(crate) use create::{create_directory_at, create_text_file, create_text_file_at};
pub(crate) use listing::{list_directory, list_task_directory_names_at};
pub(crate) use patch::MAX_PATCH_BYTES;
#[cfg(target_os = "linux")]
pub(crate) use paths::open_workspace_directory_no_follow;
pub(crate) use read::{
    capture_workspace_root, capture_workspace_root_at, capture_workspace_root_at_with_error,
    read_project_guidance_at, read_root_agents_md_at, read_text_file, read_text_file_at,
};
#[cfg(test)]
pub(crate) use replace::{apply_patch_file_at, replace_text_file_at};
pub(crate) use replace::{
    apply_patch_file_at_with_revision, preview_replace_text, preview_workspace_patch,
    replace_text_file_at_with_revision, text_revision,
};
pub(crate) use search::{search_text_files_at, search_tree_text_files_at};
pub(crate) use syscalls::encode_workspace_path;
pub(crate) use types::{
    CliError, DirHandle, FileIdentity, PatchEvidence, RootGuidanceReadError, WorkspaceRoot,
};

use listing::{list_open_directory_with_limit, list_open_directory_with_limit_and_error};
use paths::open_workspace_relative_with_resolved_path;
use syscalls::{
    duplicate_directory, duplicate_directory_file, is_directory, is_regular_file, is_symlink,
    open_at, same_inode, stat_at,
};

#[cfg(test)]
use replace::{apply_patch_file_at_with_ops, read_file_contents_at_for_verification};
#[cfg(test)]
use syscalls::{rename_file_at, write_to_file};
use types::{MAX_FILE_SIZE, MAX_TASK_DIRECTORY_ENTRIES};

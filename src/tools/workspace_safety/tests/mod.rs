use crate::test_support::TempDir;

pub(super) use super::listing::quote_json;
pub(super) use super::read::{
    read_project_guidance_at, read_project_guidance_at_with_reader,
    read_project_guidance_at_with_reader_and_hook, read_root_agents_md_at,
    read_root_agents_md_at_with_reader,
};
pub(super) use super::syscalls::write_to_file;
pub(super) use super::types::RootGuidanceReadError;

pub(super) fn temp_dir(prefix: &str) -> TempDir {
    TempDir::with_prefix(prefix)
}

mod guidance;
mod mutation;
mod other;
mod replace;
mod search;

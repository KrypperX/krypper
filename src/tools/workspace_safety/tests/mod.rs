use std::sync::atomic::AtomicUsize;

pub(super) use super::listing::{quote_json, ListedName};
pub(super) use super::read::{read_root_agents_md_at, read_root_agents_md_at_with_reader};
pub(super) use super::syscalls::write_to_fd;
pub(super) use super::types::RootGuidanceReadError;

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

mod guidance;
mod other;
mod replace;

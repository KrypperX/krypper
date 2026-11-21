//! Bounded, read-only loading of explicit skill manifests.
//!
//! Skill content is untrusted data. This module only reads manifests inside a
//! captured workspace, validates them and returns inert text. It registers no
//! tool, creates no capability grant, runs no process, downloads nothing and
//! injects nothing into model context. Selection and activation are a separate
//! later concern.

mod discovery;
mod manifest;

#[cfg(test)]
mod tests;

pub(crate) use discovery::{load_skills, MAX_SKILL_MANIFESTS};
pub(crate) use manifest::{SkillLoadError, MAX_SKILL_MANIFEST_BYTES, SKILL_MANIFEST_SCHEMA};

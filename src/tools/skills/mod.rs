//! Bounded, read-only loading of explicit skill manifests.
//!
//! Skill content is untrusted data. This module only reads manifests inside a
//! captured workspace, validates them and returns inert text. It registers no
//! tool, creates no capability grant, runs no process and downloads nothing.
//! Activation ([`activation`]) is host-owned, bounded and deterministic; it
//! renders inert advisory text for the existing context builders and never
//! registers, grants or executes anything.

mod activation;
mod discovery;
mod manifest;

#[cfg(test)]
mod tests;

pub(crate) use activation::{
    activate_skills, select_skills, ActiveSkill, SkillActivationError, SkillContext,
    MAX_ACTIVE_SKILLS, MAX_SKILL_CONTEXT_BYTES,
};
pub(crate) use discovery::{load_skills, MAX_SKILL_MANIFESTS};
pub(crate) use manifest::{SkillLoadError, MAX_SKILL_MANIFEST_BYTES, SKILL_MANIFEST_SCHEMA};

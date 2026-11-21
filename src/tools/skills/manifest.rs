//! Manifest schema, validation and the inert instruction type.
//!
//! A manifest is untrusted data. It can describe text only; it can never name a
//! tool, grant, command, executor or policy decision.

use serde::Deserialize;
use std::fmt;

/// The only skill manifest schema understood by this loader.
pub(crate) const SKILL_MANIFEST_SCHEMA: &str = "krypper-skill/v1";

/// Maximum UTF-8 bytes in one manifest file.
pub(crate) const MAX_SKILL_MANIFEST_BYTES: usize = 16 * 1024;

const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_INSTRUCTIONS_BYTES: usize = 8 * 1024;

/// Reasons a skill manifest cannot be loaded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkillLoadError {
    InvalidPath,
    OutsideWorkspace,
    Symlink,
    NotFound,
    TooManyManifests,
    TooLarge,
    InvalidManifest,
    DuplicateName,
    ReadFailed,
}

/// Untrusted skill text, never an executable instruction or authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInstructions(String);

impl SkillInstructions {
    /// Wrap instructions that have already passed manifest validation.
    pub(crate) fn from_text(text: String) -> Self {
        Self(text)
    }

    /// Return the inert text; callers must not treat it as a command or grant.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validated manifest fields ready to become a loaded skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedSkill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) instructions: SkillInstructions,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillManifest {
    schema: String,
    name: String,
    description: String,
    instructions: String,
}

/// Validate one manifest against the fixed schema and its discovery path.
///
/// Unknown fields cause rejection so capability or command declarations cannot
/// hide in a manifest. Success changes no registry, grant or CLI state.
pub(crate) fn parse_manifest(
    contents: &str,
    expected_name: &str,
) -> Result<ParsedSkill, SkillLoadError> {
    let manifest: SkillManifest =
        serde_json::from_str(contents).map_err(|_| SkillLoadError::InvalidManifest)?;
    if manifest.schema != SKILL_MANIFEST_SCHEMA
        || manifest.name != expected_name
        || !valid_skill_name(&manifest.name)
        || !valid_text(&manifest.description, MAX_DESCRIPTION_BYTES)
        || !valid_text(&manifest.instructions, MAX_INSTRUCTIONS_BYTES)
    {
        return Err(SkillLoadError::InvalidManifest);
    }
    Ok(ParsedSkill {
        name: manifest.name,
        description: manifest.description,
        instructions: SkillInstructions::from_text(manifest.instructions),
    })
}

/// Skill names are bounded ASCII identifiers, never paths or selectors.
pub(crate) fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_text(text: &str, maximum: usize) -> bool {
    !text.is_empty() && text.len() <= maximum && !text.contains('\0')
}

impl fmt::Display for SkillLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidPath => "skill manifest path is invalid",
            Self::OutsideWorkspace => "skill manifest path is outside the workspace",
            Self::Symlink => "skill manifest path is a symlink",
            Self::NotFound => "skill manifest not found",
            Self::TooManyManifests => "skill manifest limit exceeded",
            Self::TooLarge => "skill manifest exceeds 16 KiB",
            Self::InvalidManifest => "skill manifest is invalid",
            Self::DuplicateName => "skill names conflict",
            Self::ReadFailed => "skill manifest could not be read",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SkillLoadError {}

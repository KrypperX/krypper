//! Bounded, deterministic activation of already loaded skills.
//!
//! Activation is host-owned. The host names the skills to activate and their
//! order is their precedence order; task text, model output and manifest content
//! never select a skill. This keeps activation from becoming an authority: the
//! result is inert, bounded advisory text for the existing planning context
//! builders, not a tool, grant, command or policy decision.

use super::discovery::{load_skills, LoadedSkill, SkippedSkillManifest};
use super::manifest::{SkillLoadError, MAX_DESCRIPTION_BYTES};
use crate::tools::WorkspaceRoot;
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;

/// Maximum number of skills the host may activate in one request.
pub(crate) const MAX_ACTIVE_SKILLS: usize = 4;

/// Maximum UTF-8 bytes of skill descriptions and instructions rendered together.
pub(crate) const MAX_SKILL_CONTEXT_BYTES: usize = 2 * 1024;

const TRUNCATION_MARKER: &str = "\n[truncated]";

// Mirrors the repository-context neutralization list. Skill text is untrusted
// for the same reason snippet text is: it can try to impersonate instructions.
const DIRECTIVE_MARKERS: [&str; 5] = [
    "ignore previous",
    "disregard previous",
    "system prompt",
    "developer instruction",
    "you are now",
];

/// Reasons a host-requested activation cannot be honoured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkillActivationError {
    /// Loading manifests failed; the underlying load error is preserved.
    Load(SkillLoadError),
    /// A requested name has no loaded manifest. Activation fails closed.
    UnknownSkill,
    /// More names were requested than [`MAX_ACTIVE_SKILLS`].
    TooManySkills,
}

impl From<SkillLoadError> for SkillActivationError {
    fn from(error: SkillLoadError) -> Self {
        Self::Load(error)
    }
}

impl fmt::Display for SkillActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "{error}"),
            Self::UnknownSkill => formatter.write_str("requested skill is not available"),
            Self::TooManySkills => write!(
                formatter,
                "at most {MAX_ACTIVE_SKILLS} skills may be activated at once"
            ),
        }
    }
}

impl std::error::Error for SkillActivationError {}

/// One activated skill rendered as inert, bounded text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ActiveSkill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) manifest_path: PathBuf,
    pub(crate) instructions: String,
}

/// Bounded, deterministic activation result for the planning context.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SkillContext {
    pub(crate) skills: Vec<ActiveSkill>,
    pub(crate) bytes: usize,
    pub(crate) truncated: bool,
    pub(crate) neutralized_lines: usize,
    /// Host diagnostics only; never serialized into model context.
    #[serde(skip)]
    pub(crate) skipped_manifests: Vec<SkippedSkillManifest>,
}

/// Load project skills and activate the host-named subset in host order.
///
/// Unknown names and an over-limit request fail closed, so a host never gets a
/// silently partial activation. An empty request activates nothing.
pub(crate) fn activate_skills(
    root: &WorkspaceRoot,
    requested: &[&str],
) -> Result<SkillContext, SkillActivationError> {
    let report = load_skills(root, &[])?;
    let mut context = select_skills(&report.skills, requested)?;
    context.skipped_manifests = report.skipped_manifests;
    Ok(context)
}

/// Select and render host-named skills from an already loaded set.
///
/// Request order is precedence order. Repeated names keep their first
/// occurrence and drop later duplicates; the output is therefore deterministic
/// for the same input, independent of manifest discovery order.
pub(crate) fn select_skills(
    loaded: &[LoadedSkill],
    requested: &[&str],
) -> Result<SkillContext, SkillActivationError> {
    let mut names: Vec<&str> = Vec::new();
    for name in requested {
        if !names.contains(name) {
            names.push(name);
        }
    }
    if names.len() > MAX_ACTIVE_SKILLS {
        return Err(SkillActivationError::TooManySkills);
    }
    let mut selected = Vec::with_capacity(names.len());
    for name in names {
        // Host selection is an exact name match. A prefix or substring in
        // task/model text cannot activate a skill.
        let skill = loaded
            .iter()
            .find(|skill| skill.name.as_str() == name)
            .ok_or(SkillActivationError::UnknownSkill)?;
        selected.push(skill);
    }
    Ok(render(&selected))
}

fn render(selected: &[&LoadedSkill]) -> SkillContext {
    let mut context = SkillContext::default();
    if selected.is_empty() {
        return context;
    }
    // Every activated skill receives the same deterministic share of the
    // combined description and instruction budget. Descriptions are capped at
    // one quarter of that share; unused text space is not redistributed.
    let allowance = MAX_SKILL_CONTEXT_BYTES / selected.len();
    for skill in selected {
        let description_allowance = (allowance / 4).min(MAX_DESCRIPTION_BYTES);
        let (description, description_truncated) = render_skill_text(
            &skill.description,
            description_allowance,
            &mut context.neutralized_lines,
        );
        // Reserve the full description slice for every skill, even when a
        // description is short, so instruction space stays symmetric.
        let instruction_allowance = allowance.saturating_sub(description_allowance);
        let (instructions, instructions_truncated) = render_skill_text(
            skill.instructions.as_str(),
            instruction_allowance,
            &mut context.neutralized_lines,
        );
        context.truncated |= description_truncated || instructions_truncated;
        context.bytes += description.len() + instructions.len();
        context.skills.push(ActiveSkill {
            name: skill.name.clone(),
            description,
            manifest_path: skill.manifest_path.clone(),
            instructions,
        });
    }
    context
}

/// Render one skill text field within `allowance` bytes.
///
/// Lines are trimmed, control characters are replaced and directive-like lines
/// are neutralized. Truncation happens on a character boundary and is marked,
/// so a host can report that the model saw a partial skill.
fn render_skill_text(text: &str, allowance: usize, neutralized: &mut usize) -> (String, bool) {
    let mut rendered = String::new();
    let mut truncated = false;
    for line in text.lines() {
        let line = neutralized_line(line, neutralized);
        let separator = usize::from(!rendered.is_empty());
        if rendered.len() + separator + line.len() <= allowance {
            if separator == 1 {
                rendered.push('\n');
            }
            rendered.push_str(&line);
            continue;
        }
        let available = allowance.saturating_sub(rendered.len() + separator);
        if available > TRUNCATION_MARKER.len() {
            if separator == 1 {
                rendered.push('\n');
            }
            push_prefix(&mut rendered, &line, available - TRUNCATION_MARKER.len());
        }
        // Always mark truncation when the allowance can hold the marker, so a
        // host can never mistake a partial skill for the complete text.
        if allowance >= TRUNCATION_MARKER.len() {
            while rendered.len() + TRUNCATION_MARKER.len() > allowance {
                rendered.pop();
            }
            rendered.push_str(TRUNCATION_MARKER);
        }
        truncated = true;
        break;
    }
    (rendered, truncated)
}

fn neutralized_line(line: &str, neutralized: &mut usize) -> String {
    let line = line.trim_end();
    let lowered = line.to_ascii_lowercase();
    // Substring matching here only sanitizes untrusted text; it never selects
    // or activates a skill, which requires an exact host-supplied name above.
    if DIRECTIVE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        *neutralized += 1;
        return "[neutralized directive-like line]".to_owned();
    }
    line.chars()
        .map(|character| {
            if character.is_control() && character != '\t' {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn push_prefix(target: &mut String, line: &str, maximum: usize) {
    let mut used = 0;
    for character in line.chars() {
        if used + character.len_utf8() > maximum {
            break;
        }
        used += character.len_utf8();
        target.push(character);
    }
    while target.ends_with(' ') {
        target.pop();
    }
}

#[cfg(test)]
mod tests;

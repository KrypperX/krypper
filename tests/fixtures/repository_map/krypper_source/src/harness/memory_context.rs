//! Prompt composition for memory recall (#219 PR 13).
//!
//! Recall rides the existing plan and task prompt builders, so it adds no agent loop, no second
//! executor and no authority. Precedence is explicit: task goal and Harness bounds, then AGENTS.md
//! guidance, then activated skills, then repository excerpts, then memory. Memory is the lowest
//! priority because it is the least current evidence.

use super::context::{ContextError, RepositoryContext};
use super::skill_context::{
    build_plan_prompt_with_skills_and_repository_context,
    build_plan_prompt_with_targets_skills_and_repository_context,
    build_task_prompt_with_project_guidance_and_skills,
};
use crate::agent_loop::WorkspaceObservation;
use crate::memory::{append_memory_context, MemoryContext};
use crate::tools::skills::SkillContext;
use std::path::PathBuf;

/// Task prompt with skills and opt-in memory recall appended last.
pub fn build_task_prompt_with_skills_and_memory(
    goal: &str,
    observations: &[WorkspaceObservation],
    remaining_decisions: usize,
    project_guidance: Option<&str>,
    skill_context: Option<&SkillContext>,
    memory_context: Option<&MemoryContext>,
) -> Result<String, ContextError> {
    let mut prompt = build_task_prompt_with_project_guidance_and_skills(
        goal,
        observations,
        remaining_decisions,
        project_guidance,
        skill_context,
    )?;
    append_memory_context(&mut prompt, memory_context);
    Ok(prompt)
}

/// Plan prompt with skills, repository excerpts and memory recall appended last.
pub fn build_plan_prompt_with_skills_repository_and_memory(
    goal: &str,
    observations: &[WorkspaceObservation],
    maximum_operations: usize,
    project_guidance: Option<&str>,
    skill_context: Option<&SkillContext>,
    repository_context: Option<&RepositoryContext>,
    memory_context: Option<&MemoryContext>,
) -> Result<String, ContextError> {
    let mut prompt = build_plan_prompt_with_skills_and_repository_context(
        goal,
        observations,
        maximum_operations,
        project_guidance,
        skill_context,
        repository_context,
    )?;
    append_memory_context(&mut prompt, memory_context);
    Ok(prompt)
}

/// Same precedence as [`build_plan_prompt_with_skills_repository_and_memory`], preserving the
/// existing proposed mutation targets.
#[allow(clippy::too_many_arguments)]
pub fn build_plan_prompt_with_targets_skills_repository_and_memory(
    goal: &str,
    observations: &[WorkspaceObservation],
    maximum_operations: usize,
    project_guidance: Option<&str>,
    proposed_targets: &[PathBuf],
    skill_context: Option<&SkillContext>,
    repository_context: Option<&RepositoryContext>,
    memory_context: Option<&MemoryContext>,
) -> Result<String, ContextError> {
    let mut prompt = build_plan_prompt_with_targets_skills_and_repository_context(
        goal,
        observations,
        maximum_operations,
        project_guidance,
        proposed_targets,
        skill_context,
        repository_context,
    )?;
    append_memory_context(&mut prompt, memory_context);
    Ok(prompt)
}

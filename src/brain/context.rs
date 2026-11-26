//! Brain-facing access to the shared model context contract.

pub(crate) use crate::contracts::model_context::{
    append_project_guidance, build_plan_prompt_with_repository_context,
    build_plan_prompt_with_targets_and_repository_context, build_reflection_prompt,
    build_reflective_plan_prompt, build_task_prompt_with_project_guidance,
    build_task_prompt_with_repository_context_json, compact_observations_for_model, ContextError,
    ObservationCompactionReport, RepositoryContext, MAX_OBSERVATION_BYTES,
};

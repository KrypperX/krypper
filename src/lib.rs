#[cfg(test)]
pub(crate) static CURRENT_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod agent_loop;
mod brain;
pub mod catalog_data;
pub mod cli;
pub mod core;
pub mod harness;
pub mod streaming;
pub mod task_lifecycle;
pub(crate) mod tools;

pub use tools::process::{OfflineCargoPolicy, ProcessOutcome, ProcessResource, ProcessResult};

pub use brain::{BrainReflectionError, OllamaDecisionSource, WorkspacePlan, WorkspacePlanError};

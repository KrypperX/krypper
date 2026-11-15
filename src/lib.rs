#[cfg(test)]
pub(crate) static CURRENT_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod agent_loop;
pub mod cli;
pub mod core;
pub mod harness;
mod model;
pub mod task_lifecycle;
pub(crate) mod tools;

pub use model::OllamaDecisionSource;

#[cfg(test)]
pub(crate) static CURRENT_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod agent_loop;
pub mod cli;
mod file_safety;
mod model;
pub mod task_lifecycle;

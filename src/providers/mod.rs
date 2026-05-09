pub mod coding_task;
pub mod prompt;

use crate::workloads::registry;
use anyhow::Result;

pub fn register_builtins() -> Result<()> {
    registry::register(Box::new(prompt::PromptProvider))?;
    registry::register(Box::new(coding_task::CodingTaskProvider))?;
    Ok(())
}

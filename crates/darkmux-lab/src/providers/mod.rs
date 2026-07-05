pub mod coding_task;
pub mod prompt;
pub mod tool_bench;
pub mod workspace_delta;

use crate::workloads::registry;
use anyhow::Result;

pub fn register_builtins() -> Result<()> {
    registry::register(Box::new(prompt::PromptProvider))?;
    registry::register(Box::new(coding_task::CodingTaskProvider))?;
    registry::register(Box::new(tool_bench::ToolBenchProvider))?;
    Ok(())
}

pub mod coding_task;
pub(crate) mod expect;
pub mod prompt;
pub mod tool_bench;
pub mod workspace_delta;

use crate::workloads::registry;
use crate::workloads::types::LoadedWorkload;
use darkmux_crew::dispatch::LabInputs;
use anyhow::Result;

/// (#3363) A lab dispatch's pinned inputs: an eval owns its roles, so a
/// workload bound to a fixture takes its role from `sandbox_dir`, the run's
/// own copy of that fixture; a workload bound to none takes darkmux's
/// compiled-in role. Every provider's `DispatchOpts` sets it.
pub(crate) fn lab_inputs(loaded: &LoadedWorkload, sandbox_dir: &std::path::Path) -> LabInputs {
    LabInputs::for_workload(loaded.manifest.workload.requires_fixture.is_some(), sandbox_dir)
}

pub fn register_builtins() -> Result<()> {
    registry::register(Box::new(prompt::PromptProvider))?;
    registry::register(Box::new(coding_task::CodingTaskProvider))?;
    registry::register(Box::new(tool_bench::ToolBenchProvider::production()))?;
    Ok(())
}

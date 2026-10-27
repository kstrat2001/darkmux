pub mod coding_task;
pub(crate) mod expect;
pub mod prompt;
pub mod tool_bench;
pub mod workspace_delta;

use crate::workloads::registry;
use crate::workloads::types::LoadedWorkload;
use darkmux_crew::dispatch::LabInputs;
use anyhow::Result;

/// (#3363) A lab dispatch's pinned inputs, which every provider's
/// `DispatchOpts` sets: the role `lab run` read once before the run started
/// (`LoadedWorkload::lab_inputs`). A workload bound to no fixture that was
/// not given one runs on darkmux's compiled-in role; one bound to a fixture
/// is refused, because its role must come from the eval as read before the
/// run, never from the sandbox the run writes to.
pub(crate) fn lab_inputs(loaded: &LoadedWorkload) -> Result<LabInputs> {
    match (&loaded.lab_inputs, &loaded.manifest.workload.requires_fixture) {
        (Some(inputs), _) => Ok(inputs.clone()),
        (None, None) => Ok(LabInputs::Builtin),
        (None, Some(_)) => anyhow::bail!(
            "workload \"{}\" binds a fixture, but its eval's role was not read: `lab run` reads it once from the eval before the run starts",
            loaded.manifest.workload.id
        ),
    }
}

pub fn register_builtins() -> Result<()> {
    registry::register(Box::new(prompt::PromptProvider))?;
    registry::register(Box::new(coding_task::CodingTaskProvider))?;
    registry::register(Box::new(tool_bench::ToolBenchProvider::production()))?;
    Ok(())
}

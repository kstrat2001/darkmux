//! The engine facade (#316 stage 3, `docs/architecture/ENGINES.md` section 8).
//!
//! One interface the rest of darkmux asks about a model server, with one
//! backend per kind of server. LM Studio is the only backend today
//! ([`LmStudio`]), and it delegates one-to-one to `crate::lms` and
//! [`crate::gestalt_host::LmsHost`]: same return types, same bounds, same
//! fallbacks. The duplicate `lms ps` / `lms ls` parsers stay where they are;
//! merging them would change behavior (ENGINES.md section 8.2).
//!
//! The facade sits above the gestalt [`ModelHost`] port rather than widening
//! it: `ModelHost` is the planner's lifecycle port with planner-minimal facts,
//! while these questions read the full [`LoadedModel`] and [`ModelMeta`]. A
//! backend with no lifecycle (a Messages endpoint, stage 4) answers
//! [`Engine::host`] with `None` instead of a port full of `Unsupported` stubs.
//!
//! The facade takes identifiers verbatim and has no ownership method: the
//! `darkmux:` namespace (contract 4) stays the LM Studio backend's mechanism,
//! in `crate::ownership`.

mod lmstudio;

pub use lmstudio::LmStudio;

use anyhow::Result;
use darkmux_gestalt::ModelHost;
use darkmux_types::LoadedModel;

/// One row from `lms ls --json`: every model the LMStudio catalog knows
/// about (downloaded), regardless of whether it's currently loaded. Used by
/// `darkmux scan` to discover models the user could add to their profile
/// registry.
///
/// `publisher` is read from `lms ls --json` (e.g. "Qwen", "google",
/// "lmstudio-community").
#[derive(Debug, Clone)]
pub struct ModelMeta {
    pub model_key: String,
    pub display_name: String,
    pub publisher: String,
    pub size_bytes: u64,
    pub params_string: Option<String>,
    pub architecture: Option<String>,
    pub max_context_length: Option<u32>,
    pub trained_for_tool_use: bool,
    /// Type per LMStudio: "llm", "embedding", etc. We typically filter to
    /// `"llm"` since profiles are for chat/agentic dispatch.
    pub model_type: String,
}

/// A model server darkmux can ask questions of.
pub trait Engine {
    /// Every model the engine has resident. `Ok(vec![])` means the engine
    /// reported none; anything it could not interpret is an `Err`.
    fn loaded(&self) -> Result<Vec<LoadedModel>>;

    /// Every model the engine could run here (its downloaded catalog). A
    /// failed listing is an `Err`, never an empty catalog.
    fn catalog(&self) -> Result<Vec<ModelMeta>>;

    /// Unload the resident instance named `identifier`, taken verbatim. An
    /// instance that was already gone is the typed not-resident error
    /// ([`is_not_resident`]).
    fn unload(&self, identifier: &str) -> Result<()>;

    /// The engine's load lifecycle as the planner's port, or `None` when the
    /// engine has none (darkmux cannot load or evict models there).
    fn host(&self) -> Option<Box<dyn ModelHost>>;

    /// The identifiers and model keys of every resident instance, both forms
    /// a profile id could match. Dispatch's resolution probe; unlike
    /// [`Engine::loaded`] it is one unbounded spawn with no text fallback,
    /// moved as it was (ENGINES.md section 8.2).
    fn resident_ids(&self) -> Result<Vec<String>>;

    /// The engine's own version line, unparsed, or `None` when it could not
    /// say. A lab fingerprint field; never an error.
    fn version(&self) -> Option<String>;

    /// The residency ledger (what is loaded, what it costs, what is free):
    /// the machine page's `/machine/resources`, `darkmux machine resources`
    /// and the doctor's unpriceable check. Never errors: every probe degrades
    /// to a warning in the ledger.
    fn ledger(&self) -> crate::model_ledger::ModelLedger;

    /// The local binary darkmux reaches the engine through, and where that
    /// name came from (env, config or built-in), or `None` when the engine
    /// is not reached through a local binary. The doctor's binary check.
    fn binary(&self) -> Option<(String, darkmux_types::config_access::Source)>;
}

/// The engine on this machine. Today that is always LM Studio, resolving its
/// binary as before (`env(DARKMUX_LMS_BIN) > config.lms_bin > "lms"`) at each
/// call, so the fake-`lms` test seam is unchanged.
pub fn local() -> Box<dyn Engine> {
    Box::new(LmStudio)
}

/// Whether an unload error is the typed not-resident outcome (#3083). It reads
/// gestalt's [`HostError`](darkmux_gestalt::HostError), which every backend's
/// unload carries, so the check is the same whatever the engine.
pub fn is_not_resident(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<darkmux_gestalt::HostError>(),
        Some(darkmux_gestalt::HostError::NotResident { .. })
    )
}

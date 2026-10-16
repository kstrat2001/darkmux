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
//! backend with no lifecycle ([`OpenAiCompatible`], stage 4) answers
//! [`Engine::host`] with `None` instead of a port full of `Unsupported` stubs,
//! and a question it cannot answer with [`NotReported`], never an empty answer.
//!
//! The facade takes identifiers verbatim and has no ownership method: the
//! `darkmux:` namespace (contract 4) stays the LM Studio backend's mechanism,
//! in `crate::ownership`.

mod lmstudio;
mod openai_compat;

pub use lmstudio::LmStudio;
pub use openai_compat::{OpenAiCompatible, MODELS_PROBE_BOUND};

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

/// What an engine can do, declared once by the engine and read by the core,
/// which picks its behavior from the declaration and never from which engine
/// it is (ENGINES.md section 3.1). An engine declares only what it really
/// does; a variant joins an enum when an engine that declares it is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub residency: Residency,
    pub loading: Loading,
    /// [`Engine::unload`] works; otherwise it is [`NotReported`].
    pub unload: bool,
    /// The engine marks an instance as darkmux's (LM Studio's `darkmux:`
    /// identifier, contract 4); without one, question 3's own record applies.
    pub ownership_mark: bool,
    pub catalog: CatalogFacts,
}

/// Whether the engine says which models it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// [`Engine::loaded`] lists each resident with its context.
    Reported,
    /// [`Engine::loaded`] and [`Engine::resident_ids`] are [`NotReported`].
    NotReported,
}

/// How a model comes to be loaded on the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loading {
    /// darkmux loads a model at a context it chooses ([`Engine::host`]).
    Explicit,
    /// darkmux cannot load or predict what the engine holds; it sends the
    /// profile's model and says so.
    Unknown,
}

/// Which facts each [`Engine::catalog`] row carries; a fact not carried is
/// unknown, never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogFacts {
    pub size: bool,
    pub context_limit: bool,
    pub tool_training: bool,
}

/// A model server darkmux can ask questions of.
pub trait Engine {
    /// What this engine can do. The core reads this, never the engine's type.
    fn capabilities(&self) -> Capabilities;

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

/// LM Studio, the engine a managed model runs on (`"managed": "lmstudio"`,
/// and a model that names no endpoint). Every step of a managed model's
/// lifecycle asks it by name: loading, residency, eject of `darkmux:`
/// instances, the local wave track. Those are LM Studio's whatever this
/// machine's default engine is; a view of this machine's engine asks
/// [`local`] instead. Resolves its binary at each call, as [`local`] does.
pub fn lmstudio() -> Box<dyn Engine> {
    Box::new(LmStudio)
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

/// A question an engine cannot answer, such as which models a plain
/// OpenAI-compatible server holds. Typed so a caller says so plainly instead of
/// reading it as "none" (ENGINES.md section 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotReported {
    /// The engine, as the operator named it (an endpoint id).
    pub engine: String,
    /// What it does not report, worded to follow "does not report".
    pub question: &'static str,
}

impl std::fmt::Display for NotReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} does not report {}", self.engine, self.question)
    }
}

impl std::error::Error for NotReported {}

/// Whether an engine error is [`NotReported`]: the engine cannot answer, as
/// opposed to failing to.
pub fn is_not_reported(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotReported>().is_some()
}

/// The conformance check every engine's tests run against a live-enough fake:
/// what [`Engine::capabilities`] declares is what the methods do. A declared
/// capability whose method says [`NotReported`], or an undeclared one whose
/// method answers, fails here rather than in the core.
#[cfg(test)]
pub(crate) fn assert_declaration_holds(engine: &dyn Engine, unload_probe: &str) {
    let caps = engine.capabilities();
    let says_not_reported = |r: Result<()>| r.err().is_some_and(|e| is_not_reported(&e));

    let residency_unreported = [
        says_not_reported(engine.loaded().map(|_| ())),
        says_not_reported(engine.resident_ids().map(|_| ())),
    ];
    assert_eq!(
        residency_unreported,
        [caps.residency == Residency::NotReported; 2],
        "residency {:?} disagrees with loaded/resident_ids",
        caps.residency
    );
    assert_eq!(engine.host().is_some(), caps.loading == Loading::Explicit, "loading {:?} disagrees with host()", caps.loading);
    assert_eq!(!says_not_reported(engine.unload(unload_probe)), caps.unload, "unload {} disagrees with unload()", caps.unload);
    if let Ok(rows) = engine.catalog() {
        assert!(!rows.is_empty(), "the fake must list a model for the catalog facts to be checked");
        for m in &rows {
            assert_eq!(m.size_bytes != 0, caps.catalog.size, "{}: size", m.model_key);
            assert_eq!(m.max_context_length.is_some(), caps.catalog.context_limit, "{}: context limit", m.model_key);
        }
    }
}


/// (#316) A load lifecycle is LM Studio's, asked for by name: no production
/// source in the workspace asks [`local`] for one, because once the local
/// engine can be a server with no lifecycle, `local().host()` is `None` where
/// a managed model needs to load. A view of the machine asks `local()`; a
/// managed model's load asks [`lmstudio`].
#[cfg(test)]
#[test]
fn no_production_code_asks_the_local_engine_for_a_load_lifecycle() {
    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    for krate in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
        sources(&krate.path().join("src"), &mut files);
    }
    assert!(files.len() > 100, "the scan found the workspace sources: {}", files.len());
    let offenders: Vec<String> = files
        .iter()
        .filter(|f| !f.to_string_lossy().contains("test"))
        .filter(|f| {
            let code: String = std::fs::read_to_string(f)
                .unwrap()
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .flat_map(str::split_whitespace)
                .collect();
            code.contains(&["engine::local()", ".host()"].concat())
        })
        .map(|f| f.display().to_string())
        .collect();
    assert!(offenders.is_empty(), "ask engine::lmstudio() for a managed model's load lifecycle: {offenders:?}");
}

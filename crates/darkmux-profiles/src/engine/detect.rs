//! (#3569) Which engines darkmux can run work on are installed on this
//! machine, and whether each is usable: what `darkmux init` reports, the
//! doctor's `installed engines` row reads, and `GET /client/engines` serves.
//!
//! darkmux recommends nothing here. It lists what it finds and what each
//! engine itself states (its version, the models it holds, the context a
//! model's manifest declares), and says why an engine it found cannot be
//! used. Which engine or model to suggest is the app's to decide.
//!
//! Every question is read-only and bounded: a binary's `--version`, its own
//! model listing (`lms ls --json`, `tensorfold-native models --json`, the
//! revision manifests in rec's install), and `GET /models` on the machine's
//! default engine when that is a server. Nothing is started, loaded or
//! pulled, and no installer runs. An engine whose binary is not on `PATH`
//! (or at the configured path) is not installed, and is not listed.
//!
//! An engine is `usable` when work could run on it once it is configured:
//! LM Studio lists a downloaded model; Tensorfold is a verified release
//! ([`super::tensorfold::VERIFIED_RELEASES`]) that lists a model; rec is the
//! verified release ([`super::flint::VERIFIED_RELEASE`]) with one runtime and
//! a pulled revision; the default engine's server answers with a model. An
//! engine darkmux starts servers for (Tensorfold, rec's Flint) is
//! `configured` when an endpoint in `profiles.json` declares it, and can be
//! added ([`super::add`]) when it is usable and not configured.

use super::flint::{Pulled, Rec};
use super::tensorfold::{Tensorfold, Version};
use darkmux_types::{EndpointKind, ManagedBackend, ProfileRegistry};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Which engine a detected entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DetectedKind {
    /// LM Studio: darkmux loads its downloaded models; it needs no endpoint.
    Lmstudio,
    /// Tensorfold: darkmux starts `tensorfold-native serve` per model.
    Tensorfold,
    /// rec's Flint: darkmux starts rec's server per model.
    Flint,
    /// This machine's `default_engine` when it is an OpenAI-compatible
    /// server (an endpoint with a `url`) that darkmux only sends requests to.
    Url,
}

impl DetectedKind {
    /// The engine as a message names it.
    pub fn label(self) -> &'static str {
        match self {
            DetectedKind::Lmstudio => "LM Studio",
            DetectedKind::Tensorfold => "Tensorfold",
            DetectedKind::Flint => "rec's Flint",
            DetectedKind::Url => "the default engine's server",
        }
    }

    /// The `managed` token an endpoint darkmux would add declares, for an
    /// engine darkmux starts servers for.
    pub fn startable(self) -> Option<ManagedBackend> {
        match self {
            DetectedKind::Tensorfold => Some(ManagedBackend::Tensorfold),
            DetectedKind::Flint => Some(ManagedBackend::Flint),
            DetectedKind::Lmstudio | DetectedKind::Url => None,
        }
    }
}

/// Whether work could run on a detected engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DetectedState {
    /// It is a version darkmux supports and holds a model.
    Usable,
    /// It cannot be used as it is; `reason` says why.
    Unusable,
    /// A probe failed or ran out of time; `reason` names which. Never read
    /// as `unusable`. Also a state a newer darkmux states that this one does
    /// not know.
    #[serde(other)]
    Unknown,
}

/// One model an engine holds, as the engine states it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DetectedModel {
    /// What a profile model's `id` names to run it.
    pub id: String,
    /// The largest context the engine states for it, in tokens (a rec
    /// revision's manifest `max_context`); absent when the engine states
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u32>,
}

/// One engine found on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DetectedEngine {
    pub engine: DetectedKind,
    /// The version it reports, when it reported one.
    pub version: Option<String>,
    pub state: DetectedState,
    /// Why it is `unusable` or `unknown`; absent when `usable`.
    pub reason: Option<String>,
    /// darkmux already runs work on it: an endpoint in `profiles.json`
    /// declares it (`endpoints`). LM Studio needs no endpoint, and the
    /// default engine is one, so both are always configured.
    pub configured: bool,
    /// The `profiles.json` endpoints that declare it.
    pub endpoints: Vec<String>,
    /// The models it holds (for LM Studio, its downloaded language models).
    pub models: Vec<DetectedModel>,
    /// darkmux can add it (`darkmux init --add-engine`, `POST
    /// /client/engines`): an engine darkmux starts servers for, usable, and
    /// not configured.
    pub can_add: bool,
}

/// Every engine found, and whether any of them is usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Detected {
    /// Some engine found is `usable`. `false` when none is installed, or
    /// none found can be used: what an app reads to decide whether to offer
    /// one.
    pub usable: bool,
    /// Each engine found, in a fixed order (LM Studio, Tensorfold, Flint,
    /// the default engine's server). An engine not installed is not listed.
    pub detected: Vec<DetectedEngine>,
}

impl DetectedEngine {
    /// The `darkmux init` command that adds this engine, with the flags
    /// darkmux cannot fill in: the model when it holds several, the context
    /// when the engine states none.
    pub fn add_command(&self) -> String {
        let token = self
            .engine
            .startable()
            .map_or("", darkmux_types::config_enum::ConfigEnum::token);
        let mut cmd = format!("`darkmux init --add-engine {token}");
        if self.models.len() > 1 {
            cmd.push_str(" --engine-model <id>");
        }
        if self.models.iter().any(|m| m.max_context.is_none()) {
            cmd.push_str(" --n-ctx <tokens>");
        }
        cmd.push('`');
        cmd
    }

    /// The engine and its version, as a message names it.
    pub fn named(&self) -> String {
        match &self.version {
            Some(v) => format!("{} {v}", self.engine.label()),
            None => self.engine.label().to_string(),
        }
    }
}

impl Detected {
    pub fn of(detected: Vec<DetectedEngine>) -> Self {
        Detected {
            usable: detected.iter().any(|d| d.state == DetectedState::Usable),
            detected,
        }
    }
}

// ─── What each engine said, and what it means ───────────────────────────────

/// A probe's answer, its error as text.
pub type Said<T> = Result<T, String>;

/// An entry with the fields every engine fills the same way.
fn entry(
    engine: DetectedKind,
    version: Option<String>,
    endpoints: Vec<String>,
    verdict: Verdict,
) -> DetectedEngine {
    let configured = engine.startable().is_none() || !endpoints.is_empty();
    let (state, reason, models) = verdict;
    let can_add = engine.startable().is_some() && state == DetectedState::Usable && !configured;
    DetectedEngine {
        engine,
        version,
        state,
        reason,
        configured,
        endpoints,
        models,
        can_add,
    }
}

/// A state, its reason, and the models.
type Verdict = (DetectedState, Option<String>, Vec<DetectedModel>);

fn usable(models: Vec<DetectedModel>) -> Verdict {
    (DetectedState::Usable, None, models)
}

/// `unknown` when the probe ran out of time, else `unusable`, for `why`.
fn not_usable(why: String) -> Verdict {
    let state = if why.contains(darkmux_gestalt::HostError::TIMED_OUT) {
        DetectedState::Unknown
    } else {
        DetectedState::Unusable
    };
    (state, Some(why), Vec::new())
}

/// The models, or why an engine that lists none cannot be used.
fn holding(models: Vec<DetectedModel>, none: &str) -> Verdict {
    if models.is_empty() {
        not_usable(none.to_string())
    } else {
        usable(models)
    }
}

fn plain(ids: Vec<String>) -> Vec<DetectedModel> {
    ids.into_iter()
        .map(|id| DetectedModel {
            id,
            max_context: None,
        })
        .collect()
}

/// What rec's install said.
pub struct FlintFacts {
    pub version: Said<String>,
    pub runtime: Said<()>,
    pub pulled: Said<Vec<Pulled>>,
}

/// rec's entry: usable when it is the verified release, with one runtime
/// and a pulled revision.
pub fn judge_flint(facts: FlintFacts, endpoints: Vec<String>) -> DetectedEngine {
    let version = facts.version.as_ref().ok().cloned();
    entry(
        DetectedKind::Flint,
        version,
        endpoints,
        flint_verdict(facts),
    )
}

fn flint_verdict(facts: FlintFacts) -> Verdict {
    let verified = super::flint::VERIFIED_RELEASE;
    match (facts.version, facts.runtime, facts.pulled) {
        (Err(e), _, _) => not_usable(format!("rec did not report its version: {e}")),
        (Ok(v), _, _) if v != verified => not_usable(format!(
            "rec {v} is installed; this darkmux supports rec {verified}"
        )),
        (_, Err(e), _) | (_, _, Err(e)) => not_usable(e),
        (_, _, Ok(pulled)) => holding(
            pulled
                .into_iter()
                .map(|p| DetectedModel {
                    id: p.revision,
                    max_context: p.max_context,
                })
                .collect(),
            "rec holds no model; `rec pull` downloads one",
        ),
    }
}

/// What the Tensorfold binary said.
pub struct TensorfoldFacts {
    pub version: Said<Version>,
    pub models: Said<Vec<String>>,
}

/// Tensorfold's entry: usable when it is a verified release that lists a
/// model.
pub fn judge_tensorfold(facts: TensorfoldFacts, endpoints: Vec<String>) -> DetectedEngine {
    let version = facts.version.as_ref().ok().map(|v| v.to_string());
    entry(
        DetectedKind::Tensorfold,
        version,
        endpoints,
        tensorfold_verdict(facts),
    )
}

fn tensorfold_verdict(facts: TensorfoldFacts) -> Verdict {
    let verified = super::tensorfold::VERIFIED_RELEASES;
    match (facts.version, facts.models) {
        (Err(e), _) => not_usable(format!("Tensorfold did not report its version: {e}")),
        (Ok(v), _) if !v.is_verified() => not_usable(format!(
            "Tensorfold {v} is installed; this darkmux supports Tensorfold {verified}"
        )),
        (_, Err(e)) => not_usable(format!("Tensorfold could not list its models: {e}")),
        (_, Ok(ids)) => holding(
            plain(ids),
            "Tensorfold lists no model; `tensorfold-native pull <repo>` downloads one",
        ),
    }
}

/// LM Studio's entry: usable when it lists a downloaded language model.
pub fn judge_lmstudio(version: Option<String>, catalog: Said<Vec<String>>) -> DetectedEngine {
    let verdict = match catalog {
        Ok(ids) => holding(plain(ids), "LM Studio has no language model downloaded"),
        // A listing that failed is LM Studio not answering, not an engine
        // that cannot be used.
        Err(e) => (
            DetectedState::Unknown,
            Some(format!("LM Studio did not list its models: {e}")),
            Vec::new(),
        ),
    };
    entry(DetectedKind::Lmstudio, version, Vec::new(), verdict)
}

/// The default engine's entry, when it is a server: usable when it answers
/// `GET /models` with a model.
pub fn judge_url(endpoint: String, served: Said<Vec<String>>) -> DetectedEngine {
    let verdict = match served {
        Ok(ids) => holding(
            plain(ids),
            &format!("`{endpoint}` answers and lists no model"),
        ),
        Err(e) => not_usable(format!("`{endpoint}` did not answer `GET /models`: {e}")),
    };
    entry(DetectedKind::Url, None, vec![endpoint], verdict)
}

/// The `profiles.json` endpoints that declare `backend`, sorted.
pub fn declaring(registry: Option<&ProfileRegistry>, backend: ManagedBackend) -> Vec<String> {
    registry
        .map(|r| {
            r.endpoints
                .iter()
                .filter(|(_, ep)| matches!(ep.kind(), Ok(EndpointKind::Managed(b)) if b == backend))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ─── Asking the installed engines ───────────────────────────────────────────

/// Whether `bin` names a file to run: a path that is a file, or a bare name
/// found in a `PATH` directory.
pub fn installed(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(bin).is_file()))
}

fn flint_facts(rec: &Rec) -> FlintFacts {
    let msg = |e: anyhow::Error| format!("{e:#}");
    FlintFacts {
        version: rec.version().map_err(msg),
        runtime: rec.runtime().map(|_| ()).map_err(msg),
        pulled: rec.pulled().map_err(msg),
    }
}

fn tensorfold_facts(tf: &Tensorfold) -> TensorfoldFacts {
    let msg = |e: anyhow::Error| format!("{e:#}");
    TensorfoldFacts {
        version: tf.version().map_err(msg),
        models: tf
            .models()
            .map(|ms| ms.into_iter().map(|m| m.repo).collect())
            .map_err(msg),
    }
}

fn lmstudio_entry() -> DetectedEngine {
    let lms = super::lmstudio();
    let catalog = lms
        .catalog()
        .map(|ms| {
            ms.into_iter()
                .filter(|m| m.model_type == "llm")
                .map(|m| m.model_key)
                .collect()
        })
        .map_err(|e| format!("{e:#}"));
    judge_lmstudio(lms.version(), catalog)
}

fn url_entry() -> DetectedEngine {
    let local = super::local();
    let served = local
        .catalog()
        .map(|ms| ms.into_iter().map(|m| m.model_key).collect())
        .map_err(|e| format!("{e:#}"));
    judge_url(local.name(), served)
}

/// One question to ask: the engine, how long to wait, the endpoints that
/// declare it (kept when it does not answer in time), and the asking.
pub struct Question {
    pub kind: DetectedKind,
    pub bound: Duration,
    pub endpoints: Vec<String>,
    pub ask: Box<dyn FnOnce() -> DetectedEngine + Send>,
}

impl Question {
    fn new(
        kind: DetectedKind,
        bound: Duration,
        endpoints: Vec<String>,
        ask: impl FnOnce() -> DetectedEngine + Send + 'static,
    ) -> Self {
        Question {
            kind,
            bound,
            endpoints,
            ask: Box::new(ask),
        }
    }
}

/// How long to wait for an engine's answer.
type Bound<'a> = &'a dyn Fn(ManagedBackend) -> Duration;

/// What to ask on this machine: each installed engine, and the default
/// engine when it is a server. `servers_only` asks only the engines darkmux
/// starts servers for (the doctor row). `listing_bound` is how long each
/// engine is waited for: a listing's bounds
/// (`super::availability::listing_bound`) in production.
fn questions(
    registry: Option<&ProfileRegistry>,
    servers_only: bool,
    listing_bound: Bound<'_>,
) -> Vec<Question> {
    let mut out: Vec<Question> = Vec::new();
    let lms_here = installed(&darkmux_types::config_access::lms_bin());
    if !servers_only && lms_here {
        out.push(Question::new(
            DetectedKind::Lmstudio,
            listing_bound(ManagedBackend::Lmstudio),
            Vec::new(),
            lmstudio_entry,
        ));
    }
    if installed(&darkmux_types::config_access::tensorfold_bin()) {
        let endpoints = declaring(registry, ManagedBackend::Tensorfold);
        let theirs = endpoints.clone();
        let ask = move || judge_tensorfold(tensorfold_facts(&Tensorfold::installed()), theirs);
        out.push(Question::new(
            DetectedKind::Tensorfold,
            listing_bound(ManagedBackend::Tensorfold),
            endpoints,
            ask,
        ));
    }
    if installed(&darkmux_types::config_access::flint_bin()) {
        let endpoints = declaring(registry, ManagedBackend::Flint);
        let theirs = endpoints.clone();
        let ask = move || judge_flint(flint_facts(&Rec::installed()), theirs);
        out.push(Question::new(
            DetectedKind::Flint,
            listing_bound(ManagedBackend::Flint),
            endpoints,
            ask,
        ));
    }
    if !servers_only && default_engine_is_a_server() {
        let endpoint = vec![darkmux_types::config_access::default_engine()
            .trim()
            .to_string()];
        out.push(Question::new(
            DetectedKind::Url,
            super::MODELS_PROBE_BOUND,
            endpoint,
            url_entry,
        ));
    }
    out
}

fn default_engine_is_a_server() -> bool {
    let engine = darkmux_types::config_access::default_engine();
    !engine
        .trim()
        .eq_ignore_ascii_case(darkmux_types::default_engine::LMSTUDIO)
}

/// How long past an engine's bound its answer is waited for: its probes are
/// killed at the bound, so it reports within a poll.
const REAP_GRACE: Duration = Duration::from_secs(1);

/// Ask each question at the same time, each bounded; an engine with no
/// answer by its bound and the grace is `unknown`.
pub fn ask_all(questions: Vec<Question>) -> Vec<DetectedEngine> {
    let start = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut asked: Vec<(DetectedKind, Duration, Vec<String>)> = Vec::new();
    for (i, q) in questions.into_iter().enumerate() {
        asked.push((q.kind, q.bound, q.endpoints));
        let (tx, deadline, ask) = (tx.clone(), start + q.bound, q.ask);
        std::thread::spawn(move || {
            // Every probe the engine runs ends by its bound, so none
            // outlives the listing that stopped waiting for it.
            let answer = crate::gestalt_host::lms_host::with_deadline_cap(Some(deadline), ask);
            let _ = tx.send((i, answer));
        });
    }
    let last = start + asked.iter().map(|(_, b, _)| *b).max().unwrap_or_default() + REAP_GRACE;
    let mut answers: Vec<Option<DetectedEngine>> = asked.iter().map(|_| None).collect();
    for _ in 0..asked.len() {
        let Ok((i, answer)) = rx.recv_timeout(last.saturating_duration_since(Instant::now()))
        else {
            break;
        };
        answers[i] = Some(answer);
    }
    answers
        .into_iter()
        .zip(asked)
        .map(|(a, (kind, bound, endpoints))| a.unwrap_or_else(|| timed_out(kind, bound, endpoints)))
        .collect()
}

/// An engine that did not answer in time: `unknown`, still configured when
/// an endpoint declares it.
fn timed_out(engine: DetectedKind, bound: Duration, endpoints: Vec<String>) -> DetectedEngine {
    let why = format!(
        "{} did not answer within {} s",
        engine.label(),
        bound.as_secs()
    );
    entry(
        engine,
        None,
        endpoints,
        (DetectedState::Unknown, Some(why), Vec::new()),
    )
}

/// Every engine installed on this machine, asked read-only and at once.
/// `registry` says which are configured; without one, none is.
pub fn detect(registry: Option<&ProfileRegistry>) -> Detected {
    Detected::of(ask_all(questions(
        registry,
        false,
        &super::availability::listing_bound,
    )))
}

/// The engines darkmux starts servers for (Tensorfold, rec's Flint) that
/// are installed: the doctor row's question, which leaves LM Studio and the
/// default engine to their own rows.
pub fn detect_servers(registry: Option<&ProfileRegistry>) -> Vec<DetectedEngine> {
    ask_all(questions(
        registry,
        true,
        &super::availability::listing_bound,
    ))
}

#[cfg(test)]
#[path = "detect_tests.rs"]
mod tests;

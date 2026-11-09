//! (#3466) Whether darkmux can make a profile's model resident, as a listing
//! reports it (DESIGN.md, "What an app reads"): `darkmux profile list --json`'s
//! `availability` list and each profile in `GET /client/profiles`.
//!
//! Availability is a question each engine answers for itself, asked through
//! [`ask`], the one match on [`ManagedBackend`]: a new engine is one arm there
//! and its own [`EngineAnswer`]. An unmanaged endpoint is `external` (darkmux
//! only sends it requests and does not vouch that it answers).
//!
//! A listing is handed the refusal of the preflight every dispatch runs
//! (`darkmux_crew::user_files::dispatch_refusal`, which this crate cannot
//! call: crew depends on it): a config, registry, role or skill file it
//! refuses refuses every dispatch, so every model reads `unavailable` with
//! the refusal as its reason, and no engine is asked. Otherwise it asks each
//! engine once ([`list`]), every engine at the same time, and waits for each
//! at most its [`listing_bound`]: 10 s for LM Studio (a cold `lms` takes
//! seconds) and 3 s for a server binary darkmux runs (Tensorfold), a local
//! binary's read-only probes. An engine that has not answered by then reads
//! `unknown`, and a probe still running then is killed with its process
//! group and reaped before the listing returns (the bounded runner's
//! deadline is capped at the engine's), so no child outlives the caller.
//! It loads and starts nothing: the
//! only questions are an engine's own read-only ones (LM Studio's residents
//! and downloaded models, the Tensorfold binary's `--version`,
//! `capabilities --json` and `models --json`), darkmux's server records, and
//! whether a port answers.
//!
//! `unknown` is a probe that could not run or ran out of time (LM Studio not
//! running, a listing that failed once the engine answered). A Tensorfold
//! binary that is missing, exits non-zero on `--version` or prints no
//! version it can read is `unavailable`: a dispatch refuses exactly that
//! with `engine_unavailable`.
//!
//! Whether a model FITS in memory right now is not availability: that changes
//! with every dispatch, and `dispatch --check`'s `fit` answers it.

use super::server::{self, Dirs};
use super::tensorfold::{Facts, Tensorfold, VERIFIED_RELEASES};
use darkmux_types::{EndpointKind, LoadedModel, ManagedBackend, ProfileModel};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long a listing waits for `backend`. Every engine is asked at once,
/// so a listing takes at most about the longest of these. The one match on
/// the backend besides [`ask`].
pub fn listing_bound(backend: ManagedBackend) -> Duration {
    match backend {
        // A cold `lms` takes seconds; a timeout reads `unknown`, which an
        // app treats as not reachable.
        ManagedBackend::Lmstudio => Duration::from_secs(10),
        ManagedBackend::Tensorfold | ManagedBackend::Flint => Duration::from_secs(3),
    }
}

/// How long past an engine's bound a listing waits for its thread to
/// report: its probes are killed at the bound, so it reports within a poll,
/// and waiting is what keeps a killed probe from outliving the listing.
const REAP_GRACE: Duration = Duration::from_secs(1);

/// Whether darkmux can make a model resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    /// Loaded or served now, at the profile's context.
    Resident,
    /// Not resident, and darkmux can load it or start its server.
    Startable,
    /// darkmux cannot make it resident; the reason says why.
    Unavailable,
    /// An endpoint darkmux only sends requests to.
    External,
    /// A probe failed or ran out of time; the reason names which. Never read
    /// as `unavailable`.
    Unknown,
}

/// A model's availability and, for every state but `resident`, `startable`
/// and `external`, a one-line reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Availability {
    pub state: AvailabilityState,
    pub reason: Option<String>,
}

impl Availability {
    fn is(state: AvailabilityState) -> Self {
        Availability {
            state,
            reason: None,
        }
    }
    fn because(state: AvailabilityState, reason: impl Into<String>) -> Self {
        Availability {
            state,
            reason: Some(reason.into()),
        }
    }
    pub fn resident() -> Self {
        Self::is(AvailabilityState::Resident)
    }
    pub fn startable() -> Self {
        Self::is(AvailabilityState::Startable)
    }
    pub fn external() -> Self {
        Self::is(AvailabilityState::External)
    }
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::because(AvailabilityState::Unavailable, reason)
    }
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::because(AvailabilityState::Unknown, reason)
    }
}

/// What one engine said, asked once per listing: judges each of its models.
pub trait EngineAnswer: Send {
    fn judge(&self, model: &ProfileModel) -> Availability;
    /// LM Studio's residents as it listed them, for the client listing's
    /// older `loaded` field; `None` for every other engine.
    fn residents(&self) -> Option<Vec<LoadedModel>> {
        None
    }
}

/// Ask `backend` its availability questions, once. The one match on the
/// backend: a new engine is one arm here.
pub fn ask(backend: ManagedBackend) -> Box<dyn EngineAnswer> {
    match backend {
        ManagedBackend::Lmstudio => Box::new(LmStudioAnswer::ask()),
        ManagedBackend::Tensorfold => Box::new(TensorfoldAnswer::ask(
            Tensorfold::installed(),
            Dirs::under_home(),
        )),
        ManagedBackend::Flint => Box::new(FlintAnswer::ask(
            super::flint::Rec::installed(),
            Dirs::under_home(),
        )),
    }
}

/// One listing's answers: each model's availability, in the order given,
/// and LM Studio's residents when it was asked and answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub each: Vec<Availability>,
    pub lmstudio_residents: Option<Vec<LoadedModel>>,
}

/// Every model's availability, each engine asked once ([`ask`]), all at
/// once, each within its [`listing_bound`]. `refusal` is the dispatch
/// preflight's (`darkmux_crew::user_files::dispatch_refusal`).
pub fn list(models: &[ProfileModel], refusal: Option<String>) -> Listing {
    list_with(models, refusal, ask, listing_bound)
}

/// [`list`] with the engines and their bounds given, for tests.
pub fn list_with(
    models: &[ProfileModel],
    refusal: Option<String>,
    ask: impl Fn(ManagedBackend) -> Box<dyn EngineAnswer> + Send + Sync + Clone + 'static,
    bound: impl Fn(ManagedBackend) -> Duration + Copy,
) -> Listing {
    if let Some(why) = refusal {
        let each = models
            .iter()
            .map(|_| Availability::unavailable(why.clone()))
            .collect();
        return Listing {
            each,
            lmstudio_residents: None,
        };
    }
    let (mut each, by_engine) = by_engine(models, bound);
    let now = std::time::Instant::now();
    let deadlines: Vec<(ManagedBackend, std::time::Instant)> = by_engine
        .iter()
        .map(|(b, _)| (*b, now + bound(*b)))
        .collect();
    let rx = ask_each(models, &by_engine, ask, &deadlines);
    let last = deadlines.iter().map(|(_, d)| *d).max().unwrap_or(now) + REAP_GRACE;
    let mut lmstudio_residents = None;
    for _ in 0..by_engine.len() {
        let Ok((backend, judged, residents)) =
            rx.recv_timeout(last.saturating_duration_since(std::time::Instant::now()))
        else {
            break;
        };
        // An answer past its engine's bound stays `unknown`: it was received
        // only so its killed probes are reaped before the listing returns.
        if deadlines
            .iter()
            .any(|(b, d)| *b == backend && std::time::Instant::now() <= *d)
        {
            accept(&mut each, &by_engine, backend, judged);
            if backend == ManagedBackend::Lmstudio {
                lmstudio_residents = residents;
            }
        }
    }
    Listing {
        each,
        lmstudio_residents,
    }
}

/// Put `backend`'s verdicts in its models' places.
fn accept(
    each: &mut [Availability],
    by_engine: &ByEngine,
    backend: ManagedBackend,
    judged: Vec<Availability>,
) {
    let idx = by_engine
        .iter()
        .find(|(b, _)| *b == backend)
        .map(|(_, idx)| idx);
    for (&i, a) in idx.into_iter().flatten().zip(judged) {
        each[i] = a;
    }
}

/// The models each engine judges, by index, in order; and each model's
/// answer until its engine gives one: `external` for an unmanaged endpoint
/// (asked nothing), `unavailable` for one that does not resolve, and for a
/// managed one, `unknown` because its engine has not answered in time.
type ByEngine = Vec<(ManagedBackend, Vec<usize>)>;

fn by_engine(
    models: &[ProfileModel],
    bound: impl Fn(ManagedBackend) -> Duration,
) -> (Vec<Availability>, ByEngine) {
    let mut by: ByEngine = Vec::new();
    let each = models
        .iter()
        .enumerate()
        .map(|(i, m)| match m.endpoint_kind() {
            Ok(EndpointKind::Unmanaged) => Availability::external(),
            Err(e) => Availability::unavailable(e.to_string()),
            Ok(EndpointKind::Managed(backend)) => {
                match by.iter_mut().find(|(b, _)| *b == backend) {
                    Some((_, idx)) => idx.push(i),
                    None => by.push((backend, vec![i])),
                }
                Availability::unknown(format!(
                    "{} did not answer within {} s",
                    engine_label(backend),
                    bound(backend).as_secs_f32()
                ))
            }
        })
        .collect();
    (each, by)
}

/// What one engine sends back: its verdicts, in its models' order, and its
/// residents (LM Studio's).
type Answered = (ManagedBackend, Vec<Availability>, Option<Vec<LoadedModel>>);

/// Ask every engine at once, each on its own thread. Detached: an engine
/// that never answers must not hold the listing, and every question it is
/// asked is bounded on its own.
fn ask_each(
    models: &[ProfileModel],
    by_engine: &ByEngine,
    ask: impl Fn(ManagedBackend) -> Box<dyn EngineAnswer> + Send + Sync + Clone + 'static,
    deadlines: &[(ManagedBackend, std::time::Instant)],
) -> std::sync::mpsc::Receiver<Answered> {
    let (tx, rx) = std::sync::mpsc::channel();
    for (backend, idx) in by_engine {
        let deadline = deadlines
            .iter()
            .find(|(b, _)| b == backend)
            .map(|(_, d)| *d);
        let (backend, tx, ask) = (*backend, tx.clone(), ask.clone());
        let theirs: Vec<ProfileModel> = idx.iter().map(|&i| models[i].clone()).collect();
        // Every probe the engine runs ends by the listing's deadline, so
        // none outlives the listing that stopped waiting for it.
        std::thread::spawn(move || {
            crate::gestalt_host::lms_host::with_deadline_cap(deadline, || {
                let answer = ask(backend);
                let judged = theirs.iter().map(|m| answer.judge(m)).collect();
                let _ = tx.send((backend, judged, answer.residents()));
            })
        });
    }
    rx
}

/// The engine, as a reason names it.
fn engine_label(backend: ManagedBackend) -> &'static str {
    match backend {
        ManagedBackend::Lmstudio => "LM Studio",
        ManagedBackend::Tensorfold => "Tensorfold",
        ManagedBackend::Flint => "Flint",
    }
}

// ─── LM Studio ───────────────────────────────────────────────────────────

/// What LM Studio said: its residents and its downloaded models, through the
/// engine facade ([`super::lmstudio`], the engine a managed model's
/// lifecycle runs on), never `lms` directly.
#[derive(Debug, Clone)]
pub struct LmStudioAnswer {
    pub loaded: Result<Vec<LoadedModel>, String>,
    /// The models it has downloaded, with every name it loads each by.
    pub catalog: Result<Vec<super::ModelMeta>, String>,
}

impl LmStudioAnswer {
    /// Its residents and its downloaded models, asked at the same time.
    pub fn ask() -> Self {
        let msg = |e: anyhow::Error| format!("{e:#}");
        let cap = crate::gestalt_host::lms_host::deadline_cap();
        std::thread::scope(|s| {
            let loaded = s.spawn(|| {
                crate::gestalt_host::lms_host::with_deadline_cap(cap, || {
                    super::lmstudio().loaded().map_err(msg)
                })
            });
            let catalog = s.spawn(|| {
                crate::gestalt_host::lms_host::with_deadline_cap(cap, || {
                    super::lmstudio().catalog().map_err(msg)
                })
            });
            LmStudioAnswer {
                loaded: loaded
                    .join()
                    .unwrap_or_else(|_| Err("listing its residents panicked".into())),
                catalog: catalog
                    .join()
                    .unwrap_or_else(|_| Err("listing its models panicked".into())),
            }
        })
    }
}

impl EngineAnswer for LmStudioAnswer {
    fn judge(&self, model: &ProfileModel) -> Availability {
        let n_ctx = match model.require_n_ctx() {
            Ok(n) => n,
            Err(e) => return Availability::unavailable(format!("{e:#}")),
        };
        let loaded = match &self.loaded {
            Ok(l) => l,
            Err(e) => return Availability::unknown(format!("LM Studio did not answer: {e}")),
        };
        // The planner's own residency rule: darkmux's instance of the model,
        // at the profile's context or more, is what a dispatch reuses. A
        // copy someone else loaded is never one (contract 4).
        let key = darkmux_gestalt::bare_model_key(&model.id).to_string();
        let placement = darkmux_gestalt::Placement {
            model_key: key.clone(),
            identifier: crate::ownership::namespaced_identifier(model),
            min_ctx: n_ctx,
            seat: "availability".into(),
        };
        let residents: Vec<darkmux_gestalt::ResidentFact> = loaded
            .iter()
            .map(|m| darkmux_gestalt::ResidentFact {
                identifier: m.identifier.clone(),
                model_key: m.model.clone(),
                ctx: m.context,
                est_bytes: None,
                parallel: 0,
            })
            .collect();
        if matches!(
            darkmux_gestalt::decide_residency(&residents, &placement),
            darkmux_gestalt::ResidencyDecision::Reuse { .. }
        ) {
            return Availability::resident();
        }
        match &self.catalog {
            Err(e) => {
                Availability::unknown(format!("LM Studio did not list its downloaded models: {e}"))
            }
            Ok(models) if models.iter().any(|m| m.answers_to(&key)) => Availability::startable(),
            Ok(_) => {
                Availability::unavailable(format!("LM Studio has no downloaded model `{key}`"))
            }
        }
    }

    fn residents(&self) -> Option<Vec<LoadedModel>> {
        self.loaded.as_ref().ok().cloned()
    }
}

// ─── Tensorfold ──────────────────────────────────────────────────────────

/// What the Tensorfold binary said about itself (the facts `darkmux doctor`
/// reads), and where darkmux's server records are.
#[derive(Debug, Clone)]
pub struct TensorfoldAnswer {
    pub tf: Tensorfold,
    pub facts: Facts,
    pub dirs: Dirs,
}

impl TensorfoldAnswer {
    pub fn ask(tf: Tensorfold, dirs: Dirs) -> Self {
        let facts = Facts::probe(&tf);
        TensorfoldAnswer { tf, facts, dirs }
    }
}

impl TensorfoldAnswer {
    /// Why the engine itself could not serve `model` (`None`: it could):
    /// an unverified or silent binary, a model it does not have, or
    /// `engine_args` it does not take, `unavailable`; a listing that failed
    /// once the binary answered, `unknown`.
    fn engine_problem(&self, model: &ProfileModel) -> Option<Availability> {
        if let Some(problem) = self.version_problem() {
            return Some(problem);
        }
        match self.facts.has_model(model) {
            Err(e) => return Some(Availability::unknown(format!("Tensorfold {e}"))),
            Ok(false) => {
                return Some(Availability::unavailable(format!(
                    "`{} models` does not list `{}`",
                    self.tf.label(),
                    model.id
                )))
            }
            Ok(true) => {}
        }
        match self.facts.args_problem(model) {
            Err(e) => Some(Availability::unknown(format!("Tensorfold {e}"))),
            Ok(Some(why)) => Some(Availability::unavailable(format!("engine_args: {why}"))),
            Ok(None) => None,
        }
    }
}

impl TensorfoldAnswer {
    /// The binary is missing, silent, or at a release darkmux was not
    /// verified against: `unavailable`, naming which.
    fn version_problem(&self) -> Option<Availability> {
        match &self.facts.version {
            Err(e) => Some(Availability::unavailable(format!(
                "the Tensorfold binary did not answer: {e}"
            ))),
            Ok(v) if !v.is_verified() => Some(Availability::unavailable(format!(
                "Tensorfold {v} is installed; this darkmux supports Tensorfold {VERIFIED_RELEASES}"
            ))),
            Ok(_) => None,
        }
    }
}

impl EngineAnswer for TensorfoldAnswer {
    /// `resident` when darkmux's server on the endpoint runs this model's
    /// exact command and answers (the rule a dispatch reuses it by);
    /// `startable` when the engine is installed at a verified version, has
    /// the model, takes its `engine_args`, and the port is free or held by
    /// darkmux's own server.
    fn judge(&self, model: &ProfileModel) -> Availability {
        let endpoint = model.endpoint.as_ref();
        let id = endpoint.and_then(|e| e.named_id()).unwrap_or_default();
        let Some(port) = endpoint.and_then(|e| e.port) else {
            return Availability::unavailable(format!("endpoint `{id}` declares no `port`"));
        };
        let (argv, served_name) = match self.tf.serve_argv(port, model) {
            Ok(a) => a,
            Err(e) => return Availability::unavailable(format!("{e:#}")),
        };
        let live = server::running_now(&self.dirs, id);
        if live
            .as_ref()
            .is_some_and(|r| server::serves(r, &argv, port, &served_name))
        {
            return Availability::resident();
        }
        if let Some(problem) = self.engine_problem(model) {
            return problem;
        }
        if live.is_none() && server::port_taken(port) {
            return Availability::unavailable(format!(
                "something darkmux did not start answers on 127.0.0.1:{port}, the port of endpoint `{id}`"
            ));
        }
        Availability::startable()
    }
}

// ─── Flint (#3453) ───────────────────────────────────────────────────────

/// What rec said (`rec --version`, verified, and `rec status`), with its
/// install read where the adapter reads it, and darkmux's server records.
#[derive(Debug, Clone)]
pub struct FlintAnswer {
    pub rec: super::flint::Rec,
    /// The verified version, or why not (missing, unreadable, unverified).
    pub version: Result<String, String>,
    /// The endpoint rec's own server serves, when it is up or starting.
    pub own_server: Result<Option<String>, String>,
    pub dirs: Dirs,
}

impl FlintAnswer {
    /// rec's two read-only probes, asked at the same time, under the
    /// caller's deadline cap (a listing's bound).
    pub fn ask(rec: super::flint::Rec, dirs: Dirs) -> Self {
        let msg = |e: anyhow::Error| format!("{e:#}");
        let cap = crate::gestalt_host::lms_host::deadline_cap();
        let (version, own_server) = std::thread::scope(|s| {
            let version = s.spawn(|| {
                crate::gestalt_host::lms_host::with_deadline_cap(cap, || {
                    rec.verified_version().map_err(msg)
                })
            });
            let own = s.spawn(|| {
                crate::gestalt_host::lms_host::with_deadline_cap(cap, || {
                    rec.own_server().map_err(msg)
                })
            });
            (
                version
                    .join()
                    .unwrap_or_else(|_| Err("`rec --version` panicked".into())),
                own.join()
                    .unwrap_or_else(|_| Err("`rec status` panicked".into())),
            )
        });
        FlintAnswer {
            rec,
            version,
            own_server,
            dirs,
        }
    }

    /// Why rec itself cannot serve now (`None`: it can): an unverified or
    /// silent rec, its own server up (`engine_in_use`), or `rec status`
    /// that could not say (`unknown`).
    fn rec_problem(
        &self,
        darkmuxs: Option<&darkmux_types::engine_record::EngineRecord>,
    ) -> Option<Availability> {
        if let Err(e) = &self.version {
            return Some(Availability::unavailable(e.clone()));
        }
        match &self.own_server {
            Err(e) => Some(Availability::unknown(format!(
                "rec could not say whether its own server runs: {e}"
            ))),
            Ok(Some(address)) if !super::flint::own_server_is_darkmuxs(address, darkmuxs) => Some(
                Availability::unavailable(super::flint::own_server_refusal(address)),
            ),
            Ok(_) => None,
        }
    }
}

impl EngineAnswer for FlintAnswer {
    /// `resident` when darkmux's server on the endpoint runs this model's
    /// command, answers and listens (the rule a dispatch reuses it by);
    /// `startable` when rec is the verified release with exactly one runtime
    /// and one model directory for the revision, rec's own server is not
    /// running, and the port is free or darkmux's.
    fn judge(&self, model: &ProfileModel) -> Availability {
        let endpoint = model.endpoint.as_ref();
        let id = endpoint.and_then(|e| e.named_id()).unwrap_or_default();
        let Some(port) = endpoint.and_then(|e| e.port) else {
            return Availability::unavailable(format!("endpoint `{id}` declares no `port`"));
        };
        let version = self
            .version
            .as_deref()
            .unwrap_or(super::flint::VERIFIED_RELEASE);
        let spec = match self.rec.serve_spec(id, port, model, version) {
            Ok(spec) => spec,
            Err(e) => return Availability::unavailable(format!("{e:#}")),
        };
        let live = server::running_now(&self.dirs, id);
        if live
            .as_ref()
            .is_some_and(|r| server::serves(r, &spec.argv, port, &spec.served_name))
        {
            return Availability::resident();
        }
        if let Some(problem) = self.rec_problem(live.as_ref()) {
            return problem;
        }
        if live.is_none() && server::port_taken(port) {
            return Availability::unavailable(format!(
                "something darkmux did not start answers on 127.0.0.1:{port}, the port of endpoint `{id}`"
            ));
        }
        Availability::startable()
    }
}

#[cfg(test)]
#[path = "availability_tests.rs"]
mod tests;

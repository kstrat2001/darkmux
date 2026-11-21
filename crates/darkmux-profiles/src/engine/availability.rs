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
//! seconds) and 3 s for a server binary darkmux runs (Tensorfold, rec's
//! Flint), a local binary's read-only probes. A probe still running then is
//! killed with its process group and reaped before the listing returns (the
//! bounded runner's deadline is capped at the engine's), so no child
//! outlives the caller; the engine's answer, which then comes just past the
//! bound, keeps what its other probes got (the killed one reads `unknown`
//! where it matters). An engine with no answer within its bound and the
//! reap grace reads `unknown`.
//! It loads and starts nothing: the
//! only questions are an engine's own read-only ones (LM Studio's residents
//! and downloaded models, the Tensorfold binary's `--version`,
//! `capabilities --json`, `serve --help` and `models --json`), darkmux's
//! server records, and whether a port answers.
//!
//! A model reads `resident` only when a dispatch would reuse it: what a
//! dispatch refuses before it looks for a server (`engine_unavailable`,
//! `engine_in_use`, `engine_args` the engine does not take) is judged first.
//!
//! Flint is `startable` only when rec is the verified release with exactly
//! one runtime and one model directory for the revision (the adapter's own
//! checks), the port is free, and rec's own server is not running. rec runs
//! one server per machine, so its own server decides as a dispatch decides
//! (`flint::own_server_use`, contract 4): `external` when it serves the
//! model and `rec auth token` gives the key a dispatch sends (asked only
//! then, the key dropped at once), else `unavailable` with the dispatch's
//! refusal (`engine_in_use` with the remedy that fits, or
//! `engine_unavailable` for the key).
//!
//! `unknown` is a probe that could not run or ran out of time (LM Studio not
//! running, a listing that failed once the engine answered). A Tensorfold
//! binary that is missing, exits non-zero on `--version` or prints no
//! version it can read is `unavailable`: a dispatch refuses exactly that
//! with `engine_unavailable`.
//!
//! (#3574) An `unavailable` carries the code a dispatch refusing for the same
//! reason carries ([`Availability::refused`]), so an app branches on it, not
//! on the reason's wording: `engine_unavailable` (an unverified or missing
//! engine, an install rec's adapter cannot read, a model LM Studio has not
//! downloaded), `model_start_failed` (`engine_args` the engine does not take,
//! a model Tensorfold does not list), `port_in_use`, and `engine_in_use`
//! (rec's own server, serving another model). A registry problem a
//! dispatch refuses before any engine is asked (no `n_ctx`, no `port`, an
//! endpoint that does not resolve, a refused user file) carries none.
//!
//! Whether a model FITS in memory right now is not availability: that changes
//! with every dispatch, and `dispatch --check`'s `fit` answers it.

use super::server::{self, Dirs};
use super::tensorfold::{Tensorfold, TensorfoldProbe, VERIFIED_RELEASES};
use darkmux_types::client_code::ClientCode;
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
/// and waiting is what keeps a killed probe from outliving the listing. An
/// answer within it is kept.
const REAP_GRACE: Duration = Duration::from_secs(1);

/// Whether darkmux can make a model resident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    /// Loaded or served now, at the profile's context.
    Resident,
    /// Not resident, and darkmux can load it or start its server.
    Startable,
    /// darkmux cannot make it resident; the reason says why.
    Unavailable,
    /// An endpoint darkmux only sends requests to; (#3574) also a Flint
    /// model rec's own server already serves, which a dispatch uses without
    /// darkmux managing it.
    External,
    /// A probe failed or ran out of time; the reason names which. Never read
    /// as `unavailable`. (#3450) Also a state a newer darkmux states that
    /// this one does not know, as a peer's machine card may carry one.
    #[serde(other)]
    Unknown,
}

/// A model's availability and, for every state but `resident`, `startable`
/// and `external`, a one-line reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct Availability {
    pub state: AvailabilityState,
    pub reason: Option<String>,
    /// (#3574) Why, for an app to branch on: the code a dispatch refusing
    /// for the same reason carries (`dispatch.error`'s `payload.code`, the
    /// client API's `ClientCode` spelling), so there is one vocabulary:
    /// `engine_in_use` while rec's own server runs and does not serve the
    /// model, `engine_unavailable`, `port_in_use`, `model_start_failed`.
    /// Absent when no dispatch refusal fits (a registry problem a dispatch
    /// refuses before any engine is asked, a state that is not
    /// `unavailable`). Additive (card schema 1.5): an older reader ignores
    /// it, and a code a newer darkmux sends that this one does not know is
    /// kept as its text.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub code: Option<String>,
}

impl Availability {
    fn is(state: AvailabilityState) -> Self {
        Availability {
            state,
            reason: None,
            code: None,
        }
    }
    fn because(state: AvailabilityState, reason: impl Into<String>) -> Self {
        Availability {
            state,
            reason: Some(reason.into()),
            code: None,
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
    /// `unavailable` for `reason`, with the code a dispatch refusing for
    /// the same reason carries.
    pub fn refused(code: ClientCode, reason: impl Into<String>) -> Self {
        let mut a = Self::unavailable(reason);
        a.code = Some(code.as_str().to_string());
        a
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
        // An answer just past its engine's bound is one whose probe was
        // killed there: what its other probes got still stands.
        accept(&mut each, &by_engine, backend, judged);
        // Only LM Studio's answer reports residents (`EngineAnswer::residents`).
        if residents.is_some() {
            lmstudio_residents = residents;
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

/// Whether a probe's error is its call running out of time (killed at the
/// listing's bound, say): that is `unknown`, never `unavailable`.
fn ran_out_of_time(error: &str) -> bool {
    error.contains(darkmux_gestalt::HostError::TIMED_OUT)
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
    /// Whether its local server (the base URL a dispatch sends to) accepts
    /// a connection. `lms` answers from the app while the server is off, so
    /// a model can be loaded and still not answer a dispatch (#3578).
    pub server_up: bool,
    /// Whether darkmux starts that server when a dispatch needs it (it is
    /// on this machine): a model then reads `startable`, never `resident`,
    /// while the server is off.
    pub server_startable: bool,
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
            let url = darkmux_types::config_access::lmstudio_url();
            let probe = url.clone();
            let server_up = s.spawn(move || server_accepts(&probe));
            LmStudioAnswer {
                server_startable: crate::lms::is_loopback_url(&url),
                server_up: server_up.join().unwrap_or(false),
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

/// Whether anything accepts a connection at a base URL's host and port
/// (`http://localhost:1234`, or `localhost:1234` with no scheme; port 80 or
/// 443 by scheme when none is given). Within about 300 ms per address.
pub fn server_accepts(base_url: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let (scheme, rest) = match base_url.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("http", base_url),
    };
    let authority = rest.split('/').next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let with_port = if authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        authority.to_string()
    } else {
        format!("{authority}:{}", if scheme == "https" { 443 } else { 80 })
    };
    let Ok(addrs) = with_port.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok())
}

/// The reason a model on LM Studio reads `unavailable` while its local
/// server is off. Clients match on its start (`LM Studio's local server is off`).
pub const LMSTUDIO_SERVER_OFF: &str =
    "LM Studio's local server is off, so a dispatch can't reach the model";

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
        if !self.server_up && !self.server_startable {
            return Availability::unavailable(LMSTUDIO_SERVER_OFF);
        }
        let key = darkmux_gestalt::bare_model_key(&model.id).to_string();
        if reuses_darkmuxs_instance(loaded, model, &key, n_ctx) {
            // Off but on this machine: a dispatch starts the server (#3578).
            return if self.server_up { Availability::resident() } else { Availability::startable() };
        }
        self.by_catalog(&key)
    }

    fn residents(&self) -> Option<Vec<LoadedModel>> {
        self.loaded.as_ref().ok().cloned()
    }
}

impl LmStudioAnswer {
    /// Not resident: `startable` when LM Studio has the model downloaded.
    fn by_catalog(&self, key: &str) -> Availability {
        match &self.catalog {
            Err(e) => {
                Availability::unknown(format!("LM Studio did not list its downloaded models: {e}"))
            }
            Ok(models) if models.iter().any(|m| m.answers_to(key)) => Availability::startable(),
            // The wave planner's `UnknownModelKey`: `engine_unavailable`.
            Ok(_) => Availability::refused(
                ClientCode::EngineUnavailable,
                format!("LM Studio has no downloaded model `{key}`"),
            ),
        }
    }
}

/// The planner's own residency rule: darkmux's instance of the model, at
/// the profile's context or more, is what a dispatch reuses. A copy
/// someone else loaded is never one (contract 4).
fn reuses_darkmuxs_instance(loaded: &[LoadedModel], model: &ProfileModel, key: &str, n_ctx: u32) -> bool {
    let placement = darkmux_gestalt::Placement {
        model_key: key.to_string(),
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
    matches!(
        darkmux_gestalt::decide_residency(&residents, &placement),
        darkmux_gestalt::ResidencyDecision::Reuse { .. }
    )
}

// ─── Tensorfold ──────────────────────────────────────────────────────────

/// What the Tensorfold binary said about itself (the facts `darkmux doctor`
/// reads), and where darkmux's server records are.
#[derive(Debug, Clone)]
pub struct TensorfoldAnswer {
    pub tf: Tensorfold,
    pub facts: TensorfoldProbe,
    pub dirs: Dirs,
}

impl TensorfoldAnswer {
    pub fn ask(tf: Tensorfold, dirs: Dirs) -> Self {
        let facts = TensorfoldProbe::probe(&tf);
        TensorfoldAnswer { tf, facts, dirs }
    }
}

impl TensorfoldAnswer {
    /// Why a dispatch would refuse `model` before it looks for a server
    /// (`None`: it would not): an unverified or silent binary, or
    /// `engine_args` it does not take, `unavailable`; flags it could not
    /// list, `unknown`.
    fn dispatch_problem(&self, model: &ProfileModel) -> Option<Availability> {
        if let Some(problem) = self.version_problem() {
            return Some(problem);
        }
        match self.facts.args_problem(model) {
            Err(e) => Some(Availability::unknown(format!("Tensorfold {e}"))),
            Ok(Some(why)) => Some(Availability::refused(
                ClientCode::ModelStartFailed,
                format!("engine_args: {why}"),
            )),
            Ok(None) => None,
        }
    }

    /// Why the engine could not start a server for `model` (`None`: it
    /// could): a model it does not have, `unavailable`; a listing that
    /// failed once the binary answered, `unknown`.
    fn model_problem(&self, model: &ProfileModel) -> Option<Availability> {
        match self.facts.has_model(model) {
            Err(e) => Some(Availability::unknown(format!("Tensorfold {e}"))),
            // The start a dispatch makes fails: `model_start_failed`.
            Ok(false) => Some(Availability::refused(
                ClientCode::ModelStartFailed,
                format!("`{} models` does not list `{}`", self.tf.label(), model.id),
            )),
            Ok(true) => None,
        }
    }
}

impl TensorfoldAnswer {
    /// The binary is missing, silent, or at a release darkmux was not
    /// verified against: `unavailable`, naming which.
    fn version_problem(&self) -> Option<Availability> {
        match &self.facts.version {
            Err(e) if ran_out_of_time(e) => Some(Availability::unknown(format!(
                "the Tensorfold binary did not answer in time: {e}"
            ))),
            Err(e) => Some(Availability::refused(
                ClientCode::EngineUnavailable,
                format!("the Tensorfold binary did not answer: {e}"),
            )),
            Ok(v) if !v.is_verified() => Some(Availability::refused(
                ClientCode::EngineUnavailable,
                format!("Tensorfold {v} is installed; this darkmux supports Tensorfold {VERIFIED_RELEASES}"),
            )),
            Ok(_) => None,
        }
    }
}

impl EngineAnswer for TensorfoldAnswer {
    /// `resident` when a dispatch would reuse darkmux's server on the
    /// endpoint: the engine is a verified release that takes the model's
    /// `engine_args` (what a dispatch checks first), and the server runs
    /// this model's exact command and answers; `startable` when the engine
    /// also has the model, and the port is free or held by darkmux's own
    /// server.
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
        if let Some(problem) = self.dispatch_problem(model) {
            return problem;
        }
        let live = server::running_now(&self.dirs, id);
        if live
            .as_ref()
            .is_some_and(|r| server::serves(r, &argv, port, &served_name))
        {
            return Availability::resident();
        }
        if let Some(problem) = self.model_problem(model) {
            return problem;
        }
        if live.is_none() && foreign_on_port(&self.dirs, id, port) {
            return foreign_port(port, id);
        }
        Availability::startable()
    }
}

/// A port something darkmux did not start holds: the server lifecycle's
/// `port_in_use`.
fn foreign_port(port: u16, id: &str) -> Availability {
    Availability::refused(
        ClientCode::PortInUse,
        format!("something darkmux did not start answers on 127.0.0.1:{port}, the port of endpoint `{id}`"),
    )
}

/// Whether something darkmux did not start answers on `port`, with no live
/// server of darkmux's on endpoint `id`: workers darkmux's exited server
/// left there are darkmux's, which a dispatch clears or reuses.
fn foreign_on_port(dirs: &Dirs, id: &str, port: u16) -> bool {
    server::port_taken(port) && server::left_behind(dirs, id).is_none()
}

// ─── Flint (#3453) ───────────────────────────────────────────────────────

/// What rec said (`rec --version`, verified, and `rec status`), with its
/// install read where the adapter reads it, and darkmux's server records.
#[derive(Debug, Clone)]
pub struct FlintAnswer {
    pub rec: super::flint::Rec,
    /// The verified version, or why not (missing, unreadable, unverified).
    pub version: Result<String, String>,
    /// What `rec status` says of rec's own server, when it is up or
    /// starting.
    pub own_server: Result<Option<super::flint::OwnServer>, String>,
    /// (#3574) Whether `rec auth token` gave a key, asked only while rec's
    /// own server is up: a dispatch that would use that server needs it.
    /// The key itself is dropped at once, never kept. `None`: not asked.
    pub key: Option<Result<(), String>>,
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
        // Asked after `rec status`, and only while rec's own server is up.
        let key = crate::gestalt_host::lms_host::with_deadline_cap(cap, || own_server_key(&rec, &own_server));
        FlintAnswer {
            rec,
            version,
            own_server,
            key,
            dirs,
        }
    }

    /// What rec itself says of the model before darkmux's own server is
    /// looked at (`None`: nothing, so darkmux's server decides): an
    /// unverified or silent rec, then what its own server means for the
    /// model ([`Self::own_server_verdict`]).
    fn rec_verdict(
        &self,
        model: &ProfileModel,
        darkmuxs: Option<&darkmux_types::engine_record::EngineRecord>,
    ) -> Option<Availability> {
        self.version_problem()
            .or_else(|| self.own_server_verdict(model, darkmuxs))
    }

    /// An unverified rec (`engine_unavailable`), or one that did not answer
    /// in time (`unknown`).
    fn version_problem(&self) -> Option<Availability> {
        match &self.version {
            Err(e) if ran_out_of_time(e) => Some(Availability::unknown(format!(
                "rec did not answer in time: {e}"
            ))),
            Err(e) => Some(Availability::refused(
                ClientCode::EngineUnavailable,
                e.clone(),
            )),
            Ok(_) => None,
        }
    }

    /// (#3574) rec's own server, judged as a dispatch judges it
    /// ([`super::flint::own_server_use`]): `external` when it serves the
    /// model and `rec auth token` gives the key a dispatch sends
    /// (`engine_unavailable` when it does not), `unavailable`
    /// (`engine_in_use`) when it is up and does not serve the model,
    /// `unknown` when `rec status` could not say, `None` when it is not up.
    fn own_server_verdict(
        &self,
        model: &ProfileModel,
        darkmuxs: Option<&darkmux_types::engine_record::EngineRecord>,
    ) -> Option<Availability> {
        let own = match &self.own_server {
            Err(e) => {
                return Some(Availability::unknown(format!(
                    "rec could not say whether its own server runs: {e}"
                )))
            }
            Ok(own) => own.as_ref(),
        };
        match keyed_own_server_use(own, model, darkmuxs, self.key.as_ref()) {
            Err(e) => Some(Availability::refused(ClientCode::EngineUnavailable, e)),
            Ok(super::flint::OwnServerUse::Free) => None,
            Ok(super::flint::OwnServerUse::Reuse { .. }) => Some(Availability::external()),
            Ok(super::flint::OwnServerUse::Refuse {
                address,
                why,
                remedy,
            }) => Some(Availability::refused(
                ClientCode::EngineInUse,
                super::flint::own_server_refusal(&address, &why, remedy),
            )),
        }
    }
}

/// (#3574) Whether `rec auth token` gives a key, asked only while rec's own
/// server is up (`own_server`, what `rec status` said): a dispatch that uses
/// that server sends the key with every request. The key itself is dropped
/// at once, never kept. `None`: not asked. A listing and doctor's
/// `flint engine` row both ask it here.
pub fn own_server_key(
    rec: &super::flint::Rec,
    own_server: &Result<Option<super::flint::OwnServer>, String>,
) -> Option<Result<(), String>> {
    matches!(own_server, Ok(Some(_)))
        .then(|| rec.auth_token().map(drop).map_err(|e| format!("{e:#}")))
}

/// (#3574) What rec's own server means for `model` as a dispatch decides
/// it: [`super::flint::own_server_use`], with a reuse refused when `rec auth
/// token` gave no key (`key`, from [`own_server_key`]), since a dispatch
/// refuses it `engine_unavailable`. `Err` is that refusal's reason. The one
/// decision a listing and doctor read; a dispatch makes the same one in
/// `darkmux_crew::engine_dispatch`, which reads the key itself to send it.
pub fn keyed_own_server_use(
    own: Option<&super::flint::OwnServer>,
    model: &ProfileModel,
    darkmuxs: Option<&darkmux_types::engine_record::EngineRecord>,
    key: Option<&Result<(), String>>,
) -> Result<super::flint::OwnServerUse, String> {
    let decided = super::flint::own_server_use(own, model, darkmuxs);
    match (&decided, key) {
        (super::flint::OwnServerUse::Reuse { .. }, Some(Err(e))) => Err(e.clone()),
        _ => Ok(decided),
    }
}

impl EngineAnswer for FlintAnswer {
    /// `external` when rec's own server serves the model (#3574, judged
    /// after the install, as a dispatch judges it); `resident` when a
    /// dispatch would reuse darkmux's server on the endpoint: rec is the
    /// verified release, its own server is not running, and darkmux's
    /// server runs this model's command, answers and listens; `startable`
    /// when rec also has exactly one runtime and one model directory for the
    /// revision, and the port is free or darkmux's.
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
        // `Rec::plan` refuses an install it cannot read `engine_unavailable`.
        let spec = match self.rec.serve_spec(id, port, model, version) {
            Ok(spec) => spec,
            Err(e) => {
                return Availability::refused(ClientCode::EngineUnavailable, format!("{e:#}"))
            }
        };
        let live = server::running_now(&self.dirs, id);
        if let Some(verdict) = self.rec_verdict(model, live.as_ref()) {
            return verdict;
        }
        if live
            .as_ref()
            .is_some_and(|r| server::serves(r, &spec.argv, port, &spec.served_name))
        {
            return Availability::resident();
        }
        if live.is_none() && foreign_on_port(&self.dirs, id, port) {
            return foreign_port(port, id);
        }
        Availability::startable()
    }
}

#[cfg(test)]
#[path = "availability_tests.rs"]
mod tests;

//! (#3448) A dispatch to a model on an engine darkmux starts servers for
//! (DESIGN.md, "Engine servers darkmux starts"): Tensorfold, and rec's Flint
//! (#3453).
//!
//! Called by each dispatch path after its `dispatch.start` and before its
//! first request, [`prepare`] makes the model resident:
//!
//! 1. The engine is installed at a release darkmux was verified against, or
//!    the dispatch fails `engine_unavailable`. A server the engine started
//!    itself (rec's own, which holds rec's single-instance lock) fails it
//!    `engine_in_use`: darkmux never stops or takes over one.
//!    A Tensorfold model's `engine_args` are checked against the installed
//!    engine's own flags first, as doctor and a listing check them, and
//!    refused `model_start_failed` before anything starts.
//! 2. When darkmux's server on the endpoint already runs this exact command,
//!    it is reused and nothing else happens.
//! 3. Otherwise darkmux makes room: while the machine's reclaimable memory is
//!    below the model's memory floor (the engine's own `info` figure),
//!    it stops its own idle engine servers on other endpoints, oldest first,
//!    then unloads its own idle LM Studio `darkmux:` loads. A resident a
//!    running dispatch holds (a residency lease) is never touched: the
//!    leases are read again before each eviction, and a server's under its
//!    endpoint's lock, where it is stopped. When nothing is left to free,
//!    the dispatch fails `busy` (something darkmux holds would free it
//!    later) or `wont_fit`.
//! 4. It starts the server ([`darkmux_profiles::engine::server::ensure`]) and
//!    waits until it answers, failing `port_in_use` or `model_start_failed`.
//!    Whether a running dispatch holds darkmux's server on the endpoint is
//!    asked under the endpoint's lock, with the decision to stop it: one
//!    that is held is reused when it runs the same command (however slowly
//!    it answers) and otherwise refused `busy`, never stopped.
//!
//! The dispatch's lease names the served model for as long as the returned
//! [`EngineReady`] lives, so no other darkmux process evicts it mid-run.
//! Every step is reported through `note` as the payload of a
//! `dispatch.model.*` record.
//!
//! (#3504) A `busy` dispatch follows `fleet.busy_policy` ([`BusyWait`]):
//! `refuse` (the default) returns the refusal at once; `queue` waits and
//! tries again from step 1, so every retry re-reads the leases and decides
//! under the endpoint's lock as the first try did, and a model a running
//! dispatch holds is never evicted however long the wait. The wait reuses
//! the endpoint budget's machinery ([`crate::budget::BudgetEnv`]): bounded
//! sleeps that notice a stop (Ctrl-C or a client's cancel, `mission abort`,
//! an abandoned phase), the pause recorded in [`darkmux_types::run_pause`]
//! so the run's wall-clock bound extends, and a frozen clock in tests. It
//! is bounded by the dispatch's timeout and ends `busy` when that passes.
//! While it waits it says so every [`QUEUED_EVERY`], on the CLI and as a
//! `dispatch.queue.wait` record, and writes `dispatch.queue.resume` when it
//! goes on. `wont_fit` never waits: no wait can make it fit.

use std::time::Duration;

use crate::budget::{BudgetCaller, BudgetEnv};
use darkmux_flow::payload::DispatchModelPayload;
use darkmux_types::config::BusyPolicy;
use darkmux_profiles::engine::flint::{self, Rec};
use darkmux_profiles::engine::server::{self, Dirs, Progress, ServerSpec};
use darkmux_profiles::engine::tensorfold::Tensorfold;
use darkmux_types::engine_record::{self, EngineRecord};
use darkmux_types::residency_lease::{LeaseGuard, Reservation};
use darkmux_types::{EndpointKind, ManagedBackend};

/// Which `dispatch.model.*` (or, #3504, `dispatch.queue.*`) record a note is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    Start,
    Loading,
    Ready,
    /// `dispatch.queue.wait`: waiting for a busy endpoint or memory.
    Queued,
    /// `dispatch.queue.resume`: the wait ended.
    Resumed,
}

/// Why a dispatch could not make its model resident, with the stable code
/// an app branches on (`dispatch.error`'s `payload.code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRefusal {
    pub code: &'static str,
    pub message: String,
    /// (#3504) For `busy`: what the dispatch would wait on, without the
    /// advice the message ends with.
    pub waiting_on: Option<String>,
}

impl std::fmt::Display for EngineRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "darkmux dispatch: {} [{}]", self.message, self.code)
    }
}

impl std::error::Error for EngineRefusal {}

pub(crate) fn refuse(code: &'static str, message: impl Into<String>) -> EngineRefusal {
    EngineRefusal { code, message: message.into(), waiting_on: None }
}

/// `busy`: `waiting_on` is held by a running dispatch; the message ends with
/// `advice`.
fn busy(waiting_on: String, advice: &str) -> EngineRefusal {
    EngineRefusal { code: "busy", message: format!("{waiting_on}; {advice}"), waiting_on: Some(waiting_on) }
}

/// A server lifecycle failure as the refusal it is, its `busy` naming what
/// holds the endpoint.
fn server_refusal(e: server::ServerError) -> EngineRefusal {
    match e {
        server::ServerError::Busy { endpoint, model } => {
            busy(format!("endpoint `{endpoint}` is serving `{model}` for a running dispatch"), "retry when it finishes")
        }
        other => refuse(other.code(), other.to_string()),
    }
}

/// (#3504) Why [`prepare`] made no model resident: a refusal carrying its
/// code, or a queued wait its run's stop ended (no code: an operator's stop
/// is not a refusal, and the stop is named on the record by its own rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotResident {
    Refused(EngineRefusal),
    Stopped(String),
}

impl NotResident {
    /// The code `dispatch.error` carries; `None` for a stopped wait.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            NotResident::Refused(r) => Some(r.code),
            NotResident::Stopped(_) => None,
        }
    }

    /// The sentence for a person.
    pub fn message(&self) -> &str {
        match self {
            NotResident::Refused(r) => &r.message,
            NotResident::Stopped(why) => why,
        }
    }

    /// As the dispatch's error: a refusal stays readable by [`refusal_code`].
    pub fn into_error(self) -> anyhow::Error {
        match self {
            NotResident::Refused(r) => anyhow::Error::from(r),
            NotResident::Stopped(why) => anyhow::anyhow!(why),
        }
    }
}

/// (#3504) What a `busy` dispatch does: this machine's `fleet.busy_policy`,
/// bounded by the dispatch's own timeout, on behalf of `caller` (whose run's
/// stop ends the wait).
pub struct BusyWait<'a> {
    pub policy: BusyPolicy,
    /// The longest the dispatch waits; `None` for a timeout of `0`, which on
    /// a darkmux bound means unbounded.
    pub bound: Option<Duration>,
    pub caller: BudgetCaller<'a>,
}

impl<'a> BusyWait<'a> {
    /// The policy this machine is configured with, and `timeout_secs` as the
    /// bound. A value the policy does not know never reaches here: every
    /// entry point that dispatches refuses it at its preflight (contract 9);
    /// a config edited to one mid-run refuses, as the default does.
    pub fn configured(timeout_secs: u64, caller: BudgetCaller<'a>) -> Self {
        BusyWait {
            policy: darkmux_types::config_access::fleet_busy_policy().unwrap_or(BusyPolicy::Refuse),
            bound: (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs)),
            caller,
        }
    }
}

/// (#3504) How often a queued dispatch says it still waits: a CLI line and
/// a `dispatch.queue.wait` record, so the wait never looks like a stall.
pub const QUEUED_EVERY: Duration = Duration::from_secs(20);
/// How often a queued dispatch tries again. Each try reads the leases and
/// the engine's records again (and LM Studio's residents, when memory is
/// short), so it is not a tight loop.
const QUEUE_RETRY: Duration = Duration::from_secs(5);

/// The model resident and answering; holds the dispatch's lease on it.
pub struct EngineReady {
    pub record: EngineRecord,
    pub started: bool,
    _lease: LeaseGuard,
}

/// Whether `target`'s model runs on a server darkmux starts.
pub fn is_engine_server(target: &crate::target::Target) -> bool {
    target.kind.starts_servers()
}

/// What [`prepare`] asks of the engine it starts a server for, one
/// implementation per engine.
pub(crate) trait ServerEngine {
    /// The command for `model` on `endpoint`, refused with the code an app
    /// branches on (`engine_unavailable` for an engine darkmux cannot run).
    fn plan(&self, endpoint: &str, port: u16, model: &darkmux_types::ProfileModel) -> Result<ServerSpec, EngineRefusal>;
    /// The memory the model needs, when the engine says.
    fn memory_need(&self, model: &darkmux_types::ProfileModel) -> Option<u64>;
    /// The address of a server the engine started itself (not darkmux),
    /// when one is up and it allows only one.
    fn own_server(&self) -> Result<Option<String>, EngineRefusal>;
}

fn unavailable(e: anyhow::Error) -> EngineRefusal {
    refuse("engine_unavailable", format!("{e:#}"))
}

impl ServerEngine for Tensorfold {
    fn plan(&self, endpoint: &str, port: u16, model: &darkmux_types::ProfileModel) -> Result<ServerSpec, EngineRefusal> {
        let version = self.verified_version().map_err(unavailable)?;
        // The installed engine's own flags, as doctor and a listing check
        // them: a flag it does not take would only fail the start later.
        if !model.engine_args.is_empty() {
            let flags = self.serve_flags().map_err(|e| unavailable(e.context("reading the flags its `serve` takes")))?;
            flags.check(&model.engine_args).map_err(|why| refuse("model_start_failed", format!("engine_args: {why}")))?;
        }
        self.serve_spec(endpoint, port, model, version).map_err(|e| refuse("model_start_failed", format!("{e:#}")))
    }
    fn memory_need(&self, model: &darkmux_types::ProfileModel) -> Option<u64> {
        self.info(&model.id).map(|i| i.memory_floor_bytes).ok()
    }
    /// Tensorfold runs as many servers as it is asked for.
    fn own_server(&self) -> Result<Option<String>, EngineRefusal> {
        Ok(None)
    }
}

/// (#3453) rec's Flint: paths from rec's own install, verified release only.
impl ServerEngine for Rec {
    fn plan(&self, endpoint: &str, port: u16, model: &darkmux_types::ProfileModel) -> Result<ServerSpec, EngineRefusal> {
        let version = self.verified_version().map_err(unavailable)?;
        self.serve_spec(endpoint, port, model, &version).map_err(unavailable)
    }
    /// The weights on disk: rec says nothing of a model's memory.
    fn memory_need(&self, model: &darkmux_types::ProfileModel) -> Option<u64> {
        self.model_dir(&model.id).ok().and_then(|d| flint::weights_bytes(&d))
    }
    fn own_server(&self) -> Result<Option<String>, EngineRefusal> {
        Rec::own_server(self).map_err(unavailable)
    }
}

/// The `code` of a dispatch failure that [`prepare`] refused, if it was one:
/// read off the error anywhere in its chain, or off a crew-of-one run's
/// error that carries it ([`RefusedRun`]).
pub fn refusal_code(err: &anyhow::Error) -> Option<&'static str> {
    err.chain().find_map(|e| {
        e.downcast_ref::<EngineRefusal>().map(|r| r.code).or_else(|| e.downcast_ref::<RefusedRun>().map(|r| r.code))
    })
}

/// (#3505) A crew-of-one dispatch's error when its role execution was
/// refused: the step graph hands the error back as its text, so the code
/// rides beside it ([`stash_refusal`]) to be read by [`refusal_code`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedRun {
    pub code: &'static str,
    /// The error as the run reports it.
    pub text: String,
}

impl std::fmt::Display for RefusedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::error::Error for RefusedRun {}

/// Refusal codes of crew-of-one dispatches between their step and the run,
/// by session.
fn stashed() -> &'static std::sync::Mutex<std::collections::HashMap<String, &'static str>> {
    static STASH: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, &'static str>>> =
        std::sync::OnceLock::new();
    STASH.get_or_init(Default::default)
}

/// (#3505) Keep `err`'s refusal code for `session`'s run to read back
/// ([`take_refusal`]); nothing when `err` carries none.
pub(crate) fn stash_refusal(session: &darkmux_types::session_id::SessionId, err: &anyhow::Error) {
    if let Some(code) = refusal_code(err) {
        stashed().lock().unwrap_or_else(|p| p.into_inner()).insert(session.wire(), code);
    }
}

/// (#3505) The refusal code stashed for `session`, removed.
pub(crate) fn take_refusal(session: &darkmux_types::session_id::SessionId) -> Option<&'static str> {
    stashed().lock().unwrap_or_else(|p| p.into_inner()).remove(&session.wire())
}

/// Make `target`'s model resident on its engine server, a `busy` model
/// waiting as `wait` says. See the module doc.
pub fn prepare(
    target: &crate::target::Target,
    wait: &BusyWait<'_>,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<EngineReady, NotResident> {
    let (dirs, bounds) = (Dirs::under_home(), server::Bounds::default());
    let engine: Box<dyn ServerEngine> = match target.kind {
        EndpointKind::Managed(ManagedBackend::Tensorfold) => Box::new(Tensorfold::installed()),
        EndpointKind::Managed(ManagedBackend::Flint) => Box::new(Rec::installed()),
        EndpointKind::Managed(_) | EndpointKind::Unmanaged => {
            return Err(NotResident::Refused(refuse("model_start_failed", "darkmux starts no server for this model's endpoint")))
        }
    };
    let base = queue_payload(target);
    crate::budget::with_env(|env| {
        let machine = ThisMachine { profiles_file: wait.caller.profiles_file };
        wait_while_busy(wait, env, &base, &mut |note| prepare_in(target, &dirs, engine.as_ref(), bounds, &machine, note), note)
    })
}

/// What a `dispatch.queue.*` record names: the engine, endpoint and model.
fn queue_payload(target: &crate::target::Target) -> DispatchModelPayload {
    use darkmux_types::config_enum::ConfigEnum;
    DispatchModelPayload {
        engine: match target.kind {
            EndpointKind::Managed(b) => b.token().to_string(),
            EndpointKind::Unmanaged => String::new(),
        },
        endpoint: target.endpoint.named_id().unwrap_or_default().to_string(),
        model: target.model.id.clone(),
        ..Default::default()
    }
}

/// One try at making the model resident, reporting through the note it is given.
type Attempt<'a, T> = dyn FnMut(&mut dyn FnMut(Note, DispatchModelPayload)) -> Result<T, EngineRefusal> + 'a;

/// (#3504) Run `attempt`; while it is `busy` and the policy is `queue`, wait
/// and try again, until it is not busy, the bound passes (`busy`, saying how
/// long it waited) or the caller's run is stopped (nothing is tried after
/// the stop is seen).
fn wait_while_busy<T>(
    wait: &BusyWait<'_>,
    env: &dyn BudgetEnv,
    base: &DispatchModelPayload,
    attempt: &mut Attempt<'_, T>,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<T, NotResident> {
    let mut q = Waiting { wait, env, base, bound_ms: wait.bound.map(|b| b.as_millis() as u64), waited_ms: 0, said_at: None };
    loop {
        if let Some(stopped) = q.stopped() {
            return Err(stopped);
        }
        let refusal = match q.try_once(attempt, note) {
            Ok(done) => return done,
            Err(busy) => busy,
        };
        let waiting_on = refusal.waiting_on.unwrap_or(refusal.message);
        if let Some(timed_out) = q.timed_out(&waiting_on) {
            return Err(timed_out);
        }
        q.say_if_due(waiting_on, note);
        q.sleep();
    }
}

/// A queued dispatch's wait so far ([`wait_while_busy`]).
struct Waiting<'a> {
    wait: &'a BusyWait<'a>,
    env: &'a dyn BudgetEnv,
    base: &'a DispatchModelPayload,
    bound_ms: Option<u64>,
    waited_ms: u64,
    /// When it last said it waits (`waited_ms`); `None` before the first.
    said_at: Option<u64>,
}

impl Waiting<'_> {
    /// Its run's stop, seen after it has waited: nothing is tried after it.
    fn stopped(&self) -> Option<NotResident> {
        let why = self.env.stop_reason(&self.wait.caller).filter(|_| self.waited_ms > 0)?;
        Some(NotResident::Stopped(format!(
            "darkmux dispatch: stopped waiting for `{}` ({why}); nothing was started or evicted",
            self.base.model
        )))
    }

    /// One try: `Ok` with its outcome when it is done (anything but a busy
    /// it should wait out), `Err` with the busy refusal otherwise. A wait
    /// that ends says so before the try's own records (its start), at the
    /// try's first record, or after it when it wrote none.
    fn try_once<T>(
        &self,
        attempt: &mut Attempt<'_, T>,
        note: &mut dyn FnMut(Note, DispatchModelPayload),
    ) -> Result<Result<T, NotResident>, EngineRefusal> {
        let mut resumed = self.said_at.is_none();
        let tried = attempt(&mut |n, p| {
            if !resumed {
                resumed = true;
                self.resume(note);
            }
            note(n, p)
        });
        match tried {
            Err(r) if r.code == "busy" && self.wait.policy == BusyPolicy::Queue => Err(r),
            done => {
                if !resumed {
                    self.resume(note);
                }
                Ok(done.map_err(NotResident::Refused))
            }
        }
    }

    /// `dispatch.queue.resume`, and its line.
    fn resume(&self, note: &mut dyn FnMut(Note, DispatchModelPayload)) {
        self.env.say(&format!(
            "darkmux dispatch: `{}` is no longer waiting after {}; going on",
            self.base.model,
            crate::budget::human_duration(self.waited_ms / 1000)
        ));
        note(Note::Resumed, DispatchModelPayload { elapsed_ms: Some(self.waited_ms), ..self.base.clone() });
    }

    /// `busy`, saying how long it waited, once it has waited its bound.
    fn timed_out(&self, waiting_on: &str) -> Option<NotResident> {
        let bound = self.bound_ms.filter(|b| self.waited_ms >= *b)?;
        Some(NotResident::Refused(busy(
            waiting_on.to_string(),
            &format!(
                "still busy after waiting {}, this dispatch's timeout, so retry when it finishes",
                crate::budget::human_duration(bound / 1000)
            ),
        )))
    }

    /// The `queued` line and a `dispatch.queue.wait` record, when the wait
    /// starts and every [`QUEUED_EVERY`] after.
    fn say_if_due(&mut self, waiting_on: String, note: &mut dyn FnMut(Note, DispatchModelPayload)) {
        let every = QUEUED_EVERY.as_millis() as u64;
        if self.said_at.is_some_and(|at| self.waited_ms < at + every) {
            return;
        }
        self.env.say(&queued_line(&waiting_on, self.waited_ms, self.bound_ms, &self.wait.caller));
        note(
            Note::Queued,
            DispatchModelPayload {
                elapsed_ms: Some(self.waited_ms),
                waiting_on: Some(waiting_on),
                wait_bound_ms: self.bound_ms,
                ..self.base.clone()
            },
        );
        self.said_at = Some(self.waited_ms);
    }

    /// Sleep to the next try, the next line or the bound, whichever is
    /// first, recording the pause.
    fn sleep(&mut self) {
        let every = QUEUED_EVERY.as_millis() as u64;
        let next_line = self.said_at.map_or(every, |at| at + every).saturating_sub(self.waited_ms);
        let left = self.bound_ms.map_or(u64::MAX, |b| b - self.waited_ms);
        let span = (QUEUE_RETRY.as_millis() as u64).min(next_line).min(left).max(1);
        let slept = crate::budget::sleep_sliced(self.env, Duration::from_millis(span), &self.wait.caller);
        self.waited_ms = self.waited_ms.saturating_add(slept);
        self.env.paused(slept);
    }
}

/// The `queued` line: what the dispatch waits on, for how long so far and
/// at most, and how to end the wait.
fn queued_line(waiting_on: &str, waited_ms: u64, bound_ms: Option<u64>, caller: &BudgetCaller<'_>) -> String {
    let human = |ms: u64| crate::budget::human_duration(ms / 1000);
    let most = match bound_ms {
        Some(b) => format!("waiting up to {} (its timeout)", human(b)),
        None => "waiting with no bound (its timeout is 0)".to_string(),
    };
    let stop = match caller.session.mission_id() {
        Some(mid) => format!("Ctrl-C or `darkmux mission abort {mid}` ends the wait"),
        None => "Ctrl-C ends the wait".to_string(),
    };
    format!(
        "darkmux dispatch: queued: {waiting_on}. fleet.busy_policy is `queue`, so the dispatch is {most}, {} so far; \
         nothing it needs is evicted while another dispatch holds it. {stop}, starting nothing.",
        human(waited_ms)
    )
}

/// What [`prepare`] asks of the machine beyond the engine server itself, so
/// a test never reads the real memory or touches the real LM Studio.
pub(crate) trait Machine {
    /// Bytes the OS could hand out now, reclaimable cache included.
    fn reclaimable(&self) -> Option<u64>;
    fn footprint(&self, pid: u32) -> Option<u64>;
    /// darkmux's own (`darkmux:`) LM Studio residents.
    fn lmstudio_residents(&self) -> Vec<String>;
    fn lmstudio_unload(&self, identifier: &str) -> anyhow::Result<()>;
    /// Served model names every OTHER darkmux dispatch is using: other
    /// processes', and other dispatches' in this one (#2663), never `own`'s.
    fn leased(&self, own: &LeaseGuard) -> Vec<String>;
    /// The machine's memory, for a model that could never fit.
    fn capacity(&self) -> Option<u64>;
    /// (#3509) The memory other dispatches' starts still in flight reserve.
    fn reserved(&self, own: &LeaseGuard) -> u64 {
        own.reserved_by_others()
    }
    /// (#3507) The standing utility binding's LM Studio identifier, held like
    /// a leased model: never evicted to make room.
    fn utility(&self) -> Option<String> {
        None
    }
    /// (#3509) Keep a started server's reservation until its memory pages
    /// in ([`settle_reservation`]), on a thread of its own: the dispatch
    /// goes on at once.
    fn settle(&self, res: Reservation, pid: u32, need: u64) {
        std::thread::spawn(move || {
            settle_reservation(
                res,
                need,
                &|| darkmux_platform::current().process_footprint(pid),
                &std::thread::sleep,
                SETTLE_BOUND,
            )
        });
    }
}

/// (#3509) How long a started server's reservation may outlive its answer
/// while its memory pages in. A server answers before it has paged in all
/// its weights (MLX maps them and touches them on first use), so the
/// reservation is kept until the server's footprint reaches the need, and
/// at most this long: two minutes covers the first request of a large
/// model reading from disk, and bounds the time a footprint that never
/// reaches the floor (its margin is never all resident) keeps a phantom
/// reservation that refuses other starts as `busy`.
pub(crate) const SETTLE_BOUND: Duration = Duration::from_secs(120);
/// How often the reservation is shrunk to what has not paged in yet.
const SETTLE_POLL: Duration = Duration::from_millis(500);

/// Shrink `res` to what of `need` the server's `footprint` has not reached,
/// each [`SETTLE_POLL`], until it reaches it or `bound` passes; then end it.
/// An unreadable footprint keeps the whole need. Stops at once when the
/// dispatch's lease is gone.
pub(crate) fn settle_reservation(
    res: Reservation,
    need: u64,
    footprint: &dyn Fn() -> Option<u64>,
    sleep: &dyn Fn(Duration),
    bound: Duration,
) {
    let mut waited = Duration::ZERO;
    while waited < bound {
        let left = footprint().map_or(need, |f| need.saturating_sub(f));
        if left == 0 || !res.set(left) {
            break;
        }
        sleep(SETTLE_POLL);
        waited += SETTLE_POLL;
    }
    res.set(0);
}

pub(crate) struct ThisMachine<'a> {
    /// The registry the dispatch resolved from, for its utility binding.
    pub(crate) profiles_file: Option<&'a str>,
}

impl Machine for ThisMachine<'_> {
    fn utility(&self) -> Option<String> {
        crate::concurrent_dispatch::standing_utility_binding(self.profiles_file)
    }
    fn reclaimable(&self) -> Option<u64> {
        darkmux_types::residency_lock::reclaimable_bytes()
    }
    fn footprint(&self, pid: u32) -> Option<u64> {
        darkmux_platform::current().process_footprint(pid)
    }
    fn lmstudio_residents(&self) -> Vec<String> {
        darkmux_profiles::engine::lmstudio()
            .loaded()
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.identifier)
            .filter(|id| darkmux_gestalt::is_darkmux_owned(id))
            .collect()
    }
    fn lmstudio_unload(&self, identifier: &str) -> anyhow::Result<()> {
        darkmux_profiles::engine::lmstudio()
            .unload(identifier)
            .or_else(|e| if darkmux_profiles::engine::is_not_resident(&e) { Ok(()) } else { Err(e) })
    }
    fn leased(&self, own: &LeaseGuard) -> Vec<String> {
        own.all_live_leased_models()
    }
    fn capacity(&self) -> Option<u64> {
        darkmux_platform::current().ram_installed_bytes()
    }
}

fn prepare_in(
    target: &crate::target::Target,
    dirs: &Dirs,
    engine: &dyn ServerEngine,
    bounds: server::Bounds,
    machine: &dyn Machine,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<EngineReady, EngineRefusal> {
    let endpoint = target.endpoint.named_id().unwrap_or_default().to_string();
    let port = target.endpoint.port.unwrap_or_default();
    let spec = engine.plan(&endpoint, port, &target.model)?;
    refuse_engines_own_server(engine, dirs, &endpoint)?;
    // (#3504) No lease is written until this dispatch has decided to run:
    // a dispatch that turns out `busy`, and so may queue, holds nothing
    // another dispatch would read as held (every Flint server answers
    // `flint`, so a waiter's lease would pin them all).
    let lease = LeaseGuard::acquire();
    // A reuse decides nothing about memory and takes no lock. (#3511) It is
    // decided under the endpoint's lock and never starts anything: a server
    // judged reusable and then lost (an evictor stopped it in between) is
    // started below, through the memory decision.
    if server::running(dirs, &endpoint).is_some_and(|r| r.argv == spec.argv) {
        write_lease(&lease, &spec.served_name, None)?;
        let held = |r: &EngineRecord| held_now(machine, &lease).contains(&served_name_of(r));
        if let Some(record) = server::reuse(&spec, dirs, bounds, &held, &mut |_| {}).map_err(server_refusal)? {
            let _ = lease.mark_loaded(std::slice::from_ref(&spec.served_name));
            return Ok(EngineReady { record, started: false, _lease: lease });
        }
    }
    start_server(&spec, engine.memory_need(&target.model), dirs, bounds, machine, lease, note)
}

/// Start `spec`'s server, which needs `need` bytes. (#3509) Decided under
/// both the machine-wide residency lock and the endpoint's own lock, taken
/// in that order ([`take_locks`]): the endpoint is read again under them, so
/// a start another dispatch made in between is reused (the same command) or
/// refused `busy` (another), never decided on a stale look. The residency
/// lock is never held while waiting on the endpoint's lock, nor past the
/// spawn: from then on the start's reservation counts it.
fn start_server(
    spec: &ServerSpec,
    need: Option<u64>,
    dirs: &Dirs,
    bounds: server::Bounds,
    machine: &dyn Machine,
    lease: LeaseGuard,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<EngineReady, EngineRefusal> {
    let start = Start { spec, need, dirs, bounds, machine, lease: &lease };
    loop {
        let (machine_lock, endpoint_lock) = take_locks(dirs, &spec.endpoint)?;
        if let Some((record, started)) = start.under(machine_lock, &endpoint_lock, note)? {
            drop(endpoint_lock);
            mark_ready(&lease, &spec.served_name, &record, started.then_some(need).flatten(), machine);
            return Ok(EngineReady { record, started, _lease: lease });
        }
    }
}

/// The residency lock, then the endpoint's. While another holder has the
/// endpoint's (a start in flight there, until its server answers), the
/// residency lock is let go before waiting for it, and both are taken again.
fn take_locks(
    dirs: &Dirs,
    endpoint: &str,
) -> Result<(darkmux_types::residency_lock::ResidencyLock, darkmux_types::flock::FlockGuard), EngineRefusal> {
    loop {
        let machine_lock = residency_lock()?;
        if let Some(endpoint_lock) = server::try_endpoint_lock(dirs, endpoint).map_err(server_refusal)? {
            return Ok((machine_lock, endpoint_lock));
        }
        drop(machine_lock);
        drop(server::endpoint_lock(dirs, endpoint).map_err(server_refusal)?);
    }
}

/// One start's inputs ([`start_server`]).
struct Start<'a> {
    spec: &'a ServerSpec,
    need: Option<u64>,
    dirs: &'a Dirs,
    bounds: server::Bounds,
    machine: &'a dyn Machine,
    lease: &'a LeaseGuard,
}

impl Start<'_> {
    /// Decide and start under both locks: `Some((record, started))` when the
    /// model answers, `None` to take the locks and decide again (a server of
    /// the same command was found unhealthy and stopped).
    fn under(
        &self,
        machine_lock: darkmux_types::residency_lock::ResidencyLock,
        endpoint_lock: &darkmux_types::flock::FlockGuard,
        note: &mut dyn FnMut(Note, DispatchModelPayload),
    ) -> Result<Option<(EngineRecord, bool)>, EngineRefusal> {
        let (spec, dirs, bounds, machine, lease) = (self.spec, self.dirs, self.bounds, self.machine, self.lease);
        let endpoint = spec.endpoint.as_str();
        let held = |r: &EngineRecord| held_now(machine, lease).contains(&served_name_of(r));
        let current = server::running(dirs, endpoint);
        if current.as_ref().is_some_and(|r| r.argv == spec.argv) {
            // Started by another dispatch since this one looked: reuse it.
            drop(machine_lock);
            return self.reuse(endpoint_lock).map(|reused| reused.map(|record| (record, false)));
        }
        let leased = held_now(machine, lease);
        refuse_a_held_server(current.as_ref(), &leased, endpoint)?;
        let freed_by_replacing = current.as_ref().and_then(|r| machine.footprint(r.pid)).unwrap_or(0);
        let room = Room { need: self.need, freed_by_replacing, endpoint, leased: &leased, lease };
        let evicted = make_room(&room, dirs, bounds, machine)?;
        // Decided: the lease, with what the start needs reserved.
        write_lease(lease, &spec.served_name, self.need)?;
        let base = DispatchModelPayload {
            engine: spec.engine.clone(),
            endpoint: endpoint.to_string(),
            model: spec.model.clone(),
            ..Default::default()
        };
        let replacing = current.as_ref().map(|r| r.model.clone());
        let mut machine_lock = Some(machine_lock);
        let mut progress = |p: Progress| {
            if matches!(p, Progress::Spawned) {
                machine_lock.take();
            }
            report(p, &base, &replacing, &evicted, note)
        };
        let ensured = server::ensure_locked(spec, dirs, bounds, &held, &mut progress, endpoint_lock).map_err(server_refusal)?;
        let started = matches!(ensured, server::Ensured::Started(_));
        Ok(Some((ensured.record().clone(), started)))
    }
}

impl Start<'_> {
    /// The same command's server, reused under the endpoint's lock with this
    /// dispatch's lease on it; `None`, the lease withdrawn, when it was found
    /// unhealthy and stopped.
    fn reuse(&self, endpoint_lock: &darkmux_types::flock::FlockGuard) -> Result<Option<EngineRecord>, EngineRefusal> {
        let (spec, machine, lease) = (self.spec, self.machine, self.lease);
        write_lease(lease, &spec.served_name, None)?;
        let held = |r: &EngineRecord| held_now(machine, lease).contains(&served_name_of(r));
        let reused = server::reuse_locked(spec, self.dirs, self.bounds, &held, &mut |_| {}, endpoint_lock).map_err(server_refusal)?;
        if reused.is_none() {
            let _ = lease.write(&[]);
        }
        Ok(reused)
    }
}

/// The model answers: its lease is marked loaded. (#3509) A server this
/// dispatch started keeps `need` reserved until its memory pages in
/// ([`Machine::settle`]); one it reused reserves nothing.
fn mark_ready(lease: &LeaseGuard, served_name: &str, record: &EngineRecord, need: Option<u64>, machine: &dyn Machine) {
    let models = [served_name.to_string()];
    match need {
        Some(bytes) => {
            if let Ok(res) = lease.mark_loaded_reserving(&models, bytes) {
                machine.settle(res, record.pid, bytes);
            }
        }
        None => {
            let _ = lease.mark_loaded(&models);
        }
    }
}

/// The machine-wide residency lock, or the refusal saying why not.
fn residency_lock() -> Result<darkmux_types::residency_lock::ResidencyLock, EngineRefusal> {
    darkmux_types::residency_lock::acquire().map_err(|e| {
        refuse("model_start_failed", format!("could not take darkmux's residency lock ({e:#}); check that darkmux's home is writable"))
    })
}

/// darkmux's own server on this endpoint (`current`) runs another model or
/// context; one a running dispatch is using is never replaced under it.
/// Refused here before anything is evicted for a start that cannot happen;
/// `ensure` asks again under the endpoint's lock.
fn refuse_a_held_server(current: Option<&EngineRecord>, leased: &[String], endpoint: &str) -> Result<(), EngineRefusal> {
    match current.filter(|r| leased.contains(&served_name_of(r))) {
        Some(cur) => Err(busy(
            format!("endpoint `{endpoint}` is serving `{}` for a running dispatch", cur.model),
            "retry when it finishes",
        )),
        None => Ok(()),
    }
}

/// Record this dispatch's lease on `served_name`, reserving `need` bytes for
/// a start. Without it another darkmux process reads the server as idle and
/// may stop it under this dispatch, or decide on memory the start has not
/// paged in yet: refuse rather than run unguarded.
fn write_lease(lease: &LeaseGuard, served_name: &str, need: Option<u64>) -> Result<(), EngineRefusal> {
    let models = [served_name.to_string()];
    let written = match need {
        Some(bytes) => lease.write_starting(&models, bytes),
        None => lease.write(&models),
    };
    written.map_err(|e| {
        refuse(
            "model_start_failed",
            format!(
                "could not record this dispatch's lease on `{served_name}` ({e:#}), so another darkmux process could \
                 stop its server mid-run; check that darkmux's home is writable"
            ),
        )
    })
}

/// What other dispatches hold now, plus (#3507) the machine's standing
/// utility binding: the compactor a running dispatch loads on demand holds
/// no lease, and evicting it fails that dispatch's next compaction.
fn held_now(machine: &dyn Machine, lease: &LeaseGuard) -> Vec<String> {
    let mut held = machine.leased(lease);
    held.extend(machine.utility());
    held
}

/// (#3453) Refuse `engine_in_use` when the engine's own server (one darkmux
/// did not start) is up: rec holds a single-instance lock, and darkmux never
/// stops or takes it over. The server darkmux itself runs on this endpoint
/// is not that one when the engine names its port and darkmux's recorded
/// server is what listens there (`server::listens`, #3453 review): a name
/// or a port alone proves nothing.
fn refuse_engines_own_server(engine: &dyn ServerEngine, dirs: &Dirs, endpoint: &str) -> Result<(), EngineRefusal> {
    let Some(address) = engine.own_server()? else { return Ok(()) };
    if flint::own_server_is_darkmuxs(&address, server::running(dirs, endpoint).as_ref()) {
        return Ok(());
    }
    Err(refuse("engine_in_use", flint::own_server_refusal(&address)))
}


/// One of darkmux's own residents that could be evicted to make room.
pub(crate) enum Resident {
    Server(EngineRecord),
    /// A `darkmux:` load in LM Studio, by identifier.
    Load(String),
}

impl Resident {
    pub(crate) fn name(&self) -> String {
        match self {
            Resident::Server(r) => format!("{} ({})", r.model, r.endpoint),
            Resident::Load(id) => id.clone(),
        }
    }
}

/// One lifecycle step as the `dispatch.model.*` note it is: the start names
/// the model it replaces and what was evicted; loading and ready say how long.
fn report(
    p: Progress,
    base: &DispatchModelPayload,
    replacing: &Option<String>,
    evicted: &[String],
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) {
    let mut payload = base.clone();
    let kind = match p {
        Progress::Starting => {
            payload.replacing = replacing.clone();
            payload.evicted = evicted.to_vec();
            Note::Start
        }
        Progress::Waiting { elapsed } => {
            payload.elapsed_ms = Some(elapsed.as_millis() as u64);
            Note::Loading
        }
        Progress::Ready { elapsed } => {
            payload.elapsed_ms = Some(elapsed.as_millis() as u64);
            Note::Ready
        }
        Progress::Replacing { .. } | Progress::Spawned => return,
    };
    note(kind, payload);
}

/// What a start needs room for.
struct Room<'a> {
    /// The model's memory floor; `None` when the engine did not say.
    need: Option<u64>,
    /// Memory darkmux's own server on this endpoint frees when replaced.
    freed_by_replacing: u64,
    endpoint: &'a str,
    /// What other dispatches held when the dispatch looked; read again
    /// (through `lease`) before each eviction.
    leased: &'a [String],
    lease: &'a LeaseGuard,
}

/// Evict darkmux's own idle residents until `need` bytes are reclaimable
/// (less what starts in flight reserve), returning what it evicted. A model
/// larger than the machine is `wont_fit` before free memory is read; a
/// missing figure (the model's need, or free memory) refuses rather than
/// starting blind.
fn make_room(room: &Room<'_>, dirs: &Dirs, bounds: server::Bounds, machine: &dyn Machine) -> Result<Vec<String>, EngineRefusal> {
    let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
    // (#3510) A figure darkmux cannot read never lets a start through blind.
    let Some(need) = room.need else {
        return Err(refuse(
            "model_start_failed",
            "the engine gave no memory figure for this model, so darkmux cannot tell whether it fits; it starts \
             nothing it cannot account for",
        ));
    };
    // Larger than the machine: evicting or waiting can never help. Asked
    // first, so it holds whatever free memory reads.
    if let Some(total) = machine.capacity().filter(|t| need > *t) {
        return Err(refuse(
            "wont_fit",
            format!("the model needs {:.1} GiB and this machine has {:.1} GiB in all", gib(need), gib(total)),
        ));
    }
    let freed_by_replacing = room.freed_by_replacing;
    // (#3509) Starts still in flight count, though their memory has not
    // paged in yet.
    let reserved = machine.reserved(room.lease);
    let fits = |free: u64| free.saturating_add(freed_by_replacing).saturating_sub(reserved) >= need;
    let unread = || {
        refuse(
            "model_start_failed",
            format!(
                "darkmux could not read this machine's free memory, so it cannot tell whether the model's {:.1} GiB \
                 fits; it starts nothing it cannot account for",
                gib(need)
            ),
        )
    };
    // Room already: nothing is listed, LM Studio is not asked.
    if fits(machine.reclaimable().ok_or_else(unread)?) {
        return Ok(Vec::new());
    }
    let (mut idle, mut held) = darkmux_residents(dirs, room.endpoint, room.leased, machine.lmstudio_residents());
    let mut evicted = Vec::new();
    loop {
        let free = machine.reclaimable().ok_or_else(unread)?;
        if fits(free) {
            return Ok(evicted);
        }
        let Some(next) = idle.pop() else {
            return Err(short_of_memory(need, free.saturating_add(freed_by_replacing).saturating_sub(reserved), &held, reserved));
        };
        if evict(&next, dirs, bounds, machine, room.lease)? {
            evicted.push(next.name());
        } else {
            // A dispatch took it up since the list was read.
            held.push(next.name());
        }
    }
}

/// (#3510) Short of memory with nothing of darkmux's idle left to evict:
/// `busy` when running dispatches (or starts in flight) hold some of it, so
/// it frees when they finish; otherwise `memory_in_use`, programs darkmux
/// did not start hold it. Both are retryable: `wont_fit` is only for a
/// model larger than the machine.
pub(crate) fn short_of_memory(need: u64, free: u64, held: &[String], reserved: u64) -> EngineRefusal {
    let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
    let short = format!(
        "the model needs {:.1} GiB and {:.1} GiB is free after evicting every idle darkmux resident",
        gib(need),
        gib(free)
    );
    let mut holders = Vec::new();
    if !held.is_empty() {
        holders.push(format!("running dispatches hold {}", held.join(", ")));
    }
    if reserved > 0 {
        holders.push(format!("starts in flight reserve {:.1} GiB", gib(reserved)));
    }
    if holders.is_empty() {
        return refuse(
            "memory_in_use",
            format!("{short}; programs darkmux did not start hold the rest, so retry once they free it"),
        );
    }
    busy(format!("{short}; {}", holders.join(" and ")), "retry when they finish")
}

/// The model name a recorded server answers to: `flint` for rec's (it has
/// no `--name`, #3453), else the `--name` it was started under.
fn served_name_of(rec: &EngineRecord) -> String {
    if rec.engine == flint::ENGINE {
        return flint::SERVED_NAME.to_string();
    }
    rec.argv.iter().skip_while(|a| *a != "--name").nth(1).cloned().unwrap_or_default()
}

/// darkmux's own residents other than `endpoint`'s server: the idle ones,
/// in eviction order (last is evicted first), and the names of those a
/// running dispatch holds.
pub(crate) fn darkmux_residents(dirs: &Dirs, endpoint: &str, leased: &[String], lms: Vec<String>) -> (Vec<Resident>, Vec<String>) {
    let (mut idle, mut held) = (Vec::new(), Vec::new());
    let mut servers: Vec<EngineRecord> = engine_record::list_in(&dirs.records)
        .0
        .into_iter()
        .filter(|r| r.endpoint != endpoint && server::alive(r))
        .collect();
    // Newest first, so the oldest server is popped first.
    servers.sort_by_key(|r| std::cmp::Reverse(r.started_at_ms));
    for id in lms {
        if leased.contains(&id) {
            held.push(id);
        } else {
            idle.push(Resident::Load(id));
        }
    }
    for r in servers {
        if leased.contains(&served_name_of(&r)) {
            held.push(format!("{} ({})", r.model, r.endpoint));
        } else {
            idle.push(Resident::Server(r));
        }
    }
    (idle, held)
}

/// Evict `resident` unless a running dispatch holds it now: `Ok(false)`
/// when held. A server's lease is read under its endpoint's lock, where it
/// is stopped; a load's right before its unload.
pub(crate) fn evict(resident: &Resident, dirs: &Dirs, bounds: server::Bounds, machine: &dyn Machine, lease: &LeaseGuard) -> Result<bool, EngineRefusal> {
    match resident {
        Resident::Server(r) => {
            let held = |r: &EngineRecord| held_now(machine, lease).contains(&served_name_of(r));
            // (#3509) Never waits on that endpoint's lock under the machine's:
            // one taken (a start or a reuse there) is left running.
            server::try_stop_unless_held(dirs, r, bounds, &held).map_err(server_refusal)
        }
        Resident::Load(id) => {
            if held_now(machine, lease).contains(id) {
                return Ok(false);
            }
            machine
                .lmstudio_unload(id)
                .map_err(|e| refuse("model_start_failed", format!("could not unload `{id}` to make room: {e:#}")))?;
            Ok(true)
        }
    }
}

#[cfg(test)]
#[path = "engine_dispatch_tests.rs"]
pub(crate) mod tests;

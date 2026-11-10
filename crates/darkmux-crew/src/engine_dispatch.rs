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
//! **No dispatch waits blind on an endpoint's lock.** A start in flight
//! holds its endpoint's lock until its server answers, which takes minutes
//! for a large model. A dispatch that finds the lock taken reads the
//! endpoint's record and the leases first: when the server there runs
//! another command for a running dispatch, it is `busy` at once, as it is
//! before any lock is asked for. Otherwise (the same command is starting, or
//! another dispatch is deciding there) it asks for the lock again every
//! [`server::Bounds::poll`] ([`Start::endpoint_lock`]), saying
//! `dispatch.model.loading` as a start of its own does, and gives up at the
//! dispatch's timeout (`busy`) or at its run's stop ([`Patience`]).
//!
//! The dispatch's lease names the served model for as long as the returned
//! [`EngineReady`] lives, so no other darkmux process evicts it mid-run.
//! What a start needs is reserved apart from the lease, in a record of its
//! own bound to the server ([`darkmux_types::residency_reservation`]): it
//! stays counted while the server's memory pages in, whatever becomes of the
//! dispatch, and never makes the model read as held.
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
//! is bounded by the dispatch's timeout and ends `busy` when that passes
//! (one bound for all of a dispatch's waiting: [`Patience`]).
//! While it waits it says so every [`QUEUED_EVERY`], on the CLI and as a
//! `dispatch.queue.wait` record, and writes `dispatch.queue.resume` when it
//! goes on. `wont_fit` never waits: no wait can make it fit.

use std::cell::Cell;
use std::time::Duration;

use crate::budget::{BudgetCaller, BudgetEnv};
use darkmux_flow::payload::DispatchModelPayload;
use darkmux_types::config::BusyPolicy;
use darkmux_profiles::engine::flint::{self, Rec};
use darkmux_profiles::engine::server::{self, Dirs, Progress, ServerSpec};
use darkmux_profiles::engine::tensorfold::Tensorfold;
use darkmux_types::engine_record::{self, EngineRecord};
use darkmux_types::residency_lease::LeaseGuard;
use darkmux_types::residency_reservation::{self, Reserved};
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

impl From<EngineRefusal> for NotResident {
    fn from(refusal: EngineRefusal) -> Self {
        NotResident::Refused(refusal)
    }
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

/// How long a dispatch may wait in all, and what ends a wait early: its
/// timeout and its run's stop. Every wait [`prepare`] makes goes through
/// one (the queue's, and the wait for an endpoint another dispatch has
/// locked), so none is unbounded or deaf to a stop, and together they never
/// outlast the timeout.
pub(crate) struct Patience<'a> {
    /// Sleep up to the span, ending early at a stop; the milliseconds slept.
    sleep: &'a dyn Fn(Duration) -> u64,
    /// Why the dispatch's run was stopped, when it was.
    stop: &'a dyn Fn() -> Option<String>,
    bound_ms: Option<u64>,
    waited_ms: Cell<u64>,
}

impl<'a> Patience<'a> {
    pub(crate) fn new(sleep: &'a dyn Fn(Duration) -> u64, stop: &'a dyn Fn() -> Option<String>, bound: Option<Duration>) -> Self {
        Patience { sleep, stop, bound_ms: bound.map(|b| b.as_millis() as u64), waited_ms: Cell::new(0) }
    }

    /// The milliseconds waited so far.
    fn waited(&self) -> u64 {
        self.waited_ms.get()
    }

    /// Sleep `span`, less when the bound or a stop comes first; the
    /// milliseconds slept.
    fn sleep(&self, span: Duration) -> u64 {
        let left = self.bound_ms.map_or(u64::MAX, |b| b.saturating_sub(self.waited()));
        if left == 0 {
            return 0;
        }
        let slept = (self.sleep)(span.min(Duration::from_millis(left)));
        self.waited_ms.set(self.waited().saturating_add(slept));
        slept
    }

    /// The bound, once the dispatch has waited it.
    fn spent(&self) -> Option<u64> {
        self.bound_ms.filter(|b| self.waited() >= *b)
    }

    /// The wait's end, when it has one: the run's stop, or `busy` on
    /// `waiting_on` once the dispatch has waited its timeout.
    fn ended(&self, model: &str, waiting_on: &str) -> Option<NotResident> {
        if let Some(why) = (self.stop)() {
            return Some(stopped_waiting(model, &why));
        }
        self.spent().map(|bound| NotResident::Refused(still_busy(waiting_on, bound)))
    }
}

/// A wait its run's stop ended.
fn stopped_waiting(model: &str, why: &str) -> NotResident {
    NotResident::Stopped(format!("darkmux dispatch: stopped waiting for `{model}` ({why}); nothing was started or evicted"))
}

/// `busy`, saying the dispatch waited its whole timeout (`bound_ms`).
fn still_busy(waiting_on: &str, bound_ms: u64) -> EngineRefusal {
    busy(
        waiting_on.to_string(),
        &format!(
            "still busy after waiting {}, this dispatch's timeout, so retry when it finishes",
            crate::budget::human_duration(bound_ms / 1000)
        ),
    )
}

/// How often a dispatch waiting for an endpoint another dispatch is starting
/// says so (`dispatch.model.loading`), after the first: a start's own
/// cadence ([`server::ensure`]'s `Progress::Waiting`).
const LOADING_EVERY: Duration = Duration::from_secs(10);

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
        let mut attempt =
            |patience: &Patience<'_>, note: &mut dyn FnMut(Note, DispatchModelPayload)| prepare_with(target, &dirs, engine.as_ref(), bounds, &machine, patience, note);
        wait_while_busy(wait, env, &base, &mut attempt, note)
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

/// One try at making the model resident, waiting only as its [`Patience`]
/// allows and reporting through the note it is given.
type Attempt<'a, T> = dyn FnMut(&Patience<'_>, &mut dyn FnMut(Note, DispatchModelPayload)) -> Result<T, NotResident> + 'a;

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
    let sleep = |span: Duration| crate::budget::sleep_sliced(env, span, &wait.caller);
    let stop = || env.stop_reason(&wait.caller);
    let patience = Patience::new(&sleep, &stop, wait.bound);
    let mut q = Waiting { wait, env, base, patience: &patience, said_at: None };
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
    /// The dispatch's whole wait: this queue's, and its tries' own.
    patience: &'a Patience<'a>,
    /// When it last said it waits (milliseconds waited); `None` before the
    /// first.
    said_at: Option<u64>,
}

impl Waiting<'_> {
    /// Its run's stop, seen after it has waited: nothing is tried after it.
    fn stopped(&self) -> Option<NotResident> {
        let why = self.env.stop_reason(&self.wait.caller).filter(|_| self.patience.waited() > 0)?;
        Some(stopped_waiting(&self.base.model, &why))
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
        let tried = attempt(self.patience, &mut |n, p| {
            if !resumed {
                resumed = true;
                self.resume(note);
            }
            note(n, p)
        });
        match tried {
            Err(NotResident::Refused(r)) if r.code == "busy" && self.wait.policy == BusyPolicy::Queue => Err(r),
            done => {
                if !resumed {
                    self.resume(note);
                }
                Ok(done)
            }
        }
    }

    /// `dispatch.queue.resume`, and its line.
    fn resume(&self, note: &mut dyn FnMut(Note, DispatchModelPayload)) {
        self.env.say(&format!(
            "darkmux dispatch: `{}` is no longer waiting after {}; going on",
            self.base.model,
            crate::budget::human_duration(self.patience.waited() / 1000)
        ));
        note(Note::Resumed, DispatchModelPayload { elapsed_ms: Some(self.patience.waited()), ..self.base.clone() });
    }

    /// `busy`, saying how long it waited, once it has waited its bound.
    fn timed_out(&self, waiting_on: &str) -> Option<NotResident> {
        self.patience.spent().map(|bound| NotResident::Refused(still_busy(waiting_on, bound)))
    }

    /// The `queued` line and a `dispatch.queue.wait` record, when the wait
    /// starts and every [`QUEUED_EVERY`] after.
    fn say_if_due(&mut self, waiting_on: String, note: &mut dyn FnMut(Note, DispatchModelPayload)) {
        let (every, waited) = (QUEUED_EVERY.as_millis() as u64, self.patience.waited());
        if self.said_at.is_some_and(|at| waited < at + every) {
            return;
        }
        self.env.say(&queued_line(&waiting_on, waited, self.patience.bound_ms, &self.wait.caller));
        note(
            Note::Queued,
            DispatchModelPayload {
                elapsed_ms: Some(waited),
                waiting_on: Some(waiting_on),
                wait_bound_ms: self.patience.bound_ms,
                ..self.base.clone()
            },
        );
        self.said_at = Some(waited);
    }

    /// Sleep to the next try, the next line or the bound, whichever is
    /// first, recording the pause.
    fn sleep(&mut self) {
        let every = QUEUED_EVERY.as_millis() as u64;
        let next_line = self.said_at.map_or(every, |at| at + every).saturating_sub(self.patience.waited());
        let span = (QUEUE_RETRY.as_millis() as u64).min(next_line).max(1);
        let slept = self.patience.sleep(Duration::from_millis(span));
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
    /// (#3509) The memory starts still paging in reserve
    /// ([`darkmux_types::residency_reservation`]), each engine server's read
    /// against its footprint now. Not `replacing`'s, a server this decision
    /// stops: what it has not paged in, it never will.
    fn reserved(&self, replacing: Option<u32>) -> u64 {
        let servers = residency_reservation::Servers { alive: &server::process_alive, footprint: &|pid| self.footprint(pid) };
        residency_reservation::reserved_except(replacing, &servers)
    }
    /// (#3507) The standing utility binding's LM Studio identifier, held like
    /// a leased model: never evicted to make room.
    fn utility(&self) -> Option<String> {
        None
    }
}

/// (#3509) How long a started server's reservation may outlive its answer
/// while its memory pages in. A server answers before it has paged in all
/// its weights (MLX maps them and touches them on first use), so the
/// reservation is kept until the server's footprint reaches the need, and
/// at most this long: two minutes covers the first request of a large
/// model reading from disk, and bounds the time a footprint that never
/// reaches the floor (its margin is never all resident) keeps a phantom
/// reservation that refuses other starts as `busy`. An LM Studio load's
/// reservation has the same bound ([`crate::lms_room::settle_load`]).
pub(crate) const SETTLE_BOUND: Duration = Duration::from_secs(120);
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

/// [`prepare`]'s one try, against the directories, engine, bounds and
/// machine it is given.
pub(crate) fn prepare_with(
    target: &crate::target::Target,
    dirs: &Dirs,
    engine: &dyn ServerEngine,
    bounds: server::Bounds,
    machine: &dyn Machine,
    patience: &Patience<'_>,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<EngineReady, NotResident> {
    let endpoint = target.endpoint.named_id().unwrap_or_default().to_string();
    let port = target.endpoint.port.unwrap_or_default();
    let spec = engine.plan(&endpoint, port, &target.model)?;
    refuse_engines_own_server(engine, dirs, &endpoint)?;
    // (#3504) No lease is written until this dispatch has decided to run:
    // a dispatch that turns out `busy`, and so may queue, holds nothing
    // another dispatch would read as held (every Flint server answers
    // `flint`, so a waiter's lease would pin them all).
    let lease = LeaseGuard::acquire();
    let start = Start { spec: &spec, need: engine.memory_need(&target.model), dirs, bounds, machine, lease: &lease, patience };
    // A reuse decides nothing about memory and takes no residency lock.
    // (#3511) It is decided under the endpoint's lock and never starts
    // anything: a server judged reusable and then lost (an evictor stopped it
    // in between) is started below, through the memory decision.
    if server::running(dirs, &endpoint).is_some_and(|r| r.argv == spec.argv) {
        write_lease(&lease, &spec.served_name)?;
        let endpoint_lock = start.endpoint_lock(note)?;
        if let Some(record) = start.reuse(&endpoint_lock)? {
            drop(endpoint_lock);
            let _ = lease.mark_loaded(std::slice::from_ref(&spec.served_name));
            return Ok(EngineReady { record, started: false, _lease: lease });
        }
    }
    let (record, started) = start.server(note)?;
    let _ = lease.mark_loaded(std::slice::from_ref(&spec.served_name));
    Ok(EngineReady { record, started, _lease: lease })
}

/// One start's inputs.
struct Start<'a> {
    spec: &'a ServerSpec,
    need: Option<u64>,
    dirs: &'a Dirs,
    bounds: server::Bounds,
    machine: &'a dyn Machine,
    lease: &'a LeaseGuard,
    patience: &'a Patience<'a>,
}

impl Start<'_> {
    /// Start the server, or reuse the one another dispatch started
    /// meanwhile: the record, and whether this dispatch started it. (#3509)
    /// Decided under both the machine-wide residency lock and the endpoint's
    /// own lock, taken in that order ([`Self::locks`]): the endpoint is read
    /// again under them, so a start another dispatch made in between is
    /// reused (the same command) or refused `busy` (another), never decided
    /// on a stale look. The residency lock is never held while waiting on
    /// the endpoint's lock, nor past the spawn: from then on the start's
    /// reservation counts it.
    fn server(&self, note: &mut dyn FnMut(Note, DispatchModelPayload)) -> Result<(EngineRecord, bool), NotResident> {
        loop {
            let (machine_lock, endpoint_lock) = self.locks(note)?;
            if let Some(done) = self.under(machine_lock, &endpoint_lock, note)? {
                return Ok(done);
            }
        }
    }

    /// The residency lock, then the endpoint's. While another holder has the
    /// endpoint's (a start in flight there, until its server answers), the
    /// residency lock is let go before waiting for it
    /// ([`Self::endpoint_lock`]), and both are taken again. A server there
    /// that runs another command for a running dispatch is `busy` before any
    /// lock is asked for.
    fn locks(
        &self,
        note: &mut dyn FnMut(Note, DispatchModelPayload),
    ) -> Result<(darkmux_types::residency_lock::ResidencyLock, darkmux_types::flock::FlockGuard), NotResident> {
        loop {
            self.refuse_another_commands_held_server()?;
            let machine_lock = residency_lock()?;
            if let Some(endpoint_lock) = server::try_endpoint_lock(self.dirs, &self.spec.endpoint).map_err(server_refusal)? {
                return Ok((machine_lock, endpoint_lock));
            }
            drop(machine_lock);
            drop(self.endpoint_lock(note)?);
        }
    }

    /// The endpoint's lock, never waited for blind (the module doc): asked
    /// for again every poll, until it is free, the server there turns out to
    /// run another command for a running dispatch (`busy`, at once), the
    /// dispatch has waited its timeout (`busy`), or its run is stopped.
    /// While the same command is starting there for another dispatch, the
    /// wait says `dispatch.model.loading`: at its first poll, then every
    /// [`LOADING_EVERY`].
    fn endpoint_lock(&self, note: &mut dyn FnMut(Note, DispatchModelPayload)) -> Result<darkmux_types::flock::FlockGuard, NotResident> {
        let (spec, began) = (self.spec, self.patience.waited());
        let mut said_at: Option<u64> = None;
        loop {
            if let Some(lock) = server::try_endpoint_lock(self.dirs, &spec.endpoint).map_err(server_refusal)? {
                return Ok(lock);
            }
            let current = server::running_now(self.dirs, &spec.endpoint);
            self.refuse_held(current.as_ref())?;
            let starting = current.is_some_and(|r| r.argv == spec.argv);
            let waiting_on = if starting {
                format!("endpoint `{}` is starting `{}` for another dispatch", spec.endpoint, spec.model)
            } else {
                format!("another dispatch is deciding what endpoint `{}` serves", spec.endpoint)
            };
            if let Some(ended) = self.patience.ended(&spec.model, &waiting_on) {
                return Err(ended);
            }
            let waited = self.patience.waited() - began;
            if starting && waited > 0 && said_at.is_none_or(|at| waited >= at + LOADING_EVERY.as_millis() as u64) {
                said_at = Some(waited);
                note(Note::Loading, DispatchModelPayload { elapsed_ms: Some(waited), ..self.payload() })
            }
            self.patience.sleep(self.bounds.poll);
        }
    }

    /// `busy` when darkmux's server on the endpoint runs another command
    /// than this dispatch's and a running dispatch holds it: read without
    /// any lock, so it is answered at once, however long a start in flight
    /// there holds the endpoint's lock.
    fn refuse_another_commands_held_server(&self) -> Result<(), EngineRefusal> {
        self.refuse_held(server::running_now(self.dirs, &self.spec.endpoint).as_ref())
    }

    /// [`Self::refuse_another_commands_held_server`], given the server.
    fn refuse_held(&self, current: Option<&EngineRecord>) -> Result<(), EngineRefusal> {
        let Some(other) = current.filter(|r| r.argv != self.spec.argv) else { return Ok(()) };
        refuse_a_held_server(Some(other), &held_now(self.machine, self.lease), &self.spec.endpoint)
    }

    /// What every `dispatch.model.*` record of this start names.
    fn payload(&self) -> DispatchModelPayload {
        DispatchModelPayload {
            engine: self.spec.engine.clone(),
            endpoint: self.spec.endpoint.clone(),
            model: self.spec.model.clone(),
            ..Default::default()
        }
    }

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
            write_lease(lease, &spec.served_name)?;
            return self.reuse(endpoint_lock).map(|reused| reused.map(|record| (record, false)));
        }
        let leased = held_now(machine, lease);
        refuse_a_held_server(current.as_ref(), &leased, endpoint)?;
        let room = Room { need: self.need, replacing: current.as_ref(), endpoint, leased: &leased, lease };
        let (need, evicted) = make_room(&room, dirs, bounds, machine)?;
        // Decided: the lease, and what the start needs, reserved apart from it.
        write_lease(lease, &spec.served_name)?;
        let mut reserved = Some(reserve(need, &spec.served_name)?);
        let base = self.payload();
        let replacing = current.as_ref().map(|r| r.model.clone());
        let mut machine_lock = Some(machine_lock);
        let mut progress = |p: Progress| {
            if matches!(p, Progress::Spawned) {
                // The memory is the server's from here: bound to it before
                // the lock goes, so no decision reads the start uncounted.
                reserved = reserved.take().map(|r| bound_to_server(r, dirs, endpoint, bounds));
                machine_lock.take();
            }
            report(p, &base, &replacing, &evicted, note)
        };
        // A start that fails ends its reservation with `reserved`.
        let ensured = server::ensure_locked(spec, dirs, bounds, &held, &mut progress, endpoint_lock).map_err(server_refusal)?;
        let started = matches!(ensured, server::Ensured::Started(_));
        if let Some(reserved) = reserved.filter(|_| started) {
            leave_settling(reserved);
        }
        Ok(Some((ensured.record().clone(), started)))
    }

    /// The same command's server, reused under the endpoint's lock with this
    /// dispatch's lease (already written) on it; `None`, the lease
    /// withdrawn, when it was found unhealthy and stopped.
    fn reuse(&self, endpoint_lock: &darkmux_types::flock::FlockGuard) -> Result<Option<EngineRecord>, EngineRefusal> {
        let (spec, machine, lease) = (self.spec, self.machine, self.lease);
        let held = |r: &EngineRecord| held_now(machine, lease).contains(&served_name_of(r));
        let reused = server::reuse_locked(spec, self.dirs, self.bounds, &held, &mut |_| {}, endpoint_lock).map_err(server_refusal)?;
        if reused.is_none() {
            let _ = lease.write(&[]);
        }
        Ok(reused)
    }
}

/// Reserve the `need` bytes a start is about to take
/// ([`darkmux_types::residency_reservation`]), or refuse: a start no other
/// decision counts would be admitted beside one it does not fit with.
fn reserve(need: u64, served_name: &str) -> Result<Reserved, EngineRefusal> {
    Reserved::begin(need).map_err(|e| {
        refuse(
            "model_start_failed",
            format!(
                "could not record the memory `{served_name}` is about to take ({e:#}), so another start could be \
                 admitted beside it; check that darkmux's home is writable"
            ),
        )
    })
}

/// `reserved`, bound to the server just spawned on `endpoint`: it lasts
/// while that server's memory pages in, for a whole start and
/// [`SETTLE_BOUND`] at the longest, whatever becomes of this dispatch. A
/// record that cannot be read or written leaves it this dispatch's.
fn bound_to_server(mut reserved: Reserved, dirs: &Dirs, endpoint: &str, bounds: server::Bounds) -> Reserved {
    if let Ok(Some(rec)) = engine_record::read_in(&dirs.records, endpoint) {
        let longest = (bounds.start + SETTLE_BOUND).as_millis() as u64;
        let _ = reserved.bind_server(rec.pid, rec.start_stamp, residency_reservation::now_ms().saturating_add(longest));
    }
    reserved
}

/// The server answers: what it has not paged in stays reserved for
/// [`SETTLE_BOUND`] at the longest, left to the server (every decision reads
/// it against the server's footprint, and ends it).
fn leave_settling(mut reserved: Reserved) {
    let _ = reserved.settle_by(residency_reservation::now_ms().saturating_add(SETTLE_BOUND.as_millis() as u64));
    reserved.leave_to_server();
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

/// Record this dispatch's lease on `served_name`. Without it another darkmux
/// process reads the server as idle and may stop it under this dispatch:
/// refuse rather than run unguarded.
fn write_lease(lease: &LeaseGuard, served_name: &str) -> Result<(), EngineRefusal> {
    lease.write(&[served_name.to_string()]).map_err(|e| {
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
    /// darkmux's own server on this endpoint, which the start replaces.
    replacing: Option<&'a EngineRecord>,
    endpoint: &'a str,
    /// What other dispatches held when the dispatch looked; read again
    /// (through `lease`) before each eviction.
    leased: &'a [String],
    lease: &'a LeaseGuard,
}

/// Evict darkmux's own idle residents until the model's need is reclaimable
/// (less what starts still paging in reserve, read again after each
/// eviction: a stopped server reserves nothing), returning the need and what
/// it evicted. A model larger than the machine is `wont_fit` before free
/// memory is read; a missing figure (the model's need, or free memory)
/// refuses rather than starting blind.
fn make_room(room: &Room<'_>, dirs: &Dirs, bounds: server::Bounds, machine: &dyn Machine) -> Result<(u64, Vec<String>), EngineRefusal> {
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
    // The server this start replaces frees its memory, and reserves none.
    let freed_by_replacing = room.replacing.and_then(|r| machine.footprint(r.pid)).unwrap_or(0);
    // (#3509) Starts still paging in count, though the OS does not show
    // their memory as used yet.
    let reserved = || machine.reserved(room.replacing.map(|r| r.pid));
    let usable = |free: u64| free.saturating_add(freed_by_replacing).saturating_sub(reserved());
    let fits = |free: u64| usable(free) >= need;
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
        return Ok((need, Vec::new()));
    }
    let (mut idle, mut held) = darkmux_residents(dirs, room.endpoint, room.leased, machine.lmstudio_residents());
    let mut evicted = Vec::new();
    loop {
        let free = machine.reclaimable().ok_or_else(unread)?;
        if fits(free) {
            return Ok((need, evicted));
        }
        let Some(next) = idle.pop() else {
            return Err(short_of_memory(need, usable(free), &held, reserved()));
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
/// `busy` when running dispatches hold some of it, or starts still paging in
/// reserve some, so it frees when they finish or settle; otherwise `memory_in_use`, programs darkmux
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

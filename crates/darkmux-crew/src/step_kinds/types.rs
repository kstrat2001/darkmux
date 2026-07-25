//! `StepKind` trait + `StepOutcome` — the step-kind execution contract.

use crate::types::{Step, Task};
use anyhow::Result;
use darkmux_flow::FlowRecord;
use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// (#1442) One remote-token bucket metering the per-EXECUTION remote
/// allowance (`DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`, where one
/// execution = one pipeline stage). Local dispatches never touch it. A
/// `budget` of 0 is exhausted from the FIRST item (`used (0) >= budget
/// (0)`), so a zero allowance refuses every hosted call — the same hard
/// opt-out `admit_remote_execution` gives a single hosted dispatch.
///
/// **The ceiling is SOFT (approximate), by construction (#1451 gate).**
/// Admission is checked BEFORE a call ([`admit`](Self::admit)) and tokens
/// are spent AFTER it ([`spend`](Self::spend)), so a stage can overshoot
/// `budget` by at most ONE granted call — the per-item `max_tokens` is
/// clamped to what the bucket has LEFT ([`remaining`](Self::remaining), gate
/// C6), which bounds that overshoot to whatever the endpoint itself reports
/// ABOVE its granted cap. This is deliberate: a per-execution allowance is a
/// spend GUARDRAIL, not a hard byte gate, and a call's exact cost is
/// unknowable until it runs.
///
/// **Bucket-group semantics (#1442 gate — the highest-stakes carry-forward
/// of the ship-2b rewiring).** Where it once lived private in `builtins`,
/// scoped to a SINGLE `dispatch.map` step, the bucket now lives here so the
/// SCHEDULER can own a `group -> Arc<Mutex<MapRemoteBucket>>` map and hand
/// the SAME bucket to every sibling step that names a `bucket_group` in its
/// config (`run_step_graph`). That is what stops `seats x k` sibling
/// `dispatch.map` probe steps from EACH minting a fresh full per-execution
/// allowance (which would multiply the effective stage ceiling by the step
/// count — the "allowance multiplication" the block's own doc named as the
/// follow-on's obligation). The block stays Tier-1-pure: a `dispatch.map`
/// step that names NO group still creates its own step-scoped bucket from
/// the same budget, so grouped and ungrouped both read the one-execution
/// contract; the caller's `Arc` never becomes a field of the kind itself,
/// it arrives through the scheduler-supplied [`StepRunCtx`].
#[derive(Debug)]
pub struct MapRemoteBucket {
    budget: u64,
    used: u64,
    skipped: u32,
}

impl MapRemoteBucket {
    pub(crate) fn new(budget: u64) -> Self {
        Self { budget, used: 0, skipped: 0 }
    }
    pub(crate) fn exhausted(&self) -> bool {
        self.used >= self.budget
    }
    /// What is left to grant a single call (#1442 gate C6): per-item
    /// `max_tokens` clamps to THIS, not the full budget, so one late item
    /// cannot request more than the bucket has left.
    pub(crate) fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.used)
    }
    /// (#1442 fan-out) Admit one call and RESERVE its granted `max_tokens`
    /// up front, returning the granted (clamped) cap — or `None` when the
    /// bucket is already exhausted (counted as skipped, for the item's
    /// named-reason result). The reserve-then-[`settle`](Self::settle) shape
    /// replaces the old admit-then-spend-after pair because sibling
    /// `dispatch.map` steps of one `bucket_group` (the probe stage's
    /// `seats x k` fan-out) run CONCURRENTLY on `run_bounded` worker
    /// threads: with spend-after accounting, every in-flight sibling could
    /// admit against the same untouched balance and the stage would
    /// overshoot by one call PER SIBLING. Reserving the granted cap at
    /// admission bounds the whole group's overshoot to what an endpoint
    /// itself reports ABOVE a granted cap — the same soft-ceiling reading as
    /// before, now independent of sibling concurrency. In a sequential
    /// per-item loop the observable behavior is identical to the old pair
    /// (settle always lands before the next admit).
    pub(crate) fn admit_reserve(&mut self, requested: u32) -> Option<u32> {
        if self.exhausted() {
            self.skipped += 1;
            return None;
        }
        let granted = u32::try_from(self.remaining()).unwrap_or(u32::MAX).min(requested);
        self.used = self.used.saturating_add(u64::from(granted));
        Some(granted)
    }
    /// Settle a reserved call against its ACTUAL spend (the reply's reported
    /// usage, or — conservatively — the granted cap when the endpoint omits
    /// usage; see `conservative_hosted_spend`). Replaces the reservation
    /// with the real number; an endpoint that reports above its granted cap
    /// pushes the bucket over (the documented soft-ceiling overshoot), one
    /// that reports under releases the difference back to its siblings.
    pub(crate) fn settle(&mut self, granted: u32, actual: u64) {
        self.used = self.used.saturating_sub(u64::from(granted)).saturating_add(actual);
    }
    /// Count of calls refused because the bucket was exhausted — read by
    /// tests asserting the skip path fired.
    #[cfg(test)]
    pub(crate) fn skipped(&self) -> u32 {
        self.skipped
    }
}

/// (#1442, ship-2b probe/verify retirement) One dispatch a `dispatch.map`
/// item is about to make, surfaced to the scheduler-supplied
/// [`MapDispatchOverride`] test seam. Field-parallel to the union of the
/// LOCAL ([`crate::single_shot::SingleShotRequest`]) and HOSTED
/// ([`crate::single_shot::HostedSingleShotRequest`]) request shapes:
/// `endpoint: Some` marks the HOSTED dialect (where `temperature` is
/// meaningless and carried as `0.0` — the hosted wire request has no such
/// field), `None` the LOCAL one.
pub struct OverrideDispatchCall<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    /// LOCAL dialect only — `0.0` on a hosted call (no wire field).
    pub temperature: f32,
    /// The granted (already bucket-clamped, on the hosted arm) completion
    /// cap this call would send.
    pub max_tokens: u32,
    pub timeout_seconds: u32,
    pub endpoint: Option<&'a darkmux_types::ModelEndpoint>,
}

/// (#1442, ship-2b — the operator-recorded seam decision on PR #1455) An
/// optional dispatch interceptor for `dispatch.map` items, carried on
/// [`StepRunCtx`] so it crosses the `run_bounded` WORKER-THREAD boundary —
/// the same injection discipline as `darkmux-lab`'s
/// `ReviewStepContext::chat_override` (an `Arc<dyn Fn + Send + Sync>`
/// field, `None` at every production call site). A thread-local seam
/// cannot serve here: the scheduler executes steps on spawned scoped
/// threads (`concurrent_dispatch::run_remote_batches` /
/// `run_local_waves`), where a test thread's thread-local is invisible.
/// When present, `dispatch.map` routes every item's call through it INSTEAD
/// of the real `single_shot_chat`/`single_shot_chat_hosted` transport —
/// budget metering, retry semantics, telemetry, and per-item records all
/// still apply exactly as on the real path (the override replaces the
/// TRANSPORT, never the accounting).
pub type MapDispatchOverride =
    Arc<dyn for<'a> Fn(&OverrideDispatchCall<'a>) -> Result<crate::single_shot::SingleShotReply> + Send + Sync>;

// ─── Port declarations + the run-scoped artifact bus (#1530 Packet 0) ─────
//
// The foundation for the "pure building-block graphs" arc: a `StepKind`
// declares what it PRODUCES and CONSUMES beyond the ordinary Step-input/
// output wiring, and the scheduler materializes the declared shared state
// once per graph run. Nothing in this block changes what any EXISTING kind
// does — every kind's `provides`/`requires` default to empty (see
// `StepKind::provides`/`StepKind::requires`), so a Tier 1 graph with no
// ports declared behaves byte-identically to before this packet.

/// One named declaration a [`StepKind`] makes against the run's data flow —
/// what it hands off ([`StepKind::provides`]) or expects available
/// ([`StepKind::requires`]). This is the contract future `StepKind`
/// implementations (Packets 1/2 of #1530: the review pipeline's dedup/judge
/// accumulators, the coder-phase pipeline's shared worktree state, …)
/// declare against, so read this doc before adding a new port.
///
/// **Typing is by-convention (name matching), not a schema system.** A
/// `Port` carries a `&'static str` name and nothing else that constrains
/// shape — two kinds agree on a port by using the SAME name and (for an
/// [`Artifact`](PortKind::Artifact) port) the same concrete Rust type `T`
/// on both ends of [`StepRunCtx::artifact::<T>`]. There is no registry
/// that validates a `requires` name resolves to a matching `provides`, and
/// no runtime type tag beyond `std::any::Any`'s own — a name collision
/// between two UNRELATED kinds that happen to pick the same string is a
/// caller bug (a downcast miss from [`ArtifactBus::get`] returns `None`,
/// never panics), the same discipline `Step.config`'s dotted-key lookups
/// already use throughout this crate. Do NOT build a type registry on top
/// of this — see the module-level `CLAUDE.md` "KISS for local-AI
/// infrastructure" note; a name-matching convention is the right amount of
/// structure for a single-binary, single-operator system.
///
/// Two shapes, named by [`PortKind`]:
///
/// - [`PortKind::Data`] — a value that flows step→step through the
///   ORDINARY wiring already in place: a `StepKind::run`'s returned
///   [`StepOutcome::output`] becomes the downstream step's `input` entry
///   keyed by the producing step's id (`scheduler::gather_inputs`). A
///   `Data` port declares INTENT — "this kind's output is meant to satisfy
///   a port of this name" — for a future consumer (a graph validator, a
///   viewer annotation) to read; the scheduler does nothing extra with it
///   today. Needs only a `name`.
/// - [`PortKind::Artifact`] — a RUN-SCOPED SHARED HANDLE, materialized once
///   per graph run (not once per step) and handed to every step by
///   reference through [`StepRunCtx::artifact`]. Use this for state that
///   must be visible to and mutated by MULTIPLE steps across the same run
///   — an accumulator, a shared counter, a scratch collection — the same
///   shape `MapRemoteBucket`'s `bucket_group` already proves out for the
///   one case that exists today (see the module doc note on why that
///   mechanism is NOT retrofitted onto this bus in this packet). Carries a
///   `factory` the scheduler calls (at most once per name, per run) to
///   MATERIALIZE the artifact without needing to know its concrete type —
///   see [`ArtifactBus::materialize`].
///
/// The `factory` field is a plain `fn() -> Arc<dyn Any + Send + Sync>` —
/// NOT a boxed closure (`Box<dyn Fn() -> _>`) — specifically so `Port`
/// stays `Copy` and fully `const`-constructible: a `StepKind` impl can
/// declare its ports as a `const` `'static` array literal (mirroring
/// `id()`/`display_name()`'s own `&'static str` returns) with no
/// allocation at declaration time, and the default `&[]` on
/// [`StepKind::provides`]/[`StepKind::requires`] stays a zero-cost empty
/// slice rather than an allocated empty `Vec`. A future artifact that
/// genuinely needs a runtime-captured factory (e.g. a budget value read
/// from config, the shape `MapRemoteBucket` needs) is exactly the case
/// that stays OUTSIDE this mechanism — see the module doc note below.
#[derive(Clone, Copy)]
pub struct Port {
    pub name: &'static str,
    pub kind: PortKind,
}

impl Port {
    /// A `Data`-kind port — flows through the ordinary `Step.output` →
    /// `gather_inputs` wiring; declares intent only.
    pub const fn data(name: &'static str) -> Self {
        Port { name, kind: PortKind::Data }
    }

    /// An `Artifact`-kind port — a run-scoped shared handle the scheduler
    /// materializes once (via `factory`) and hands to every step by
    /// reference through [`StepRunCtx::artifact`].
    pub const fn artifact(name: &'static str, factory: fn() -> Arc<dyn Any + Send + Sync>) -> Self {
        Port { name, kind: PortKind::Artifact(factory) }
    }
}

/// What kind of contract a [`Port`] declares. See [`Port`]'s doc for the
/// full picture; this enum only distinguishes the two shapes.
#[derive(Clone, Copy)]
pub enum PortKind {
    /// Flows through the ordinary `Step.output` → `gather_inputs` wiring.
    /// A declaration of intent; the scheduler does nothing extra with it.
    Data,
    /// A run-scoped shared handle, materialized once per graph run by
    /// calling this `factory`, then shared by reference into every step
    /// via [`StepRunCtx::artifact`]. The factory returns a type-erased
    /// `Arc<dyn Any + Send + Sync>` so the SCHEDULER (which materializes
    /// ports from a `dyn StepKind` it holds no concrete type for) never
    /// needs to know the concrete artifact type — only the `StepKind`
    /// implementations reading/writing it (via [`StepRunCtx::artifact::<T>`])
    /// need to agree on `T`.
    Artifact(fn() -> Arc<dyn Any + Send + Sync>),
}

/// The run-scoped named-artifact bus (#1530 Packet 0). Materialized ONCE
/// per graph run, on the scheduler's MAIN thread, BEFORE any wave's workers
/// spawn — `run_step_graph` scans every step kind actually present in the
/// graph, calls [`Port::artifact`]'s factory for each declared
/// [`PortKind::Artifact`] port (get-or-create by name, mirroring the
/// proven `bucket_groups` discipline that same function already uses for
/// `dispatch.map`'s `bucket_group` — see that call site), and wraps the
/// result in an `Arc` shared by reference into every step's [`StepRunCtx`]
/// for the WHOLE run.
///
/// **Thread-safety discipline: build-then-freeze.** Every `materialize`
/// call happens on the main thread before the first `run_bounded` worker
/// spawns; from that point on the bus is read-only — a worker thread only
/// ever calls [`Self::get`] (a lookup + clone + downcast), never inserts.
/// This is why `ArtifactBus` itself needs no internal locking: the
/// `BTreeMap`'s key set is closed before it ever crosses a thread
/// boundary, and each entry's own concurrency (if any — e.g. an
/// `Arc<Mutex<Vec<_>>>` artifact) is the CONCRETE type's own concern, not
/// the bus's. Contrast with `MapRemoteBucket`'s `bucket_group` map, which
/// stays mutable across the whole run (a NEW group name can appear in a
/// later wave) and is therefore resolved per-step, inline in the main
/// loop, rather than pre-scanned like this bus — see the module doc note
/// for why that mechanism is not unified with this one in this packet.
#[derive(Default)]
pub struct ArtifactBus {
    entries: BTreeMap<&'static str, Arc<dyn Any + Send + Sync>>,
}

impl ArtifactBus {
    /// An empty bus — the default for any graph run whose step kinds
    /// declare no `Artifact` ports (every kind shipped before #1530
    /// Packet 0, and every Tier 1 builtin as of this packet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create the named artifact, calling `factory` only the FIRST
    /// time `name` is seen (idempotent re-registration — two step kinds
    /// that happen to declare the SAME `Artifact` port name share the one
    /// instance, the same "first declaration wins" semantics
    /// `bucket_groups`' `.entry(...).or_insert_with(...)` already
    /// establishes for bucket groups). Intended to be called only from the
    /// scheduler's pre-wave-loop scan, on the main thread.
    pub fn materialize(&mut self, name: &'static str, factory: fn() -> Arc<dyn Any + Send + Sync>) {
        self.entries.entry(name).or_insert_with(factory);
    }

    /// Look up a named artifact and downcast it to `T`. Returns `None`
    /// when no port materialized `name` in this run, OR when it did but at
    /// a different concrete type than `T` — a caller/kind naming mismatch
    /// (see [`Port`]'s doc on why typing here is by-convention, not
    /// schema-checked). A downcast miss is a bug to catch during
    /// development, never a panic.
    pub fn get<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.entries.get(name)?.clone().downcast::<T>().ok()
    }

    /// Unconditionally set a named artifact to a CALLER-SUPPLIED value
    /// (#1530 Packet 1), overwriting whatever a kind's `provides()` factory
    /// may already have materialized for `name` — the caller-seed path
    /// `run_step_graph` merges AFTER its own `provides()` pre-scan (see
    /// that function's doc), so a caller wins over a factory default for
    /// any name it names. Use this for run-scoped state only the CALLER
    /// knows the real value of (a pre-stamped envelope, a value read from
    /// the run's own inputs) — a `Port::artifact` factory can only ever
    /// build a context-free default (see `Port`'s doc on why its factory is
    /// a plain `fn`, not a capturing closure). Like [`Self::materialize`],
    /// intended to be called only from the scheduler's pre-wave-loop setup
    /// (or, symmetrically, from the CALLER assembling `seed_artifacts`
    /// before that call), never after the bus has crossed into a worker
    /// thread.
    pub fn seed(&mut self, name: &'static str, value: Arc<dyn Any + Send + Sync>) {
        self.entries.insert(name, value);
    }
}

/// (#1442, bus seam added #1530 Packet 0) The execution context the
/// SCHEDULER supplies to each step's [`StepKind::run_streaming`] — seams
/// that must originate OUTSIDE the step (so the step kind holds no caller
/// `Arc` of its own and stays tier-pure):
///
/// 1. **Live emitter (#1442 gate C3, "no blind runs").** A channel back to
///    the scheduler's own emission seam. A step that produces per-item
///    records mid-run sends them through [`StepRunCtx::emit`] so they reach
///    the flow stream LIVE — before the step completes — instead of
///    batching into [`StepOutcome::flow_records`] at wave-drain. The step
///    NEVER touches the global flow sink directly: the scheduler owns the
///    sink (and the lab/fleet boundary that picks WHICH sink), so routing
///    through this channel preserves that boundary.
/// 2. **Scheduler-supplied shared remote bucket (#1442).** When a step
///    names a `bucket_group`, the scheduler resolves the group's shared
///    [`MapRemoteBucket`] and hands it here; sibling steps of the same group
///    meter one allowance BETWEEN them. `None` when the step named no group
///    (the kind falls back to a step-scoped bucket). Deliberately NOT
///    unified with seam 4 below — see [`ArtifactBus`]'s doc for why.
/// 3. **The caller-supplied dispatch interceptor** for `dispatch.map` items
///    (`None` on every production path — see [`MapDispatchOverride`]).
/// 4. **The run-scoped [`ArtifactBus`] (#1530 Packet 0).** Materialized
///    once by the scheduler before the graph's wave loop starts, shared by
///    reference into every step for the WHOLE run. A step reads its
///    declared [`PortKind::Artifact`] ports through [`StepRunCtx::artifact`].
///    Always present (an empty bus when no kind in the graph declares an
///    `Artifact` port), so no `Option` wrapping is needed at this layer —
///    a lookup by name is the "is it there" check.
pub struct StepRunCtx {
    emitter: Option<std::sync::mpsc::Sender<WaveSignal>>,
    remote_bucket: Option<Arc<Mutex<MapRemoteBucket>>>,
    dispatch_override: Option<MapDispatchOverride>,
    artifacts: Arc<ArtifactBus>,
}

/// (#1483 Bug 3) One message on a wave's live streaming channel from a
/// `run_bounded` worker thread back to `run_step_graph`'s main-thread drain.
/// Two shapes ride ONE channel so the main thread interleaves them without a
/// `select` (`std::sync::mpsc` has none):
///
/// - [`WaveSignal::Record`] — a per-item flow record a step emits mid-run via
///   [`StepRunCtx::emit`] (the #1442 gate-C3 live-emission seam).
/// - [`WaveSignal::StepTerminal`] — the step's OWN terminal transition, sent
///   by the scheduler's job wrapper the moment THAT job finishes. Before
///   #1483, every step's terminal status was applied at wave-drain (after
///   `run_bounded` returned, i.e. after the SLOWEST job in the wave), so a
///   fast seat's node stayed `running` — clock ticking — until the whole wave
///   flushed. Streaming the terminal transition freezes each done seat's node
///   the instant its own dispatch completes, WITHOUT relaxing the wave
///   scheduling barrier (the next wave still waits for `run_bounded` to
///   return, so a dependent step never starts early).
// `Record` carries a whole `FlowRecord` by value — the same unboxed payload
// the pre-#1483 `Sender<FlowRecord>` channel already moved per send. Boxing it
// to shrink the enum would add a heap allocation to the hot per-item record
// path for no behavioral gain, so the size asymmetry with `StepTerminal` is
// deliberate.
#[allow(clippy::large_enum_variant)]
pub enum WaveSignal {
    /// A live per-item flow record (from [`StepRunCtx::emit`]).
    Record(FlowRecord),
    /// A step's terminal transition, keyed by its position in the wave's
    /// `ready_ids`. `at` is that step's own completion epoch (seconds);
    /// `result` is `Ok(output)` / `Err(message)`; `flow_records` are the
    /// step's batched [`StepOutcome::flow_records`] to emit just before the
    /// `step complete`/`step error` lifecycle record (empty for the live-
    /// streaming kinds, which emit per-item via `Record`).
    StepTerminal {
        index: usize,
        at: u64,
        result: std::result::Result<String, String>,
        flow_records: Vec<FlowRecord>,
    },
}

impl StepRunCtx {
    /// `pub` (not `pub(crate)`) since #1530 Packet 1 — a `StepKind` that
    /// migrated its bespoke `Arc<Mutex<_>>` handles onto the [`ArtifactBus`]
    /// (e.g. `darkmux-lab`'s review pipeline) now needs its OWN unit tests,
    /// outside this crate, to exercise `StepKind::run_streaming` directly
    /// with a hand-built context rather than only through a full
    /// `run_step_graph` call — the same reason `ArtifactBus`/`Port` were
    /// already `pub`. Every production caller still goes through
    /// `run_step_graph`, which is the only place that assembles the OTHER
    /// scheduler-owned seams (the live emitter, a `bucket_group`'s shared
    /// bucket) correctly; a hand-built `StepRunCtx` in a test typically
    /// passes `None`/`None` for those two and only a real `ArtifactBus`.
    pub fn new(
        emitter: Option<std::sync::mpsc::Sender<WaveSignal>>,
        remote_bucket: Option<Arc<Mutex<MapRemoteBucket>>>,
        dispatch_override: Option<MapDispatchOverride>,
        artifacts: Arc<ArtifactBus>,
    ) -> Self {
        Self { emitter, remote_bucket, dispatch_override, artifacts }
    }

    /// Emit one flow record LIVE through the scheduler's emission seam
    /// (#1442 gate C3). A `None` emitter (a context with no streaming sink —
    /// e.g. a step kind exercised in a unit test outside the scheduler)
    /// silently drops it; the kind's batched [`StepOutcome::flow_records`]
    /// remains the fallback path.
    pub fn emit(&self, record: FlowRecord) {
        if let Some(tx) = &self.emitter {
            // A closed channel (the scheduler stopped draining, e.g. on an
            // early return) is not a step-level error — the record is best-
            // effort observability, never load-bearing control flow.
            let _ = tx.send(WaveSignal::Record(record));
        }
    }

    /// The scheduler-supplied shared remote-token bucket for this step's
    /// `bucket_group`, if the step named one. A grouped `dispatch.map` uses
    /// THIS across its whole collection loop; an ungrouped one gets `None`
    /// here and creates its own step-scoped bucket.
    pub fn remote_bucket(&self) -> Option<&Arc<Mutex<MapRemoteBucket>>> {
        self.remote_bucket.as_ref()
    }

    /// The caller-supplied dispatch interceptor for `dispatch.map` items —
    /// `None` on every production path (see [`MapDispatchOverride`]).
    pub fn dispatch_override(&self) -> Option<&MapDispatchOverride> {
        self.dispatch_override.as_ref()
    }

    /// Look up this run's shared artifact by `name`, downcast to `T`
    /// (#1530 Packet 0). `None` when no [`PortKind::Artifact`] port in this
    /// graph materialized `name` under this concrete type — see
    /// [`ArtifactBus::get`]'s doc for the by-convention-naming caveat. The
    /// returned `Arc<T>` is a clone of the SAME instance every other step
    /// of this run sees for `name` — a kind wanting mutation typically
    /// declares its artifact as `Arc<Mutex<_>>` (or another interior-
    /// mutable shape) so `T` here is that wrapper, not the raw payload.
    pub fn artifact<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        self.artifacts.get::<T>(name)
    }
}

/// One step kind's completed outcome. `output` becomes the Step's
/// persisted `Step.output` on success, and is what downstream Steps
/// see as their own `input` entry keyed by this step's id (see
/// `scheduler::gather_inputs`). `flow_records` are ADDITIONAL records
/// the step kind wants emitted alongside the scheduler's own step-
/// lifecycle bookends (most built-in kinds return an empty `Vec` today —
/// their own dispatch primitives already emit their own records via the
/// ordinary thread-safe `darkmux_flow::record()` free function, which,
/// unlike `darkmux_flow::bookend::BookendGuard`, has no non-`Send`
/// state and is safe to call from inside a `run_bounded` worker thread
/// directly).
#[derive(Debug)]
pub struct StepOutcome {
    pub output: String,
    pub flow_records: Vec<FlowRecord>,
}

/// One registered step-kind implementation. `run` is synchronous and
/// blocking (matches every other dispatch primitive in darkmux — see
/// `workloads::types::WorkloadProvider`'s own doc: "darkmux is a single-
/// task CLI so blocking is fine"). `Send + Sync` so an `Arc<dyn
/// StepKind>` can be cloned into a `run_bounded` worker's `'static`
/// job closure (see the module doc on `StepKindRegistry`).
///
/// `input` is the gathered `output` text of every already-`Complete`
/// dependency, keyed by that dependency's Step id (`scheduler::
/// gather_inputs`) — a step kind decides for itself whether/how to
/// weave prior-step output into its own request (see
/// `DispatchInternalStepKind`/`DispatchSingleShotStepKind` for the
/// convention used).
///
/// `task` is the Step's OWNING `Task` — resolved by `run_step_graph` from
/// `Step.task_id` (falling back to a synthetic empty `Task` if the graph's
/// caller never registered one, e.g. a scheduler-level test exercising
/// pure Step scheduling with no Task-assignment concerns — see
/// `scheduler::run_step_graph`'s doc). A Task is the ASSIGNABLE unit
/// (#1230/#1341) — like a Jira ticket assigned to one crew member,
/// `task.role_id`/`task.profile_name`/`task.workdir`/`task.image` are
/// properties of the whole job, fixed for its duration, not re-declared at
/// every Step; a dispatch-shaped step kind (`DispatchInternalStepKind`)
/// sources its assignment from THESE fields first, falling back to
/// `Step.config` only when the Task leaves a field unset. Purely-procedural
/// step kinds (`procedural.*`) ignore `task` entirely.
pub trait StepKind: Send + Sync {
    fn id(&self) -> &'static str;
    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome>;

    /// (#1442) The scheduler's ACTUAL entry point — `run` with the
    /// scheduler-supplied [`StepRunCtx`] (live emitter + shared remote
    /// bucket) threaded in. Defaults to ignoring the context and delegating
    /// to [`StepKind::run`], so every existing kind keeps its exact behavior
    /// (records batched into [`StepOutcome::flow_records`], a step-scoped
    /// bucket) with no change. A kind that wants LIVE per-item emission or a
    /// scheduler-shared `bucket_group` (`dispatch.map`) overrides THIS and
    /// leaves `run` as the ctx-free path unit tests still drive directly.
    ///
    /// The context is Arc/channel-backed and `Send` so it crosses the
    /// `run_bounded` worker-thread boundary alongside the job closure.
    fn run_streaming(
        &self,
        step: &Step,
        task: &Task,
        input: &BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let _ = ctx;
        self.run(step, task, input)
    }

    /// (#1402) A short, human-facing name for this kind — the graph lens,
    /// the viewer's mission drill-down, and `mission status` all render
    /// THIS instead of the raw registry id (`"dispatch.internal"` reads as
    /// "Dispatch"). Registered once beside each kind's constructor, right
    /// next to `id()`.
    ///
    /// Defaults to `id()` — a kind that hasn't been given a nicer label yet
    /// still renders something legible rather than a hole in the fallback
    /// chain (StepKind display name → kind id → step id → `"unknown"`, see
    /// `darkmux-serve`'s `mission_graph` module doc). Every Tier 1 builtin
    /// and every Tier 3 kind shipped with darkmux overrides this; the
    /// default exists for third-party/future kinds that haven't yet.
    fn display_name(&self) -> &'static str {
        self.id()
    }

    /// (#1230 Packet 3) Which local model, if any, this step needs
    /// resident before it can run — feeds `run_step_graph`'s per-step
    /// `Residency::Local(Placement)` vs `Residency::Remote` classification
    /// (`concurrent_dispatch::run_bounded`'s wave-safety mechanism).
    ///
    /// `None` (the default — every kind's behavior before this hook
    /// existed, and every step kind that isn't a local-model dispatch,
    /// e.g. `procedural.*`) classifies the step `Residency::Remote`:
    /// cap-bounded concurrency only, no RAM-safety wave reasoning. A
    /// dispatch-shaped local kind overrides this to resolve a real
    /// `Placement` from its own config/role/Task assignment.
    ///
    /// **Best-effort, fails open.** This is a SCHEDULING CLASSIFICATION
    /// hint, not the dispatch's own model resolution — the step's `run`
    /// method (and whatever it wraps, e.g. `dispatch::dispatch`'s own
    /// preflight) still does its own full, strict resolution when it
    /// actually executes. If this can't cleanly resolve a placement (no
    /// role, unknown role, quarantined profile, remote endpoint, …) it
    /// returns `None` rather than erroring — worst case the step is
    /// scheduled as `Remote` (today's behavior for every kind), never a
    /// hard failure purely from misclassification.
    ///
    /// `input` is the SAME gathered dependency-output map `run` will
    /// receive (the scheduler computes it once per ready step, before
    /// residency classification — see `scheduler::run_step_graph`). A kind
    /// whose model need is DATA-DEPENDENT can inspect it and return `None`
    /// when the inputs make its dispatch a guaranteed no-op, so the wave
    /// loader never loads a model the step is certain not to use (#1426
    /// ship-2 operator decision — the review verify seat with an empty
    /// confirmed docket is the first consumer). Kinds with static needs
    /// ignore it.
    fn residency(
        &self,
        step: &Step,
        task: &Task,
        input: &std::collections::BTreeMap<String, String>,
    ) -> Option<darkmux_gestalt::Placement> {
        let _ = (step, task, input);
        None
    }

    /// (#1530 Packet 0) The [`Port`]s this kind PRODUCES — what a future
    /// consumer (a graph validator, the viewer's port-wiring annotation)
    /// can expect available after this step runs. For an
    /// [`PortKind::Artifact`] port, this is ALSO what `run_step_graph`
    /// scans to know which artifacts to materialize onto the run's
    /// [`ArtifactBus`] before its wave loop starts (see that scan's own
    /// doc in `scheduler::run_step_graph`).
    ///
    /// Defaults to `&[]` — every kind's behavior before this hook existed,
    /// and every Tier 1 builtin as of this packet (`dispatch.internal`,
    /// `dispatch.map`, `dispatch.single_shot`, `procedural.shell`,
    /// `procedural.noop`): none of them declare ports, so nothing about
    /// their execution changes by this method's mere existence. A kind
    /// that shares state across steps overrides this — see [`Port`]'s doc
    /// for the two port shapes and when to use each.
    fn provides(&self) -> &'static [Port] {
        &[]
    }

    /// (#1530 Packet 0) The [`Port`]s this kind CONSUMES — the mirror of
    /// [`StepKind::provides`]. Declares intent for a future consumer
    /// (validator/viewer); the scheduler does not enforce that a
    /// `requires` name resolves to a matching `provides` anywhere in the
    /// graph — see [`Port`]'s doc on why typing here is by-convention, not
    /// schema-checked. Defaults to `&[]`, same rationale as `provides`.
    fn requires(&self) -> &'static [Port] {
        &[]
    }

    /// (#1530 Packet 2) Declares whether this kind is a SIGN-OFF GATE — a
    /// step whose completion should hold the owning Task/Phase/Mission at a
    /// human/frontier-reviewable checkpoint rather than letting the graph's
    /// caller finalize automatically. Defaults to `false` — every kind's
    /// behavior before this hook existed, and every Tier 1 builtin today
    /// (none of them gate anything; gating is a judgment-boundary concern,
    /// never a generic config-driven kind's job).
    ///
    /// This is the property `darkmux`'s coder-phase launcher
    /// (`coder_phase_gate_outcome`) reads to find WHICH step in a graph is
    /// its gate, rather than hardcoding a step-id naming convention
    /// (`"<phase>-verify-step"`) — a caller scans the graph's steps for the
    /// one whose registered kind reports `is_gate() == true`. Today exactly
    /// one kind overrides this (`darkmux`'s own
    /// `coder_phase::MissionVerifyStepKind`), and exactly one caller reads
    /// it (`coder_phase_gate_outcome`) — but the declaration lives here, on
    /// the trait, specifically so a FUTURE generic runner (#1530 Packet 3)
    /// can apply the same gate-holding behavior to ANY graph purely from
    /// this property, with no coder-phase-specific knowledge baked into the
    /// runner itself. A graph may have zero or one gate step today (this
    /// crate does not enforce "at most one" — a caller that finds more than
    /// one is a config/kind-authoring bug to surface, not silently resolve).
    fn is_gate(&self) -> bool {
        false
    }
}

//! `StepKind` trait + `StepOutcome` — the step-kind execution contract.

use crate::remote_budget::RemoteBudget;
use crate::types::{Step, Task};
use anyhow::Result;
use darkmux_flow::FlowRecord;
use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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
///   shape `RemoteBudget`'s `bucket_group` already proves out for the
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
/// from config, the shape `RemoteBudget` needs) is exactly the case
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
/// the bus's. Contrast with `RemoteBudget`'s `bucket_group` map, which
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

    /// Is `name` on the bus at all, whatever its concrete type?
    ///
    /// (#1530) The scheduler's composition check needs presence WITHOUT
    /// knowing `T` — it validates a graph's declared `requires()` ports
    /// against what was materialized or seeded, and it holds only
    /// `dyn StepKind`, so the concrete artifact type isn't available to it.
    /// Deliberately distinct from [`ArtifactBus::get`]: `get` conflates
    /// "absent" with "present at another type" (both `None`), which is the
    /// right call for a reader picking up its own artifact but the wrong
    /// one for a presence check that must not silently pass a type
    /// mismatch off as a missing port.
    pub fn has(&self, name: &str) -> bool {
        self.entries.contains_key(name)
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
///    [`RemoteBudget`] and hands it here; sibling steps of the same group
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
    remote_bucket: Option<Arc<Mutex<RemoteBudget>>>,
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
    /// (#2517) THIS step's own dispatch is actually beginning — sent from
    /// inside the job closure, on its own worker thread, the instant
    /// before `kind.run_streaming(...)` is called. The main thread applies
    /// it by stamping `Step::started_ts` and persisting — see that
    /// field's own doc for why this can no longer happen at wave
    /// admission (every step in a wave used to share ONE `now_unix()`
    /// stamped before any of them had dispatched, #2517). Not a terminal
    /// signal: `index` is NOT added to `applied` on receipt.
    StepDispatching { index: usize, at: u64 },
    /// A step's terminal transition, keyed by its position in the wave's
    /// `ready_ids`. `at` is that step's own completion epoch (seconds);
    /// `result` is `Ok(output)` / `Err(message)`; `flow_records` are the
    /// step's batched [`StepOutcome::flow_records`] to emit just before the
    /// `step complete`/`step error` lifecycle record (empty for the live-
    /// streaming kinds, which emit per-item via `Record`).
    StepTerminal {
        index: usize,
        at: u64,
        /// (#1877 item 3) This job's OWN `kind.run_streaming(...)` duration,
        /// timed with an `Instant` pair taken strictly around that one call
        /// inside the job closure that produced it — never derived from `at`
        /// (whole-second epoch, too coarse) and never from the step's
        /// `started_ts` (which, post-#2517, IS also taken right before this
        /// same call — but as a whole-second `now_unix()` stamp, still too
        /// coarse for a millisecond-accurate duration; this field stays its
        /// own `Instant` pair regardless). This is what makes a per-step
        /// [`crate::run_record::StepRecord::wall_ms`] correct under
        /// concurrency: each sibling's duration reflects only its own
        /// dispatch, not the wave's.
        wall_ms: u64,
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
        remote_bucket: Option<Arc<Mutex<RemoteBudget>>>,
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
    pub fn remote_bucket(&self) -> Option<&Arc<Mutex<RemoteBudget>>> {
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

/// (#2394) What ONE step consumes — the exhaustive classification
/// [`StepKind::seat`] returns and the scheduler dispatches on. This is the
/// polymorphic replacement for `residency() -> Option<Placement>`, whose
/// default `None` conflated three unrelated facts and, being the DEFAULT,
/// applied to every kind that had never thought about the question.
///
/// **Exhaustive on purpose, and matched with no `_` arm anywhere.** Adding
/// a variant here is a compile error at every site that classifies one —
/// [`crate::concurrent_dispatch::run_bounded`]'s partition and the
/// scheduler's observability stamp — which is exactly the property the old
/// `Option` did not have. Extension is a new variant plus the compiler
/// naming every place that must handle it.
pub enum SeatClaim {
    /// A LOCAL model this step needs resident before it can run. Wave-
    /// planned by gestalt and #1487 lease-protected: the wave loader makes
    /// this [`darkmux_gestalt::Placement`] resident first, and the run's
    /// residency lease keeps a concurrent darkmux command's `Exclusive`
    /// reconcile from evicting it mid-generation.
    LocalModel(darkmux_gestalt::Placement),
    /// A HOSTED endpoint seat. Consumes zero local pool (#1177/#1260),
    /// never reaches gestalt's planner, bounded only by
    /// `remote.concurrent_cap` — which is what that cap is FOR.
    RemoteEndpoint,
    /// This step runs NO model at all: `procedural.shell`,
    /// `procedural.noop`, `mods.gate`, `records.gather`,
    /// `deliver.github_review`, the crawl planners, every render/collect
    /// half that only shuffles data. Bounded by its own
    /// `runtime.dispatch_free_concurrency`, never by the endpoint cap —
    /// each such step is already individually bounded by
    /// `runtime.step_command_timeout_seconds`.
    ///
    /// This variant is the whole point of #2394: before it existed, a
    /// dispatch-free step was indistinguishable from a hosted-endpoint
    /// dispatch, so six independent `procedural.shell` waits ran strictly
    /// one at a time.
    NoModel,
    /// A LOCAL seat whose [`darkmux_gestalt::Placement`] could NOT be
    /// resolved — an unresolvable role, an unloadable profile registry, no
    /// active profile, a local model with no declared `n_ctx`. #1509's
    /// fail-open, now NAMED instead of silent.
    ///
    /// Scheduled like a remote seat (its historical behavior, unchanged),
    /// but never quietly: the scheduler `eprintln!`s and emits a `Warn`
    /// flow record naming the step and `reason`, because this dispatch
    /// meant to be local and just lost its #1487 lease protection — a
    /// concurrent command's reconcile can evict its model mid-generation
    /// with no warning at all otherwise.
    LocalModelUnresolved { reason: String },
}

impl SeatClaim {
    /// The stable wire label stamped onto the `step start` flow record's
    /// `payload.seat_class` (FLOW 1.41.0). Kept next to the variants so a
    /// new one cannot ship without a label — the `match` here is exhaustive
    /// with no `_` arm, same discipline as every other consumer.
    pub fn label(&self) -> &'static str {
        match self {
            SeatClaim::LocalModel(_) => "local_model",
            SeatClaim::RemoteEndpoint => "remote_endpoint",
            SeatClaim::NoModel => "no_model",
            SeatClaim::LocalModelUnresolved { .. } => "local_model_unresolved",
        }
    }
}

/// (#2577) How a [`StepKind`] that spawns a subprocess resolves the
/// directory it runs in — specifically, whether it can ever fall back to
/// the darkmux PROCESS's own ambient working directory (`std::env::
/// current_dir()`), the one value every scheduler worker thread shares
/// with every other test and every other step in the same run.
///
/// **Origin.** `procedural.shell` used to inherit that ambient directory
/// unconditionally when a step declared no `cwd`/`workdir` (#2532) — fine
/// for a single darkmux CLI invocation, but this project's own worktree
/// workflow routinely deletes the directory darkmux was started from,
/// and worse, in a scheduler test the SAME process-global is shared by
/// every one of a wave's concurrently-dispatched sibling steps (#2577:
/// `scheduler::tests::dispatch_free_siblings_do_not_serialize_behind_the_
/// remote_cap` runs four of these on four worker threads at once). #2532
/// fixed `procedural.shell` itself: resolve a documented tier chain
/// (`step_kinds::builtins::resolve_shell_cwd`) and refuse loudly, naming
/// the tier, when nothing resolves. This enum is the DECLARATION half of
/// that fix — every [`StepKind`] that spawns a subprocess states which
/// side of the line it is on, so a future sibling that copies
/// `procedural.shell`'s shape (or invents a new one) cannot silently land
/// on the unconditional-inheritance behavior #2532 retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdPolicy {
    /// This kind spawns no subprocess at all, OR every subprocess it
    /// spawns is always given a directory resolved from step/task config
    /// (or a value computed earlier in the same run, e.g. a materialized
    /// worktree path) — the process's own ambient working directory is
    /// never consulted. The overwhelming majority of kinds are this.
    NoAmbientDependency,
    /// This kind spawns a subprocess that MAY run in the darkmux
    /// process's own ambient working directory, when nothing more
    /// specific is configured — and REFUSES loudly (rather than silently
    /// inheriting a vanished one) when that directory no longer exists.
    /// See `step_kinds::builtins::resolve_shell_cwd`'s own doc for the
    /// full resolution chain and the refusal message it produces.
    ///
    /// **Exactly one kind should ever report this: `procedural.shell`.**
    /// `step_kinds::registry`'s `with_builtins_has_exactly_one_kind_with_
    /// ambient_cwd_fallback` test asserts that over the actual registered
    /// registry (a real enumeration, not a source scan) for every Tier 1
    /// builtin; it cannot see Tier 2/3 kinds registered by an individual
    /// mission (`mods.gate`, the crawl planners `crawl.plan`/`plan.sites`,
    /// the crawl unit kinds `crawl.unit`/`crawl.summary`, `mission.worktree`/
    /// `mission.coder`/`mission.verify`, `deliver.github_review`,
    /// `records.gather` — ten kinds total, see that test's own comment)
    /// since those live in separate crates with their own registration
    /// functions and no single shared registry walks all of them today —
    /// each carries its own explicit `cwd_policy()` override recording a
    /// #2577 hand audit instead (see that issue's investigation, and each
    /// kind's own doc), and every one of them either spawns no subprocess
    /// or always resolves an explicit directory first.
    AmbientWithRefusal,
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
/// `Step.config` only when the Task leaves a field unset.
///
/// (#2532) Purely-procedural step kinds (`procedural.*`) read at most ONE
/// of these fields, and only `procedural.shell` reads any: `task.workdir`,
/// as the directory its command runs in, under the SAME task-before-step-
/// config tier order stated above (`builtins::resolve_shell_cwd`). That
/// kind's own `cwd` key still outranks both — `cwd` has no Task
/// counterpart, so no tier question arises for it. Every other
/// `procedural.*` kind ignores `task` entirely.
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

    /// (#1979) The `session_id` this kind's own DISPATCH records land under —
    /// the FORWARD direction (step -> session), which only the kind can
    /// answer, because the kind is what chooses it at dispatch time.
    ///
    /// **Do not confuse this with attribution.** Mapping a record BACK to its
    /// step is the reverse direction, and it needs no kind knowledge at all:
    /// `darkmux-serve`'s `step_for_record` and the viewer's `stepForRecord`
    /// resolve it from the record alone via `payload.step_id` -> a
    /// step-scoped `session_id` -> `handle`. A consumer that switches on
    /// `step.kind` to attribute a record is a bug. This method exists only
    /// because ghost-suppression must predict a step's session BEFORE any
    /// record for it exists.
    ///
    /// Why it has to be asked rather than matched: `darkmux-serve`'s
    /// `step_session_id` used to re-derive this with `match
    /// step.kind.as_str()` and a `_ => None` arm, so the convention lived in
    /// two files that nothing kept agreeing. A new DISPATCHING kind fell into
    /// the catch-all, its session went unclaimed, and its records surfaced as
    /// a duplicate untracked "ghost" row on the runs board — with no test, no
    /// doctor check and no compile error to say so. The failure needed an
    /// operator to notice a doubled row.
    ///
    /// Defaults to `session_id::step(&step.id)`, matching that helper's own
    /// documented role as "the step-scoped default dispatch session id", so a
    /// new dispatching kind is claimed by construction. An explicit
    /// `config["session_id"]` always wins — a caller that names the session
    /// owns it.
    ///
    /// Return `None` ONLY for a kind that genuinely never dispatches
    /// (`procedural.*`). That is a deliberate opt-out, not a fallback: the
    /// registry conformance test in `step_kinds::registry` requires every
    /// registered kind to either resolve a session or be named on the
    /// documented no-dispatch list, so "nobody implemented it yet" cannot
    /// masquerade as "there is nothing here".
    ///
    /// This is the kind's OWN dispatch session only. Every step ALSO has its
    /// scheduler-emitted lifecycle records under
    /// `session_id::task(&step.task_id)` (`scheduler::step_lifecycle_record`)
    /// — that is the scheduler's invariant, true for every kind, so a
    /// consumer adds it once rather than asking each kind about it.
    fn dispatch_session_id(&self, step: &Step) -> Option<String> {
        if let Some(sid) = step.config.get("session_id").and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
        Some(darkmux_types::session_id::step(&step.id))
    }

    /// (#2394) What this step CONSUMES — the seat it claims. **Required:
    /// there is no default body, on purpose.** The compiler is the
    /// completeness check: a new `StepKind` cannot compile without saying
    /// what it consumes, and a new [`SeatClaim`] variant cannot compile
    /// until every place that dispatches on one handles it.
    ///
    /// This replaced `residency() -> Option<Placement>`, whose DEFAULT
    /// `None` was a lie. `None` meant BOTH "a hosted endpoint, cap-bounded"
    /// AND "no model at all" AND "a local seat whose placement would not
    /// resolve" — three genuinely different things collapsed into one
    /// silence, and the silence was the default, so a kind that never said
    /// anything was classified as a remote model dispatch. #2394 is what
    /// that cost live: six independent `procedural.shell` waits, none of
    /// which speaks to a model, executed strictly one at a time behind a
    /// `remote_cap: 1` meant to protect a hosted endpoint — up to 54
    /// minutes for a 9-minute window.
    ///
    /// The four claims, and what the scheduler does with each:
    ///
    /// - [`SeatClaim::LocalModel`] — gestalt-wave-planned and #1487
    ///   lease-protected; the wave loader makes the [`darkmux_gestalt::
    ///   Placement`] resident before the step runs.
    /// - [`SeatClaim::RemoteEndpoint`] — a hosted endpoint; consumes zero
    ///   local pool, never reaches gestalt's planner, bounded by
    ///   `remote.concurrent_cap`.
    /// - [`SeatClaim::NoModel`] — dispatch-free. Bounded by its OWN
    ///   `runtime.dispatch_free_concurrency` (these are shell/store
    ///   operations, already bounded individually by
    ///   `runtime.step_command_timeout_seconds`), never by the endpoint cap.
    /// - [`SeatClaim::LocalModelUnresolved`] — a local seat whose placement
    ///   could not be resolved (#1509's fail-open). Runs under the remote
    ///   cap as it always has, but LOUDLY: the scheduler `eprintln!`s and
    ///   emits a `Warn` flow record naming the step and the reason, because
    ///   this dispatch just lost its residency-lease protection.
    ///
    /// **Still best-effort for the LOCAL case, in the same sense as before**
    /// — this is a SCHEDULING CLASSIFICATION, not the dispatch's own model
    /// resolution. The step's `run` method (and whatever it wraps) does its
    /// own full, strict resolution when it actually executes. What changed
    /// is that "I could not resolve" is now a NAMED claim rather than an
    /// unmarked fallthrough.
    ///
    /// `input` is the SAME gathered dependency-output map `run` will
    /// receive (the scheduler computes it once per ready step, before
    /// classification — see `scheduler::run_step_graph`). A kind whose
    /// model need is DATA-DEPENDENT can inspect it and claim
    /// [`SeatClaim::NoModel`] when the inputs make its dispatch a
    /// guaranteed no-op, so the wave loader never loads a model the step is
    /// certain not to use (#1426 ship-2 operator decision — the review
    /// verify seat with an empty confirmed docket is the first consumer).
    /// Kinds with static needs ignore it.
    ///
    /// `ctx` (#1530 Packet 3a) is the SAME [`StepRunCtx`] `run_streaming`
    /// receives — most callers ignore it, but a kind whose decision depends
    /// on run-scoped [`ArtifactBus`] state reads it here via
    /// [`StepRunCtx::artifact`]. The scheduler passes the run-scoped bus
    /// materialized before its wave loop starts; `seat()` runs on the main
    /// thread, one call ahead of `run_streaming`'s own ctx.
    fn seat(
        &self,
        step: &Step,
        task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> SeatClaim;

    /// (#1511) The role id this step will ACTUALLY dispatch under — the
    /// SAME answer the kind's own dispatch resolves, from the SAME source
    /// [`StepKind::seat`] resolves its placement from. **Required: there is
    /// no default body, on purpose**, for exactly the reason `seat` has
    /// none — the compiler is the completeness check, and a default here
    /// would be a guess made on behalf of a kind that never thought about
    /// the question.
    ///
    /// The scheduler's licensed-adjacent consent filter
    /// (`scheduler::run_step_graph`) is the consumer: it asks THIS, then
    /// refuses the step if that role has no recorded operator ack, strictly
    /// before the wave loader can make any model resident.
    ///
    /// **Why it is a method and not a field read.** #1511's first fix read
    /// `task.role_id` (falling back to `step.config.role_id`) in the
    /// scheduler — a PARALLEL GUESS at what the kind would do. Nothing tied
    /// the two together, and two shipping kinds resolve their dispatch role
    /// somewhere else entirely: `mission.coder` takes it off the run's
    /// [`ArtifactBus`] (`task.role_id` is never read), and `crawl.unit`
    /// defaults to `"crawler"` when the task names nothing. Both therefore
    /// loaded a licensed-adjacent model with the gate fully in place — the
    /// guess said `"coder"`, or `None`, while the seat resolved the
    /// forbidden role. One method, asked of the kind itself, is what
    /// removes the second source of truth.
    ///
    /// **Contract, and what `None` may mean.** Return `Some(role)` for any
    /// seat that will actually make a model resident or reach an endpoint —
    /// i.e. whenever [`StepKind::seat`] claims [`SeatClaim::LocalModel`] or
    /// [`SeatClaim::RemoteEndpoint`]. `None` is legal in exactly two cases,
    /// and neither of them can load a model behind the gate's back:
    ///
    /// - [`SeatClaim::NoModel`] — the kind dispatches nothing at all
    ///   (`procedural.*`, the render/collect halves). There is no role to
    ///   consent to. Note this ALSO covers a data-dependent no-op: an EMPTY
    ///   `dispatch.map` claims `NoModel` and returns `None` here, so it is
    ///   not refused for a role it was never going to dispatch.
    /// - [`SeatClaim::LocalModelUnresolved`] — the kind meant to dispatch
    ///   locally and could not resolve WHAT. The wave loader performs no
    ///   load for this claim (see that variant's own doc), so nothing
    ///   reaches RAM here; whatever the step's own body then does still
    ///   passes through `dispatch_internal`'s in-body consent check before
    ///   its own load. Returning `None` because the role is genuinely
    ///   unknown is honest; returning `None` while claiming a real model
    ///   seat is the fail-open this method exists to make impossible, and
    ///   the registry conformance tests assert exactly that implication.
    ///
    /// A kind that dispatches a bare model rather than a ROLE
    /// (`dispatch.single_shot`, `dispatch.map` — config `model` + `user`,
    /// no role prompt anywhere) returns `None`. The consent gate is keyed
    /// on role ids because it discloses a ROLE'S prompt doctrine; a kind
    /// with no role has nothing for it to disclose.
    ///
    /// Parameters are identical to [`StepKind::seat`]'s, so a kind whose
    /// role and placement come from one source can read that source once in
    /// both.
    ///
    /// **What the gate's `ctx` and `input` actually carry — read this
    /// before sourcing a role from anything else.** The scheduler calls
    /// this from its consent filter, EARLIER in the same wave-loop
    /// iteration than the job loop that calls `seat`, so the two get the
    /// same parameter list but not identical contents. Guaranteed here:
    ///
    /// - `ctx.artifact::<T>` — the run-scoped [`ArtifactBus`], the same
    ///   `Arc` the job loop's `ctx` and every `run_streaming` get. That is
    ///   the source `mission.coder` reads, and a scheduler test
    ///   (`the_gates_ctx_carries_the_run_scoped_bus_the_kind_reads_its_role_from`)
    ///   pins it.
    /// - `ctx`'s `dispatch.map` override — cloned from the same
    ///   caller-supplied seam the job loop threads through.
    ///
    /// NOT populated here, and `None` where the job loop passes a real
    /// value: the wave-channel record EMITTER (that channel does not exist
    /// yet — this filter reports through `apply_step_terminal` instead),
    /// and the shared REMOTE TOKEN BUCKET (a consent check spends
    /// nothing). And `input` is gathered BEFORE this wave's ready steps
    /// flip to `Running`, where the job loop's is gathered after — so a
    /// sibling's status can read differently between the two maps.
    ///
    /// So: source a dispatch role from the Step, the Task, or the artifact
    /// bus. A kind reaching for the emitter or the remote bucket here gets
    /// `None`; one keying off a sibling's `Running` status in `input` is
    /// reading something its own `seat` will disagree with — which is the
    /// second-source-of-truth defect this method exists to remove.
    fn dispatch_role(
        &self,
        step: &Step,
        task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Option<String>;

    /// (#2614 review, MUST FIX) A fallible, kind-owned precheck the
    /// scheduler's `run_step_graph` consults for EVERY ready step, on the
    /// main thread, strictly before `plan_waves`/`ensure_wave_loaded` can
    /// make anything resident — the exact same hoist point, and the exact
    /// same "defaults to success, fails only the offending step" shape,
    /// the licensed-adjacent ack gate (`dispatch_role` above +
    /// `licensed_adjacent_ack_status`) already established. Where that
    /// gate asks a SEPARATE function keyed on the role this kind
    /// resolves, this one asks the KIND ITSELF, because the precheck
    /// isn't role-keyed — `dispatch.internal`'s is `--resume-from`
    /// checkpoint validation (#2585/#2614), read straight off its own
    /// step config.
    ///
    /// **Why a scheduler-level hook, not a wrapper hoist.** #2585 first
    /// fixed this ONLY on `darkmux dispatch`'s own crew-of-one path
    /// (`dispatch_as_crew_of_one_with` calling the checkpoint validation
    /// before `run_step_graph` even starts) — the identical shape #1510
    /// used for the ack gate before #1511 hoisted THAT into the
    /// scheduler. #2614's review caught the same gap #1511 closed: a
    /// mission config (`mission launch <config>`, the panel) staffing a
    /// `dispatch.internal` step with a `resume_from` in its config never
    /// went through the CLI wrapper at all, so it paid the full residency
    /// cost before the step's own in-body checkpoint check ever ran. This
    /// method is the general fix — one gate, asked once per ready step,
    /// covering every caller of `run_step_graph` (the CLI's crew-of-one
    /// wrapper included) rather than one hoist per entry point. The CLI
    /// wrapper's own pre-mint hoist was deleted once this shipped —
    /// duplicating a NON-INTERACTIVE check (unlike the ack gate's
    /// prompting variant, which still needs its wrapper copy) is pure
    /// drift risk with nothing gained.
    ///
    /// **Never validates the working directory itself.** A kind that
    /// checks `resume_from` here must NOT call
    /// `darkmux_types::workdir::validate_workdir` (or otherwise demand the
    /// intended workspace already exist on disk) — a mission graph's
    /// working directory can legitimately be a path a still-earlier step
    /// in the SAME run materializes (`CwdPolicy`'s own doc: "a value
    /// computed earlier in the same run, e.g. a materialized worktree
    /// path"), so a scheduler-side existence/symlink check here would
    /// refuse work that only becomes valid once the wave actually runs.
    /// `dispatch_internal::validate_resume_checkpoint_content` is the
    /// workdir-INDEPENDENT half of the checkpoint gate (existence, JSON
    /// shape, schema version, role match) — the half safe to hoist here.
    /// The workdir-DEPENDENT half (the origin-workspace/mount-mode match)
    /// stays exactly where it always lived, inside
    /// `dispatch_internal::dispatch`'s own call, which runs after the
    /// wave loads and therefore after the real workspace is resolved.
    ///
    /// Defaults to `Ok(())` — every kind that never reads `resume_from`
    /// (which is all of them except `dispatch.internal`) is unaffected by
    /// this hook's mere existence, same discipline `seat`/`provides`/
    /// `requires` all use for their own no-op defaults.
    fn resume_precheck(
        &self,
        step: &Step,
        task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Result<()> {
        let _ = (step, task, input, ctx);
        Ok(())
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

    /// (#2577) See [`CwdPolicy`]'s own doc for the full origin and
    /// contract. Defaults to [`CwdPolicy::NoAmbientDependency`] — safe for
    /// every kind that spawns no subprocess, which is most of them; a kind
    /// that DOES spawn one and ever falls back to the process's own
    /// ambient working directory must override this and say so.
    fn cwd_policy(&self) -> CwdPolicy {
        CwdPolicy::NoAmbientDependency
    }
}

//! An LM Studio load is planned in the machine's one memory budget (DESIGN.md,
//! "What darkmux does on each dispatch", step 3; ENGINES.md, stage 5).
//!
//! (#3508) A wave's loads are priced as `darkmux machine resources` prices
//! a resident ([`LoadPricing`]: the weights, the KV cache at the context,
//! a margin) and planned against the machine's reclaimable memory
//! ([`ServerRoom::pools`]), the figure the engine path reads, never free
//! pages alone. (#3506) An engine server darkmux started (Tensorfold, rec's
//! Flint) holds its model's memory for as long as it runs, and outlives the
//! dispatch that started it; the planner sees only LM Studio's residents.
//! [`make_room`] runs between the plan and its execution, before anything
//! is unloaded or loaded:
//!
//! 1. The plan's need is what its loads take, less what its own unloads
//!    free ([`darkmux_gestalt::plan_room`]), two ways (#3514): the
//!    commitment (what a load takes when it is made: a GGUF model's cache
//!    is allocated then, an MLX model's grows as the context fills) and the
//!    potential (the whole context filled). A commitment larger than the
//!    whole machine is refused `wont_fit` before free memory is read:
//!    evicting or waiting can never help. With no figure for free memory,
//!    or none for the need, the load goes ahead and LM Studio decides, with
//!    a warning; a load whose architecture is unreadable has no commitment
//!    and is never refused.
//! 2. While the plan's potential does not fit the reclaimable memory, it stops
//!    darkmux's idle servers, the least recently started first, with the
//!    engine path's own eviction ([`crate::engine_dispatch::evict`]): the
//!    leases are read again under the endpoint's lock, where the server is
//!    stopped, and a server a running dispatch holds is never stopped. A
//!    server darkmux did not start (no record matching the live process) is
//!    never seen at all (contract 4).
//! 3. When nothing idle is left and the commitment still does not fit, it
//!    refuses with a stable code (#3510); a potential not reached is only
//!    a warning:
//!    `busy` when a running dispatch holds memory that would free later (a
//!    server, or an LM Studio load the plan keeps because it is pinned),
//!    `memory_in_use` when only memory darkmux did not take is short.
//!
//! darkmux's idle LM Studio loads are the planner's own business: the wave's
//! Exclusive scope already unloads every one the wave does not want, so only
//! servers are evicted for a wave.
//!
//! A preflight load (the compactor, the utility model, a lab run's model,
//! which load outside any wave: [`load_reserved`]) makes the same decision
//! under the same lock, with the same pricing, reservation and codes. It
//! has no Exclusive pass, so once no idle server is left it unloads
//! darkmux's idle LM Studio loads too, as the engine path does: never the
//! one it loads, one a running dispatch holds, or the standing utility
//! binding.

use crate::engine_dispatch::{darkmux_residents, evict, refuse, short_of_memory, EngineRefusal, Machine, Resident, ThisMachine};
use darkmux_gestalt::{
    decide_residency, is_darkmux_owned, plan_room, Action, CatalogFact, ResidencyDecision, Facts, FootprintEstimator, ModelHost, Placement, Plan, PlanRoom,
    PoolFact, PoolId, Pools, ResidentFact,
};
use darkmux_profiles::engine::server::{self, Dirs};
use darkmux_profiles::model_ledger::{LoadFigures, LoadPricer};
use darkmux_types::residency_lease::LeaseGuard;
use darkmux_types::residency_reservation::Reserved;

/// Where darkmux's engine servers are recorded, how long a stop may take,
/// and the machine's memory and leases.
pub(crate) struct ServerRoom<'a> {
    pub(crate) dirs: Dirs,
    pub(crate) bounds: server::Bounds,
    pub(crate) machine: &'a (dyn Machine + Sync),
    /// (#3509) Keeps a load's reservation until its memory pages in, given
    /// the reservation and LM Studio's workers' footprint read before the
    /// load.
    pub(crate) settle: fn(Reserved, Option<u64>),
    /// (#3514) LM Studio's workers' footprint now
    /// ([`darkmux_profiles::model_ledger::lmstudio_workers_footprint`]).
    pub(crate) workers: fn() -> Option<u64>,
}

/// This machine, as the LM Studio path reads it: its utility binding is the
/// default registry's.
static THIS_MACHINE: ThisMachine<'static> = ThisMachine { profiles_file: None };

impl ServerRoom<'static> {
    /// This machine: the records under darkmux's home, its real memory.
    pub(crate) fn this_machine() -> Self {
        ServerRoom {
            dirs: Dirs::under_home(),
            bounds: server::Bounds::default(),
            machine: &THIS_MACHINE,
            settle: settle_on_this_machine,
            workers: darkmux_profiles::model_ledger::lmstudio_workers_footprint,
        }
    }
}

/// (#3509) A load answers before its memory has all paged in (MLX maps its
/// weights and touches them on first use), so its reservation outlives the
/// load: [`settle_load`], on a thread of its own, with the growth of LM
/// Studio's workers' footprint since the load began as the load's footprint
/// (LM Studio names no process per model). (#3514) Not the fall in the
/// machine's free memory: another app, an engine start or another load
/// would end the reservation early. A concurrent LM Studio load does grow
/// the workers, but holds its own reservation.
///
/// The thread owns that one load's reservation and nothing else: no lease
/// (the model is held only while a dispatch runs on it), and no other
/// load's reservation, though the same dispatch made it.
fn settle_on_this_machine(res: Reserved, before: Option<u64>) {
    std::thread::spawn(move || {
        settle_load(
            res,
            before,
            &darkmux_profiles::model_ledger::lmstudio_workers_footprint,
            &std::thread::sleep,
            crate::engine_dispatch::SETTLE_BOUND,
        )
    });
}

/// What a load has paged in: how far LM Studio's workers have grown since
/// `before`. `None` when either reading is missing, which keeps the whole
/// reservation until the bound (never ends it early).
pub(crate) fn grown_since(before: Option<u64>, now: Option<u64>) -> Option<u64> {
    Some(now?.saturating_sub(before?))
}

impl ServerRoom<'_> {
    /// The machine as the planner's one pool: its memory, and what is
    /// reclaimable now. None when either is unknown (the planner then
    /// plans with no pool, as before).
    pub(crate) fn pools(&self) -> Pools {
        self.pool_with(0)
    }

    /// (#3509) Less what starts still in flight reserve: their memory has
    /// not paged in yet.
    fn pool_with(&self, credit: u64) -> Pools {
        let reserved = self.machine.reserved(None);
        match (self.machine.capacity(), self.machine.reclaimable()) {
            (Some(capacity_bytes), Some(free)) => Pools::from([(
                PoolId(darkmux_profiles::gestalt_host::UNIFIED_POOL.into()),
                PoolFact {
                    capacity_bytes,
                    available_bytes: free.saturating_add(credit).saturating_sub(reserved).min(capacity_bytes),
                },
            )]),
            _ => Pools::new(),
        }
    }

    /// (#3508) The pool a mission's waves are packed in: the reclaimable
    /// memory plus what darkmux would free for them, its idle LM Studio
    /// loads (a wave's Exclusive pass unloads them) and its idle servers
    /// (stopped to make room). A wave that would not fit runs after the one
    /// before it, instead of beside it.
    ///
    /// A resident a placement reuses, reconciles or replaces is not
    /// credited: the packer accounts for it itself (a reuse costs nothing, a
    /// reconcile's stale is freed), and crediting it here too counted it
    /// twice (#3514). An idle GGUF load's mapped weights may already be
    /// partly reclaimable, so crediting its full size can over-admit by
    /// those pages; the room step re-reads memory before any load.
    pub(crate) fn wave_pools(&self, residents: &[ResidentFact], placements: &[Placement], utility_binding: Option<&str>) -> Pools {
        let lease = LeaseGuard::acquire();
        let leased = self.machine.leased(&lease);
        let kept: Vec<String> = placements.iter().filter_map(|p| kept_by(residents, p)).collect();
        let idle_load = |r: &&ResidentFact| {
            is_darkmux_owned(&r.identifier)
                && !leased.contains(&r.identifier)
                && utility_binding != Some(r.identifier.as_str())
                && !kept.contains(&r.identifier)
        };
        let loads: u64 = residents.iter().filter(idle_load).filter_map(|r| r.est_bytes).sum();
        let (idle, _) = darkmux_residents(&self.dirs, NO_ENDPOINT, &leased, Vec::new());
        let servers: u64 = idle
            .iter()
            .filter_map(|r| match r {
                Resident::Server(rec) => self.machine.footprint(rec.pid),
                Resident::Load(_) => None,
            })
            .sum();
        self.pool_with(loads.saturating_add(servers))
    }
}

/// The resident `p` reuses, reconciles or sits beside, if any: the wave
/// packer accounts for it.
fn kept_by(residents: &[ResidentFact], p: &Placement) -> Option<String> {
    match decide_residency(residents, p) {
        ResidencyDecision::Reuse { identifier, .. } => Some(identifier),
        ResidencyDecision::Reconcile { stale_identifier, .. } => Some(stale_identifier),
        ResidencyDecision::OtherOwnedCopy { other_identifier, .. } => Some(other_identifier),
        _ => None,
    }
}

/// (#3508) What a wave's loads take, two ways (#3514): the commitment a
/// refusal may rest on, and the potential eviction and wave packing aim
/// for ([`LoadFigures`]). The caller's estimate, when it has one (a test's
/// fixed figure), is both. Otherwise the ledger's pricing over LM Studio's
/// catalog and each model's own architecture.
pub(crate) struct LoadPricing<'a> {
    caller: &'a dyn FootprintEstimator,
    measured: Option<(LoadPricer, Vec<CatalogFact>)>,
}

impl<'a> LoadPricing<'a> {
    /// Asks `host` for its catalog and the models' architecture only when
    /// a placement will load (one resident at its context is reused, at no
    /// cost) and the caller cannot price it.
    pub(crate) fn for_placements(
        placements: &[Placement],
        residents: &[ResidentFact],
        caller: &'a dyn FootprintEstimator,
        host: &mut dyn ModelHost,
    ) -> Self {
        let loads = |p: &&Placement| !matches!(decide_residency(residents, p), ResidencyDecision::Reuse { .. });
        let unpriced: Vec<&str> = placements
            .iter()
            .filter(loads)
            .filter(|p| caller.estimate_bytes(&p.model_key, p.min_ctx, None).is_none())
            .map(|p| p.model_key.as_str())
            .collect();
        if unpriced.is_empty() {
            return LoadPricing { caller, measured: None };
        }
        Self::measured(&unpriced, caller, host)
    }

    /// One preflight load's pricing: `model_key` measured on `host` unless
    /// the caller prices it, or no figure when there is no host to ask.
    pub(crate) fn for_load(model_key: &str, min_ctx: u32, caller: &'a dyn FootprintEstimator, host: Option<&mut dyn ModelHost>) -> Self {
        match host.filter(|_| caller.estimate_bytes(model_key, min_ctx, None).is_none()) {
            Some(host) => Self::measured(&[model_key], caller, host),
            None => LoadPricing { caller, measured: None },
        }
    }

    /// `keys` priced from `host`'s catalog and their architecture.
    fn measured(keys: &[&str], caller: &'a dyn FootprintEstimator, host: &mut dyn ModelHost) -> Self {
        let catalog = host.list_catalog().unwrap_or_default();
        let arch = host.arch_facts(keys);
        LoadPricing { caller, measured: Some((LoadPricer::new(arch), catalog)) }
    }

    fn figures(&self, model_key: &str, min_ctx: u32, catalog: Option<&[CatalogFact]>) -> Option<LoadFigures> {
        if let Some(bytes) = self.caller.estimate_bytes(model_key, min_ctx, catalog) {
            return Some(LoadFigures { commitment: Some(bytes), potential: bytes });
        }
        let (pricer, own) = self.measured.as_ref()?;
        pricer.figures(model_key, min_ctx, Some(catalog.unwrap_or(own)))
    }

    /// The figure a load may be refused on.
    pub(crate) fn commitment(&self) -> Commitment<'_, 'a> {
        Commitment(self)
    }

    /// The figure eviction and wave packing aim for.
    pub(crate) fn potential(&self) -> Potential<'_, 'a> {
        Potential(self)
    }
}

/// [`LoadPricing`]'s commitment, as the planner's estimator.
pub(crate) struct Commitment<'p, 'a>(&'p LoadPricing<'a>);

impl FootprintEstimator for Commitment<'_, '_> {
    fn estimate_bytes(&self, model_key: &str, min_ctx: u32, catalog: Option<&[CatalogFact]>) -> Option<u64> {
        self.0.figures(model_key, min_ctx, catalog)?.commitment
    }
}

/// [`LoadPricing`]'s potential, as the planner's estimator.
pub(crate) struct Potential<'p, 'a>(&'p LoadPricing<'a>);

impl FootprintEstimator for Potential<'_, '_> {
    fn estimate_bytes(&self, model_key: &str, min_ctx: u32, catalog: Option<&[CatalogFact]>) -> Option<u64> {
        self.0.figures(model_key, min_ctx, catalog).map(|f| f.potential)
    }
}

/// What a room step may free beyond the plan's own unloads, and what it
/// keeps.
pub(crate) struct Evict<'a> {
    pub(crate) lease: &'a LeaseGuard,
    /// What every other running dispatch holds: never evicted, and memory
    /// that frees when they finish.
    pub(crate) pinned: &'a [String],
    /// darkmux's idle LM Studio loads it may unload once no idle server is
    /// left: a preflight load's. A wave names none: its own Exclusive pass
    /// unloads every one it does not want.
    pub(crate) loads: Vec<String>,
}

/// Stop darkmux's idle engine servers (then unload `evict.loads`) until
/// `plan`'s loads fit, recording each in `stopped`. See the module doc.
///
/// (#3514) Two figures. Idle servers are stopped to reach the loads'
/// potential (their whole context filled), but only the commitment (what
/// the load takes the moment it is made) can refuse it: an MLX model's KV
/// cache grows as its context fills, so refusing a long-context load on its
/// potential would refuse loads LM Studio makes fine. A potential not
/// reached is a warning; a load with no commitment (its architecture
/// unreadable) is never refused.
pub(crate) fn make_room(
    plan: &Plan,
    facts: &Facts,
    pricing: &LoadPricing<'_>,
    evict: &Evict<'_>,
    room: &ServerRoom<'_>,
    stopped: &mut Vec<String>,
) -> Result<(), EngineRefusal> {
    if !plan.actions.iter().any(|a| matches!(a.action, Action::Load { .. })) {
        return Ok(());
    }
    let commitment = plan_room(plan, facts, &pricing.commitment());
    let Some(target) = plan_room(plan, facts, &pricing.potential()) else {
        eprintln!("darkmux: warning: no memory figure for this LM Studio load; loading it unchecked and LM Studio decides");
        return Ok(());
    };
    // Larger than the machine at the moment of loading: evicting or
    // waiting can never help.
    if let Some((c, total)) = commitment.zip(room.machine.capacity()).filter(|(c, t)| c.need_bytes > *t) {
        return Err(refuse(
            "wont_fit",
            format!("the model needs {:.1} GiB and this machine has {:.1} GiB in all", gib(c.need_bytes), gib(total)),
        ));
    }
    let Some(free) = room.machine.reclaimable() else {
        eprintln!("darkmux: warning: this machine's free memory is unreadable; loading unchecked and LM Studio decides");
        return Ok(());
    };
    // (#3509) Starts still paging in count, though the OS does not show
    // their memory as used yet.
    if target.fits(free.saturating_sub(room.machine.reserved(None))) {
        return Ok(());
    }
    let (free, held) = stop_idle_servers_toward(target, evict, plan, facts, room, stopped)?;
    // Read again: a server stopped just now reserves nothing.
    let reserved = room.machine.reserved(None);
    match commitment {
        Some(c) if !c.fits(free.saturating_sub(reserved)) => Err(out_of_room(c, free, &held, reserved)),
        _ if target.fits(free.saturating_sub(reserved)) => Ok(()),
        _ => {
            eprintln!(
                "darkmux: warning: this LM Studio load can grow to {:.1} GiB with its whole context filled, and \
                 {:.1} GiB is free after evicting every idle darkmux resident; loading it, and LM Studio decides as \
                 its context fills",
                gib(target.need_bytes),
                gib(free.saturating_add(target.freed_bytes).saturating_sub(reserved))
            );
            Ok(())
        }
    }
}

/// Stop darkmux's idle servers, the least recently started first, then
/// unload `frees.loads`, until `target` fits or none is left; returns the
/// free memory then, and what a running dispatch holds.
fn stop_idle_servers_toward(
    target: PlanRoom,
    frees: &Evict<'_>,
    plan: &Plan,
    facts: &Facts,
    room: &ServerRoom<'_>,
    stopped: &mut Vec<String>,
) -> Result<(u64, Vec<String>), EngineRefusal> {
    let leased = room.machine.leased(frees.lease);
    // Every darkmux server counts: an LM Studio load replaces none of them.
    let (mut idle, mut held) = darkmux_residents(&room.dirs, NO_ENDPOINT, &leased, frees.loads.clone());
    held.extend(held_loads(plan, facts, frees.pinned));
    loop {
        let free = room.machine.reclaimable().unwrap_or(0);
        if target.fits(free.saturating_sub(room.machine.reserved(None))) {
            return Ok((free, held));
        }
        let Some(next) = idle.pop() else { return Ok((free, held)) };
        if evict(&next, &room.dirs, room.bounds, room.machine, frees.lease)? {
            stopped.push(made_room(&next));
        } else {
            // A dispatch took it up since the list was read.
            held.push(next.name());
        }
    }
}

/// What evicting `r` did, said once on stderr and returned for the
/// refusal's note.
fn made_room(r: &Resident) -> String {
    let done = match r {
        Resident::Server(_) => "stopped the idle engine server",
        Resident::Load(_) => "unloaded the idle LM Studio load",
    };
    eprintln!("darkmux: {done} {} to make room for an LM Studio load", r.name());
    format!("{done} {}", r.name())
}

/// No server darkmux started is on this endpoint id (an endpoint id is
/// never empty), so [`darkmux_residents`] counts every one.
const NO_ENDPOINT: &str = "";

fn gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 30)
}

/// darkmux's LM Studio loads a running dispatch holds that the plan keeps
/// (pinned, so never unloaded) and this wave does not itself use: memory
/// that frees when that dispatch finishes.
///
/// (#3507) The standing utility binding counts as held: a running dispatch
/// loads it on demand to compact, and the planner never unloads it, so its
/// memory frees only when no dispatch needs it.
fn held_loads(plan: &Plan, facts: &Facts, pinned: &[String]) -> Vec<String> {
    let held = |id: &str| pinned.iter().any(|p| p == id) || facts.utility_binding.as_deref() == Some(id);
    let wanted = |id: &str| {
        plan.actions.iter().any(|a| match &a.action {
            Action::Load { identifier, .. } | Action::Reuse { identifier, .. } => identifier == id,
            _ => false,
        })
    };
    facts
        .residents
        .iter()
        .map(|r| r.identifier.as_str())
        .filter(|id| held(id) && is_darkmux_owned(id) && !wanted(id))
        .map(str::to_string)
        .collect()
}

/// The refusal once nothing idle is left (#3510), in the engine path's
/// words ([`short_of_memory`]): `busy` when something a running dispatch
/// holds, or a start still in flight reserves, would free the memory later,
/// `memory_in_use` when only memory darkmux did not take is short. Neither
/// is `wont_fit`: the load fits this machine, so retrying can help.
fn out_of_room(need: PlanRoom, free: u64, held: &[String], reserved: u64) -> EngineRefusal {
    short_of_memory(need.need_bytes, free.saturating_add(need.freed_bytes).saturating_sub(reserved), held, reserved)
}

/// (#3507) The residency lease a container dispatch holds for as long as it
/// runs, on darkmux's own LM Studio loads it uses: its job model on the LM
/// Studio route (`job`), and the compactor it may call mid-run, on any
/// route. Without it, another dispatch's Exclusive pass, or an engine
/// server's start, reads them as idle and unloads them under this one (a
/// lab run calls `dispatch()` directly, with no wave and no lease of its
/// own). Only `darkmux:` loads are leased: a server darkmux starts is leased
/// by its own start, and nothing else is darkmux's to protect.
pub(crate) fn hold_dispatch_models(lease: &LeaseGuard, job: Option<&str>, compactor: Option<&str>) -> anyhow::Result<()> {
    let mut models: Vec<String> = Vec::new();
    for id in [job, compactor].into_iter().flatten().filter(|id| is_darkmux_owned(id)) {
        if !models.iter().any(|m| m == id) {
            models.push(id.to_string());
        }
    }
    if models.is_empty() {
        return Ok(());
    }
    lease.write(&models).map_err(|e| {
        anyhow::anyhow!(
            "darkmux dispatch: could not record this dispatch's residency lease on {} ({e:#}), so another darkmux \
             process could unload them mid-run; check that darkmux's home is writable",
            models.join(", ")
        )
    })?;
    // Resident by now: a racing acquirer must read them as in use.
    let _ = lease.mark_loaded(&models);
    Ok(())
}

/// (#3514) The residency protocol around the dispatch preflight's own LM
/// Studio loads (`ensure_model_resident_from`: the compactor, the utility
/// model, a lab run's model), which load outside any wave, and the LM Studio
/// they load into.
pub(crate) trait Preflight {
    /// What every running darkmux dispatch holds now.
    fn held_by_others(&self) -> Vec<String>;
    fn lock(&self) -> anyhow::Result<darkmux_types::residency_lock::ResidencyLock>;
    /// LM Studio's residents, as `lms ps` lists them.
    fn loaded(&self) -> Vec<darkmux_types::LoadedModel>;
    fn unload(&self, identifier: &str) -> anyhow::Result<()>;
    /// Load `model_key` as `identifier` at `n_ctx`, within the load deadline.
    fn load(&self, model_key: &str, identifier: &str, n_ctx: u32) -> anyhow::Result<()>;
    /// LM Studio as the planner's host: its residents' footprints, its
    /// catalog and its models' architecture, read to price a load.
    fn host(&self) -> Option<Box<dyn ModelHost>>;
    /// A figure the caller has for a load before LM Studio is asked (a
    /// test's fixed one); none on this machine.
    fn estimator(&self) -> &dyn FootprintEstimator;
    /// The machine the load takes its memory from: its memory, darkmux's
    /// engine servers, and how a load's reservation settles.
    fn room(&self) -> ServerRoom<'_>;
}

/// This machine's [`Preflight`].
pub(crate) struct ThisPreflight;

/// No figure before LM Studio is asked.
struct Unpriced;

impl FootprintEstimator for Unpriced {
    fn estimate_bytes(&self, _model_key: &str, _min_ctx: u32, _catalog: Option<&[CatalogFact]>) -> Option<u64> {
        None
    }
}

impl Preflight for ThisPreflight {
    fn held_by_others(&self) -> Vec<String> {
        LeaseGuard::acquire().all_live_leased_models()
    }
    fn lock(&self) -> anyhow::Result<darkmux_types::residency_lock::ResidencyLock> {
        darkmux_types::residency_lock::acquire()
    }
    fn loaded(&self) -> Vec<darkmux_types::LoadedModel> {
        darkmux_profiles::engine::lmstudio().loaded().unwrap_or_default()
    }
    fn unload(&self, identifier: &str) -> anyhow::Result<()> {
        darkmux_profiles::engine::lmstudio().unload(identifier)
    }
    fn load(&self, model_key: &str, identifier: &str, n_ctx: u32) -> anyhow::Result<()> {
        crate::dispatch_internal::load_at_ctx_bounded(model_key, identifier, n_ctx)
    }
    fn host(&self) -> Option<Box<dyn ModelHost>> {
        darkmux_profiles::engine::lmstudio().host()
    }
    fn estimator(&self) -> &dyn FootprintEstimator {
        &Unpriced
    }
    fn room(&self) -> ServerRoom<'_> {
        ServerRoom::this_machine()
    }
}

/// How often a load's reservation is shrunk to what has not paged in yet.
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Keep a load's reservation until LM Studio's `workers` have grown by its
/// need since `before`, or `bound` passes; then end it. Each
/// [`SETTLE_POLL`] it is shrunk to what the workers have not grown by yet,
/// and only ever shrunk: workers that fall back (the model was unloaded)
/// never bring it back. Unreadable workers keep it whole until the bound.
/// The reservation says when it ends, so a decision past the bound stops
/// counting it even if this process stalls.
pub(crate) fn settle_load(
    mut res: Reserved,
    before: Option<u64>,
    workers: &dyn Fn() -> Option<u64>,
    sleep: &dyn Fn(std::time::Duration),
    bound: std::time::Duration,
) {
    use darkmux_types::residency_reservation::now_ms;
    let need = res.need_bytes();
    let _ = res.settle_by(now_ms().saturating_add(bound.as_millis() as u64));
    let mut waited = std::time::Duration::ZERO;
    while waited < bound {
        let left = grown_since(before, workers()).map_or(need, |grown| need.saturating_sub(grown));
        if left == 0 {
            break;
        }
        res.shrink_to(left);
        sleep(SETTLE_POLL);
        waited += SETTLE_POLL;
    }
}

/// (#3514) A preflight reload's unload of the stale instance, refused
/// `busy` when a running dispatch holds that instance: it is serving that
/// dispatch at its own context, and is never unloaded under it. Read right
/// before the unload, as the engine path reads a load's lease.
pub(crate) fn unload_unless_held(gate: &dyn Preflight, identifier: &str, unload: &dyn Fn(&str) -> anyhow::Result<()>) -> anyhow::Result<()> {
    if gate.held_by_others().iter().any(|h| h == identifier) {
        return Err(refuse(
            "busy",
            format!("`{identifier}` is serving a running dispatch at its own context, so it is not reloaded under it; retry when it finishes"),
        )
        .into());
    }
    unload(identifier)
}

/// (#3514) A preflight load in the residency protocol: under the lock, its
/// lease and a reservation of what it commits; the lock let go before the
/// load begins; the reservation kept past the load until its memory pages
/// in. A load that cannot be priced is leased with no reservation.
///
/// The lease ends with the load: it says a dispatch is acquiring the model,
/// and whichever dispatch then runs on it holds its own
/// ([`hold_dispatch_models`]). The reservation is not a lease and outlives
/// it, so a model just loaded for nobody (the compactor, the utility model)
/// counts against free memory while it pages in, and is neither held nor
/// `busy`.
pub(crate) fn load_reserved(
    gate: &dyn Preflight,
    model_key: &str,
    identifier: &str,
    n_ctx: u32,
    load: &dyn Fn(&str, &str, u32) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let room = gate.room();
    let (lease, reserved, before) = reserve_preflight(gate, &room, model_key, identifier, n_ctx)?;
    // A load that fails ends its reservation with `reserved`.
    load(model_key, identifier, n_ctx)?;
    drop(lease);
    if let Some(res) = reserved {
        (room.settle)(res, before);
    }
    Ok(())
}

/// [`load_reserved`]'s part under the lock: the lease and its reservation,
/// and LM Studio's workers' footprint read before the load. The lock is let
/// go on return.
fn reserve_preflight(
    gate: &dyn Preflight,
    room: &ServerRoom<'_>,
    model_key: &str,
    identifier: &str,
    n_ctx: u32,
) -> anyhow::Result<(LeaseGuard, Option<Reserved>, Option<u64>)> {
    let _lock = gate.lock().map_err(|e| {
        refuse("model_start_failed", format!("could not take darkmux's residency lock ({e:#}); check that darkmux's home is writable"))
    })?;
    let lease = LeaseGuard::acquire();
    let want = Placement { model_key: model_key.into(), identifier: identifier.into(), min_ctx: n_ctx, seat: String::new() };
    let need = preflight_room(gate, room, &lease, &want)?;
    // Read after the room step: a load it unloaded no longer counts.
    let before = (room.workers)();
    lease.write(&[identifier.to_string()])?;
    let reserved = need.map(|n| Reserved::begin(n.need_bytes)).transpose()?;
    Ok((lease, reserved, before))
}

/// A preflight load's memory decision, a wave's own: priced two ways
/// ([`LoadPricing`]), and [`make_room`] stops darkmux's idle servers, then
/// unloads its idle LM Studio loads (never one a running dispatch holds,
/// nor the standing utility binding), or refuses with the wave's code.
/// Returns what the load commits, for its reservation.
fn preflight_room(gate: &dyn Preflight, room: &ServerRoom<'_>, lease: &LeaseGuard, want: &Placement) -> anyhow::Result<Option<PlanRoom>> {
    let mut host = gate.host();
    let residents = host.as_mut().and_then(|h| h.list_resident().ok()).unwrap_or_default();
    let facts = Facts { residents, utility_binding: room.machine.utility(), ..Default::default() };
    let host = host.as_deref_mut().map(|h| h as &mut dyn ModelHost);
    let pricing = LoadPricing::for_load(&want.model_key, want.min_ctx, gate.estimator(), host);
    let plan = loading(want);
    let pinned = gate.held_by_others();
    let evict = Evict { lease, pinned: &pinned, loads: idle_loads(&facts, &pinned, &want.identifier) };
    let mut stopped = Vec::new();
    make_room(&plan, &facts, &pricing, &evict, room, &mut stopped).map_err(|refusal| {
        let note = if stopped.is_empty() { String::new() } else { format!(" (after it {})", stopped.join(", ")) };
        anyhow::Error::new(refusal).context(format!("darkmux: loading `{}` was refused{note}", want.identifier))
    })?;
    Ok(plan_room(&plan, &facts, &pricing.commitment()))
}

/// The plan of one preflight load, for the room step: the load alone (a
/// reload's stale instance is already unloaded).
fn loading(want: &Placement) -> Plan {
    let action = Action::Load { model_key: want.model_key.clone(), identifier: want.identifier.clone(), min_ctx: want.min_ctx };
    let precondition = darkmux_gestalt::Precondition::NoResidentForModelKey { model_key: want.model_key.clone() };
    let load = darkmux_gestalt::PlannedAction { action, reason: darkmux_gestalt::Reason::NoResident, precondition };
    Plan { actions: vec![load], ..Default::default() }
}

/// darkmux's LM Studio loads a preflight may unload to make room: not the
/// one it loads, not one a running dispatch holds, not the standing utility
/// binding.
fn idle_loads(facts: &Facts, pinned: &[String], loading: &str) -> Vec<String> {
    let kept = |id: &str| id == loading || pinned.iter().any(|p| p == id) || facts.utility_binding.as_deref() == Some(id);
    facts.residents.iter().map(|r| r.identifier.as_str()).filter(|id| is_darkmux_owned(id) && !kept(id)).map(str::to_string).collect()
}

/// (#3514) No machine: the room of a model host whose loads take none of
/// this machine's memory (a test's double). It has no memory figures, so the
/// planner plans with no pool and the room step decides nothing, and it has
/// no engine-server records, so no server is ever stopped for it.
pub(crate) struct NoMachine;

impl Machine for NoMachine {
    fn reclaimable(&self) -> Option<u64> {
        None
    }
    fn footprint(&self, _pid: u32) -> Option<u64> {
        None
    }
    fn lmstudio_residents(&self) -> Vec<String> {
        Vec::new()
    }
    fn lmstudio_unload(&self, _identifier: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn leased(&self, own: &LeaseGuard) -> Vec<String> {
        own.all_live_leased_models()
    }
    fn capacity(&self) -> Option<u64> {
        None
    }
}

impl ServerRoom<'static> {
    /// [`NoMachine`]'s room.
    pub(crate) fn without_machine() -> Self {
        ServerRoom {
            dirs: Dirs { records: std::path::PathBuf::new(), logs: std::path::PathBuf::new() },
            bounds: server::Bounds::default(),
            machine: &NoMachine,
            settle: |res, _| drop(res),
            workers: || None,
        }
    }

    /// (#3514) The room for loads `host` makes: this machine when they take
    /// its memory (LM Studio's), else [`Self::without_machine`]. A host that
    /// loads into nothing on this machine is never planned against this
    /// machine's memory, so a test's double never reads the host it runs on.
    pub(crate) fn for_host(host: &dyn ModelHost) -> Self {
        if host.on_this_machine() {
            Self::this_machine()
        } else {
            Self::without_machine()
        }
    }
}

#[cfg(test)]
#[path = "lms_room_tests.rs"]
pub(crate) mod tests;

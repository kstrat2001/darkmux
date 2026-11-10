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
//!    free ([`darkmux_gestalt::plan_room`]). A load larger than the whole
//!    machine is refused `wont_fit` before free memory is read: evicting or
//!    waiting can never help. With no figure for the need or for free
//!    memory, the load goes ahead and LM Studio decides, with a warning.
//! 2. While the plan does not fit the reclaimable memory, it stops
//!    darkmux's idle servers, the least recently started first, with the
//!    engine path's own eviction ([`crate::engine_dispatch::evict`]): the
//!    leases are read again under the endpoint's lock, where the server is
//!    stopped, and a server a running dispatch holds is never stopped. A
//!    server darkmux did not start (no record matching the live process) is
//!    never seen at all (contract 4).
//! 3. When nothing idle is left it refuses with a stable code (#3510):
//!    `busy` when a running dispatch holds memory that would free later (a
//!    server, or an LM Studio load the plan keeps because it is pinned),
//!    `memory_in_use` when only memory darkmux did not take is short.
//!
//! darkmux's idle LM Studio loads are the planner's own business: the wave's
//! Exclusive scope already unloads every one the wave does not want, so only
//! servers are evicted here.

use crate::engine_dispatch::{darkmux_residents, evict, refuse, EngineRefusal, Machine, Resident, ThisMachine};
use darkmux_gestalt::{
    decide_residency, is_darkmux_owned, plan_room, Action, CatalogFact, ResidencyDecision, Facts, FootprintEstimator, ModelHost, Placement, Plan, PlanRoom,
    PoolFact, PoolId, Pools, ResidentFact,
};
use darkmux_profiles::engine::server::{self, Dirs};
use darkmux_profiles::model_ledger::ArchWithSizeFallback;
use darkmux_types::residency_lease::LeaseGuard;

/// Where darkmux's engine servers are recorded, how long a stop may take,
/// and the machine's memory and leases.
pub(crate) struct ServerRoom<'a> {
    pub(crate) dirs: Dirs,
    pub(crate) bounds: server::Bounds,
    pub(crate) machine: &'a (dyn Machine + Sync),
}

impl ServerRoom<'static> {
    /// This machine: the records under darkmux's home, its real memory.
    pub(crate) fn this_machine() -> Self {
        ServerRoom { dirs: Dirs::under_home(), bounds: server::Bounds::default(), machine: &ThisMachine }
    }
}

impl ServerRoom<'_> {
    /// The machine as the planner's one pool: its memory, and what is
    /// reclaimable now. None when either is unknown (the planner then
    /// plans with no pool, as before).
    pub(crate) fn pools(&self) -> Pools {
        self.pool_with(0)
    }

    fn pool_with(&self, credit: u64) -> Pools {
        match (self.machine.capacity(), self.machine.reclaimable()) {
            (Some(capacity_bytes), Some(free)) => Pools::from([(
                PoolId(darkmux_profiles::gestalt_host::UNIFIED_POOL.into()),
                PoolFact { capacity_bytes, available_bytes: free.saturating_add(credit).min(capacity_bytes) },
            )]),
            _ => Pools::new(),
        }
    }

    /// (#3508) The pool a mission's waves are packed in: the reclaimable
    /// memory plus what darkmux would free for them, its idle LM Studio
    /// loads (a wave's Exclusive pass unloads them) and its idle servers
    /// (stopped to make room). A wave that would not fit runs after the one
    /// before it, instead of beside it.
    pub(crate) fn wave_pools(&self, residents: &[ResidentFact], utility_binding: Option<&str>) -> Pools {
        let lease = LeaseGuard::acquire();
        let leased = self.machine.leased(&lease);
        let idle_load = |r: &&ResidentFact| {
            is_darkmux_owned(&r.identifier) && !leased.contains(&r.identifier) && utility_binding != Some(r.identifier.as_str())
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

/// (#3508) What a wave's loads take: the caller's estimate when it has one
/// (a test's fixed figure), else the ledger's pricing over LM Studio's
/// catalog and each model's own architecture.
pub(crate) struct LoadPricing<'a> {
    caller: &'a dyn FootprintEstimator,
    measured: Option<(ArchWithSizeFallback, Vec<CatalogFact>)>,
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
        let catalog = host.list_catalog().unwrap_or_default();
        let arch = host.arch_facts(&unpriced);
        LoadPricing { caller, measured: Some((ArchWithSizeFallback::new(arch), catalog)) }
    }
}

impl FootprintEstimator for LoadPricing<'_> {
    fn estimate_bytes(&self, model_key: &str, min_ctx: u32, catalog: Option<&[CatalogFact]>) -> Option<u64> {
        self.caller.estimate_bytes(model_key, min_ctx, catalog).or_else(|| {
            let (measured, own) = self.measured.as_ref()?;
            measured.estimate_bytes(model_key, min_ctx, Some(catalog.unwrap_or(own)))
        })
    }
}

/// Stop darkmux's idle engine servers until `plan`'s loads fit, recording
/// each stop in `stopped`. See the module doc.
pub(crate) fn make_room(
    plan: &Plan,
    facts: &Facts,
    est: &dyn FootprintEstimator,
    lease: &LeaseGuard,
    pinned: &[String],
    room: &ServerRoom<'_>,
    stopped: &mut Vec<String>,
) -> Result<(), EngineRefusal> {
    if !plan.actions.iter().any(|a| matches!(a.action, Action::Load { .. })) {
        return Ok(());
    }
    let Some(need) = plan_room(plan, facts, est) else {
        eprintln!("darkmux: warning: no memory figure for this LM Studio load; loading it unchecked and LM Studio decides");
        return Ok(());
    };
    // Larger than the machine: evicting or waiting can never help.
    if let Some(total) = room.machine.capacity().filter(|t| need.need_bytes > *t) {
        return Err(refuse(
            "wont_fit",
            format!("the model needs {:.1} GiB and this machine has {:.1} GiB in all", gib(need.need_bytes), gib(total)),
        ));
    }
    let Some(free) = room.machine.reclaimable() else {
        eprintln!("darkmux: warning: this machine's free memory is unreadable; loading unchecked and LM Studio decides");
        return Ok(());
    };
    if need.fits(free) {
        return Ok(());
    }
    let leased = room.machine.leased(lease);
    // Every darkmux server counts: an LM Studio load replaces none of them.
    let (mut idle, mut held) = darkmux_residents(&room.dirs, NO_ENDPOINT, &leased, Vec::new());
    held.extend(held_loads(plan, facts, pinned));
    loop {
        let Some(free) = room.machine.reclaimable() else { return Ok(()) };
        if need.fits(free) {
            return Ok(());
        }
        let Some(next) = idle.pop() else { return Err(out_of_room(need, free, &held)) };
        if evict(&next, &room.dirs, room.bounds, room.machine, lease)? {
            eprintln!("darkmux: stopped darkmux's idle engine server {} to make room for an LM Studio load", next.name());
            stopped.push(format!("stopped the idle engine server {}", next.name()));
        } else {
            // A dispatch took it up since the list was read.
            held.push(next.name());
        }
    }
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
fn held_loads(plan: &Plan, facts: &Facts, pinned: &[String]) -> Vec<String> {
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
        .filter(|id| pinned.iter().any(|p| p == id) && is_darkmux_owned(id) && !wanted(id))
        .map(str::to_string)
        .collect()
}

/// The refusal once nothing idle is left (#3510): `busy` when something a
/// running dispatch holds would free the memory later, `memory_in_use`
/// when only memory darkmux did not take is short. Neither is `wont_fit`:
/// the load fits this machine, so retrying can help.
fn out_of_room(need: PlanRoom, free: u64, held: &[String]) -> EngineRefusal {
    let short = format!(
        "the model needs {:.1} GiB and {:.1} GiB is free after evicting every idle darkmux resident",
        gib(need.need_bytes),
        gib(free.saturating_add(need.freed_bytes))
    );
    if held.is_empty() {
        refuse(
            "memory_in_use",
            format!("{short}; memory darkmux did not take holds the rest, so retry when it frees"),
        )
    } else {
        refuse("busy", format!("{short}; running dispatches hold {}, so retry when they finish", held.join(", ")))
    }
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

/// A machine whose memory is unknown, for tests that plan without one.
#[cfg(test)]
pub(crate) struct UnknownMachine;

#[cfg(test)]
impl Machine for UnknownMachine {
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

#[cfg(test)]
impl ServerRoom<'static> {
    /// No machine figures: the planner plans with no pool, as it did before
    /// it read the machine.
    pub(crate) fn unknown_machine() -> Self {
        ServerRoom { dirs: Dirs::under_home(), bounds: server::Bounds::default(), machine: &UnknownMachine }
    }
}

#[cfg(test)]
#[path = "lms_room_tests.rs"]
mod tests;

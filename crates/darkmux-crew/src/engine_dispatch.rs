//! (#3448) A dispatch to a model on an engine darkmux starts servers for
//! (DESIGN.md, "Engine servers darkmux starts"): Tensorfold first.
//!
//! Called by each dispatch path after its `dispatch.start` and before its
//! first request, [`prepare`] makes the model resident:
//!
//! 1. The engine is installed at a release darkmux was verified against, or
//!    the dispatch fails `engine_unavailable`.
//! 2. When darkmux's server on the endpoint already runs this exact command,
//!    it is reused and nothing else happens.
//! 3. Otherwise darkmux makes room: while the machine's reclaimable memory is
//!    below the model's memory floor (the engine's own `info` figure),
//!    it stops its own idle engine servers on other endpoints, oldest first,
//!    then unloads its own idle LM Studio `darkmux:` loads. A resident a
//!    running dispatch holds (a residency lease) is never touched. When
//!    nothing is left to free, the dispatch fails `busy` (something darkmux
//!    holds would free it later) or `wont_fit`.
//! 4. It starts the server ([`darkmux_profiles::engine::server::ensure`]) and
//!    waits until it answers, failing `port_in_use` or `model_start_failed`.
//!
//! The dispatch's lease names the served model for as long as the returned
//! [`EngineReady`] lives, so no other darkmux process evicts it mid-run.
//! Every step is reported through `note` as the payload of a
//! `dispatch.model.*` record.

use darkmux_flow::payload::DispatchModelPayload;
use darkmux_profiles::engine::server::{self, Dirs, Progress};
use darkmux_profiles::engine::tensorfold::Tensorfold;
use darkmux_types::engine_record::{self, EngineRecord};
use darkmux_types::residency_lease::LeaseGuard;
use darkmux_types::{EndpointKind, ManagedBackend};

/// Which `dispatch.model.*` record a note is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    Start,
    Loading,
    Ready,
}

/// Why a dispatch could not make its model resident, with the stable code
/// an app branches on (`dispatch.error`'s `payload.code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRefusal {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for EngineRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "darkmux dispatch: {} [{}]", self.message, self.code)
    }
}

impl std::error::Error for EngineRefusal {}

fn refuse(code: &'static str, message: impl Into<String>) -> EngineRefusal {
    EngineRefusal { code, message: message.into() }
}

/// The model resident and answering; holds the dispatch's lease on it.
pub struct EngineReady {
    pub record: EngineRecord,
    pub started: bool,
    _lease: LeaseGuard,
}

/// Whether `target`'s model runs on a server darkmux starts.
pub fn is_engine_server(target: &crate::target::Target) -> bool {
    matches!(target.kind, EndpointKind::Managed(ManagedBackend::Tensorfold))
}

/// The `code` of a dispatch failure that [`prepare`] refused, if it was one.
pub fn refusal_code(err: &anyhow::Error) -> Option<&'static str> {
    err.downcast_ref::<EngineRefusal>().map(|r| r.code)
}

/// Make `target`'s model resident on its engine server. See the module doc.
pub fn prepare(
    target: &crate::target::Target,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<EngineReady, EngineRefusal> {
    prepare_in(target, &Dirs::under_home(), &Tensorfold::installed(), server::Bounds::default(), &ThisMachine, note)
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
    /// Served model names other darkmux processes are dispatching to.
    fn leased(&self) -> Vec<String>;
}

struct ThisMachine;

impl Machine for ThisMachine {
    fn reclaimable(&self) -> Option<u64> {
        darkmux_platform::current().reclaimable_memory_bytes()
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
    fn leased(&self) -> Vec<String> {
        darkmux_types::residency_lease::live_leased_models(std::process::id())
    }
}

fn prepare_in(
    target: &crate::target::Target,
    dirs: &Dirs,
    tf: &Tensorfold,
    bounds: server::Bounds,
    machine: &dyn Machine,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<EngineReady, EngineRefusal> {
    let endpoint = target.endpoint.named_id().unwrap_or_default().to_string();
    let port = target.endpoint.port.unwrap_or_default();
    let version = tf.verified_version().map_err(|e| refuse("engine_unavailable", format!("{e:#}")))?;
    let spec = tf
        .serve_spec(&endpoint, port, &target.model, version)
        .map_err(|e| refuse("model_start_failed", format!("{e:#}")))?;
    let lease = LeaseGuard::acquire();
    let _ = lease.write(std::slice::from_ref(&spec.served_name));
    let base = DispatchModelPayload {
        engine: spec.engine.clone(),
        endpoint: endpoint.clone(),
        model: spec.model.clone(),
        ..Default::default()
    };
    let current = server::running(dirs, &endpoint);
    let reusable = current.as_ref().is_some_and(|r| r.argv == spec.argv);
    let mut evicted = Vec::new();
    if !reusable {
        let need = tf.info(&target.model.id).map(|i| i.memory_floor_bytes).ok();
        let freed_by_replacing = current.as_ref().and_then(|r| machine.footprint(r.pid)).unwrap_or(0);
        evicted = make_room(need, freed_by_replacing, dirs, &endpoint, bounds, machine)?;
    }
    let replacing = current.as_ref().map(|r| r.model.clone());
    let mut progress = |p: Progress| report(p, &base, &replacing, &evicted, note);
    let ensured = server::ensure(&spec, dirs, bounds, &mut progress).map_err(|e| refuse(e.code(), e.to_string()))?;
    let _ = lease.mark_loaded(std::slice::from_ref(&spec.served_name));
    let started = matches!(ensured, server::Ensured::Started(_));
    Ok(EngineReady { record: ensured.record().clone(), started, _lease: lease })
}

/// One of darkmux's own residents that could be evicted to make room.
enum Resident {
    Server(EngineRecord),
    /// A `darkmux:` load in LM Studio, by identifier.
    Load(String),
}

impl Resident {
    fn name(&self) -> String {
        match self {
            Resident::Server(r) => format!("{} ({})", r.model, r.endpoint),
            Resident::Load(id) => id.clone(),
        }
    }
}

/// Evict darkmux's own idle residents until `need` bytes are reclaimable,
/// returning what it evicted. With no figure for `need` or for free memory,
/// it evicts nothing and lets the engine decide (a start that does not fit
/// fails `model_start_failed`, naming the engine's own words).
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
        Progress::Replacing { .. } => return,
    };
    note(kind, payload);
}

fn make_room(
    need: Option<u64>,
    freed_by_replacing: u64,
    dirs: &Dirs,
    endpoint: &str,
    bounds: server::Bounds,
    machine: &dyn Machine,
) -> Result<Vec<String>, EngineRefusal> {
    let Some(need) = need else { return Ok(Vec::new()) };
    let fits = |free: u64| free.saturating_add(freed_by_replacing) >= need;
    // Room already: nothing is listed, LM Studio is not asked.
    if machine.reclaimable().is_none_or(fits) {
        return Ok(Vec::new());
    }
    let (mut idle, held) = darkmux_residents(dirs, endpoint, &machine.leased(), machine.lmstudio_residents());
    let mut evicted = Vec::new();
    loop {
        let Some(free) = machine.reclaimable() else { return Ok(evicted) };
        if fits(free) {
            return Ok(evicted);
        }
        let Some(next) = idle.pop() else {
            let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
            let short = format!(
                "the model needs {:.1} GiB and {:.1} GiB is free after evicting every idle darkmux resident",
                gib(need),
                gib(free.saturating_add(freed_by_replacing))
            );
            return Err(if held.is_empty() {
                refuse("wont_fit", format!("{short}; it does not fit this machine"))
            } else {
                refuse("busy", format!("{short}; running dispatches hold {}, so retry when they finish", held.join(", ")))
            });
        };
        evict(&next, dirs, bounds, machine)?;
        evicted.push(next.name());
    }
}

/// darkmux's own residents other than `endpoint`'s server: the idle ones,
/// in eviction order (last is evicted first), and the names of those a
/// running dispatch holds.
fn darkmux_residents(dirs: &Dirs, endpoint: &str, leased: &[String], lms: Vec<String>) -> (Vec<Resident>, Vec<String>) {
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
        let served = r.argv.iter().skip_while(|a| *a != "--name").nth(1).cloned().unwrap_or_default();
        if leased.contains(&served) {
            held.push(format!("{} ({})", r.model, r.endpoint));
        } else {
            idle.push(Resident::Server(r));
        }
    }
    (idle, held)
}

fn evict(resident: &Resident, dirs: &Dirs, bounds: server::Bounds, machine: &dyn Machine) -> Result<(), EngineRefusal> {
    match resident {
        Resident::Server(r) => server::stop(dirs, r, bounds).map_err(|e| refuse(e.code(), e.to_string())),
        Resident::Load(id) => machine
            .lmstudio_unload(id)
            .map_err(|e| refuse("model_start_failed", format!("could not unload `{id}` to make room: {e:#}"))),
    }
}

#[cfg(test)]
#[path = "engine_dispatch_tests.rs"]
mod tests;

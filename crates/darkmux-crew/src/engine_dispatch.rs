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
use darkmux_profiles::engine::flint::{self, Rec};
use darkmux_profiles::engine::server::{self, Dirs, Progress, ServerSpec};
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

/// The `code` of a dispatch failure that [`prepare`] refused, if it was one.
pub fn refusal_code(err: &anyhow::Error) -> Option<&'static str> {
    err.downcast_ref::<EngineRefusal>().map(|r| r.code)
}

/// Make `target`'s model resident on its engine server. See the module doc.
pub fn prepare(
    target: &crate::target::Target,
    note: &mut dyn FnMut(Note, DispatchModelPayload),
) -> Result<EngineReady, EngineRefusal> {
    let (dirs, bounds) = (Dirs::under_home(), server::Bounds::default());
    match target.kind {
        EndpointKind::Managed(ManagedBackend::Tensorfold) => {
            prepare_in(target, &dirs, &Tensorfold::installed(), bounds, &ThisMachine, note)
        }
        EndpointKind::Managed(ManagedBackend::Flint) => prepare_in(target, &dirs, &Rec::installed(), bounds, &ThisMachine, note),
        EndpointKind::Managed(_) | EndpointKind::Unmanaged => {
            Err(refuse("model_start_failed", "darkmux starts no server for this model's endpoint"))
        }
    }
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
    let leased = machine.leased(&lease);
    let mut evicted = Vec::new();
    if !reusable {
        // darkmux's own server on this endpoint runs another model or
        // context; one a running dispatch is using is never replaced under it.
        if let Some(cur) = current.as_ref().filter(|r| leased.contains(&served_name_of(r))) {
            return Err(refuse(
                "busy",
                format!(
                    "endpoint `{endpoint}` is serving `{}` for a running dispatch; retry when it finishes",
                    cur.model
                ),
            ));
        }
        let need = engine.memory_need(&target.model);
        let freed_by_replacing = current.as_ref().and_then(|r| machine.footprint(r.pid)).unwrap_or(0);
        let room = Room { need, freed_by_replacing, endpoint: &endpoint, leased: &leased };
        evicted = make_room(&room, dirs, bounds, machine)?;
    }
    let replacing = current.as_ref().map(|r| r.model.clone());
    let mut progress = |p: Progress| report(p, &base, &replacing, &evicted, note);
    let ensured = server::ensure(&spec, dirs, bounds, &mut progress).map_err(|e| refuse(e.code(), e.to_string()))?;
    let _ = lease.mark_loaded(std::slice::from_ref(&spec.served_name));
    let started = matches!(ensured, server::Ensured::Started(_));
    Ok(EngineReady { record: ensured.record().clone(), started, _lease: lease })
}

/// (#3453) Refuse `engine_in_use` when the engine's own server (one darkmux
/// did not start) is up: rec holds a single-instance lock, and darkmux never
/// stops or takes it over. The server darkmux itself runs on this endpoint
/// is not that one when the engine names its port and darkmux's recorded
/// server is what listens there (`server::listens`, #3453 review): a name
/// or a port alone proves nothing.
fn refuse_engines_own_server(engine: &dyn ServerEngine, dirs: &Dirs, endpoint: &str) -> Result<(), EngineRefusal> {
    let Some(address) = engine.own_server()? else { return Ok(()) };
    let ours = server::running(dirs, endpoint).is_some_and(|r| flint::endpoint_port(&address) == Some(r.port) && server::listens(&r));
    if ours {
        return Ok(());
    }
    Err(refuse(
        "engine_in_use",
        format!(
            "rec's own server is up or starting (at {address}), and rec runs one server per machine; darkmux never stops or \
             takes over a server it did not start. Run `rec stop`, then dispatch again"
        ),
    ))
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

/// What a start needs room for.
struct Room<'a> {
    /// The model's memory floor; `None` when the engine did not say.
    need: Option<u64>,
    /// Memory darkmux's own server on this endpoint frees when replaced.
    freed_by_replacing: u64,
    endpoint: &'a str,
    leased: &'a [String],
}

fn make_room(room: &Room<'_>, dirs: &Dirs, bounds: server::Bounds, machine: &dyn Machine) -> Result<Vec<String>, EngineRefusal> {
    let Some(need) = room.need else { return Ok(Vec::new()) };
    let freed_by_replacing = room.freed_by_replacing;
    let fits = |free: u64| free.saturating_add(freed_by_replacing) >= need;
    // Room already: nothing is listed, LM Studio is not asked.
    if machine.reclaimable().is_none_or(fits) {
        return Ok(Vec::new());
    }
    // Larger than the machine: evicting or waiting can never help.
    if let Some(total) = machine.capacity().filter(|t| need > *t) {
        let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
        return Err(refuse(
            "wont_fit",
            format!("the model needs {:.1} GiB and this machine has {:.1} GiB in all", gib(need), gib(total)),
        ));
    }
    let (mut idle, held) = darkmux_residents(dirs, room.endpoint, room.leased, machine.lmstudio_residents());
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
        if leased.contains(&served_name_of(&r)) {
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
pub(crate) mod tests;

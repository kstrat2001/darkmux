//! (#2779) The source facade the thermal/power probes resolve through, so
//! the escalation ladder can be regression-tested against simulated
//! hardware instead of a machine somebody has to actually cook.
//!
//! # Why this exists
//!
//! #2774's escalation ladder shipped after five review rounds and thirteen
//! MUST FIX findings, two of them safety inversions. Every one was proven
//! by a hand-written throwaway probe that was deleted afterwards, because
//! there was no way to put a machine into `serious` on demand. The live
//! dogfood that followed could exercise only the duty-cycle WIRING: the
//! machine stayed `nominal` through 33 containers and a 35B dispatch
//! (`above_nominal_ms: 0`), so tiers 2 through 5 have never run on real
//! hardware and cannot be forced — the obvious lever (`resume_at =
//! nominal`) is exactly what the ladder now disarms by design.
//!
//! # The seam is one `cfg` branch, not a new parameter
//!
//! [`crate::host_probe::thermal::sample`] and
//! [`crate::host_probe::battery::sample`] are free functions over IOKit
//! that ALREADY substitute at compile time — on anything that is not
//! macOS/aarch64 both return `None`. This module replaces that single
//! branch with a RESOLVED source, so nothing threads a parameter through
//! the call graph.
//!
//! # `read` and `advance` are separate on purpose
//!
//! A scripted source has a cursor, and three of the four call sites are
//! one-shot reads that must not move it:
//!
//! | call site | calls |
//! |---|---|
//! | [`crate::host_probe::HostProbe::sample`] (the sampler's source) | `advance` then `read` |
//! | [`crate::host_probe::HostProbe::new`]'s capability check | `read` |
//! | `host_probe::power_posture` | `read` |
//! | `preflight`'s battery floor gate | `read` |
//!
//! `HostProbe::sample` already computes the REAL elapsed since its own
//! previous sample (`interval_ms`), and that is what it advances the
//! scripted clock by — so the scenario's simulated time and the
//! `elapsed_ms` the governor is fed advance by the same measured gap and
//! cannot DRIFT apart tick over tick.
//!
//! They are not one number, and the difference is worth stating rather
//! than rounding off. The sampler's own `thermal_elapsed_ms` is
//! `at_ms - prev_thermal_at_ms` read AFTER `probe.sample()` returns, with
//! `prev_thermal_at_ms` starting at `0`, while `interval_ms` is
//! probe-t0 to probe-t0. So on iteration ONE the governor is fed
//! everything since sampler start — including `lms_tracker.tick()`, which
//! can stall ~30s — while the scripted cursor advances by `0`. That is a
//! constant offset established once, not a cumulative divergence: every
//! later tick moves both by the same measured amount, and no ladder
//! invariant keys on the absolute value. A test driving the governors directly
//! ([`crate::host_scenario::ScenarioDriver`]) advances by a fixed tick
//! instead, which is what compresses a 40-minute escalation into
//! microseconds.
//!
//! # Never fake silently
//!
//! A machine reporting `nominal` while it actually cooks is strictly worse
//! than no governor at all, so a scripted source is announced on four
//! surfaces, not one:
//!
//! 1. `darkmux doctor`'s `host probe` check goes **Warn** and names the
//!    scenario path (`describe_host_probe`).
//! 2. The dispatch prints a warning line at sampler start, beside the
//!    ladder's own disarm notes.
//! 3. Every flow record whose payload carries a host reading — or whose
//!    very EXISTENCE was decided by one — carries
//!    `simulated_host_source: "<path>"` beside it ([`stamp`]).
//!
//!    **The producers are enumerated in [`HOST_READING_ACTIONS`], not in
//!    this paragraph, and that move is the point.** This list used to name
//!    them by hand. It was correct when written and went stale twice:
//!    `machine.rollup` landed one commit later and was never added,
//!    `machine.battery` predated the facade and was never retrofitted, and
//!    the dispatch-side `thermal.stop_unresolved` /
//!    `thermal.tier5_eject*` / `battery.pause_unsupported` records were
//!    never in scope at all. Prose cannot fail; the table can, and
//!    [`audit`] makes it. Read the table for what is stamped, what is
//!    exempt, and why.
//!
//!    The FILES the table is checked against are discovered, not
//!    remembered: a workspace walk asserts that every `.rs` file carrying a
//!    watched action literal is classified as a producer or as a
//!    non-producer with a reason. The residual gap is narrow and named — an
//!    action ASSEMBLED rather than written (`format!("{}.x", "machine")`),
//!    or a brand-new action family; see [`WATCHED_ACTION_PREFIXES`] and
//!    [`watched_action_literals`] for both boundaries.
//!
//!    The machine-SCOPED records matter most: with Redis enabled they ride
//!    the fleet stream to another machine's machine lens, which an
//!    unstamped one would show hitting `critical` with nothing in the data
//!    saying otherwise.
//! 4. The run artifact's `host_window` block carries the same field — but
//!    UNCONDITIONALLY (`null` on a real run), not absent-when-real like
//!    the flow records. The two surfaces disagree on purpose; see
//!    [`stamp`]'s own note for why, and do not learn the rule from one
//!    surface and apply it to the other.
//!
//! A scenario file that is named but cannot be LOADED does not silently
//! become a real read either: the resolution keeps [`RealSource`] (the safe
//! direction — the operator gets a working governor) and records the error
//! in [`Provenance`], which doctor reports as a **Warn** naming the path and
//! the load error, and which the dispatch prints. Nothing is simulated in
//! that case, so nothing is stamped either — marking real readings as
//! simulated would be its own lie. "No silent wrong key" applies to a path
//! as much as to a name.

use crate::host_probe::{battery, thermal, BatterySample, ThermalSample};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// One tick's worth of the two readings a governor consumes. Both halves
/// are independently absent, exactly as the real probes are: a desktop has
/// no battery, and the OS thermal read can come back `None` on its own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostReading {
    pub thermal: Option<ThermalSample>,
    pub battery: Option<BatterySample>,
}

/// Where the thermal + battery readings come from.
///
/// `Send + Sync` because the resolved source is a process-wide `&'static`
/// read from the dispatch sampler thread, the daemon's host sampler, and
/// `darkmux doctor` alike.
pub trait HostSource: Send + Sync {
    /// The reading as of the source's CURRENT position. Never advances.
    fn read(&self) -> HostReading;

    /// Move the source's own notion of time forward by `elapsed_ms`. A
    /// no-op for every source that reads real hardware — real hardware
    /// keeps its own time.
    fn advance(&self, _elapsed_ms: u64) {}
}

/// Today's IOKit path, unchanged and untestable by construction — see the
/// boundary this module's issue states: a simulated source proves the
/// LADDER, never that an IOKit reading is interpreted correctly.
pub struct RealSource;

impl HostSource for RealSource {
    fn read(&self) -> HostReading {
        HostReading { thermal: thermal::sample(), battery: battery::sample() }
    }
}

// ── The scenario file ────────────────────────────────────────────────────

/// A scripted thermal reading. `state` is the OS vocabulary
/// ([`thermal::THERMAL_STATES`]) and is NOT validated here — a scenario
/// that wants to pin how an unrecognized state is handled (the breaker
/// owns it) must be able to write one.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScriptedThermal {
    pub state: String,
    /// Defaults to 100 — "no cap recorded", which is what a cool machine
    /// reports and what every scenario that is not about the speed-limit
    /// floor wants. See [`ThermalSample::cpu_speed_limit_pct`].
    #[serde(default = "no_cap_recorded")]
    pub cpu_speed_limit_pct: u64,
}

fn no_cap_recorded() -> u64 {
    100
}

/// A scripted battery reading. Only `charge_pct` is required; the rest
/// default to a discharging laptop with no estimate, which is the shape
/// every battery scenario in the shipped library needs.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScriptedBattery {
    pub charge_pct: u8,
    #[serde(default)]
    pub on_ac: bool,
    #[serde(default)]
    pub charging: bool,
    #[serde(default)]
    pub minutes_to_empty: Option<u32>,
}

/// One line of a scenario file: a reading, and how long it holds.
///
/// An ABSENT `thermal` (or `battery`) is not a missing field to be
/// defaulted — it is the `None` reading itself, the "time passed, no new
/// information" case the governor has its own arm for. That is why both
/// are `Option` rather than required.
///
/// Unknown fields are tolerated (`note` is the conventional one) so a
/// fixture can explain what it pins on the line that pins it, and stay
/// readable by `jq`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ScenarioFrame {
    /// Simulated milliseconds this reading holds for. Must be > 0 — a
    /// zero-length frame is unreachable and is almost always a typo.
    pub hold_ms: u64,
    #[serde(default)]
    pub thermal: Option<ScriptedThermal>,
    #[serde(default)]
    pub battery: Option<ScriptedBattery>,
    #[serde(default)]
    pub note: Option<String>,
}

impl ScenarioFrame {
    fn reading(&self) -> HostReading {
        HostReading {
            thermal: self.thermal.as_ref().map(|t| ThermalSample {
                state: t.state.clone(),
                cpu_speed_limit_pct: t.cpu_speed_limit_pct,
            }),
            battery: self.battery.as_ref().map(|b| BatterySample {
                charge_pct: b.charge_pct,
                on_ac: b.on_ac,
                charging: b.charging,
                minutes_to_empty: b.minutes_to_empty,
            }),
        }
    }
}

/// Parse a scenario document: JSONL, one [`ScenarioFrame`] per line, blank
/// lines skipped.
///
/// Every error names the 1-based LINE, because a scenario is a fixture an
/// operator hand-edits and "expected `,` at 412" is not a usable answer.
/// An EMPTY document is an error rather than a source that reads `None`
/// forever — an empty scenario is indistinguishable from a truncated write
/// and would quietly disarm every assertion built on it.
pub fn parse_scenario(text: &str) -> Result<Vec<ScenarioFrame>, String> {
    let mut frames = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let frame: ScenarioFrame = serde_json::from_str(line)
            .map_err(|e| format!("line {}: {e}", i + 1))?;
        if frame.hold_ms == 0 {
            return Err(format!("line {}: hold_ms must be > 0 (a zero-length frame is never read)", i + 1));
        }
        frames.push(frame);
    }
    if frames.is_empty() {
        return Err("no frames (an empty scenario would silently read as `None` forever)".to_string());
    }
    Ok(frames)
}

/// A source that replays a scenario against its own simulated clock.
///
/// **Past the last frame, the last frame holds forever.** A scenario
/// describes a machine's condition, not the run's length: a run that
/// outlasts its scenario is at the last stated condition, not suddenly
/// unreadable. Writing the tail explicitly ("and then it stayed at
/// `nominal`") is the scenario author's job, and every shipped fixture
/// does it.
pub struct ScriptedSource {
    frames: Vec<ScenarioFrame>,
    /// Cumulative end time of each frame, so a lookup is one scan over a
    /// handful of entries rather than a running fold.
    ends_ms: Vec<u64>,
    now_ms: AtomicU64,
    path: PathBuf,
}

impl ScriptedSource {
    /// Build from already-parsed frames. `path` is carried only for
    /// provenance messages.
    pub fn new(frames: Vec<ScenarioFrame>, path: PathBuf) -> Self {
        let mut ends_ms = Vec::with_capacity(frames.len());
        let mut acc = 0u64;
        for f in &frames {
            acc = acc.saturating_add(f.hold_ms);
            ends_ms.push(acc);
        }
        Self { frames, ends_ms, now_ms: AtomicU64::new(0), path }
    }

    /// Load and parse a scenario file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let frames = parse_scenario(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self::new(frames, path.to_path_buf()))
    }

    /// The simulated clock's current position, in ms since the first tick.
    pub fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }

    /// Total simulated span of the scenario — the point past which the last
    /// frame holds forever.
    pub fn span_ms(&self) -> u64 {
        self.ends_ms.last().copied().unwrap_or(0)
    }

    /// The frame active at `sim_ms`. Never `None`: the constructor refuses
    /// an empty frame list by only ever being fed [`parse_scenario`]'s
    /// output, and the last frame holds past the end.
    fn frame_at(&self, sim_ms: u64) -> &ScenarioFrame {
        for (i, end) in self.ends_ms.iter().enumerate() {
            if sim_ms < *end {
                return &self.frames[i];
            }
        }
        // Past the end: the last frame holds. `frames` is never empty.
        &self.frames[self.frames.len() - 1]
    }
}

impl HostSource for ScriptedSource {
    fn read(&self) -> HostReading {
        self.frame_at(self.now_ms()).reading()
    }

    fn advance(&self, elapsed_ms: u64) {
        self.now_ms.fetch_add(elapsed_ms, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for ScriptedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedSource")
            .field("path", &self.path)
            .field("frames", &self.frames.len())
            .field("span_ms", &self.span_ms())
            .finish()
    }
}

// ── Resolution + provenance ──────────────────────────────────────────────

/// What the process's resolved source actually is — the value every
/// provenance surface renders off, so they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The real IOKit reads.
    Real,
    /// A scenario file is driving the thermal + battery readings.
    Scripted { path: String, frames: usize, span_ms: u64 },
    /// A scenario file was NAMED and could not be loaded. The process is
    /// reading real hardware (the safe direction), and says so loudly
    /// rather than leaving the operator to infer it from a governor that
    /// behaves unlike the scenario they thought they set.
    ScriptedUnavailable { path: String, error: String },
}

impl Provenance {
    /// `true` when the readings are NOT this machine's.
    pub fn is_simulated(&self) -> bool {
        matches!(self, Provenance::Scripted { .. })
    }

    /// The scenario path, when one is actually driving the readings.
    pub fn simulated_path(&self) -> Option<&str> {
        match self {
            Provenance::Scripted { path, .. } => Some(path.as_str()),
            _ => None,
        }
    }

    /// One line an operator can act on, or `None` when the source is real
    /// and there is nothing to say. Rendered verbatim by `darkmux doctor`
    /// and by the dispatch's sampler-start warning, off this one value —
    /// the same "two surfaces cannot disagree" shape the ladder's own
    /// disarm notes use.
    pub fn warning(&self) -> Option<String> {
        match self {
            Provenance::Real => None,
            Provenance::Scripted { path, frames, span_ms } => Some(format!(
                "SIMULATED host readings — thermal and battery come from the scenario file \
                 {path} ({frames} frames, {span_ms}ms of simulated time), NOT from this \
                 machine. The governor is pacing against a fiction. Unset \
                 DARKMUX_HOST_SOURCE_SCRIPT to read real hardware."
            )),
            Provenance::ScriptedUnavailable { path, error } => Some(format!(
                "DARKMUX_HOST_SOURCE_SCRIPT names {path}, which could not be loaded ({error}) \
                 — reading REAL hardware instead. Nothing is simulated; fix the path or unset \
                 the variable."
            )),
        }
    }
}

struct Resolved {
    source: Box<dyn HostSource>,
    provenance: Provenance,
}

fn resolved() -> &'static Resolved {
    static CELL: OnceLock<Resolved> = OnceLock::new();
    CELL.get_or_init(|| {
        let Some(path) = darkmux_types::config_access::host_source_script() else {
            return Resolved { source: Box::new(RealSource), provenance: Provenance::Real };
        };
        match ScriptedSource::load(&path) {
            Ok(s) => {
                let provenance = Provenance::Scripted {
                    path: path.display().to_string(),
                    frames: s.frames.len(),
                    span_ms: s.span_ms(),
                };
                Resolved { source: Box::new(s), provenance }
            }
            Err(error) => Resolved {
                source: Box::new(RealSource),
                provenance: Provenance::ScriptedUnavailable { path: path.display().to_string(), error },
            },
        }
    })
}

/// The process's resolved host source. Resolved ONCE, on first use —
/// `env(DARKMUX_HOST_SOURCE_SCRIPT) > real`, through
/// `config_access` like every other knob.
pub fn current() -> &'static dyn HostSource {
    resolved().source.as_ref()
}

/// The process's resolved provenance. Every surface that announces a
/// simulated source renders off this.
pub fn provenance() -> &'static Provenance {
    &resolved().provenance
}

/// Advance a source by `elapsed_ms` and then read it — the ORDER
/// [`crate::host_probe::HostProbe::sample`] needs, in one place both it and
/// a test can call.
///
/// The order is the whole content of this function, and it is not
/// arbitrary: advancing AFTER the read would hand every tick the PREVIOUS
/// interval's reading, one tick stale, for the entire life of a run. And
/// `elapsed_ms` must be the probe's own measured gap, never a constant —
/// a sampler tick can block far longer than its nominal cadence (the `lms`
/// probe alone can stall ~30s), and a scripted clock fed a constant would
/// drift away from the `elapsed_ms` the governor is simultaneously being
/// fed from the real one.
pub fn advance_and_read(source: &dyn HostSource, elapsed_ms: u64) -> HostReading {
    source.advance(elapsed_ms);
    source.read()
}

/// Stamp `simulated_host_source` onto a flow-record payload when — and
/// only when — the readings are simulated.
///
/// Absent (never `false`, never `null`) on a real-hardware run, so the
/// field's mere PRESENCE answers "were these readings real", the same
/// shape `baseline` already uses on `telemetry.lms`.
///
/// **The run artifact deliberately does the opposite**, and a reader who
/// learns the rule here must not carry it across: `host_window`'s
/// `simulated_host_source` is serialized UNCONDITIONALLY, `null` on a real
/// run (`dispatch_internal`'s `host_window` builder says so at the site).
/// The asymmetry is intentional and follows the reader — a flow record is
/// consumed by code scanning a high-volume stream, where an absent key is
/// the cheapest possible "no"; an artifact is read by eye long after the
/// run, where an explicit `null` is a stronger statement than a key that
/// might merely have been forgotten.
pub fn stamp(payload: &mut serde_json::Value) {
    stamp_with(provenance(), payload);
}

/// [`stamp`]'s decision, with the provenance passed in rather than read
/// from the process-wide resolution.
///
/// Split out for the reason `darkmux-doctor`'s `describe_host_probe` is
/// split from `check_host_probe`: the resolution is a `OnceLock` read of an
/// env var, so a test driving `stamp` directly could only ever exercise
/// whichever variant the test process happened to resolve — which is
/// `Real`, always, and the branch that matters would be pinned by nothing.
pub fn stamp_with(provenance: &Provenance, payload: &mut serde_json::Value) {
    let Some(path) = provenance.simulated_path() else {
        return;
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("simulated_host_source".to_string(), serde_json::json!(path));
    }
}

// ── The producer registry, and why it is code rather than a paragraph ───
//
// The module doc above lists the record builders that must stamp. That
// list was written by hand, was correct when written, and went stale
// twice: `machine.rollup` landed one commit after it and was never added,
// `machine.battery` predated it and was never retrofitted, and the
// dispatch-side `thermal.*` / `battery.*` records were never in scope at
// all. Two sweeps each closed the surfaces inside their own frame and
// missed the siblings outside it — which is how one guarantee reached
// four unstamped producers.
//
// So the enumeration moved into the type system, where adding a producer
// without CLASSIFYING it fails a test instead of going unnoticed. What the
// registry enforces is MEMBERSHIP, not stamping: `audit` proves every
// watched action literal in the producing sources is classified here, and
// that every entry still has a producer. Whether a `Stamped` entry
// actually stamps is proven where it belongs — the per-record tests that
// drive each builder's `_with` split at a `Scripted` provenance. The two
// halves are deliberately separate, because a textual "is there a stamp
// call near this literal" check would pass on a stamp applied to the wrong
// payload, which is exactly the failure being guarded against.

/// What a flow-record producer owes the scripted-source contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampDuty {
    /// Either the record's CONTENT carries a host reading, or its very
    /// EXISTENCE is decided by one. A scripted source must be named on it.
    Stamped,
    /// The record neither carries a host reading nor can be caused by one.
    /// The string is the reason, and it is the whole point of the variant:
    /// the next sweep reads the call that was already made instead of
    /// re-deriving it and re-flagging the same record.
    Exempt(&'static str),
}

/// The action-string families [`audit`] watches in the producing sources.
///
/// **The known hole, stated rather than implied:** a producer that invents
/// a NEW family name (`"power.foo"`) escapes the scan until that prefix is
/// added here. The scan catches the realistic case — a new record in an
/// existing family, which is how all four unstamped producers arrived — not
/// every conceivable one. The FILE set has no such hole: see
/// [`PRODUCER_SOURCE_PATHS`].
#[cfg(test)]
const WATCHED_ACTION_PREFIXES: &[&str] = &["machine.", "thermal.", "battery.", "dispatch.rest"];

/// The files [`audit`] is given — every source in the workspace that BUILDS
/// a flow record in a watched family.
///
/// **This list is not hand-maintained on trust.** An earlier revision of
/// this registry closed the action-level hole and left the FILE-level one
/// open, which is the same rot mode one level up: `audit` only ever sees
/// the strings it is handed, so a new host-derived record landing in an
/// unscanned file — `host_probe/thermal.rs` or `host_probe/battery.rs`, the
/// two modules the facade exists to substitute, or `darkmux-flow`'s
/// `hooks.rs` — would have shipped unclassified with a green suite.
///
/// `producer_paths_are_discovered_not_remembered` walks every `.rs` file in
/// the workspace, finds the ones containing a watched literal, and asserts
/// that set equals this list plus [`NON_PRODUCER_SOURCE_PATHS`]. A new file
/// therefore cannot appear without someone deciding which of the two it is.
///
/// A `build.rs` emitting this list was the alternative and was rejected:
/// it adds a build script to a deliberately dep-light crate, it makes every
/// `.rs` edit in the workspace a rebuild trigger, and — the deciding
/// reason — it would silently WIDEN the scan to consumer files, where
/// `audit` would then flood with false `unclassified` entries for actions
/// those files only match on. Forcing a decision is the point; generating
/// the list would remove it.
#[cfg(test)]
pub(crate) const PRODUCER_SOURCE_PATHS: &[&str] = &[
    "crates/darkmux-crew/src/dispatch_internal.rs",
    "crates/darkmux-crew/src/host_probe/mod.rs",
    "crates/darkmux-serve/src/host_sampler.rs",
    "crates/darkmux-flow/src/presence_reconciler.rs",
];

/// Files that contain a watched action literal but produce no record from a
/// host reading — with the reason, so the next sweep reads the call instead
/// of re-deriving it.
#[cfg(test)]
pub(crate) const NON_PRODUCER_SOURCE_PATHS: &[(&str, &str)] = &[
    (
        "crates/darkmux-crew/src/host_source.rs",
        "this file — the registry itself. Its literals ARE the table, plus the scanner's own \
         fixtures; scanning it would classify the table as its own producer",
    ),
    (
        "crates/darkmux-crew/src/records_emitted.rs",
        "consumer: filters a record stream on `action == \"machine.telemetry\"`, builds nothing",
    ),
    (
        "crates/darkmux-serve/src/runs.rs",
        "consumer: the runs board matches on action strings, builds nothing",
    ),
    (
        "crates/darkmux-crew/src/dispatch_internal_tests.rs",
        "tests for a scanned producer, in a sibling file rather than behind the `#[cfg(test)]` \
         marker the scanner cuts at",
    ),
    (
        "crates/darkmux-serve/src/lib_tests.rs",
        "tests for the serve daemon, same sibling-file shape",
    ),
    (
        "crates/darkmux-crew/src/envelope.rs",
        "its only watched literal is a `machine.telemetry` fixture inside its own \
         `#[cfg(test)]` module, which the untruncated discovery scan sees and `audit` does \
         not; the envelope builds no flow records",
    ),
    (
        "crates/darkmux-lab/src/lab/stats.rs",
        "consumer (#2855): reads `machine.telemetry` records back out of the flow files to \
         derive a lab run's power figures, builds nothing",
    ),
    (
        "crates/darkmux-lab/src/lab/stats_tests.rs",
        "tests for that consumer, in a sibling file; its literals are fixture records",
    ),
];

/// Every flow-record action, in the sources [`audit`] scans, whose payload
/// can carry or be caused by a host reading — and what each one owes.
pub const HOST_READING_ACTIONS: &[(&str, StampDuty)] = &[
    // ── machine-scoped: these ride the fleet stream to ANOTHER machine's
    // machine lens, which is what makes an unstamped one a second machine
    // being told this one hit critical.
    ("machine.telemetry", StampDuty::Stamped),
    ("machine.thermal", StampDuty::Stamped),
    ("machine.battery", StampDuty::Stamped),
    ("machine.rollup", StampDuty::Stamped),
    (
        "machine.battery_health",
        StampDuty::Exempt(
            "battery::health() reads IOKit unconditionally and never consults host_source, on \
             macOS and on every other target — there is no simulated reading to name, and \
             stamping would label a real one. Pinned by \
             `the_probes_the_facade_substitutes_never_read_the_facade_themselves`",
        ),
    ),
    (
        "machine.online",
        StampDuty::Exempt(
            "presence edge from darkmux-flow's reconciler: `payload: None`, so there is no \
             reading in it and nothing to stamp. Emitted from a crate with no host source at all",
        ),
    ),
    (
        "machine.offline",
        StampDuty::Exempt("presence edge, same shape as machine.online"),
    ),
    // ── dispatch-scoped: session records, but the reading in them is this
    // machine's, and two of them are Warn.
    ("dispatch.rest", StampDuty::Stamped),
    ("thermal.stop_unresolved", StampDuty::Stamped),
    ("thermal.tier5_eject", StampDuty::Stamped),
    ("thermal.tier5_eject_failed", StampDuty::Stamped),
    ("battery.pause_unsupported", StampDuty::Stamped),
];

/// What [`audit`] found wrong, if anything.
#[cfg(test)]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HostReadingAudit {
    /// Action literals present in the scanned sources, in a watched
    /// family, that [`HOST_READING_ACTIONS`] does not classify — a new
    /// producer nobody decided about.
    pub unclassified: Vec<String>,
    /// Entries in [`HOST_READING_ACTIONS`] that no scanned source emits
    /// any more — a stale classification, which is how a table becomes as
    /// unreliable as the paragraph it replaced.
    pub stale: Vec<&'static str>,
}

/// Every watched action literal in one source file, in order of first
/// appearance.
///
/// The single scanner both [`audit`] and the file-discovery test run, so
/// "which files contain a literal" and "which literals are classified"
/// cannot disagree about what counts as one.
///
/// **What it catches**, verified by mutation: a plain literal, a `const`- or
/// `let`-bound one, a raw string, a literal after a trailing `//` on a code
/// line, a literal inside a `#[cfg(feature = …)]` module, and
/// `format!("machine.{}", x)` — which registers as the literal `machine.{}`
/// and fails LOUD as unclassified rather than passing silently.
///
/// **What it misses**, recorded so the boundary does not have to be
/// re-derived: an action assembled rather than written —
/// `format!("{}.invented", "machine")`, `concat!("machine", ".invented")`,
/// `String::push_str`, or macro-generated idents — and any line whose first
/// non-space character is `*`, which is skipped as a doc-comment
/// continuation. (Deref-assignment lines starting with `*` exist in the
/// scanned files today; none carries a watched literal.)
///
/// String literals are the odd-indexed segments of a `"`-split: exact for
/// action strings, none of which contain an escape, and deliberately not a
/// general Rust lexer.
#[cfg(test)]
pub(crate) fn watched_action_literals(src: &str) -> Vec<String> {
    // Tests live after this marker and assert on action strings
    // constantly; scanning them would ask the registry to classify every
    // consumer-side literal as a producer. A marker that stopped matching
    // fails LOUD here (the test tail floods `unclassified`) rather than
    // silently narrowing the scan.
    let body = match src.find("\n#[cfg(test)]\nmod tests") {
        Some(i) => &src[..i],
        None => src,
    };
    watched_action_literals_anywhere(body)
}

/// [`watched_action_literals`] WITHOUT the `#[cfg(test)]` cut — the whole
/// file, tests included.
///
/// The file-DISCOVERY scan uses this one, and the asymmetry is the whole
/// point rather than a detail. Truncating is correct for [`audit`], whose
/// question is *which LITERALS must be classified* and which would
/// otherwise drown in test assertions. It is wrong for discovery, whose
/// question is *which FILES carry one at all*: a cut there makes every
/// literal below the marker invisible, so a producer defined after a file's
/// test module — or in a file whose test module happens to sit near the top
/// — is a silent miss, and the guard is then only as strong as the
/// assumption that nobody ever appends below a `#[cfg(test)]`.
///
/// That is not hypothetical. It is exactly what swallowed the first attempt
/// at the discovery guard: the repro appended a watched literal to the end
/// of `host_probe/thermal.rs`, below its test module at line 194, and the
/// truncating scanner dropped it before discovery ever saw it — 1 test run,
/// 1 passed, on a file in neither classification list.
///
/// Over-discovery costs nothing by comparison: a file found only because of
/// its own tests is classified once as a non-producer, with a reason, and
/// never thought about again.
#[cfg(test)]
pub(crate) fn watched_action_literals_anywhere(src: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        for (i, segment) in line.split('"').enumerate() {
            if i % 2 == 0 {
                continue;
            }
            if WATCHED_ACTION_PREFIXES.iter().any(|p| segment.starts_with(p))
                && !found.iter().any(|f| f == segment)
            {
                found.push(segment.to_string());
            }
        }
    }
    found
}

/// Scan producing sources for watched action literals and reconcile them
/// against [`HOST_READING_ACTIONS`].
///
/// `#[cfg(test)] pub(crate)`: the only caller is this module's own registry
/// test, which reaches the other crates' sources through `include_str!`
/// rather than through a dependency, so the scanner has no reason to exist
/// in a shipped binary. [`StampDuty`] and [`HOST_READING_ACTIONS`] stay
/// genuinely public — `darkmux-serve`'s host-sampler tests read the
/// classification across the crate boundary, and a `cfg(test)` item is
/// invisible there. An earlier revision made the scanner `pub` on the
/// stated grounds that darkmux-serve called it; nothing ever did.
#[cfg(test)]
pub(crate) fn audit(sources: &[&str]) -> HostReadingAudit {
    let mut found: Vec<String> = Vec::new();
    for src in sources {
        for lit in watched_action_literals(src) {
            if !found.contains(&lit) {
                found.push(lit);
            }
        }
    }
    HostReadingAudit {
        unclassified: found
            .iter()
            .filter(|f| !HOST_READING_ACTIONS.iter().any(|(a, _)| a == *f))
            .cloned()
            .collect(),
        stale: HOST_READING_ACTIONS
            .iter()
            .filter(|(a, _)| !found.iter().any(|f| f == a))
            .map(|(a, _)| *a)
            .collect(),
    }
}

#[cfg(test)]
mod producer_registry_tests {
    use super::*;

    /// The three files that BUILD a flow record from a host reading.
    ///
    /// `include_str!` rather than a runtime path read, so a file that moves
    /// breaks the BUILD rather than silently scanning nothing — a scanner
    /// pointed at a path that no longer exists is the "probe that passes
    /// without executing" failure, and it would report a clean sweep
    /// forever. `host_sampler.rs` lives in `darkmux-serve`, which depends on
    /// this crate rather than the other way round; the file is read at
    /// compile time and creates no crate dependency in either direction.
    fn producer_sources() -> Vec<&'static str> {
        vec![
            include_str!("dispatch_internal.rs"),
            include_str!("host_probe/mod.rs"),
            include_str!("../../darkmux-serve/src/host_sampler.rs"),
            include_str!("../../darkmux-flow/src/presence_reconciler.rs"),
        ]
    }

    /// The workspace root, derived from this crate's own manifest dir at
    /// COMPILE time — so it points at the source tree even when the test
    /// binary runs from somewhere else.
    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("crates/<crate> has a workspace root two levels up")
            .to_path_buf()
    }

    /// Every `.rs` file under the workspace, skipping build output and
    /// vendored trees.
    fn all_rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if matches!(name.as_ref(), "target" | "node_modules" | ".git") {
                    continue;
                }
                all_rust_sources(&path, out);
            } else if name.ends_with(".rs") {
                out.push(path);
            }
        }
    }

    /// The guard the two prior sweeps did not have. A new `machine.*` /
    /// `thermal.*` / `battery.*` / `dispatch.rest` producer is not allowed
    /// to exist without someone deciding, in `HOST_READING_ACTIONS`,
    /// whether a scripted source has to be named on it.
    #[test]
    fn every_host_reading_producer_is_classified_and_every_classification_has_a_producer() {
        let result = audit(&producer_sources());
        assert_eq!(
            result,
            HostReadingAudit::default(),
            "a flow-record action in a watched family is either stamped or exempt, and the \
             decision lives in HOST_READING_ACTIONS. `unclassified` = a producer nobody decided \
             about (add it, with the reason if it is exempt). `stale` = a classification whose \
             producer is gone (remove it, so this table does not rot the way the module doc \
             above did)."
        );
    }

    /// **The MF1 guard: the scanned-FILE set is discovered, not
    /// remembered.** `audit` only ever sees the strings it is handed, so an
    /// earlier revision closed the action-level hole and left this one open
    /// — a new host-derived record in `host_probe/thermal.rs`,
    /// `host_probe/battery.rs` (the two modules the facade exists to
    /// substitute) or `darkmux-flow`'s `hooks.rs` would have shipped
    /// unclassified with a green suite.
    ///
    /// Walks every `.rs` file in the workspace with the SAME scanner
    /// `audit` uses, and requires each file carrying a watched literal to
    /// be classified as a producer or as a non-producer with a reason.
    #[test]
    fn producer_paths_are_discovered_not_remembered() {
        let root = workspace_root();
        let mut files = Vec::new();
        all_rust_sources(&root, &mut files);

        // Prove the walk can fail before trusting that it passed: a scan
        // that found nothing (wrong root, unreadable tree) looks exactly
        // like a clean result.
        assert!(
            files.len() > 200,
            "the workspace walk found only {} .rs files under {} — it is not scanning the source \
             tree, and a scan of nothing passes every assertion below",
            files.len(),
            root.display()
        );

        let mut discovered: Vec<String> = Vec::new();
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            if watched_action_literals_anywhere(&text).is_empty() {
                continue;
            }
            let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
            discovered.push(rel);
        }
        discovered.sort();

        let mut classified: Vec<String> = PRODUCER_SOURCE_PATHS
            .iter()
            .map(|p| (*p).to_string())
            .chain(NON_PRODUCER_SOURCE_PATHS.iter().map(|(p, _)| (*p).to_string()))
            .collect();
        classified.sort();

        assert_eq!(
            discovered, classified,
            "every workspace file containing a watched action literal must be classified. A file \
             in `discovered` but not `classified` is the MF1 failure: it is unscanned, so any \
             record it builds ships unstamped with a green suite — add it to \
             PRODUCER_SOURCE_PATHS (and to `producer_sources()`), or to NON_PRODUCER_SOURCE_PATHS \
             with the reason. A file in `classified` but not `discovered` no longer has one."
        );
    }

    /// `producer_sources()` hands `audit` the file CONTENTS via
    /// `include_str!`, while [`PRODUCER_SOURCE_PATHS`] names the same files
    /// by path for the discovery test — two lists, and this is what keeps
    /// them the same list. Without it, an `include_str!` could point
    /// somewhere the path list does not name and the discovery guard would
    /// be checking a set nobody scans.
    ///
    /// The `include_str!` form is kept deliberately alongside the runtime
    /// read: it makes a moved file a BUILD error, and it makes an edit to
    /// any scanned file rebuild and re-run this crate's registry tests.
    #[test]
    fn the_included_sources_are_exactly_the_paths_the_discovery_test_guards() {
        let root = workspace_root();
        let included = producer_sources();
        assert_eq!(
            included.len(),
            PRODUCER_SOURCE_PATHS.len(),
            "producer_sources() and PRODUCER_SOURCE_PATHS must name the same files"
        );
        for (path, text) in PRODUCER_SOURCE_PATHS.iter().zip(included) {
            let on_disk = std::fs::read_to_string(root.join(path))
                .unwrap_or_else(|e| panic!("{path} must exist and be readable: {e}"));
            assert_eq!(
                on_disk, text,
                "{path} is not the file `producer_sources()` includes at that position — the two \
                 lists have drifted, in order or in content"
            );
        }
    }

    /// **The MF2 tripwire, made able to observe the thing it watches.**
    ///
    /// `machine.battery_health` is registered `Exempt` on one claim:
    /// `battery::health()` reads IOKit unconditionally and never consults
    /// the facade. The test backing it used to drive the record BUILDER
    /// with a hand-built struct, which cannot observe where the reading
    /// came from — routing `health()` through the facade left all 50
    /// host-sampler tests green.
    ///
    /// A physical source check can observe it, for the same reason
    /// `the_host_probe_advances_the_source_by_its_own_measured_interval`
    /// is one. The invariant is directional: `host_source` WRAPS these two
    /// modules from outside (`ScriptedSource` substitutes their `sample`
    /// functions); neither may read it back. The moment one does, the
    /// exemption is wrong and this goes red.
    #[test]
    fn the_probes_the_facade_substitutes_never_read_the_facade_themselves() {
        for (name, src) in [
            ("host_probe/battery.rs", include_str!("host_probe/battery.rs")),
            ("host_probe/thermal.rs", include_str!("host_probe/thermal.rs")),
        ] {
            assert!(
                !src.contains("host_source"),
                "{name} must not reference `host_source`: the facade substitutes these probes \
                 from OUTSIDE, and a probe reading the facade back inverts that. For battery.rs \
                 specifically, `machine.battery_health`'s Exempt classification rests on \
                 `health()` being an unconditional IOKit read — if that changed, the record now \
                 needs a stamp and the registry entry is a lie."
            );
        }
    }

    /// The scan must actually be scanning. A rule that silently matches
    /// nothing looks identical to a clean result, so pin the floor: the
    /// sources contain the specific actions the sweep was about.
    #[test]
    fn the_scan_reaches_the_records_the_sweep_was_about() {
        let sources = producer_sources();
        let found_but_removed_from_the_table: Vec<&str> = ["machine.rollup", "machine.battery", "machine.thermal"]
            .into_iter()
            .filter(|a| audit(&sources).stale.contains(a))
            .collect();
        assert!(
            found_but_removed_from_the_table.is_empty(),
            "these actions must be visible to the scanner in the producing sources, or the \
             registry is guarding nothing: {found_but_removed_from_the_table:?}"
        );
        assert!(
            HOST_READING_ACTIONS.len() >= 10,
            "the enumeration is the deliverable — a shrunken table means a producer was dropped \
             rather than reclassified"
        );
    }

    /// Comments naming an action are prose, not producers. Without this the
    /// scan would flag every doc comment in `host_source.rs`'s own module
    /// header and the registry would be unusable.
    #[test]
    fn prose_naming_an_action_is_not_a_producer() {
        let src = "// emits \"machine.invented\" when hot\n    /// see \"thermal.invented\"\n";
        assert_eq!(audit(&[src]).unclassified, Vec::<String>::new());
        // …but the same literal in code IS one.
        assert_eq!(
            audit(&["let a = \"machine.invented\";\n"]).unclassified,
            vec!["machine.invented".to_string()]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(text: &str) -> Vec<ScenarioFrame> {
        parse_scenario(text).expect("scenario parses")
    }

    #[test]
    fn a_frame_holds_for_its_own_hold_ms_and_the_last_one_holds_forever() {
        let src = ScriptedSource::new(
            frames(
                r#"{"hold_ms": 1000, "thermal": {"state": "nominal"}}
{"hold_ms": 2000, "thermal": {"state": "fair"}}"#,
            ),
            PathBuf::from("<test>"),
        );
        assert_eq!(src.read().thermal.unwrap().state, "nominal");
        src.advance(999);
        assert_eq!(src.read().thermal.unwrap().state, "nominal", "999ms is still inside frame 1");
        src.advance(1);
        assert_eq!(src.read().thermal.unwrap().state, "fair", "1000ms is frame 2's first ms");
        src.advance(2000);
        assert_eq!(src.read().thermal.unwrap().state, "fair", "past the end, the last frame holds");
        src.advance(10_000_000);
        assert_eq!(src.read().thermal.unwrap().state, "fair");
    }

    #[test]
    fn read_does_not_advance_the_cursor() {
        let src = ScriptedSource::new(
            frames(
                r#"{"hold_ms": 100, "thermal": {"state": "nominal"}}
{"hold_ms": 100, "thermal": {"state": "critical"}}"#,
            ),
            PathBuf::from("<test>"),
        );
        for _ in 0..50 {
            assert_eq!(src.read().thermal.unwrap().state, "nominal");
        }
        assert_eq!(src.now_ms(), 0, "the three one-shot call sites must not move the cursor");
    }

    #[test]
    fn an_absent_thermal_key_is_the_none_reading_not_a_default() {
        let src = ScriptedSource::new(
            frames(r#"{"hold_ms": 100, "battery": {"charge_pct": 40}}"#),
            PathBuf::from("<test>"),
        );
        let r = src.read();
        assert!(r.thermal.is_none(), "an absent thermal key is the governor's `None` arm");
        assert_eq!(r.battery.unwrap().charge_pct, 40);
    }

    #[test]
    fn cpu_speed_limit_defaults_to_no_cap_recorded() {
        let src = ScriptedSource::new(
            frames(r#"{"hold_ms": 100, "thermal": {"state": "fair"}}"#),
            PathBuf::from("<test>"),
        );
        assert_eq!(
            src.read().thermal.unwrap().cpu_speed_limit_pct,
            100,
            "100 is `no cap recorded`, what a cool machine reports"
        );
    }

    #[test]
    fn battery_defaults_are_a_discharging_laptop_with_no_estimate() {
        let src = ScriptedSource::new(
            frames(r#"{"hold_ms": 100, "battery": {"charge_pct": 12}}"#),
            PathBuf::from("<test>"),
        );
        let b = src.read().battery.unwrap();
        assert_eq!((b.charge_pct, b.on_ac, b.charging, b.minutes_to_empty), (12, false, false, None));
    }

    #[test]
    fn a_parse_error_names_the_line() {
        let err = parse_scenario("{\"hold_ms\": 1}\nnot json\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "got {err}");
    }

    #[test]
    fn a_zero_length_frame_is_refused() {
        let err = parse_scenario(r#"{"hold_ms": 0, "thermal": {"state": "fair"}}"#).unwrap_err();
        assert!(err.contains("hold_ms must be > 0"), "got {err}");
    }

    #[test]
    fn an_empty_scenario_is_refused_rather_than_reading_none_forever() {
        assert!(parse_scenario("").is_err());
        assert!(parse_scenario("\n\n   \n").is_err());
    }

    #[test]
    fn blank_lines_are_skipped_and_notes_are_tolerated() {
        let f = frames(
            "\n{\"hold_ms\": 5, \"note\": \"why this frame exists\", \"thermal\": {\"state\": \"serious\"}}\n\n",
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].note.as_deref(), Some("why this frame exists"));
    }

    // ── provenance ───────────────────────────────────────────────────────

    #[test]
    fn a_real_source_says_nothing_and_stamps_nothing() {
        let p = Provenance::Real;
        assert!(!p.is_simulated());
        assert_eq!(p.simulated_path(), None);
        assert_eq!(p.warning(), None);
    }

    #[test]
    fn a_scripted_provenance_names_the_file_and_says_the_readings_are_not_this_machines() {
        let p = Provenance::Scripted { path: "/tmp/hot.jsonl".into(), frames: 4, span_ms: 600_000 };
        assert!(p.is_simulated());
        assert_eq!(p.simulated_path(), Some("/tmp/hot.jsonl"));
        let w = p.warning().expect("a simulated source must never be silent");
        assert!(w.contains("SIMULATED"), "{w}");
        assert!(w.contains("/tmp/hot.jsonl"), "{w}");
        assert!(w.contains("NOT from this"), "{w}");
        assert!(w.contains("DARKMUX_HOST_SOURCE_SCRIPT"), "the remedy must name the knob: {w}");
    }

    #[test]
    fn an_unloadable_scenario_reads_real_hardware_and_says_so() {
        let p = Provenance::ScriptedUnavailable {
            path: "/nope.jsonl".into(),
            error: "No such file or directory (os error 2)".into(),
        };
        assert!(!p.is_simulated(), "a failed load must not read as simulated");
        assert_eq!(p.simulated_path(), None, "nothing may be stamped when nothing is simulated");
        let w = p.warning().expect("a named-but-unloadable scenario must never be silent");
        assert!(w.contains("/nope.jsonl"), "{w}");
        assert!(w.contains("REAL hardware"), "{w}");
    }

    #[test]
    fn stamp_is_absent_not_false_when_the_source_is_real() {
        let mut payload = serde_json::json!({ "reason": "thermal" });
        stamp_with(&Provenance::Real, &mut payload);
        assert!(
            payload.get("simulated_host_source").is_none(),
            "a real run must carry no key at all, not `false` and not `null` — its mere \
             PRESENCE is what answers `were these readings real`"
        );

        let mut payload = serde_json::json!({ "reason": "thermal" });
        stamp_with(
            &Provenance::ScriptedUnavailable { path: "/nope".into(), error: "boom".into() },
            &mut payload,
        );
        assert!(
            payload.get("simulated_host_source").is_none(),
            "a scenario that failed to LOAD is reading real hardware, so nothing may be \
             stamped — stamping the path here would mark real readings as simulated"
        );
    }

    #[test]
    fn stamp_names_the_scenario_on_every_simulated_payload() {
        let mut payload = serde_json::json!({ "reason": "thermal", "state": "fair" });
        stamp_with(
            &Provenance::Scripted { path: "/s.jsonl".into(), frames: 1, span_ms: 1 },
            &mut payload,
        );
        assert_eq!(
            payload["simulated_host_source"],
            serde_json::json!("/s.jsonl"),
            "a pacing record made on scripted readings must say so — a flow stream that \
             cannot distinguish a simulated pause from a real one is a flow stream that \
             lies about the machine"
        );
        assert_eq!(payload["reason"], serde_json::json!("thermal"), "and must not disturb the rest");
    }

    #[test]
    fn advance_and_read_advances_first_so_a_tick_never_reads_stale() {
        let src = ScriptedSource::new(
            frames(
                r#"{"hold_ms": 1000, "thermal": {"state": "nominal"}}
{"hold_ms": 1000, "thermal": {"state": "serious"}}"#,
            ),
            PathBuf::from("<test>"),
        );
        // A 1000ms tick lands exactly on frame 2's first ms. Reading BEFORE
        // advancing would return `nominal` here and `serious` only on the
        // tick after — every reading one tick stale, for the whole run.
        let r = advance_and_read(&src, 1000);
        assert_eq!(r.thermal.unwrap().state, "serious");
    }

    #[test]
    fn advance_and_read_applies_the_elapsed_it_is_given() {
        let src = ScriptedSource::new(
            frames(
                r#"{"hold_ms": 10000, "thermal": {"state": "nominal"}}
{"hold_ms": 10000, "thermal": {"state": "fair"}}"#,
            ),
            PathBuf::from("<test>"),
        );
        // Five 1000ms ticks do not reach frame 2; one 10000ms tick does.
        // A wiring that passed a constant (or zero) would freeze the
        // scenario at frame 1 forever, and every scripted run would report
        // a permanently cool machine — the exact failure the facade's
        // provenance surfaces exist to make impossible to miss.
        for _ in 0..5 {
            assert_eq!(advance_and_read(&src, 1000).thermal.unwrap().state, "nominal");
        }
        assert_eq!(advance_and_read(&src, 10_000).thermal.unwrap().state, "fair");
        assert_eq!(src.now_ms(), 15_000);
    }

    #[test]
    fn the_host_probe_advances_the_source_by_its_own_measured_interval() {
        // A wiring fact no in-process test can reach: `HostProbe::sample`
        // reads the resolved source through a process-wide `OnceLock`, so a
        // test could only ever drive whichever variant this process
        // resolved (`Real`, always). Same posture as `preflight`'s own
        // `the_pre_flight_calls_the_battery_gate_at_all`: a physical source
        // check for wiring, not a comment that can drift.
        let src = include_str!("host_probe/mod.rs");
        assert!(
            src.contains("advance_and_read(source, interval_ms)"),
            "HostProbe::sample must advance the scripted clock by its OWN measured gap — a \
             constant, or zero, freezes every scenario at its first frame and reports a \
             permanently cool machine"
        );
    }

    #[test]
    fn load_reports_the_path_in_its_error() {
        let err = ScriptedSource::load(Path::new("/definitely/not/here.jsonl")).unwrap_err();
        assert!(err.contains("/definitely/not/here.jsonl"), "got {err}");
    }
}

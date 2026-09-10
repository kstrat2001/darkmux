//! (#2110 governor, #2109 breaker) Host-side thermal pacing.
//!
//! Fed one OS thermal reading (`host_probe::ThermalSample`) per tick from
//! the dispatch's per-dispatch host sampler (`dispatch_internal.rs`'s
//! `run_telemetry_sampler`, which already reads `probe.sample().thermal`
//! every `TELEMETRY_SAMPLE_INTERVAL`), this module decides whether the
//! in-flight dispatch should rest and, if the machine never cools, whether
//! the mission should stop dispatching further units.
//!
//! **Governor (#2110):** at or above `pause_at`, write the pace file
//! (`pause: true, reason: "thermal"`) — the in-flight unit rests at its
//! next turn boundary (`runtime/src/pace.rs`, #2114). Clears the pause once
//! the state has held at or below `resume_at`, continuously, for
//! `resume_hold_ms` (hysteresis — a state bouncing right at the threshold
//! must not flap the pace file). If one continuous pause episode runs past
//! `max_pause_ms` without recovering, hands off to the breaker instead of
//! resting forever.
//!
//! **Breaker (#2109):** at `critical`, or when `cpu_speed_limit_pct` drops
//! below `min_cpu_speed_limit_pct` for `speed_limit_hold_samples`
//! CONSECUTIVE samples (finding 7 of the #2110/#2109 review — a lone
//! sample below the floor is noise, not a sustained condition; the
//! `critical` state check is unaffected and still trips immediately),
//! write the pace file with `reason: "thermal-critical"` and — for a
//! crawl mission — drop the crawl's `STOP` file so no further unit gets
//! dispatched. **Never kills the container.** The in-flight unit pauses at
//! its next turn boundary with its checkpoint persisted (#2114); resume is
//! the operator's call once #2114's `--resume` CLI flag ships (not wired
//! yet — see `dispatch_internal.rs`'s `resume_checkpoint` doc). Once
//! tripped, the governor goes terminal for DECISIONS — it does not un-pause itself on
//! recovery — but it does NOT go silent: see the heartbeat contract below.
//!
//! **Pace file location (operator correction during #2110/#2109 review):**
//! NOT under the mounted `/workspace` — crawl units mount that read-only,
//! and a coder run's workspace IS the operator's own repo tree, the wrong
//! place for darkmux's own bookkeeping. It lives in the dispatch's
//! **out-dir** instead (`host_out`, mounted at `/darkmux-out`; see
//! `dispatch_internal.rs`'s `apply_volume_mounts`), same home as
//! `.prompt.txt` and the trajectory. Container path: `/darkmux-out/pace.json`.
//! The runtime-side reader (`runtime/src/pace.rs`) reads from there —
//! `pace_file_path_matches_runtime_out_base` below is the conformance test
//! that keeps the two literal join expressions in sync.
//!
//! **Heartbeat contract (redesigned #2114 cf1b1993, superseding this
//! module's earlier `expires: false` flag):** the runtime honors a pause
//! only while `written_at_ms` is fresher than `max_pause_ms` — there is
//! NO per-reason opt-out, a `thermal-critical` stop gets no exemption from
//! the ceiling, only an ACTIVE WRITER does. "Indefinite" is expressed as
//! "someone keeps renewing it," never as a flag. So both `Paused` and
//! `Broken` re-stamp the pace file on a cadence well inside `max_pause_ms`
//! (every `max_pause_ms / 4` of elapsed time — see
//! `ThermalGovernor::restamp_interval_ms`) for as long as the state holds,
//! not just on transition. A gap in OS thermal readings while paused
//! (`thermal_sample` returns `None` mid-episode) is treated as "time
//! passed, no new information" rather than frozen accounting or a stale
//! stamp — see `on_sample`'s `None` arm (finding 3 of the same review).
//! Pace-file writes are atomic (tmp file + rename) so the runtime's poll
//! never observes a partially-written file.

use crate::host_probe::thermal::THERMAL_STATES;
use crate::host_probe::ThermalSample;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Severity rank of a thermal state name. An unrecognized name (a future
/// macOS state this build doesn't know) ranks WORSE than `critical` —
/// mirrors `host_probe::mod::thermal_severity`'s reasoning: silently
/// treating an unknown state as mild would hide real thermal pressure.
fn severity(state: &str) -> usize {
    THERMAL_STATES.iter().position(|s| *s == state).unwrap_or(THERMAL_STATES.len())
}

/// `<host_out>/pace.json` — mounted into the container at
/// `/darkmux-out/pace.json`. MUST match the runtime-side join in
/// `runtime/src/pace.rs`'s `pace_file_path`
/// (`out_dir.join("pace.json")`) — `pace_file_path_matches_runtime_out_base`
/// below is the conformance test that keeps the two in sync.
pub fn pace_file_path(host_out: &Path) -> PathBuf {
    host_out.join("pace.json")
}

/// Shape written to the pace file. Mirrors `runtime/src/pace.rs`'s
/// `PaceFile` fields (`pause`/`reason`/`state`/`written_at_ms`) exactly —
/// there is deliberately no `expires` field: #2114's cf1b1993 replaced
/// that flag with a pure heartbeat contract (see this module's doc), so
/// writing an `expires` key here would be dead data the runtime no longer
/// reads. The runtime-side reader tolerates unknown/extra fields on
/// deserialize (no `deny_unknown_fields`), so this stays forward-compatible
/// regardless.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct GovernorPaceFile {
    pause: bool,
    reason: &'static str,
    state: String,
    written_at_ms: u64,
}

fn now_epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// (#2110/#2109 review nit) Atomic write: a temp file in the SAME
/// directory (so the rename is same-filesystem, hence atomic on both APFS
/// and common Linux filesystems) followed by `rename` onto the real path.
/// Without this, the runtime's poll (`PaceReader::read`, on its own ~2s
/// cadence, fully independent of this writer's cadence) could observe a
/// truncated or half-written JSON file mid-write and treat it as
/// malformed — logged once, harmless, but a needless false alarm on every
/// governor tick if the two cadences ever raced. `write` + `rename` is the
/// same pattern `checkpoint.rs` already uses for its own pace-adjacent
/// out-dir writes.
fn write_pace_file(host_out: &Path, pause: bool, reason: &'static str, state: &str) {
    let pace =
        GovernorPaceFile { pause, reason, state: state.to_string(), written_at_ms: now_epoch_ms() };
    // Best-effort — a failed write is observability/pacing, never fatal to
    // the dispatch itself (mirrors every other sampler-adjacent write in
    // `dispatch_internal.rs`).
    let _ = std::fs::create_dir_all(host_out);
    let Ok(json) = serde_json::to_string(&pace) else { return };
    let final_path = pace_file_path(host_out);
    // Unique per-write tmp name (pid + epoch-ns) — the sampler thread is
    // the only writer for a given dispatch, but a unique name means a
    // failed/aborted write from an EARLIER tick can never collide with
    // this one's tmp file if cleanup ever slips.
    let tmp_path = host_out.join(format!(
        ".pace.json.tmp.{}.{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    if std::fs::write(&tmp_path, &json).is_ok() {
        let _ = std::fs::rename(&tmp_path, &final_path);
    }
}

/// (#2109) Best-effort derivation of a crawl mission's `STOP` file path
/// from the dispatch's `record_context` (the crawl's per-unit
/// `record_context`, carrying `workspace` = the crawl manifest name, plus
/// `unit`/`rule` as crawl-specific markers). The breaker needs to write the
/// SAME path the crawl launcher's per-unit loop checks
/// (`<crawl_root>/STOP`) for "no further unit dispatches" to actually
/// hold — but this module must not depend on or edit the crawl's own module
/// (another agent owns that file for #2131 concurrently), so the formula
/// is duplicated here from that launcher's DEFAULT (no `root:` override)
/// resolution: `<darkmux root>/crawl/<manifest_name>/STOP`.
///
/// **Known gap, documented rather than guessed at:** a crawl spec with an
/// explicit `root:` override reuses `materialized.root` instead (see
/// `WorkspaceSpec::resolved_root()`) and is NOT reconstructable from
/// `record_context` alone — this function has no signal that would let it
/// tell "default root, safe to derive" apart from "root: override,
/// this derivation would be a GUESS." That gap is unchanged by finding 5
/// below; it stays a documented limitation, not a guessed write (this
/// function still only derives the one formula it can stand behind).
/// Widen if a consumer needs it — same "not yet, but named" shape as
/// #1352's other documented narrowings.
///
/// Only fires when `record_context` carries BOTH `workspace` (the manifest
/// name) and `unit` — the crawl launcher's own vocabulary — so a non-crawl
/// dispatch that happens to set `record_context` for its own reasons never
/// gets a spurious `STOP` file written under it.
pub fn stop_file_path_from_record_context(
    record_context: Option<&serde_json::Value>,
) -> Option<PathBuf> {
    let ctx = record_context?.as_object()?;
    if !ctx.contains_key("unit") {
        return None;
    }
    let manifest_name = ctx.get("workspace")?.as_str()?;
    if !valid_crawl_manifest_name(manifest_name) {
        return None;
    }
    let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root;
    Some(root.join("crawl").join(manifest_name).join("STOP"))
}

/// (#2157) A crawl manifest name is safe to join onto `<darkmux
/// root>/crawl/` only if it is a SINGLE path component that cannot smuggle
/// an absolute path, a `..` traversal, an embedded/trailing separator, or
/// an empty segment through the `PathBuf::join` above — `Path::join`
/// REPLACES the accumulated path outright when the joined component is
/// absolute, and does nothing to strip `..`, so `/etc/passwd` or
/// `../../../etc/passwd` sail straight through unless the NAME is
/// validated before the join. The result can't be checked after the fact
/// either: it may not exist on disk yet (this runs before the STOP file is
/// ever written), and `canonicalize` would follow symlinks — the wrong
/// tool for validating a name, not a location (see this module's own
/// symlink-hazard note on `write_stop_file`, which this function does not
/// address).
///
/// Deliberately a REJECT, not a sanitize: `record_context.workspace` is
/// caller/crew-supplied, and a value carrying a path separator or a
/// `.`/`..` segment is either a caller bug or an attack — it should fail
/// derivation loudly (surfaced via [`stop_file_unresolved_reason`]), not
/// get silently rewritten to some OTHER name the operator never chose and
/// would not think to look for. Rejecting also keeps this validator
/// STRUCTURAL rather than a blacklist of "bad substrings" a sanitizer
/// could miss: the whitelist below cannot express an absolute path or a
/// `..` component by construction, so there is no way past it for the
/// exploit class this exists to close.
///
/// Same character class `workspace_spec::valid_source_id` already enforces
/// for the identical reason on a different join (a workspace SOURCE id
/// onto its own materialized root) — kept independently defined rather
/// than shared, per this module's existing "must not depend on the crawl
/// module" boundary (see [`stop_file_path_from_record_context`]'s own doc).
///
/// **The one-way divergence this doc used to describe is closed (#2455).**
/// The value being checked is `WorkspaceSpec::effective_name()` (spec
/// `name`, else the spec file's stem), which reaches here as
/// `record_context.workspace` via `materialized.name` ->
/// `crawl::plan::Plan::workspace`. Until #2455, `WorkspaceSpec` validated
/// each source `id` against this class but not the spec's own `name` — so
/// a spec named `q1 corpus` or `_scratch` loaded fine, materialized to
/// `<root>/workspaces/<name>`, and was then REJECTED here: a false
/// negative where such a crawl's breaker could never write its STOP file.
/// #2455 validates `name` at its SOURCE, in `WorkspaceSpec::validate()`,
/// against the identical character class — so any spec that reached
/// `load()` successfully now already has a `name` this function accepts
/// too. The two validators stay independently DEFINED (this module still
/// must not depend on the crawl module — see
/// [`stop_file_path_from_record_context`]'s own doc) but no longer
/// diverge in what they accept, for any spec that loaded through the
/// normal path. This function is kept rather than removed: it is still
/// the only defense against a `record_context.workspace` value that
/// didn't come from a validated `WorkspaceSpec` at all (a caller/crew bug,
/// or a hand-built `record_context`) — and #2455's join-time containment
/// in `resolved_root()` is no substitute here, since it guards a
/// DIFFERENT join under a DIFFERENT root and never runs on this path.
///
/// **The narrow residue, stated rather than rounded off (#2455 review).**
/// "Any spec that loaded through `load()`" is the honest scope, and it is
/// not every spec: a `WorkspaceSpec` built as a struct literal skips
/// `validate()` entirely, and one production caller does exactly that —
/// `darkmux_lab::crawl::plan_sites_step::derive_workspace_spec` names the
/// workspace after the GitHub repo under review. So a review of
/// `owner/.github` (a real and common repository) yields
/// `record_context.workspace == ".github"`: structurally safe, contained
/// by `resolved_root()`, and still REJECTED here for its leading dot.
/// That residue is deliberately left rather than papered over by relaxing
/// this class — it is the same safe direction as before (no STOP path is
/// derived, [`stop_file_unresolved_reason`] says so out loud), it costs
/// only a breaker's STOP file on one unusual repo name, and widening a
/// path-component whitelist to buy that back is the wrong trade. Revisit
/// if a real crawl ever runs against such a repo.
fn valid_crawl_manifest_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        // Also rejects "." and ".." outright — neither starts with an
        // alphanumeric — and rejects an empty string the same way the
        // caller's old `trim().is_empty()` check did, so this subsumes it.
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// (#2110/#2109 review finding 5) Companion to
/// [`stop_file_path_from_record_context`] — distinguishes the two reasons
/// that function can return `None`:
///
/// - This dispatch simply isn't crawl-shaped (`record_context` absent, or
///   missing the crawl launcher's `unit` marker) — nothing to stop, no
///   warning warranted. Returns `None` here too.
/// - This dispatch IS crawl-shaped (`unit` present) but the STOP path
///   could not be derived — `workspace` missing, not a string, or empty.
///   Returns `Some(reason)`: the breaker tripped on a crawl unit and had
///   no trustworthy path to tell the crawl to stop, which is exactly the
///   "silent failure that poisons the next crawl" this finding exists to
///   surface. The caller (`dispatch_internal.rs`) turns this into a
///   distinguishable `stop_written: false` warning event rather than let
///   the crawl keep dispatching units past a tripped breaker with no
///   trace of why the STOP never landed.
///
/// Does NOT cover the `root:`-override gap documented on the sibling
/// function — that gap has no signal in `record_context` to detect at
/// all, so it can't be distinguished from "derivation succeeded" here
/// either. Only the two MECHANICALLY DECIDABLE cases above are covered.
pub fn stop_file_unresolved_reason(record_context: Option<&serde_json::Value>) -> Option<&'static str> {
    let ctx = record_context?.as_object()?;
    if !ctx.contains_key("unit") {
        return None;
    }
    match ctx.get("workspace").and_then(|v| v.as_str()) {
        Some(name) if valid_crawl_manifest_name(name) => None,
        Some(name) if name.trim().is_empty() => Some("record_context.workspace is present but empty"),
        // (#2157) A non-empty value that still fails validation — an
        // absolute path, a `..` component, an embedded/trailing
        // separator — is a DISTINCT reason from "empty": surfaced
        // separately so a rejected traversal attempt doesn't read in logs
        // as an ordinary missing-field case.
        Some(_) => Some(
            "record_context.workspace is not a valid manifest name (must be a single path \
             segment: alphanumeric, `.`, `_`, `-` only — no `/`, no `..`)",
        ),
        None => Some("record_context.workspace is missing or not a string"),
    }
}

/// (#2456) The two DISTINCT situations that both mean "the breaker tripped
/// and could not stop the crawl". They reach the operator through the SAME
/// `thermal.stop_unresolved` warning — the urgency is the same, a crawl may
/// keep dispatching units past a tripped breaker — but the REMEDIES are
/// different, so the record names which one it is rather than leaving an
/// operator to string-match prose out of `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopUnresolvedCause {
    /// No trustworthy STOP path could be DERIVED at all — see
    /// [`stop_file_unresolved_reason`]. The breaker never writes one at a
    /// guessed path. Operator remedy: fix the crawl's `record_context`
    /// (`workspace` missing, empty, or not a single valid path segment)
    /// — a configuration/caller problem.
    PathUnderivable,
    /// A path WAS derived, but the write was REFUSED (#2456) — something
    /// is already sitting at the STOP path or its parent as a symlink, or
    /// the write failed outright. Operator remedy: go LOOK at
    /// `<root>/crawl/<name>/` — a symlink there is a filesystem-state
    /// finding, not a config typo, and is the exact hazard #2456 closed.
    WriteRefused,
}

impl StopUnresolvedCause {
    /// The stable value carried in the record's `cause` field. Stable
    /// because a downstream reader keys on it — see the
    /// `thermal.stop_unresolved` entry in `darkmux-flow`'s schema history.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PathUnderivable => "path_underivable",
            Self::WriteRefused => "write_refused",
        }
    }
}

/// (#2456) Decide which — if either — of [`StopUnresolvedCause`]'s two
/// situations a just-tripped breaker is in, and with what reason text.
///
/// Extracted as a pure function rather than left inline at the one call
/// site (`dispatch_internal.rs`'s telemetry sampler) precisely so it is
/// TESTABLE: the sampler runs on its own thread inside a live dispatch and
/// is not reachable from a unit test, so an inline version of this
/// selection was — measurably — pinned by nothing at all.
///
/// `None` means the breaker either wrote its STOP successfully or this
/// dispatch was never crawl-shaped; there is nothing to warn about.
pub fn stop_unresolved_cause<'a>(
    stop_path: Option<&Path>,
    derivation_reason: Option<&'a str>,
    write_error: Option<&'a str>,
) -> Option<(StopUnresolvedCause, &'a str)> {
    match stop_path {
        // No path was derived — the only thing that can be wrong is the
        // derivation, and `stop_file_unresolved_reason` already decided
        // whether that is worth warning about (a non-crawl dispatch is
        // not).
        None => derivation_reason.map(|r| (StopUnresolvedCause::PathUnderivable, r)),
        // A path WAS derived, so derivation is not the story; the only
        // remaining failure is the write itself.
        Some(_) => write_error.map(|e| (StopUnresolvedCause::WriteRefused, e)),
    }
}

/// **Fixed (#2456, filed out of the #2157 audit):** `stop_file`'s NAME is
/// validated (see [`valid_crawl_manifest_name`]), but until this fix
/// nothing verified its final PATH component before writing. If
/// `<root>/crawl/<name>` already existed as a symlink to somewhere else —
/// planted by anything with write access to `<root>/crawl/` before the
/// breaker ever fired — `create_dir_all` followed it and the fixed
/// `thermal-critical\n` content landed wherever the symlink pointed, not
/// under `<root>/crawl/`. Reachability requires local write access to
/// `<root>/crawl/` already, which is a strictly stronger position than the
/// unvalidated-name bug this module fixes for #2157 (that one is reachable
/// from a plain crawl manifest name/`record_context` value, no filesystem
/// write access needed at all). Did not fall out of the name-validation
/// fix because that one is string validation and this one is a
/// filesystem-state check.
///
/// The remedy is `crate::exclusive_fs::write_file_refusing_symlinks_0600`
/// — see its own doc for the full reasoning (why this can't just be
/// `create_new` at the final path: a LATER mission's own breaker trip
/// legitimately re-stamps an existing STOP file over an older one, per
/// [`stop_file_body`]'s doc, so "must not already exist" is the wrong
/// shape here; "must not already be a symlink" is the right one).
///
/// Fire-and-forget by necessity — this is the breaker's LAST ACTION under
/// thermal duress and must never panic or block — but NOT silent on
/// refusal: the `Err`, when there is one, is captured by every call site
/// into `self.last_stop_write_error` so the caller
/// (`dispatch_internal.rs`) can turn a refusal into a loud
/// `thermal.stop_unresolved` warning rather than let the crawl keep
/// dispatching units past a tripped breaker with no trace of why the STOP
/// never landed. See [`ThermalGovernor::last_stop_write_error`]'s own doc.
fn write_stop_file(stop_file: &Path, owner: Option<&str>) -> Result<(), String> {
    crate::exclusive_fs::write_file_refusing_symlinks_0600(
        stop_file,
        stop_file_body(owner).as_bytes(),
    )
}

/// (#2454) The STOP file's one line: the reason, plus — when the breaker
/// knows it — the mission whose run it fired in.
///
/// **Why the owner is in the file at all.** Nothing has EVER removed this
/// file: the retired launcher (`src/crawl_launch.rs`, gone in #2301) only
/// read it, and no code path in this repo has ever deleted one. That was
/// coherent while the only writer was a human `touch`ing it as a manual
/// kill switch — a hand-written sentinel is a hand-removed sentinel. It
/// stopped being coherent the moment #2109 made the BREAKER a writer: the
/// path is `<root>/crawl/<manifest>/STOP`, scoped to the WORKSPACE and not
/// to any run, so one thermal event would otherwise refuse every unit of
/// every future crawl on that workspace, forever, with no automatic way
/// back. Stamping the owner scopes the machine-written file to the run
/// that wrote it — which is exactly what the breaker means ("no FURTHER
/// unit gets dispatched") — while leaving an unattributed file (a human's
/// `touch`, or one from a pre-#2454 binary) honored by everyone, since
/// only a human can know what that one meant. See [`stop_hold_for_mission`]
/// for the read side.
fn stop_file_body(owner: Option<&str>) -> String {
    match owner.map(str::trim).filter(|m| !m.is_empty()) {
        // Deliberately one greppable line rather than JSON: an operator
        // finding this file wants `cat` to answer "what is this and who
        // left it", and the reader below only needs the one field.
        Some(mission) => format!("{STOP_FILE_REASON} mission={mission}\n"),
        None => format!("{STOP_FILE_REASON}\n"),
    }
}

/// The STOP file's reason word — the same string the pace file carries as
/// `reason` on a breaker trip, so the two artifacts of one event read alike.
pub const STOP_FILE_REASON: &str = "thermal-critical";

/// (#2454) Why a reader is honoring a STOP file it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopHold {
    /// This mission's own breaker wrote it, this run. The remaining units
    /// of THIS run must not dispatch.
    ThisMission,
    /// The file names no mission — a human's `touch`, or a breaker from a
    /// binary older than #2454. Honored by every mission, because nothing
    /// here can know what it meant; only a human removes it.
    Unattributed,
}

/// (#2454) Should the mission `mission_id` honor the STOP file at
/// `stop_file`? `None` — dispatch — when there is no such file, when it is
/// unreadable, or when it names a DIFFERENT mission.
///
/// A file naming another mission is a PREVIOUS run's thermal event. A new
/// mission only exists because the operator launched it, and that launch is
/// their own "go" — the breaker's job was to stop the run it fired in, and
/// that run is over. If the machine is still hot, the new run's own
/// governor trips again within a few samples and re-stamps this file under
/// the new mission's name, so nothing is lost by not inheriting the old
/// one. This is what keeps a transient thermal event from becoming a
/// permanent, workspace-wide refusal (see [`stop_file_body`]).
///
/// An unreadable file is treated as ABSENT rather than as a stop, matching
/// every other best-effort read in this module: a stop that cannot be
/// attributed to anything is not evidence of a thermal condition, and
/// failing closed here would brick the workspace on a truncated write,
/// which is the exact failure this function exists to prevent.
pub fn stop_hold_for_mission(stop_file: &Path, mission_id: &str) -> Option<StopHold> {
    let body = std::fs::read_to_string(stop_file).ok()?;
    match stop_file_owner(&body) {
        None => Some(StopHold::Unattributed),
        Some(owner) if owner == mission_id.trim() => Some(StopHold::ThisMission),
        Some(_) => None,
    }
}

/// The `mission=<id>` field of a STOP file's body, when it has one.
/// Tolerant on read: any line may carry it, and a body without one is a
/// legitimate (unattributed) shape, not a parse error.
fn stop_file_owner(body: &str) -> Option<&str> {
    body.split_whitespace()
        .find_map(|tok| tok.strip_prefix("mission="))
        .map(str::trim)
        .filter(|m| !m.is_empty())
}

/// One state change the governor made this tick — the caller (the sampler
/// loop in `dispatch_internal.rs`) turns each into a `dispatch.rest`-family
/// flow record so a slowed or stopped run is attributable. The periodic
/// heartbeat re-stamp (see the module doc) is NOT an event — it changes
/// nothing observable about the dispatch's pacing, only keeps the existing
/// pause file fresh, so it doesn't fire a `dispatch.rest` record on every
/// re-stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThermalEvent {
    /// Paused for thermal pacing (`reason: "thermal"`).
    Paused { state: String },
    /// Resumed after the hysteresis hold (`reason: "thermal"`, `pause: false`).
    Resumed { state: String },
    /// Breaker tripped (`reason: "thermal-critical"`) — max pause exceeded,
    /// `critical` state, or the CPU speed-limit floor held for
    /// `speed_limit_hold_samples` consecutive samples.
    Breaker { state: String },
}

/// Resolved thermal-governor tuning (`config_access::thermal_*`).
#[derive(Debug, Clone)]
pub struct ThermalGovernorConfig {
    pub enabled: bool,
    pub pause_at: String,
    pub resume_at: String,
    pub resume_hold_ms: u64,
    pub max_pause_ms: u64,
    pub min_cpu_speed_limit_pct: u64,
    /// (finding 7) Consecutive samples below `min_cpu_speed_limit_pct`
    /// required before the breaker trips on that signal. Does NOT apply
    /// to the `critical` state check.
    pub speed_limit_hold_samples: u32,
}

impl ThermalGovernorConfig {
    /// Resolve from the standard `env > config.json > default` precedence
    /// (`darkmux_types::config_access::thermal_*`).
    pub fn from_env() -> Self {
        Self {
            enabled: darkmux_types::config_access::thermal_enabled(),
            pause_at: darkmux_types::config_access::thermal_pause_at(),
            resume_at: darkmux_types::config_access::thermal_resume_at(),
            resume_hold_ms: darkmux_types::config_access::thermal_resume_hold_ms(),
            max_pause_ms: darkmux_types::config_access::thermal_max_pause_ms(),
            min_cpu_speed_limit_pct: darkmux_types::config_access::thermal_min_cpu_speed_limit_pct(),
            speed_limit_hold_samples: darkmux_types::config_access::thermal_speed_limit_hold_samples(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Paused,
    /// Terminal for DECISIONS: the breaker tripped and the governor never
    /// un-pauses itself on recovery — resume is out-of-band, the
    /// operator's call once #2114's `--resume` CLI flag ships. NOT terminal for the
    /// pace file's freshness: see the module doc's heartbeat contract —
    /// `on_sample` keeps re-stamping while `Broken`, just as while `Paused`.
    Broken,
}

/// Per-dispatch state machine. One instance lives for the dispatch's
/// sampler-thread lifetime (constructed alongside `run_telemetry_sampler`,
/// dropped when the sampler thread returns).
pub struct ThermalGovernor {
    config: ThermalGovernorConfig,
    state: State,
    /// ms accumulated in the CURRENT pause episode (resets to 0 on resume).
    pause_episode_ms: u64,
    /// ms the state has continuously held at/below `resume_at` while
    /// paused (resets to 0 the moment it rises back above `resume_at`).
    resume_hold_accum_ms: u64,
    /// (finding 3) Last thermal state name from a real `Some` sample —
    /// only ever set from `on_sample`'s `Some` arm, so it's populated by
    /// the time `Paused`/`Broken` is reachable (both require at least one
    /// prior `Some` to enter). Used to keep the pace file's `state` field
    /// and the breaker's `STOP`/event state meaningful on a tick where the
    /// OS thermal reading itself came back `None`.
    last_known_state: String,
    /// ms accumulated since the pace file's `written_at_ms` was last
    /// refreshed — drives the heartbeat re-stamp cadence (module doc)
    /// while `Paused` or `Broken`. Reset to 0 on every write, including
    /// state-transition writes (pause start, resume, breaker trip) — those
    /// already produce a fresh stamp, so the next periodic re-stamp is due
    /// a full interval later, not immediately.
    ms_since_stamp: u64,
    /// (finding 7) Consecutive samples with `cpu_speed_limit_pct` below
    /// the floor — reset to 0 the moment a sample reads at/above it, OR a
    /// `None` reading arrives (N4 of the #2110/#2109 review: a missing
    /// reading is not evidence the CPU is throttled, so it must not
    /// preserve or extend a streak, matching `resume_hold_accum_ms`'s own
    /// None-arm reset).
    speed_limit_low_streak: u32,
    /// (N1 of the #2110/#2109 review) Wall-clock anchors for the LAST
    /// actual pace-file write — an independent backstop against
    /// `ms_since_stamp`'s tick-accounted math, which trusts the CALLER's
    /// `elapsed_ms` argument. `Instant` catches a long BLOCKING tick
    /// (e.g. `lms ps` stalling up to 30s inside one sampler iteration);
    /// `SystemTime` catches a host SLEEP/WAKE wall-clock jump `Instant`
    /// might not reflect. Set on construction (no write has happened yet,
    /// so nothing is stale) and refreshed by `mark_stamped` alongside
    /// every `ms_since_stamp` reset.
    last_stamp_instant: Instant,
    last_stamp_wall: SystemTime,
    /// (#2454) The mission this governor is pacing, stamped into any STOP
    /// file the breaker writes so a LATER mission can tell "my own run's
    /// breaker tripped" from "some previous run's did" — see
    /// [`stop_file_body`] for why an unowned STOP file is a permanent,
    /// workspace-wide refusal. `None` for a dispatch with no mission (a
    /// bare `darkmux dispatch`), which writes the unattributed shape; that
    /// path never derives a STOP path today anyway, since the derivation
    /// requires the crawl `record_context` a mission-run unit stamps.
    stop_owner: Option<String>,
    /// (#2456) The reason the most recent STOP-file write attempt was
    /// refused, if it was. `None` after a successful write, or when no
    /// STOP write has been attempted yet. `write_stop_file`'s call is
    /// fire-and-forget BY NECESSITY — it is the breaker's LAST ACTION
    /// under thermal duress and must never panic or block — but a
    /// refusal must not be SILENT, so this field is how the caller
    /// (`dispatch_internal.rs`) learns of it: it polls
    /// [`ThermalGovernor::last_stop_write_error`] right after a `Breaker`
    /// event and, if `Some`, emits the SAME `thermal.stop_unresolved`
    /// warning shape `stop_file_unresolved_reason` already produces for
    /// the "path couldn't even be derived" case — from the operator's
    /// point of view both are "the breaker tried and could not stop the
    /// crawl," and deserve the same visibility.
    last_stop_write_error: Option<String>,
}

impl ThermalGovernor {
    pub fn new(config: ThermalGovernorConfig) -> Self {
        Self {
            config,
            state: State::Idle,
            pause_episode_ms: 0,
            resume_hold_accum_ms: 0,
            last_known_state: String::new(),
            ms_since_stamp: 0,
            speed_limit_low_streak: 0,
            last_stamp_instant: Instant::now(),
            last_stamp_wall: SystemTime::now(),
            stop_owner: None,
            last_stop_write_error: None,
        }
    }

    /// (#2456) See the field's own doc.
    pub fn last_stop_write_error(&self) -> Option<&str> {
        self.last_stop_write_error.as_deref()
    }

    /// (#2454) Name the mission whose run this governor is pacing. Builder
    /// form because [`ThermalGovernor::new`] is called from ~50 tests that
    /// have no mission and want the unattributed default; only the live
    /// dispatch path (`dispatch_internal.rs`) has an id to give.
    #[must_use]
    pub fn owned_by(mut self, mission_id: Option<&str>) -> Self {
        self.stop_owner = mission_id.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string);
        self
    }

    /// (N1) True once REAL time since the last pace-file write has
    /// reached the heartbeat interval, independent of whatever
    /// `elapsed_ms` the caller reported this tick. Takes the LARGER of
    /// the `Instant`-based and `SystemTime`-based ages so either kind of
    /// gap (a long blocking tick, or a wall-clock jump across a host
    /// sleep) is caught — a clock that goes BACKWARD on either side reads
    /// as 0 elapsed (not evidence of staleness, not underflowed into a
    /// huge one).
    fn real_age_past_interval(&self) -> bool {
        let interval = self.restamp_interval_ms();
        let by_instant = Instant::now().duration_since(self.last_stamp_instant).as_millis() as u64;
        let by_wall = SystemTime::now()
            .duration_since(self.last_stamp_wall)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        by_instant.max(by_wall) >= interval
    }

    /// Every site that just wrote a fresh pace file calls this instead of
    /// hand-resetting `ms_since_stamp` — keeps the tick-accounted counter
    /// and the two real-clock anchors (N1) from ever drifting apart.
    fn mark_stamped(&mut self) {
        self.ms_since_stamp = 0;
        self.last_stamp_instant = Instant::now();
        self.last_stamp_wall = SystemTime::now();
    }

    /// The heartbeat cadence: re-stamp at least this often while holding a
    /// pause, well inside `max_pause_ms` so a normal sampler-cadence jitter
    /// (or one slow tick) can never accidentally cross the runtime's
    /// expiry ceiling between two real writes. `.max(1)` guards a
    /// pathological `max_pause_ms` of 0..3 from producing a zero interval
    /// (which would busy-restamp every tick — harmless but wasteful).
    fn restamp_interval_ms(&self) -> u64 {
        (self.config.max_pause_ms / 4).max(1)
    }

    /// Feed one thermal sample. `elapsed_ms` is the wall time since the
    /// previous sample — the sampler's own cadence in production
    /// (`TELEMETRY_SAMPLE_INTERVAL`), injectable here so tests drive a
    /// scripted sequence without real sleeps. `host_out` is the dispatch's
    /// out-dir (pace file lives at `<host_out>/pace.json`); `stop_file`,
    /// when `Some`, is the crawl `STOP` path to drop on breaker (from
    /// [`stop_file_path_from_record_context`]) — `None` for a non-crawl
    /// dispatch, where there is no "further unit" concept to stop.
    ///
    /// Returns the event that fired this tick, if any (pause / resume /
    /// breaker are mutually exclusive per tick — at most one fires). A
    /// periodic heartbeat re-stamp while `Paused`/`Broken` writes the pace
    /// file but is NOT an event (see `ThermalEvent`'s own doc).
    pub fn on_sample(
        &mut self,
        thermal: Option<&ThermalSample>,
        elapsed_ms: u64,
        host_out: &Path,
        stop_file: Option<&Path>,
    ) -> Option<ThermalEvent> {
        if !self.config.enabled {
            return None;
        }

        if self.state == State::Broken {
            // (finding 2, redesigned for the heartbeat contract) Broken is
            // terminal for DECISIONS but must keep re-stamping — without
            // an active writer the runtime's pure expiry rule (#2114
            // cf1b1993: no `expires` opt-out) would silently resume the
            // unit on a still-critical machine.
            self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
            if self.ms_since_stamp >= self.restamp_interval_ms() || self.real_age_past_interval() {
                self.mark_stamped();
                write_pace_file(host_out, true, "thermal-critical", &self.last_known_state);
            }
            return None;
        }

        // (finding 3) A missing OS thermal reading is "time passed, no new
        // information" — NOT evidence of recovery, and NOT nothing. Only
        // matters while actively `Paused`: accumulate the elapsed time into
        // the episode (so `max_pause_ms` escalation still fires on a
        // machine that stays hot through a reading gap), reset the resume
        // hold (a gap is not a continuous hold at/below `resume_at`), and
        // let the periodic heartbeat re-stamp still fire on its own
        // cadence. `Idle` has nothing to accumulate; a `None` there is
        // simply a no-op tick.
        let thermal = match thermal {
            Some(t) => {
                self.last_known_state = t.state.clone();
                t
            }
            None => {
                // (N4 of the #2110/#2109 review) A missing reading is not
                // evidence the CPU is throttled — reset the consecutive
                // low-sample streak unconditionally (Idle included, so a
                // stale streak never survives into a later real reading),
                // matching how the None arm already resets
                // `resume_hold_accum_ms` rather than freezing or extending it.
                self.speed_limit_low_streak = 0;
                if self.state == State::Paused {
                    self.pause_episode_ms = self.pause_episode_ms.saturating_add(elapsed_ms);
                    self.resume_hold_accum_ms = 0;
                    self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
                    if self.pause_episode_ms >= self.config.max_pause_ms {
                        self.state = State::Broken;
                        self.mark_stamped();
                        write_pace_file(host_out, true, "thermal-critical", &self.last_known_state);
                        if let Some(stop) = stop_file {
                            self.last_stop_write_error =
                                write_stop_file(stop, self.stop_owner.as_deref()).err();
                        }
                        return Some(ThermalEvent::Breaker {
                            state: self.last_known_state.clone(),
                        });
                    }
                    if self.ms_since_stamp >= self.restamp_interval_ms() || self.real_age_past_interval() {
                        self.mark_stamped();
                        write_pace_file(host_out, true, "thermal", &self.last_known_state);
                    }
                }
                return None;
            }
        };

        let sev = severity(&thermal.state);
        // (finding 7) The speed-limit floor requires N CONSECUTIVE
        // low samples; a single low reading is common DVFS noise. The
        // `critical` state check is untouched — a discrete OS-reported
        // state trips immediately, same as before.
        if thermal.cpu_speed_limit_pct < self.config.min_cpu_speed_limit_pct {
            self.speed_limit_low_streak = self.speed_limit_low_streak.saturating_add(1);
        } else {
            self.speed_limit_low_streak = 0;
        }
        let is_breaker_condition = sev >= severity("critical")
            || self.speed_limit_low_streak >= self.config.speed_limit_hold_samples.max(1);

        if is_breaker_condition {
            self.state = State::Broken;
            self.mark_stamped();
            write_pace_file(host_out, true, "thermal-critical", &thermal.state);
            if let Some(stop) = stop_file {
                self.last_stop_write_error = write_stop_file(stop, self.stop_owner.as_deref()).err();
            }
            return Some(ThermalEvent::Breaker { state: thermal.state.clone() });
        }

        match self.state {
            State::Idle => {
                if sev >= severity(&self.config.pause_at) {
                    self.state = State::Paused;
                    self.pause_episode_ms = 0;
                    self.resume_hold_accum_ms = 0;
                    self.mark_stamped();
                    write_pace_file(host_out, true, "thermal", &thermal.state);
                    Some(ThermalEvent::Paused { state: thermal.state.clone() })
                } else {
                    None
                }
            }
            State::Paused => {
                self.pause_episode_ms = self.pause_episode_ms.saturating_add(elapsed_ms);
                self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
                if sev <= severity(&self.config.resume_at) {
                    self.resume_hold_accum_ms = self.resume_hold_accum_ms.saturating_add(elapsed_ms);
                } else {
                    // Still hot enough to matter — hysteresis resets: only
                    // a CONTINUOUS hold at/below resume_at counts, so a
                    // state that ticks back up must restart the clock,
                    // not just pause it. Without this reset a state
                    // bouncing right at the threshold (fair, serious,
                    // fair, serious, ...) would eventually cross
                    // `resume_hold_ms` on ACCUMULATED good ticks alone and
                    // clear the pause while the machine is still flapping
                    // hot — exactly the flapping this hysteresis exists to
                    // prevent.
                    self.resume_hold_accum_ms = 0;
                }
                if self.resume_hold_accum_ms >= self.config.resume_hold_ms {
                    self.state = State::Idle;
                    self.pause_episode_ms = 0;
                    self.resume_hold_accum_ms = 0;
                    self.mark_stamped();
                    write_pace_file(host_out, false, "thermal", &thermal.state);
                    return Some(ThermalEvent::Resumed { state: thermal.state.clone() });
                }
                if self.pause_episode_ms >= self.config.max_pause_ms {
                    self.state = State::Broken;
                    self.mark_stamped();
                    write_pace_file(host_out, true, "thermal-critical", &thermal.state);
                    if let Some(stop) = stop_file {
                        self.last_stop_write_error = write_stop_file(stop, self.stop_owner.as_deref()).err();
                    }
                    return Some(ThermalEvent::Breaker { state: thermal.state.clone() });
                }
                // (finding 2, redesigned) Still paused, no transition this
                // tick — heartbeat: re-stamp once the interval elapses so
                // written_at_ms never goes stale mid-episode.
                if self.ms_since_stamp >= self.restamp_interval_ms() || self.real_age_past_interval() {
                    self.mark_stamped();
                    write_pace_file(host_out, true, "thermal", &thermal.state);
                }
                None
            }
            State::Broken => unreachable!("returned early above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(state: &str, cpu_speed_limit_pct: u64) -> ThermalSample {
        ThermalSample { state: state.to_string(), cpu_speed_limit_pct }
    }

    fn cfg() -> ThermalGovernorConfig {
        ThermalGovernorConfig {
            enabled: true,
            pause_at: "serious".to_string(),
            resume_at: "fair".to_string(),
            resume_hold_ms: 60_000,
            max_pause_ms: 900_000,
            min_cpu_speed_limit_pct: 50,
            speed_limit_hold_samples: 3,
        }
    }

    fn read_pace(host_out: &Path) -> serde_json::Value {
        let raw = std::fs::read_to_string(pace_file_path(host_out)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn nominal_never_pauses() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        let ev = gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None);
        assert_eq!(ev, None);
        assert!(!pace_file_path(dir.path()).exists(), "no pace file until a pause fires");
    }

    #[test]
    fn serious_pauses_and_writes_pace_file_with_written_at_ms() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(ev, Some(ThermalEvent::Paused { state: "serious".to_string() }));
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true));
        assert_eq!(pace["reason"], serde_json::json!("thermal"));
        assert_eq!(pace["state"], serde_json::json!("serious"));
        assert!(pace["written_at_ms"].as_u64().unwrap() > 0, "written_at_ms must be stamped");
        assert!(pace.get("expires").is_none(), "expires must never be written (#2114 cf1b1993)");
    }

    #[test]
    fn full_hysteresis_sequence_nominal_serious_fair_hold_nominal() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());

        // nominal: no-op
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);

        // serious: pauses
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Paused { state: "serious".to_string() })
        );

        // fair, held for 60s total in 2s ticks: no resume until the hold
        // completes. 29 ticks * 2000ms = 58000ms < 60000ms hold — still paused.
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        }
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true), "hold not yet complete");

        // 30th tick crosses 60000ms — resumes.
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "fair".to_string() })
        );
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));

        // nominal after resume: no-op, stays idle.
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
    }

    #[test]
    fn a_tick_back_above_resume_at_resets_the_hold_no_flapping() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);

        // 58s of "fair" (just under the 60s hold)...
        for _ in 0..29 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        // ...then one tick back at "serious" — must reset the hold clock,
        // not just pause it.
        assert_eq!(gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None), None);

        // Even 58 more seconds of "fair" must NOT be enough to resume,
        // because the hold restarted at the "serious" tick above — this is
        // the assertion that fails red if the reset (`= 0`, not skip) is
        // removed, proving the hysteresis is load-bearing.
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        }
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "fair".to_string() }),
            "resume only after a FRESH continuous 60s hold"
        );
    }

    // ── #2454: the STOP file is scoped to the run that wrote it ──

    /// (#2454) The breaker stamps its own mission into the STOP file, and
    /// the read side hands that run — and only that run — a stop.
    ///
    /// Load-bearing because NOTHING removes this file: not the retired
    /// launcher (`src/crawl_launch.rs`, which only ever read it), not this
    /// module, not any other code path in the repo. The path is
    /// `<root>/crawl/<manifest>/STOP`, scoped to the WORKSPACE rather than
    /// to a run, so an unattributed machine-written stop would refuse every
    /// unit of every future crawl on that workspace, permanently, from one
    /// transient thermal event.
    #[test]
    fn the_breaker_stamps_its_mission_and_only_that_mission_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("crawl-root").join("STOP");
        let mut gov = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-1"));

        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));
        assert!(stop.exists());

        assert_eq!(
            stop_hold_for_mission(&stop, "crawl-m-1"),
            Some(StopHold::ThisMission),
            "the run whose breaker tripped must be stopped"
        );
        assert_eq!(
            stop_hold_for_mission(&stop, "crawl-m-2"),
            None,
            "a LATER mission must not inherit an older run's thermal stop — nothing deletes this \
             file, so inheriting it bricks the workspace forever"
        );
    }

    /// (#2454) A governor with no mission (a bare `darkmux dispatch`, or a
    /// build older than #2454) writes the unattributed shape, which every
    /// mission honors — a hand-`touch`ed STOP is a hand-removed STOP, and
    /// this module cannot know what a human meant by it.
    #[test]
    fn an_unowned_breaker_writes_a_stop_every_mission_honors() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("crawl-root").join("STOP");
        let mut gov = ThermalGovernor::new(cfg());

        gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(std::fs::read_to_string(&stop).unwrap(), "thermal-critical\n");
        assert_eq!(stop_hold_for_mission(&stop, "anything"), Some(StopHold::Unattributed));
    }

    #[test]
    fn an_absent_or_unreadable_stop_file_is_not_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(stop_hold_for_mission(&dir.path().join("nope"), "m"), None);
        // A DIRECTORY where the file should be: `read_to_string` fails, and
        // failing closed here would brick the workspace on a bad write —
        // exactly the outcome this scoping exists to prevent.
        let as_dir = dir.path().join("STOP");
        std::fs::create_dir_all(&as_dir).unwrap();
        assert_eq!(stop_hold_for_mission(&as_dir, "m"), None);
    }

    #[test]
    fn a_blank_owner_is_the_unattributed_shape_not_a_mission_named_empty() {
        assert_eq!(stop_file_body(Some("   ")), "thermal-critical\n");
        assert_eq!(stop_file_owner("thermal-critical mission=\n"), None);
        assert_eq!(stop_file_owner("thermal-critical mission=m-9\n"), Some("m-9"));
    }

    // ── #2456: the breaker's STOP write refuses a pre-planted symlink ──

    /// (#2456) The record's `cause` must tell the two situations apart.
    /// They share one action and one urgency but NOT one remedy: a
    /// `path_underivable` is a crawl `record_context` bug, a
    /// `write_refused` means something is sitting at the STOP path on
    /// disk. An operator filtering the flow stream must not have to
    /// string-match prose out of `reason` to know which they have.
    #[test]
    fn the_two_stop_unresolved_situations_are_distinguishable_in_the_record() {
        let path = PathBuf::from("/tmp/does-not-matter/STOP");

        // No path derived, derivation had a reason to complain.
        assert_eq!(
            stop_unresolved_cause(None, Some("workspace missing"), None),
            Some((StopUnresolvedCause::PathUnderivable, "workspace missing"))
        );

        // A path WAS derived and the write was refused — a DIFFERENT
        // cause, not the derivation one.
        assert_eq!(
            stop_unresolved_cause(Some(&path), None, Some("refusing to write ... symlink")),
            Some((StopUnresolvedCause::WriteRefused, "refusing to write ... symlink"))
        );

        // The two `cause` strings are distinct and stable — a reader keys
        // on these, so a rename is a record-contract change.
        assert_eq!(StopUnresolvedCause::PathUnderivable.as_str(), "path_underivable");
        assert_eq!(StopUnresolvedCause::WriteRefused.as_str(), "write_refused");
        assert_ne!(
            StopUnresolvedCause::PathUnderivable.as_str(),
            StopUnresolvedCause::WriteRefused.as_str()
        );
    }

    /// (#2456) The quiet cases: nothing to warn about. A successful write
    /// under a derived path, and a non-crawl dispatch (no derived path AND
    /// no derivation complaint) must BOTH stay silent — a warning on every
    /// non-crawl breaker trip would bury the real ones.
    #[test]
    fn a_successful_or_non_crawl_stop_write_warns_about_nothing() {
        let path = PathBuf::from("/tmp/does-not-matter/STOP");
        assert_eq!(stop_unresolved_cause(Some(&path), None, None), None);
        assert_eq!(stop_unresolved_cause(None, None, None), None);
        // A stale derivation reason must NOT be reported once a path was
        // in fact derived, and a stale write error must not be reported
        // when no path was derived — each cause reads only the input that
        // belongs to its own branch.
        assert_eq!(stop_unresolved_cause(Some(&path), Some("stale"), None), None);
        assert_eq!(stop_unresolved_cause(None, None, Some("stale")), None);
    }

    /// (#2456) `<root>/crawl/<name>` — the STOP file's PARENT — planted as
    /// a symlink before the breaker ever trips must not redirect the
    /// write to wherever it points. This is the exact hazard named in the
    /// issue title.
    #[test]
    fn breaker_does_not_follow_a_symlinked_stop_parent() {
        let dir = tempfile::tempdir().unwrap();
        let real_target = dir.path().join("attacker-owned-dir");
        std::fs::create_dir_all(&real_target).unwrap();
        let crawl_dir = dir.path().join("crawl-root");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_target, &crawl_dir).unwrap();
        let stop = crawl_dir.join("STOP");

        let mut gov = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-1"));
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));

        assert!(
            !real_target.join("STOP").exists(),
            "must refuse the symlinked parent, never write through it onto the real target"
        );
        let err = gov
            .last_stop_write_error()
            .expect("a symlinked parent must be a LOUD refusal, not a silent no-op");
        assert!(err.contains("symlink"), "unexpected error: {err}");
    }

    /// (#2456) The STOP file's own path planted directly as a symlink
    /// (parent is a real directory; only the leaf name is hijacked) must
    /// also refuse rather than write through it.
    #[test]
    fn breaker_does_not_follow_a_symlinked_stop_file_itself() {
        let dir = tempfile::tempdir().unwrap();
        let crawl_dir = dir.path().join("crawl-root");
        std::fs::create_dir_all(&crawl_dir).unwrap();
        let attacker_file = dir.path().join("attacker-owned-file");
        std::fs::write(&attacker_file, b"pre-existing\n").unwrap();
        let stop = crawl_dir.join("STOP");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&attacker_file, &stop).unwrap();

        let mut gov = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-1"));
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));

        assert_eq!(
            std::fs::read_to_string(&attacker_file).unwrap(),
            "pre-existing\n",
            "must never write through a symlink planted at the STOP path itself"
        );
        let err = gov
            .last_stop_write_error()
            .expect("a symlinked STOP path must be a LOUD refusal, not a silent replace");
        assert!(err.contains("symlink"), "unexpected error: {err}");
    }

    /// (#2456) A refusal must not be silent: the breaker's write is its
    /// LAST ACTION under thermal duress, so the caller
    /// (`dispatch_internal.rs`) needs a way to know it tried and could
    /// not, in order to warn rather than let the crawl keep dispatching
    /// units past a tripped breaker with no trace of why the STOP never
    /// landed.
    #[test]
    fn a_refused_stop_write_is_surfaced_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let real_target = dir.path().join("attacker-owned-dir");
        std::fs::create_dir_all(&real_target).unwrap();
        let crawl_dir = dir.path().join("crawl-root");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_target, &crawl_dir).unwrap();
        let stop = crawl_dir.join("STOP");

        let mut gov = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-1"));
        assert_eq!(gov.last_stop_write_error(), None, "nothing attempted yet");
        gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        let err = gov.last_stop_write_error().expect("a refused write must be surfaced, not silent");
        assert!(err.contains("symlink"), "unexpected error: {err}");
    }

    /// (#2456) The ordinary, no-symlink path must be unaffected: the
    /// breaker still writes a real STOP file when nothing is in the way,
    /// and a later mission's trip on the SAME workspace still overwrites
    /// it cleanly (the load-bearing re-stamp `stop_file_body` documents —
    /// nothing ever deletes this file).
    #[test]
    fn ordinary_stop_write_still_works_and_a_later_mission_can_overwrite_it() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("crawl-root").join("STOP");

        let mut gov_a = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-1"));
        gov_a.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(gov_a.last_stop_write_error(), None);
        assert_eq!(stop_hold_for_mission(&stop, "crawl-m-1"), Some(StopHold::ThisMission));

        // #2454's reader still works against the new writer, and a
        // second mission's own trip re-stamps the SAME file cleanly.
        let mut gov_b = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-2"));
        gov_b.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(gov_b.last_stop_write_error(), None);
        assert_eq!(
            stop_hold_for_mission(&stop, "crawl-m-2"),
            Some(StopHold::ThisMission),
            "a later mission's own trip must be able to re-stamp the file"
        );
        assert_eq!(
            stop_hold_for_mission(&stop, "crawl-m-1"),
            None,
            "the old owner must no longer be held by the re-stamped file"
        );
    }

    #[test]
    fn serious_held_past_max_pause_trips_the_breaker() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("crawl-root").join("STOP");
        let mut cfg = cfg();
        cfg.max_pause_ms = 10_000;
        let mut gov = ThermalGovernor::new(cfg);

        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));
        // 4 more ticks of "serious" = 8000ms more, total 8000ms < 10000ms.
        for _ in 0..4 {
            let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));
            assert_ne!(ev, Some(ThermalEvent::Breaker { state: "serious".to_string() }));
        }
        assert!(!stop.exists(), "breaker must not fire before max_pause_ms elapses");

        // One more tick crosses 10000ms.
        let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "serious".to_string() }));
        assert!(stop.exists(), "breaker must drop the crawl STOP file");
        let pace = read_pace(dir.path());
        assert_eq!(pace["reason"], serde_json::json!("thermal-critical"));
        assert!(pace.get("expires").is_none());

        // Terminal for decisions: further samples, even nominal, produce
        // no NEW event — but the heartbeat below proves the pace file
        // itself keeps getting re-stamped.
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
    }

    #[test]
    fn critical_trips_the_breaker_immediately_no_hold_needed() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg());
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));
        assert!(stop.exists());
    }

    // ── finding 7: speed-limit breaker needs N consecutive low samples ──

    #[test]
    fn low_cpu_speed_limit_needs_consecutive_samples_before_tripping() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg()); // speed_limit_hold_samples: 3

        // "nominal" state, CPU throttled below the floor — but only ONE
        // sample so far. Must NOT trip yet.
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, None, "a single low sample is noise, not a sustained condition");
        assert!(!stop.exists());

        // A second low sample — still short of the 3-sample hold.
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, None);
        assert!(!stop.exists());

        // A third CONSECUTIVE low sample crosses the hold.
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "nominal".to_string() }));
        assert!(stop.exists(), "breaker must fire on the 3rd consecutive low sample");
    }

    #[test]
    fn low_cpu_speed_limit_streak_resets_on_a_single_good_sample() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg());

        // Two low samples...
        gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        // ...then ONE good sample resets the streak — this is the
        // assertion that fails red if the streak isn't reset on a
        // non-low reading.
        let ev = gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, None);

        // Two MORE low samples (only 2 consecutive since the reset) must
        // still not trip.
        gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, None, "streak restarted after the good sample — only 2 consecutive here");
        assert!(!stop.exists());
    }

    #[test]
    fn zero_speed_limit_hold_samples_does_not_trip_on_the_first_sample() {
        // (N2, final re-check) speed_limit_hold_samples=0 must NOT mean
        // "trip on every sample regardless of reading" — with a naive
        // `streak >= hold_samples` comparison, 0 >= 0 is trivially true
        // even before any low sample is ever seen, tripping the breaker
        // unconditionally forever. Clamped to `.max(1)` at point of use:
        // a configured 0 behaves like 1 (trips on the first REAL low
        // sample), never on a sample that isn't low at all.
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut cfg = cfg();
        cfg.speed_limit_hold_samples = 0;
        let mut gov = ThermalGovernor::new(cfg);

        // Nominal state, CPU NOT throttled — must not trip even with
        // hold_samples=0, proving 0 doesn't mean "always breaker."
        let ev = gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, None, "a non-low reading must never trip the breaker, even with hold_samples=0");
        assert!(!stop.exists());

        // A genuinely low reading DOES trip on the first sample (0 clamped to 1).
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "nominal".to_string() }));
        assert!(stop.exists());
    }

    // ── N4 (final re-check): a None speed-limit reading resets the streak ──

    #[test]
    fn none_reading_resets_the_speed_limit_streak() {
        // (N4) A missing OS reading is not evidence the CPU is throttled
        // — it must reset the consecutive low-sample streak, matching how
        // a None reading already resets resume_hold_accum_ms rather than
        // freezing or (worse) silently preserving progress toward a trip.
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg()); // speed_limit_hold_samples: 3

        // Two low samples...
        gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        // ...then a MISSING reading — must reset the streak, not just
        // leave it frozen at 2 (which would let a single low sample right
        // after the gap complete a 3-in-a-row that was never actually
        // consecutive).
        assert_eq!(gov.on_sample(None, 2000, dir.path(), Some(&stop)), None);

        // Only ONE more low sample after the reset — must NOT trip,
        // because the streak restarted at the None tick.
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, None, "streak restarted after the None tick — only 1 consecutive low sample here");
        assert!(!stop.exists());

        // Two more low samples complete a genuine 3-in-a-row post-reset.
        gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        let ev = gov.on_sample(Some(&sample("nominal", 30)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "nominal".to_string() }));
        assert!(stop.exists());
    }

    #[test]
    fn critical_state_still_trips_immediately_ignoring_the_speed_limit_hold() {
        // The consecutive-sample requirement is scoped to the speed-limit
        // signal only — `critical` is a discrete OS-reported state and
        // must keep tripping on the FIRST sample, same as before finding 7.
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg());
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));
        assert!(stop.exists());
    }

    #[test]
    fn ordinary_pause_and_resume_writes_never_carry_expires() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let pace = read_pace(dir.path());
        assert!(pace.get("expires").is_none(), "ordinary pause must not set expires: {pace}");
    }

    // ── finding 2 (redesigned): heartbeat re-stamp while Paused/Broken ──

    #[test]
    fn paused_re_stamps_written_at_ms_periodically_not_only_on_pause_start() {
        // (#2140 review finding 2, redesigned after #2114 cf1b1993 removed
        // `expires` in favor of a pure heartbeat: EVERY pause, thermal or
        // otherwise, is honored only while written_at_ms stays fresh — so
        // a writer that only stamps once at pause-start and then goes
        // silent for the rest of a long episode would let the runtime
        // expire the pause mid-episode on a machine that never actually
        // cooled.) max_pause_ms=10_000 -> restamp_interval_ms=2_500 (10s/4).
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg();
        cfg.max_pause_ms = 10_000;
        let mut gov = ThermalGovernor::new(cfg);

        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let first_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();

        // 1000ms of "fair-but-not-enough-to-resume" ticks... actually stay
        // at "serious" so no resume/breaker transition fires, and drive
        // exactly to the 2500ms restamp boundary (2000 + 2000 = 4000 >=
        // 2500 crosses it on the 2nd tick after the seed).
        std::thread::sleep(std::time::Duration::from_millis(2));
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None); // ms_since_stamp=2000
        let mid_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        assert_eq!(mid_stamp, first_stamp, "not yet at the 2500ms restamp interval");

        std::thread::sleep(std::time::Duration::from_millis(2));
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None); // ms_since_stamp=4000 >= 2500
        let restamped = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        assert!(
            restamped > first_stamp,
            "written_at_ms must advance once the heartbeat interval elapses, not stay pinned to \
             the pause-start stamp for the whole episode"
        );
    }

    #[test]
    fn broken_re_stamps_written_at_ms_periodically() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut cfg = cfg();
        cfg.max_pause_ms = 10_000; // restamp_interval_ms = 2_500
        let mut gov = ThermalGovernor::new(cfg);

        gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        let first_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(ev, None, "Broken produces no further EVENT — only the heartbeat write");
        let mid_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        assert_eq!(mid_stamp, first_stamp, "not yet at the 2500ms restamp interval");

        std::thread::sleep(std::time::Duration::from_millis(2));
        gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        let restamped = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        assert!(
            restamped > first_stamp,
            "written_at_ms must advance while Broken, not freeze at the trip-time write — a \
             silent writer would let the runtime's heartbeat ceiling expire the stop and resume \
             a still-critical machine"
        );
    }

    // ── N1 (final re-check): real age, not accounted ticks ──

    #[test]
    fn a_single_large_elapsed_ms_tick_re_stamps_within_one_call() {
        // (N1) dispatch_internal.rs now feeds the REAL elapsed time since
        // the last sample, not a hardcoded constant — a slow tick (e.g.
        // `lms ps` blocking up to 30s) reports that real gap as ONE big
        // `elapsed_ms` value on its next call. The existing tick-accounted
        // math must cross the restamp interval from that single value
        // alone, not require several small ticks to accumulate past it.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg();
        cfg.max_pause_ms = 100_000; // restamp_interval_ms = 25_000
        let mut gov = ThermalGovernor::new(cfg);

        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let first_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        // ONE tick reporting a real 40s gap — must cross the 25s interval
        // and re-stamp immediately, within this single call.
        gov.on_sample(Some(&sample("serious", 100)), 40_000, dir.path(), None);
        let restamped = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        assert!(restamped > first_stamp, "a single large elapsed_ms tick must re-stamp within one call");
    }

    #[test]
    fn real_wall_clock_gap_re_stamps_even_if_caller_under_reports_elapsed_ms() {
        // (N1 backstop) The governor's OWN Instant/SystemTime-tracked age
        // since the last write is independent ground truth — even if the
        // `elapsed_ms` ARGUMENT stays tiny (simulating a caller regression
        // that stops measuring real time correctly, or any future caller
        // that doesn't feed real elapsed at all), a genuine wall-clock gap
        // past the heartbeat interval still forces a re-stamp.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = cfg();
        cfg.max_pause_ms = 20; // restamp_interval_ms = (20/4).max(1) = 5
        let mut gov = ThermalGovernor::new(cfg);

        gov.on_sample(Some(&sample("serious", 100)), 1, dir.path(), None);
        let first_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();

        // Real sleep far past the 5ms interval, but the elapsed_ms
        // ARGUMENT stays tiny — tick-accounted math alone (ms_since_stamp
        // += 1) would never cross 5 on this argument.
        std::thread::sleep(std::time::Duration::from_millis(30));
        gov.on_sample(Some(&sample("serious", 100)), 1, dir.path(), None);
        let restamped = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        assert!(
            restamped > first_stamp,
            "real wall-clock age must force a re-stamp even when elapsed_ms under-reports it"
        );
    }

    // ── finding 3: a missing OS thermal reading mid-pause ──

    #[test]
    fn none_reading_while_paused_accumulates_toward_max_pause_and_trips_the_breaker() {
        // (#2140 review finding 3) `let thermal = thermal?;` used to bail
        // out BEFORE touching any accounting on a `None` sample, freezing
        // `pause_episode_ms` for as long as OS thermal readings kept
        // coming back empty — so a machine that stayed hot through a
        // reading gap could pause forever without ever escalating to the
        // breaker. This proves the breaker fires from None-tick elapsed
        // time ALONE, with no intervening real reading.
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut cfg = cfg();
        cfg.max_pause_ms = 10_000;
        let mut gov = ThermalGovernor::new(cfg);

        // Seed with one real "serious" reading — enters Paused.
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));

        // 4 ticks of a MISSING reading = 8000ms more elapsed, total 8000ms
        // — not yet at the 10000ms ceiling.
        for _ in 0..4 {
            let ev = gov.on_sample(None, 2000, dir.path(), Some(&stop));
            assert_eq!(ev, None, "not yet at max_pause_ms");
        }
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true), "still paused through the reading gap");
        assert_eq!(pace["state"], serde_json::json!("serious"), "state carries the last known reading");
        assert!(!stop.exists());

        // One more None tick crosses 10000ms — breaker fires without ever
        // seeing another real reading.
        let ev = gov.on_sample(None, 2000, dir.path(), Some(&stop));
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "serious".to_string() }));
        assert!(stop.exists(), "breaker must fire from accumulated None-tick elapsed time alone");
        let pace = read_pace(dir.path());
        assert_eq!(pace["reason"], serde_json::json!("thermal-critical"));
    }

    #[test]
    fn none_reading_while_paused_resets_the_resume_hold() {
        // A reading gap is NOT evidence of recovery — it must not count
        // toward (or preserve) a continuous resume_at hold, matching the
        // "still hot" reset the Some(above-resume_at) branch already
        // applies.
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);

        // 58s of "fair" (just under the 60s hold)...
        for _ in 0..29 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        // ...then one None tick — must reset the hold clock, same as a
        // tick back above resume_at would.
        assert_eq!(gov.on_sample(None, 2000, dir.path(), None), None);

        // Even 58 more seconds of "fair" must NOT be enough to resume,
        // because the hold restarted at the None tick above.
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        }
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "fair".to_string() }),
            "resume only after a FRESH continuous 60s hold following the None-tick reset"
        );
    }

    #[test]
    fn none_reading_while_idle_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        assert_eq!(gov.on_sample(None, 2000, dir.path(), None), None);
        assert!(!pace_file_path(dir.path()).exists(), "no pace file — Idle has nothing to accumulate");
    }

    // ── finding 1: host/runtime pace-file location + shape conformance ──

    /// (#2140 review finding 1) `thermal_governor.rs`'s `pace_file_path`
    /// and `runtime/src/pace.rs`'s `pace_file_path` must join the SAME
    /// literal file name onto their root — the runtime crate is not a
    /// workspace member and cannot depend on `darkmux-crew` (or vice
    /// versa), so there is no shared type to enforce this at compile time.
    /// This reads the runtime source at test time and asserts the join
    /// literal is still `"pace.json"` — a rename on either side that isn't
    /// mirrored on the other breaks this test instead of silently going
    /// inert (exactly what shipped in the earlier stacked-but-inert state
    /// this finding caught). Also asserts the runtime source no longer
    /// mentions an `expires` field (#2114 cf1b1993's heartbeat redesign) —
    /// if the runtime ever re-adds one, this drifts against the (correct,
    /// unmodified) `GovernorPaceFile` shape above until it's reconciled by
    /// hand, rather than silently mismatching again.
    #[test]
    fn pace_file_path_matches_runtime_out_base() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime_pace_rs = manifest_dir.join("../../runtime/src/pace.rs");
        let source = std::fs::read_to_string(&runtime_pace_rs)
            .unwrap_or_else(|e| panic!("reading {}: {e}", runtime_pace_rs.display()));
        assert!(
            source.contains(r#"out_dir.join("pace.json")"#),
            "runtime/src/pace.rs's pace_file_path must join \"pace.json\" onto its out_dir root \
             to match crates/darkmux-crew/src/thermal_governor.rs's host-side pace_file_path \
             (host_out.join(\"pace.json\")) — got:\n{source}"
        );
        assert!(
            !source.contains("pub expires"),
            "runtime/src/pace.rs must not carry an `expires` field — #2114 cf1b1993 replaced it \
             with a pure heartbeat contract; this crate's GovernorPaceFile intentionally has no \
             `expires` field to match. If the runtime re-adds one, reconcile both sides by hand \
             rather than let this test go silently inert."
        );
        // The host side, asserted the same way for symmetry — if this ever
        // drifts from `host_out.join("pace.json")` the two literals no
        // longer describe the same file even though this crate's own
        // `pace_file_path` still compiles fine.
        assert_eq!(
            pace_file_path(std::path::Path::new("/darkmux-out")),
            std::path::PathBuf::from("/darkmux-out/pace.json")
        );
    }

    #[test]
    fn disabled_never_writes_anything() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut cfg = cfg();
        cfg.enabled = false;
        let mut gov = ThermalGovernor::new(cfg);
        for state in ["serious", "critical", "nominal"] {
            assert_eq!(gov.on_sample(Some(&sample(state, 10)), 2000, dir.path(), Some(&stop)), None);
        }
        assert!(!pace_file_path(dir.path()).exists());
        assert!(!stop.exists());
    }

    #[test]
    fn no_stop_file_target_never_panics_on_breaker() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        // `stop_file: None` — the non-crawl-dispatch case. Breaker still
        // pauses via the pace file; there's simply nothing crawl-shaped to
        // stop.
        let ev = gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), None);
        assert_eq!(ev, Some(ThermalEvent::Breaker { state: "critical".to_string() }));
    }

    // ── stop_file_path_from_record_context ──

    #[serial_test::serial]
    #[test]
    fn stop_file_path_derives_from_crawl_record_context() {
        let ctx = serde_json::json!({
            "workspace": "my-manifest",
            "source": "github",
            "sha": "abc123",
            "rule": "some-rule",
            "rules": ["some-rule"],
            "unit": "unit-1",
        });
        let path = stop_file_path_from_record_context(Some(&ctx)).unwrap();
        assert!(path.ends_with("crawl/my-manifest/STOP"), "{}", path.display());
    }

    #[test]
    fn stop_file_path_none_without_unit_marker() {
        // A record_context that isn't crawl-shaped (no `unit`) must not
        // synthesize a STOP path — avoids a spurious write under an
        // unrelated dispatch's context.
        let ctx = serde_json::json!({ "workspace": "my-manifest" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[test]
    fn stop_file_path_none_when_absent() {
        assert_eq!(stop_file_path_from_record_context(None), None);
    }

    // ── #2157: manifest name must not escape <root>/crawl/ ──

    #[test]
    fn stop_file_path_none_for_absolute_manifest_name() {
        // `PathBuf::join` REPLACES the accumulated path outright when the
        // joined component is absolute — unvalidated, this would return
        // `/etc/passwd/STOP`, discarding `<root>/crawl/` entirely.
        let ctx = serde_json::json!({ "workspace": "/etc/passwd", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[test]
    fn stop_file_path_none_for_dotdot_traversal() {
        let ctx = serde_json::json!({ "workspace": "../../../../etc/passwd", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[test]
    fn stop_file_path_none_for_nested_dotdot_that_only_escapes_after_joining() {
        // A name that looks locally harmless component-by-component but
        // still escapes `<root>/crawl/` once joined and walked.
        let ctx = serde_json::json!({ "workspace": "a/../../b", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[test]
    fn stop_file_path_none_for_bare_separator() {
        let ctx = serde_json::json!({ "workspace": "/", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[test]
    fn stop_file_path_none_for_trailing_separator() {
        let ctx = serde_json::json!({ "workspace": "my-manifest/", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[test]
    fn stop_file_path_none_for_embedded_empty_component() {
        let ctx = serde_json::json!({ "workspace": "a//b", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
    }

    #[serial_test::serial]
    #[test]
    fn stop_file_path_ordinary_manifest_name_still_resolves() {
        // The direction most likely to be broken by an over-eager fix: a
        // realistic manifest name (digits, dot, underscore, hyphen) must
        // still produce the expected path.
        let ctx = serde_json::json!({ "workspace": "crawl_v2.1-final", "unit": "unit-1" });
        let path = stop_file_path_from_record_context(Some(&ctx)).unwrap();
        assert!(path.ends_with("crawl/crawl_v2.1-final/STOP"), "{}", path.display());
    }

    #[test]
    fn stop_file_unresolved_reason_some_when_workspace_is_traversal() {
        let ctx = serde_json::json!({ "workspace": "../escape", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None, "sibling still returns None");
        assert!(
            stop_file_unresolved_reason(Some(&ctx)).is_some(),
            "an invalid manifest name must warn, not silently agree with the non-crawl case"
        );
    }


    // ── #2157 REVIEW: adversarial corpus against the central claim ──

    /// The central claim, asserted STRUCTURALLY rather than case by case:
    /// no `record_context.workspace` value can make the derivation produce
    /// a path outside `<root>/crawl/`.
    ///
    /// Note the containment check is deliberately TWO assertions.
    /// `Path::starts_with` alone is VACUOUS here: it compares components
    /// lexically, so `<root>/crawl/../../etc/STOP` *does* start_with
    /// `<root>/crawl` (components: .., .., etc are simply *after* the
    /// prefix). A containment test written with `starts_with` on its own
    /// would pass for the very traversal it exists to reject — so the
    /// absence of any `ParentDir` component is asserted separately, and
    /// the shape is pinned exactly (`<name>/STOP`, two components past
    /// `<root>/crawl`).
    #[serial_test::serial]
    #[test]
    fn no_workspace_value_can_escape_the_crawl_root() {
        let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root;
        let crawl_root = root.join("crawl");

        let corpus = [
            // absolute / separator smuggling
            "/etc/passwd",
            "/",
            "//",
            "../../../../Users/x/Library/LaunchAgents",
            "..",
            ".",
            "./ok",
            "ok/..",
            "a/../../b",
            "ok/",
            "/ok",
            "a//b",
            // windows-shaped
            r"C:\Windows\System32",
            r"a\b",
            r"..\..\evil",
            // unicode that renders or normalizes like a separator
            "\u{FF0F}etc\u{FF0F}passwd", // FULLWIDTH SOLIDUS
            "a\u{2044}b",                // FRACTION SLASH
            "a\u{2215}b",                // DIVISION SLASH
            "\u{0430}bc",                // CYRILLIC homoglyph 'a'
            "\u{FF41}bc",                // FULLWIDTH LATIN SMALL A
            "caf\u{00E9}",               // ordinary non-ASCII
            "e\u{0301}tude",             // combining acute
            "\u{202E}gnp.exe",           // RTL override
            "\u{FEFF}ok",                // BOM
            // control / whitespace
            "",
            " ",
            "\t",
            "   ",
            " ok ",
            "a\nb",
            "a\0b",
            "\0",
            // dotfiles
            ".hidden",
            "-rf",
            "_scratch",
        ];

        for name in corpus {
            let ctx = serde_json::json!({ "workspace": name, "unit": "u1" });
            let Some(path) = stop_file_path_from_record_context(Some(&ctx)) else {
                continue; // rejected outright — the desired outcome
            };
            assert!(
                path.starts_with(&crawl_root),
                "escaped <root>/crawl: {:?} -> {}",
                name,
                path.display()
            );
            assert!(
                !path.components().any(|c| matches!(c, std::path::Component::ParentDir)),
                "resolved path carries a `..` component: {:?} -> {}",
                name,
                path.display()
            );
            let rest: Vec<_> = path.strip_prefix(&crawl_root).unwrap().components().collect();
            assert_eq!(
                rest.len(),
                2,
                "must be exactly <name>/STOP under <root>/crawl: {:?} -> {}",
                name,
                path.display()
            );
        }
    }


    /// Pins the accepted CHARACTER CLASS exactly, which the containment
    /// corpus above cannot do on its own: a non-ASCII character is not a
    /// containment threat on POSIX (nothing normalizes U+FF0F FULLWIDTH
    /// SOLIDUS into U+002F, and APFS's normalization-insensitivity is
    /// NFC/NFD only), so widening the class to Unicode would leave
    /// `no_workspace_value_can_escape_the_crawl_root` perfectly green.
    /// The ASCII restriction is defense-in-depth against homoglyph
    /// confusion, and it needs its own guard or it can be dropped
    /// silently.
    #[serial_test::serial]
    #[test]
    fn accepted_character_class_is_ascii_only() {
        let sweep = (0u32..=0x2FF)
            .chain([0x2044, 0x2215, 0xFEFF, 0xFF0F, 0xFF41, 0x202E, 0x1D400])
            .filter_map(char::from_u32);
        for c in sweep {
            let want_lead = c.is_ascii_alphanumeric();
            let want_tail = c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');

            let lead = serde_json::json!({ "workspace": format!("{c}x"), "unit": "u1" });
            assert_eq!(
                stop_file_path_from_record_context(Some(&lead)).is_some(),
                want_lead,
                "leading char U+{:04X} ({c:?})",
                c as u32
            );

            let tail = serde_json::json!({ "workspace": format!("x{c}"), "unit": "u1" });
            assert_eq!(
                stop_file_path_from_record_context(Some(&tail)).is_some(),
                want_tail,
                "trailing char U+{:04X} ({c:?})",
                c as u32
            );
        }
    }

    /// The inverse direction: names that are legitimate single components
    /// must still resolve, or the breaker silently stops working for real
    /// crawls. Pins that the validator is not over-broad.
    #[serial_test::serial]
    #[test]
    fn ordinary_names_still_resolve() {
        for name in ["a", "1", "acme", "crawl_v2.1-final", "a..b", "a.", "UPPER-case_9"] {
            let ctx = serde_json::json!({ "workspace": name, "unit": "u1" });
            let path = stop_file_path_from_record_context(Some(&ctx))
                .unwrap_or_else(|| panic!("legitimate name rejected: {name:?}"));
            assert!(path.ends_with(format!("crawl/{name}/STOP")), "{}", path.display());
        }
    }

    /// A very long name is accepted by the character class (the write
    /// itself would fail with ENAMETOOLONG, which `write_stop_file`
    /// already swallows) — pinned so the behavior is deliberate, and to
    /// prove it cannot escape either.
    #[serial_test::serial]
    #[test]
    fn very_long_name_is_contained_even_though_accepted() {
        let name = "a".repeat(4096);
        let ctx = serde_json::json!({ "workspace": name, "unit": "u1" });
        let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root;
        let path = stop_file_path_from_record_context(Some(&ctx)).unwrap();
        assert!(path.starts_with(root.join("crawl")));
    }

    /// The two predicates must never disagree: for EVERY input, the path
    /// resolving is exactly the reason-being-`None` case, on a
    /// crawl-shaped context. This is the drift guard — one predicate is
    /// shared today, and this fails the moment a second one is introduced.
    #[serial_test::serial]
    #[test]
    fn path_and_unresolved_reason_never_disagree() {
        let names = [
            "ok", "", " ", "..", ".", "/etc", "a/b", "a\\b", "caf\u{00E9}", "a..b", "-x", "_x",
            "\u{FF0F}x", "1",
        ];
        for name in names {
            let ctx = serde_json::json!({ "workspace": name, "unit": "u1" });
            let path = stop_file_path_from_record_context(Some(&ctx));
            let reason = stop_file_unresolved_reason(Some(&ctx));
            assert_eq!(
                path.is_some(),
                reason.is_none(),
                "predicates disagree for {name:?}: path={path:?} reason={reason:?}"
            );
        }
    }

    // ── finding 5: distinguishable warning when the STOP path can't be derived ──

    #[test]
    fn stop_file_unresolved_reason_none_for_non_crawl_dispatch() {
        // Not crawl-shaped at all — nothing to warn about, and this must
        // agree with stop_file_path_from_record_context's own None here.
        let ctx = serde_json::json!({ "workspace": "my-manifest" });
        assert_eq!(stop_file_unresolved_reason(Some(&ctx)), None);
        assert_eq!(stop_file_unresolved_reason(None), None);
    }

    #[serial_test::serial]
    #[test]
    fn stop_file_unresolved_reason_none_when_derivation_succeeds() {
        let ctx = serde_json::json!({ "workspace": "my-manifest", "unit": "unit-1" });
        assert!(stop_file_path_from_record_context(Some(&ctx)).is_some());
        assert_eq!(
            stop_file_unresolved_reason(Some(&ctx)),
            None,
            "derivation succeeded — no warning, and the two functions must agree"
        );
    }

    #[test]
    fn stop_file_unresolved_reason_some_when_crawl_shaped_but_workspace_missing() {
        let ctx = serde_json::json!({ "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None, "sibling still returns None");
        assert!(
            stop_file_unresolved_reason(Some(&ctx)).is_some(),
            "but THIS function must distinguish it as a crawl-shaped dispatch with no derivable \
             path, not silently the same as a non-crawl dispatch"
        );
    }

    #[test]
    fn stop_file_unresolved_reason_some_when_workspace_empty() {
        let ctx = serde_json::json!({ "workspace": "   ", "unit": "unit-1" });
        assert_eq!(stop_file_path_from_record_context(Some(&ctx)), None);
        assert!(stop_file_unresolved_reason(Some(&ctx)).is_some());
    }
}

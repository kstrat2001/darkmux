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
//! the operator's call — `darkmux dispatch <role> --resume-from <out-dir>`
//! (wired since #2114's follow-up; see `dispatch_internal.rs`'s
//! `resume_checkpoint` family). Once tripped, the governor goes terminal
//! for DECISIONS — it does not un-pause itself on recovery — but it does
//! NOT go silent: see the heartbeat contract below.
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
//!
//! **The five-tier escalation ladder (#2774, from the operator's own
//! overnight-crawl thermal experience — kstrat2001/darkmux#2774).** The
//! governor/breaker above are tiers 1/3/5's mechanism; this module adds
//! tiers 2 and 4 around them rather than replacing anything:
//!
//! | tier | condition | response |
//! |---|---|---|
//! | 1 | `nominal` | no delay |
//! | 2 | `fair` sustained (`resume_at`, held `resume_hold_ms`) | duty-cycle: pace file `pause: false, turn_delay_ms: Some(_)` |
//! | 3 | `serious` (`pause_at`) | full pause until back to `resume_at`, THEN resume with the duty-cycle delay DOUBLED (`ratchet_factor`) for the rest of the run |
//! | 4 | the Nth `serious` EPISODE (`episode_threshold`, default 2; `0` = unbounded) | indefinite pause (`reason: "thermal-episode-limit"`) — resumes only on operator intervention, never automatically |
//! | 5 | `critical` | the pre-existing breaker (unchanged) — PLUS, in `dispatch_internal.rs`, `swap::eject_all_managed` once a turn boundary is reached or a short bound elapses |
//!
//! **Tier 2 (`DutyCycle`).** Entering and leaving both require a SUSTAINED
//! hold at the boundary (reusing `resume_hold_ms` for both directions,
//! rather than adding a second hold knob for what is the same "how long at
//! `resume_at`" question) — see `on_sample`'s `Idle`/`DutyCycle` handling.
//! `current_duty_delay_ms` starts at `duty_delay_ms` and is what actually
//! rides the pace file; `serious_episodes()`/`current_duty_delay_ms()` are
//! the getters `dispatch_internal.rs` reads to record both in the run
//! artifact alongside `above_nominal_ms` (#2774's own citation of #1247's
//! principle: "was this run throttled, and how hard" answerable from data).
//!
//! **Tier 3's ratchet is ONE-WAY for the life of the run.** Recovering to
//! `resume_at` releases the pause; it never restores the pre-doubling
//! delay — `current_duty_delay_ms` is multiplied by `ratchet_factor` on
//! every successful `Paused` -> resume transition and never divided back
//! down. An episode that goes straight to tier 4 (below) does NOT ratchet
//! — there is no resume to ratchet on.
//!
//! **Tier 4's episode count is a TRANSITION, not a sample.** `serious_episodes`
//! increments exactly once per `Idle`/`DutyCycle` -> `Paused`-or-`OperatorHold`
//! transition (`enter_paused`), never per tick spent AT `serious` — a single
//! sustained stretch at `serious` is one episode, however many samples it
//! spans. When the Nth transition would be reached (`tier4_enabled &&
//! episode_threshold != 0 && serious_episodes >= episode_threshold`), that
//! episode goes straight to `OperatorHold` instead of the ordinary `Paused`
//! tier-3 flow — no ratchet, no automatic resume, ever. `OperatorHold`
//! shares `Broken`'s heartbeat-forever mechanics (re-stamps on the same
//! cadence, terminal for decisions) with its own `reason` string so a flow
//! reader can tell "the count escalated" from "the machine got critical."
//! (#2774 round-3 C8) That reason now also reaches the crawl `STOP` file
//! tier 4 drops — the artifact an operator opens first, which used to say
//! `thermal-critical` regardless, so the two artifacts of one event
//! disagreed about what the event was.
//!
//! **The soft tiers need a usable BAND, and refuse to run without one.**
//! Tiers 2/3/4 all key off `pause_at`/`resume_at`, and each needs a band of
//! readings that is both inhabited (something can be in it) and proper
//! (something the tier can actually see is outside it — otherwise whatever
//! the complement gates, such as leaving a duty cycle or clearing a pause,
//! is unreachable). `ThermalGovernor::new` resolves all three bands once,
//! through [`crate::thermal_bands::ThermalBands`], whose only band
//! constructor REFUSES a degenerate one. A tier whose band was refused does
//! not run, and says why ([`ThermalGovernor::disarm_notes`] — rendered
//! identically by the dispatch-start warning and `darkmux doctor`, off that
//! one value). The BREAKER compares against its own thresholds and is
//! unaffected. See the `thermal_bands` module doc for the three shipped
//! defects — one per review round, each introduced by the previous round's
//! fix — that produced this design (#2774 rounds 1-4).
//!
//! Two consequences worth knowing before reading the tier table above,
//! because the table's own rows would otherwise imply these configs work:
//!
//! - **`resume_at = "nominal"` disarms tier 2.** Every reading below
//!   `pause_at` would be inside the duty band, so the duty cycle could be
//!   entered and never exited — measured at 900 samples of `nominal`
//!   yielding a permanent, ratcheting 15s -> 300s per-turn delay on a cold
//!   machine. Tier 1's own row above (`nominal` | no delay) is the
//!   contradiction that made it visible.
//! - **`pause_at = "critical"` disarms tiers 3 and 4.** The breaker owns
//!   that reading and fires first, so the pause ladder's entry band is
//!   empty over the readings a soft tier can ever be handed.

use crate::host_probe::ThermalSample;
use crate::thermal_bands::{SoftReading, ThermalBands};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

// (#2774 round-4) There is deliberately NO `severity(&str) -> usize` in this
// module any more. Every soft tier reads its band off [`ThermalBands`]
// instead, because three of the four review rounds on #2774 each shipped a
// defect of exactly one shape: a hand-written comparison against a raw
// severity rank whose band turned out to be unsatisfiable or tautological.
// A raw rank is the thing that made those writable; `SoftReading` +
// `Band::contains` is what replaced it. See `thermal_bands`' module doc.
//
// The one comparison that is NOT a band is the breaker's, and it does not
// need one: `SoftReading::of` returns `None` for exactly the readings the
// breaker owns (`critical`, and any name this build does not recognize), so
// `on_sample` matches on that `None` rather than ranking anything.

/// `<host_out>/pace.json`, re-exported from [`crate::pace_file`] — this
/// module was the pace file's only writer until #2706 added the battery
/// governor, at which point the path formula, the wire shape and the
/// atomic write moved to a module both can share. Re-exported rather than
/// relocated at every call site so the name this module has always
/// published (`thermal_governor::pace_file_path`) keeps resolving, and the
/// conformance test below keeps testing the literal the runtime joins.
pub use crate::pace_file::path as pace_file_path;


/// Write the pace file for a THERMAL reason. A thin alias over
/// [`crate::pace_file::write`] kept so this module's nine call sites read
/// unchanged; the atomicity, the heartbeat stamp and the wire shape all
/// live in that module now (see its doc for why sharing them is the point).
fn write_pace_file(host_out: &Path, pause: bool, reason: &'static str, state: &str) {
    crate::pace_file::write(host_out, pause, reason, state)
}

/// (#2774 tier 2) Same as [`write_pace_file`], plus the pace file's third
/// state — a host-set turn delay for a duty-cycle instruction
/// (`pause: false`, `turn_delay_ms: Some(ms)`).
fn write_pace_file_with_delay(host_out: &Path, reason: &'static str, state: &str, turn_delay_ms: u64) {
    crate::pace_file::write_with_turn_delay(host_out, false, reason, state, Some(turn_delay_ms))
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
    Some(crawl_root_from_record_context(record_context)?.join("STOP"))
}

/// (#2774 review F5) Same derivation [`stop_file_path_from_record_context`]
/// uses, for a DIFFERENT sentinel: `<root>/crawl/<manifest>/thermal-ladder.json`
/// — the mission-scoped ladder state (episode count + live duty-cycle
/// delay) a NEW dispatch's governor seeds itself from, so "for the rest of
/// the run" means the whole crawl mission (which constructs one governor
/// PER UNIT dispatch) rather than resetting every time. Same gaps as the
/// STOP-file derivation (an explicit `root:` override isn't
/// reconstructable from `record_context` alone) for the identical reason:
/// this function has no signal to tell "default root" from "override."
pub fn ladder_state_file_path_from_record_context(
    record_context: Option<&serde_json::Value>,
) -> Option<PathBuf> {
    Some(crawl_root_from_record_context(record_context)?.join("thermal-ladder.json"))
}

/// (#2774 review F5, extracted from [`stop_file_path_from_record_context`])
/// `<darkmux root>/crawl/<manifest_name>` — the one crawl-workspace
/// derivation both the STOP file and the ladder-state file build their own
/// filename onto. See [`stop_file_path_from_record_context`]'s own doc for
/// the full reasoning (the "must not depend on the crawl module" boundary,
/// the `root:`-override gap, and the manifest-name validation).
fn crawl_root_from_record_context(record_context: Option<&serde_json::Value>) -> Option<PathBuf> {
    let ctx = record_context?.as_object()?;
    if !ctx.contains_key("unit") {
        return None;
    }
    let manifest_name = ctx.get("workspace")?.as_str()?;
    if !valid_crawl_manifest_name(manifest_name) {
        return None;
    }
    let root = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto).root;
    Some(root.join("crawl").join(manifest_name))
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
///
/// (#2774 round-3 C8) `reason` is a PARAMETER, not the fixed
/// [`STOP_FILE_REASON`] this used to hard-code. The module doc's stated
/// purpose for tier 4 carrying its own reason — "so a reader can tell a
/// count-based escalation from a hardware-critical one" — was failing for
/// the artifact an operator `cat`s FIRST: tier 4 wrote
/// `reason: "thermal-episode-limit"` into the pace file and then a STOP
/// file that said `thermal-critical`, i.e. the two artifacts of one event
/// disagreed about what the event was. Pass the SAME string both writes
/// use; see [`STOP_FILE_REASON_EPISODE_LIMIT`].
fn write_stop_file(stop_file: &Path, owner: Option<&str>, reason: &str) -> Result<(), String> {
    crate::exclusive_fs::write_file_refusing_symlinks_0600(
        stop_file,
        stop_file_body(owner, reason).as_bytes(),
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
fn stop_file_body(owner: Option<&str>, reason: &str) -> String {
    match owner.map(str::trim).filter(|m| !m.is_empty()) {
        // Deliberately one greppable line rather than JSON: an operator
        // finding this file wants `cat` to answer "what is this and who
        // left it", and the reader below only needs the one field.
        Some(mission) => format!("{reason} mission={mission}\n"),
        None => format!("{reason}\n"),
    }
}

/// The STOP file's reason word for a BREAKER trip — the same string the
/// pace file carries as `reason` on that trip, so the two artifacts of one
/// event read alike.
pub const STOP_FILE_REASON: &str = "thermal-critical";

/// (#2774 round-3 C8) The STOP file's reason word for a TIER-4 escalation
/// — the same string the pace file carries as `reason` on that hold, for
/// exactly the reason [`STOP_FILE_REASON`] exists. Tier 4 is a COUNT-based
/// escalation ("this machine has had N `serious` episodes"), not a
/// hardware-critical one; an operator reading `thermal-critical` in the
/// STOP file of a machine that never reported `critical` would be reading
/// the wrong story off the first artifact they open.
pub const STOP_FILE_REASON_EPISODE_LIMIT: &str = "thermal-episode-limit";

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

/// (#2774 round-4 C2) One STOP file, read: whose stop it is, and WHAT the
/// writer said happened.
///
/// Round 3's C8 fixed the WRITER — tier 4 stopped stamping
/// `thermal-critical` into a file it dropped for a count-based escalation —
/// but the only READER still split the body for the `mission=` token and
/// threw the rest away. So the same artifact disagreement C8 was filed to
/// close simply moved one consumer out: a tier-4 hold printed "the thermal
/// breaker's STOP file is present (#2109)" and stamped
/// `UnitOutcome.reason = "thermal breaker tripped (#2109) — …"` on a
/// machine that never reported `critical`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopFileHold {
    /// Whose stop this is — see [`StopHold`].
    pub scope: StopHold,
    /// The reason word the writer recorded ([`STOP_FILE_REASON`],
    /// [`STOP_FILE_REASON_EPISODE_LIMIT`], or something a future writer or
    /// a human put there). `None` for a body carrying none — a human's bare
    /// `touch`, or a token this reader will not render (see
    /// [`stop_file_reason`]).
    pub reason: Option<String>,
}

impl StopFileHold {
    /// What this stop says happened, in a clause that fits mid-sentence.
    /// The ONE place a reason word becomes operator-facing prose, so a
    /// consumer cannot re-describe a tier-4 hold as a breaker trip by
    /// writing its own message — which is exactly what C2 found.
    pub fn what_happened(&self) -> String {
        match self.reason.as_deref() {
            Some(STOP_FILE_REASON) => "the thermal breaker tripped (#2109)".to_string(),
            Some(STOP_FILE_REASON_EPISODE_LIMIT) => {
                "the thermal ladder's episode-count hold escalated (#2774 tier 4)".to_string()
            }
            Some(other) => format!("a thermal stop was recorded with reason `{other}`"),
            None => "a stop was recorded with no reason".to_string(),
        }
    }
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
pub fn stop_hold_for_mission(stop_file: &Path, mission_id: &str) -> Option<StopFileHold> {
    let body = std::fs::read_to_string(stop_file).ok()?;
    let scope = match stop_file_owner(&body) {
        None => StopHold::Unattributed,
        Some(owner) if owner == mission_id.trim() => StopHold::ThisMission,
        Some(_) => return None,
    };
    Some(StopFileHold { scope, reason: stop_file_reason(&body).map(str::to_string) })
}

/// (#2774 round-4 C2) The reason word of a STOP file's body: the first
/// whitespace-separated token that is not the `mission=` field.
///
/// **Bounded and charset-restricted deliberately.** This value comes off
/// disk and ends up in a terminal line, and a byte-bounded value rendered
/// in an indented row is how a forged line gets built (a long token wraps
/// and the continuation looks like darkmux's own output). A token that is
/// not short lowercase-ASCII-plus-dash reads as `None` — "a stop with no
/// reason" — rather than being echoed: an unrecognized reason changes
/// nothing about honoring the stop, so there is nothing to gain by
/// rendering it and a forgery surface to lose.
fn stop_file_reason(body: &str) -> Option<&str> {
    body.split_whitespace()
        .find(|tok| !tok.starts_with("mission="))
        .filter(|tok| {
            tok.len() <= 32 && tok.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
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
    /// (#2774 tier 2) Entered the duty-cycle state after a sustained hold
    /// at/above `resume_at` (and below `pause_at`) — the pace file now
    /// carries `pause: false, turn_delay_ms: Some(delay_ms)`, where
    /// `delay_ms` is the CURRENT (possibly already-ratcheted)
    /// `current_duty_delay_ms`.
    DutyCycleEntered { state: String, delay_ms: u64 },
    /// (#2774 tier 2) Left the duty-cycle state after a sustained hold
    /// back at nominal — the pace file now carries a plain
    /// `pause: false` with no `turn_delay_ms`.
    DutyCycleExited { state: String },
    /// (#2774 tier 4) The Nth `serious` EPISODE (`episode_threshold`) —
    /// escalated straight to an indefinite, operator-gated pause
    /// (`reason: "thermal-episode-limit"`) instead of the ordinary tier-3
    /// pause/resume. `episode` is the 1-based count that triggered this.
    OperatorHold { state: String, episode: u32 },
}

/// (#2774) The two artifact-worthy numbers a governor accumulates over its
/// lifetime — see [`ThermalGovernor::ladder_summary`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThermalLadderSummary {
    /// Count of `serious` EPISODES (transitions, not samples) this run saw.
    pub serious_episodes: u32,
    /// The LIVE (possibly-ratcheted) duty-cycle delay at the end of this
    /// governor's life — `config.duty_delay_ms` if tier 2/3 never engaged.
    pub current_duty_delay_ms: u64,
}

/// (#2774 review F5) The on-disk shape of
/// `<crawl-root>/thermal-ladder.json` — the mission-scoped ladder state a
/// NEW dispatch's governor seeds itself from (see
/// [`ThermalGovernor::seeded_from_mission`]).
///
/// It is [`ThermalLadderSummary`] plus an `owner`, and the owner is the
/// load-bearing half. The file lives at a path keyed on the crawl
/// MANIFEST NAME, not on the mission — exactly like the `STOP` file next
/// to it — so re-crawling the same workspace next week lands on the same
/// file, and nothing removes it in between. Seeding from it unconditionally
/// would let a previous run's episode count escalate a brand-new mission
/// straight into a terminal `OperatorHold` on its FIRST `serious` episode.
/// So the owner is stamped in and compared, the same reason and the same
/// shape as `stop_file_body`'s own owner line (#2454).
///
/// A mismatched (or absent) owner reads as "not my state": the governor
/// starts fresh and its first persist overwrites the stale file. Seeding
/// requires BOTH sides to be `Some` and equal — an unattributed run
/// (no `mission_id`, e.g. a bare `darkmux dispatch`) can never match
/// another unattributed run's leftovers, and degrades to the pre-F5
/// per-dispatch scoping rather than to a wrong carry-forward.
///
/// (#2774 round-3 C10) Carries a `schema_version`, like every other
/// darkmux persisted shape (CLAUDE.md cross-system contract 5). The reader
/// is lenient — a file written before this field existed, or by a NEWER
/// binary with a higher version, still seeds, because every field is
/// optional-on-read and the shape has only ever grown. The version is here
/// so a future BREAKING change (a retyped or removed field) has something
/// to gate on; adding it while the reader is lenient costs nothing, and
/// adding it after a breaking change is impossible without stranding every
/// file already on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedLadderState {
    /// (#2774 round-3 C10) `LADDER_STATE_SCHEMA_VERSION` at write time;
    /// `None` for a file written before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schema_version: Option<u32>,
    /// The mission that wrote this state (`ThermalGovernor::stop_owner`).
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    serious_episodes: u32,
    #[serde(default)]
    current_duty_delay_ms: u64,
}

/// (#2774 round-3 C10) The ladder-state file's data-shape version. Bump
/// on a BREAKING change to [`PersistedLadderState`] (a field renamed,
/// retyped or removed); a purely additive optional field does not need
/// one, since the reader is lenient by construction.
const LADDER_STATE_SCHEMA_VERSION: u32 = 1;

/// (#2774 round-3 C12) Read the ladder-state file, refusing to follow a
/// SYMLINK at the final path component.
///
/// The threat model is the one `write_stop_file` already documents and
/// #2456 already accepted for the STOP file next to this one: reaching it
/// needs local write access to `<root>/crawl/<manifest>/` in the first
/// place. It is closed anyway because it is one `symlink_metadata` call,
/// and because the two files sit in the same directory with the same
/// lifetime — leaving one guarded and the other not is the kind of
/// asymmetry a later reader has to re-derive.
///
/// `None` for every not-a-regular-file case (absent, a symlink, a
/// DIRECTORY, unreadable), which is exactly what both callers already
/// treat as "no prior state."
fn read_ladder_state_refusing_symlinks(path: &Path) -> Option<String> {
    // `symlink_metadata` does NOT traverse the final component, so a
    // symlink reports as a symlink rather than as whatever it points at.
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    std::fs::read_to_string(path).ok()
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
    /// (#2774 tier 2) Base duty-cycle turn delay (ms) — the value
    /// `current_duty_delay_ms` starts at and the ratchet (below) grows
    /// from. Never itself mutated; `ThermalGovernor` tracks the live,
    /// possibly-ratcheted value separately so a governor can be
    /// re-inspected against its own unmodified config.
    pub duty_delay_ms: u64,
    /// (#2774 tier 3) Multiplier applied to the duty-cycle delay on every
    /// `serious`-episode recovery. `.max(1)` at the call site — a `0`
    /// would defeat the ratchet by zeroing the delay on first escalation.
    pub ratchet_factor: u32,
    /// (#2774 tier 4) How many `serious` EPISODES (transitions, not
    /// samples) this run tolerates before the Nth escalates to
    /// `OperatorHold`. `0` means unbounded — never escalate.
    pub episode_threshold: u32,
    /// (#2774 tier 4) Whether the episode-count escalation is active at
    /// all. `false` makes every episode an ordinary tier-3 pause/resume,
    /// regardless of `episode_threshold`.
    pub tier4_enabled: bool,
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
            duty_delay_ms: darkmux_types::config_access::thermal_duty_delay_ms(),
            ratchet_factor: darkmux_types::config_access::thermal_ratchet_factor(),
            episode_threshold: darkmux_types::config_access::thermal_episode_threshold(),
            tier4_enabled: darkmux_types::config_access::thermal_tier4_enabled(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// (#2774 tier 2) Sustained at/above `resume_at` and below `pause_at` —
    /// the pace file carries a host-set `turn_delay_ms` rather than a
    /// pause.
    DutyCycle,
    Paused,
    /// (#2774 tier 4) The Nth `serious` episode — terminal for DECISIONS
    /// exactly like `Broken` (heartbeats forever, never auto-resumes), but
    /// with its OWN `reason` (`"thermal-episode-limit"`) so a flow reader
    /// can tell a count-based escalation from a hardware-critical one.
    OperatorHold,
    /// Terminal for DECISIONS: the breaker tripped and the governor never
    /// un-pauses itself on recovery — resume is out-of-band, the
    /// operator's own `--resume-from` call (see the module doc). NOT
    /// terminal for the pace file's freshness: see the module doc's
    /// heartbeat contract — `on_sample` keeps re-stamping while `Broken`,
    /// just as while `Paused`.
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
    /// (#2774 tier 2) ms the state has continuously held in the OPPOSITE
    /// band from the current `Idle`/`DutyCycle` state — while `Idle`, ms
    /// held at/above `resume_at` (candidate to ENTER duty-cycle); while
    /// `DutyCycle`, ms held below `resume_at` (candidate to LEAVE it).
    /// Resets to 0 the instant the sample falls back into the "current"
    /// band, same hysteresis shape `resume_hold_accum_ms` uses for
    /// `Paused` -> resume. Meaningless (and left untouched) in any other
    /// state.
    duty_hold_accum_ms: u64,
    /// (#2774 tier 3) The LIVE duty-cycle delay this governor applies —
    /// starts at `config.duty_delay_ms` and is multiplied by
    /// `config.ratchet_factor` on every `Paused` -> resume transition,
    /// ONE-WAY for the life of this governor. Never reset by a return to
    /// `Idle`/nominal.
    current_duty_delay_ms: u64,
    /// (#2774 tier 4) Count of `serious` EPISODES this governor has seen —
    /// incremented exactly once per `Idle`/`DutyCycle` -> `Paused`-or-
    /// `OperatorHold` TRANSITION (never per sample spent at `serious`).
    /// See [`ThermalGovernor::serious_episodes`].
    serious_episodes: u32,
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
    /// (#2774 review F5) Where `serious_episodes`/`current_duty_delay_ms`
    /// persist ACROSS dispatches in the same mission — see
    /// [`ThermalGovernor::seeded_from_mission`]. `None` for a dispatch with
    /// no mission-scoped ladder file (a bare `darkmux dispatch`, or
    /// derivation failure), which keeps the pre-F5 behavior: state scoped
    /// to this one governor's lifetime only.
    ladder_state_file: Option<PathBuf>,
    /// (#2774 round-3 MF1, rebuilt round-4) The SOFT tiers' severity bands
    /// — tier 2's duty band, tiers 3/4's pause-entry and recovery pair —
    /// resolved ONCE at construction from `pause_at`/`resume_at`. A tier
    /// whose band could not be built (unsatisfiable, or tautological, so
    /// its complement would be unreachable) is `None` here and does not
    /// run. See [`ThermalGovernor::soft_tiers_armed`] and the
    /// [`crate::thermal_bands`] module doc.
    bands: ThermalBands,
    /// (#2774 round-3 C12) Set once a `persist_ladder_state` failure has
    /// been reported, so a failure that repeats every episode (the ladder
    /// path being a DIRECTORY, say) says so ONCE rather than either
    /// staying silent for a whole mission or printing on every episode.
    ladder_persist_error_reported: bool,
}

impl ThermalGovernor {
    pub fn new(config: ThermalGovernorConfig) -> Self {
        let current_duty_delay_ms = config.duty_delay_ms;
        // (#2774 round-3 MF1, rebuilt round-4) Decided ONCE, here, from the
        // config alone — never re-derived per sample, so no sample-path
        // predicate can disagree with the arming decision. Round 3 made
        // that decision a single `bool` from one pair comparison; round 4
        // found the pair comparison says nothing about whether either
        // tier's own band is inhabited, so it is now a value that carries
        // each tier's band and refuses to build a degenerate one.
        let bands = ThermalBands::resolve(&config.pause_at, &config.resume_at);
        Self {
            config,
            state: State::Idle,
            pause_episode_ms: 0,
            resume_hold_accum_ms: 0,
            last_known_state: String::new(),
            duty_hold_accum_ms: 0,
            current_duty_delay_ms,
            serious_episodes: 0,
            ms_since_stamp: 0,
            speed_limit_low_streak: 0,
            last_stamp_instant: Instant::now(),
            last_stamp_wall: SystemTime::now(),
            stop_owner: None,
            last_stop_write_error: None,
            ladder_state_file: None,
            bands,
            ladder_persist_error_reported: false,
        }
    }

    /// (#2774 round-3 MF1) Whether ANY soft tier runs at all on this
    /// governor: tier 2 (duty-cycle), tier 3 (pause/resume) and tier 4
    /// (operator hold). `false` when no tier's band could be built.
    ///
    /// **(#2774 round-4) This is now a summary, not the decision.** Arming
    /// is PER TIER and lives in [`crate::thermal_bands::ThermalBands`]: a
    /// tier runs iff its band survived construction, and a band survives
    /// only when at least one reading is inside it and at least one reading
    /// the tier can actually see is outside it. So `true` here means "some
    /// tier is armed", never "every tier is". The consumers that want the
    /// specifics read [`ThermalGovernor::disarm_notes`] — `darkmux doctor`
    /// and the dispatch-start warning both render those, off this one
    /// value, so they cannot disagree about what the ladder will do.
    ///
    /// **Why disarm rather than patch the predicate.** Three rounds of this
    /// review each produced a defect from the same root, and each was a run
    /// DYING on a machine that was never in trouble:
    ///
    /// - Round 1's shape: one unchanging reading satisfied BOTH "enter
    ///   `pause_at`" and "resume to `resume_at`", so the governor cycled,
    ///   manufacturing a fresh EPISODE every `resume_hold_ms` and reaching
    ///   tier 4's terminal `OperatorHold` in about a minute.
    /// - Round 2's fix added "and strictly milder than `pause_at`" to the
    ///   recovery predicate, which for `pause_at = "nominal"` demanded
    ///   `sev < 0` on a `usize` — unsatisfiable. Entry (`sev >= 0`) was
    ///   always true. So the governor paused on its FIRST sample, could
    ///   never leave, and handed off to the breaker at `max_pause_ms`:
    ///   `pause: true, reason: "thermal-critical"` plus a crawl `STOP`
    ///   file, after ~15 minutes of an unchanging `nominal` reading. Even
    ///   for the `fair`/`fair` case that fix was written for, the end
    ///   state became the BREAKER (labeled `thermal-critical` on evidence
    ///   that only ever said `fair`) rather than the operator-gated hold.
    /// - Round 3's fix — the pair comparison this method used to BE —
    ///   caught both of those and nothing else, because it never looked at
    ///   either threshold against the enum's ends. Round 4 found
    ///   `resume_at = "nominal"` making tier 2's `sev >= 0` a tautology, so
    ///   `DutyCycle` could be entered and never exited: a cold machine
    ///   picked up a permanent 15s/turn delay that ratcheted to 300s.
    ///
    /// Each attempt tried to give a degenerate config some SAFE behavior.
    /// There isn't one: when a band covers every reading or none, the
    /// ladder is acting on evidence it does not have, whichever way the
    /// tie is broken. Disarming says that plainly — the affected tier does
    /// nothing, the operator is told at dispatch start, and `darkmux
    /// doctor` names the fix.
    ///
    /// **What stays armed: the breaker.** An OS-reported `critical` state
    /// and the sustained `cpu_speed_limit_pct` floor are compared against
    /// their OWN thresholds, not against `pause_at`/`resume_at`, so they
    /// are unaffected by an incoherent pair and keep running. The machine
    /// is not left unprotected against the hardware-danger signal; it
    /// loses only the graduated soft response it could not have coherently
    /// received anyway.
    pub fn soft_tiers_armed(&self) -> bool {
        self.bands.any_armed()
    }

    /// (#2774 round-4) Why any tier that will not run on this config is
    /// disarmed, each with the `darkmux config set` line that fixes it.
    /// Empty when the whole ladder is armed.
    ///
    /// This is the ONE source both operator-facing surfaces render — the
    /// dispatch-start warning (`dispatch_internal.rs`) and `darkmux
    /// doctor`'s thermal check. Round 3 had each surface derive its own
    /// verdict from the raw thresholds, which is how doctor came to report
    /// **Pass** on the config round 4 proved wedges a cold machine into a
    /// permanent duty cycle.
    pub fn disarm_notes(&self) -> &[crate::thermal_bands::DisarmNote] {
        self.bands.disarm_notes()
    }

    /// (#2774 review F5) Seed `serious_episodes`/`current_duty_delay_ms`
    /// from a PRIOR dispatch's mission-scoped ladder state, and arrange to
    /// persist this governor's own updates back to the SAME file — so
    /// "the ratchet doubles for the rest of the run" and "the Nth
    /// `serious` episode" mean the whole crawl MISSION (which constructs
    /// one governor per unit dispatch), not just this one dispatch's own
    /// short lifetime. Builder form because `ThermalGovernor::new` is
    /// called from every test in this module with no mission context;
    /// only the live dispatch path (`dispatch_internal.rs`) has a
    /// [`ladder_state_file_path_from_record_context`] to give.
    ///
    /// `None` (a bare `darkmux dispatch`, or a crawl unit whose
    /// derivation failed) keeps this governor scoped to its own
    /// lifetime — the pre-F5 behavior, unchanged.
    ///
    /// A missing, malformed, or FOREIGN-OWNED state file reads as "no
    /// prior state" (starts fresh at episode 0 / the configured base
    /// delay) rather than an error — the overwhelmingly common cases are
    /// the FIRST unit of a mission, where nothing has been written yet,
    /// and a re-crawl of a workspace some EARLIER mission left a file
    /// under. See [`PersistedLadderState`] for why the owner check is the
    /// load-bearing half of this.
    ///
    /// Call AFTER [`Self::owned_by`] — the owner comparison reads
    /// `self.stop_owner`, so seeding before it is stamped can only ever
    /// fail to match. The one live call site
    /// (`dispatch_internal::run_telemetry_sampler`) chains them in that
    /// order, and `seeding_before_owned_by_never_matches` pins the
    /// consequence.
    #[must_use]
    pub fn seeded_from_mission(mut self, ladder_state_file: Option<&Path>) -> Self {
        let Some(path) = ladder_state_file else { return self };
        if let Some(raw) = read_ladder_state_refusing_symlinks(path) {
            if let Ok(prior) = serde_json::from_str::<PersistedLadderState>(&raw) {
                // Both sides must be `Some` and equal. `None == None` is
                // deliberately NOT a match: see [`PersistedLadderState`].
                let mine_owns_it = match (&self.stop_owner, &prior.owner) {
                    (Some(mine), Some(theirs)) => mine == theirs,
                    _ => false,
                };
                if mine_owns_it {
                    self.serious_episodes = prior.serious_episodes;
                    // Never seed BELOW the configured base. The ratchet
                    // only ever grows (`enter_paused`'s own invariant), so
                    // a prior value smaller than `duty_delay_ms` can only
                    // come from a hand edit or a config change mid-mission
                    // — and a `0` there would silently disable tier 2 for
                    // every remaining unit.
                    self.current_duty_delay_ms =
                        prior.current_duty_delay_ms.max(self.config.duty_delay_ms);
                }
            }
        }
        self.ladder_state_file = Some(path.to_path_buf());
        self
    }

    /// (#2774 review F5) Write the current `serious_episodes`/
    /// `current_duty_delay_ms` to [`Self::ladder_state_file`], if this
    /// governor has one — called right after every mutation of either
    /// field so the NEXT unit's governor (`seeded_from_mission`) always
    /// reads a value at least as current as this dispatch's last state
    /// change. Atomic (tmp file + rename, the pattern
    /// `exclusive_fs::write_file_refusing_symlinks_0600` implements and
    /// `pace_file`'s own writer shares) so a reader mid-write never
    /// observes a truncated file.
    ///
    /// Best-effort like every other sampler-thread side effect in this
    /// crate: a write failure here loses cross-dispatch carry-forward for
    /// this one change, never the dispatch itself.
    ///
    /// (#2774 round-3 C6) **Read-modify-MAX, not blind overwrite.** An
    /// earlier revision wrote an ABSOLUTE snapshot and described the
    /// clobber window as "two units racing through this at the same
    /// instant." That was wrong about the window, which is a LIFETIME, not
    /// an instant: a governor seeds ONCE at construction and every later
    /// write is absolute, so a second unit constructed early and still
    /// alive would, on its FIRST episode, write `{episodes: 1, delay:
    /// base}` over a first unit's `{episodes: 3, delay: 8x base}` — the
    /// mission counter going BACKWARDS and the ratchet discarded. A crawl
    /// with two rules on different model identifiers has exactly that
    /// concurrency.
    ///
    /// Both persisted fields are MONOTONIC by their own definitions (an
    /// episode count only counts up; the ratchet is one-way — "multiplied,
    /// never divided" is `on_sample`'s own invariant), so taking the max
    /// against whatever is on disk right now IS the correct merge, and it
    /// makes the write order-independent. Still not a compare-and-swap:
    /// two writers can interleave read/write and one max can be computed
    /// against a stale read, which loses an INCREMENT at worst. It can no
    /// longer go backwards, which is the failure that was observed.
    ///
    /// The write itself refuses a symlink at the final path
    /// (`exclusive_fs::write_file_refusing_symlinks_0600`), the same guard
    /// `write_stop_file` uses for the STOP file next to it.
    fn persist_ladder_state(&mut self) {
        let Some(path) = self.ladder_state_file.clone() else { return };
        // Read-modify-max against whatever is on disk RIGHT NOW — but only
        // against state this same mission wrote. A FOREIGN owner's file is
        // "not my state" on the read side (`seeded_from_mission`), and
        // must not raise our counters here either, or a stale file from
        // last week's crawl would escalate this mission by the back door.
        let prior = read_ladder_state_refusing_symlinks(&path)
            .and_then(|raw| serde_json::from_str::<PersistedLadderState>(&raw).ok())
            .filter(|prior| match (&self.stop_owner, &prior.owner) {
                (Some(mine), Some(theirs)) => mine == theirs,
                _ => false,
            });
        let (prior_episodes, prior_delay) = prior
            .map(|p| (p.serious_episodes, p.current_duty_delay_ms))
            .unwrap_or((0, 0));
        let snapshot = PersistedLadderState {
            schema_version: Some(LADDER_STATE_SCHEMA_VERSION),
            owner: self.stop_owner.clone(),
            serious_episodes: self.serious_episodes.max(prior_episodes),
            current_duty_delay_ms: self.current_duty_delay_ms.max(prior_delay),
        };
        let Ok(json) = serde_json::to_string(&snapshot) else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = crate::exclusive_fs::write_file_refusing_symlinks_0600(&path, json.as_bytes())
        {
            // (#2774 round-3 C12) Say it ONCE. A DIRECTORY at this path
            // makes every episode's write fail, which silently turns
            // cross-dispatch carry-forward off for a whole mission —
            // exactly the kind of thing an operator must not have to infer
            // from a ratchet that never grows.
            if !self.ladder_persist_error_reported {
                self.ladder_persist_error_reported = true;
                eprintln!(
                    "darkmux: ⚠ could not write the thermal ladder state at {} ({e}) — the \
                     `serious` episode count and the duty-cycle ratchet will not carry \
                     forward to this mission's later units",
                    path.display()
                );
            }
        }
    }

    /// (#2774) The LIVE duty-cycle delay (post-ratchet) — what
    /// `dispatch_internal.rs` reads to record "how hard was this run
    /// throttled" in the run artifact, alongside `above_nominal_ms`
    /// (#1247's own principle, cited by #2774). Starts at
    /// `config.duty_delay_ms` and only ever grows for the life of this
    /// governor.
    pub fn current_duty_delay_ms(&self) -> u64 {
        self.current_duty_delay_ms
    }

    /// (#2774 tier 4) How many `serious` EPISODES (transitions, never
    /// samples) this governor has counted this run — the artifact field
    /// alongside [`ThermalGovernor::current_duty_delay_ms`].
    pub fn serious_episodes(&self) -> u32 {
        self.serious_episodes
    }

    /// (#2774) Both artifact fields at once — what `dispatch_internal.rs`
    /// threads out of the sampler thread (via `run_telemetry_sampler`'s
    /// return value) into `host_window_json`'s payload, alongside
    /// `above_nominal_ms`, so "was this run throttled, and how hard" is
    /// answerable from the run's own data.
    pub fn ladder_summary(&self) -> ThermalLadderSummary {
        ThermalLadderSummary {
            serious_episodes: self.serious_episodes,
            current_duty_delay_ms: self.current_duty_delay_ms,
        }
    }

    /// (#2706; widened #2774) Whether this governor is currently holding
    /// the pace file — anything but `Idle` (`DutyCycle`, `Paused`,
    /// `OperatorHold`, or `Broken`). `DutyCycle` counts too: it writes
    /// `pause: false, turn_delay_ms: Some(_)`, which is just as much "this
    /// governor owns the pace file right now" as an actual pause is.
    ///
    /// **NOT the value `power_policy::BatteryGovernor::on_sample` gates
    /// on** — see [`Self::is_pausing`] for that, and for the #2774 review
    /// finding (F1) this split exists to fix.
    ///
    /// (#2774 round-3 C11) **`#[cfg(test)]`, deliberately.** It has zero
    /// production callers and sits ONE CHARACTER from `is_pausing` on a
    /// safety predicate — the exact shape of the F1 inversion. Gating it
    /// out of the production build makes reinstating that inversion a
    /// COMPILE error rather than a silent, still-green regression; the
    /// distinction it names is real enough to keep testable, and the
    /// source-level conformance test
    /// `the_battery_governor_call_site_gates_on_is_pausing` pins the call
    /// site itself. Un-gate it the day a production caller genuinely wants
    /// "is this governor touching the file at all."
    #[cfg(test)]
    pub fn is_pacing(&self) -> bool {
        self.state != State::Idle
    }

    /// (#2774 review F1) Whether this governor is holding an ACTUAL pause
    /// — `Paused`, `OperatorHold`, or `Broken`. Excludes `DutyCycle`,
    /// which writes `pause: false` and therefore is not a condition
    /// anything else needs to defer to.
    ///
    /// This is what `power_policy::BatteryGovernor::on_sample` gates on.
    /// Before this split, that call site used [`Self::is_pacing`] (any
    /// non-`Idle` state), which was safe by accident before tier 2
    /// existed — every non-`Idle` thermal state wrote `pause: true`. Once
    /// `DutyCycle` was added, gating on `is_pacing` made the battery
    /// governor stand down (write nothing) for the ENTIRE duration of a
    /// duty-cycle episode, silently dropping a real battery-critical
    /// pause: a machine could hit 9% with a 20% floor, thermal could enter
    /// `DutyCycle` 60s later, and the run would keep draining to 0% since
    /// nothing was checking the battery any more. Gating on `is_pausing`
    /// instead means the battery governor runs its own logic normally
    /// while thermal only duty-cycles, and only stands down for a
    /// GENUINE thermal pause — the case where deferring is actually safe,
    /// because thermal's own pause already satisfies "stop the run."
    ///
    /// This is race-free, not just usually-right: `dispatch_internal.rs`'s
    /// sampler loop calls `thermal_governor.on_sample` and THEN
    /// `battery_governor.on_sample` synchronously in the same single-
    /// threaded tick, before anything sleeps or any external reader gets
    /// to poll the file. So on any tick where thermal only duty-cycles
    /// (heartbeats or transitions with `pause: false`), the battery
    /// governor's OWN call on that SAME tick runs immediately after and,
    /// if the charge is below floor, writes `pause: true` last — the file
    /// on disk at the end of every tick is whichever governor most
    /// recently had something to enforce, never a stale write from one
    /// governor sitting unchallenged for multiple ticks.
    pub fn is_pausing(&self) -> bool {
        matches!(self.state, State::Paused | State::OperatorHold | State::Broken)
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
    /// pathological `max_pause_ms` of 1..3 from producing a zero interval
    /// (which would busy-restamp every tick — harmless but wasteful).
    ///
    /// (#2774 round-6 MF2) Derived from the STALENESS-CEILING reading of
    /// `max_pause_ms`, not the raw one — `0` means an unbounded EPISODE,
    /// and the cadence's whole job is to stay inside the window the
    /// container actually enforces (which substitutes the default for `0`
    /// for the reasons `thermal_pace_staleness_ceiling_ms` states). Reading
    /// `0` here literally would restamp every single tick forever, on the
    /// one config where the pause is meant to last longest.
    fn restamp_interval_ms(&self) -> u64 {
        let ceiling = match self.config.max_pause_ms {
            0 => darkmux_types::config_access::THERMAL_MAX_PAUSE_MS_DEFAULT,
            n => n,
        };
        (ceiling / 4).max(1)
    }

    /// Whether a reading counts as RECOVERY from an active pause.
    ///
    /// (#2774 round-4) Reads tier 3's recovery BAND rather than comparing
    /// ranks. Round 2 carried a second clause here — "and strictly milder
    /// than `pause_at`" — to keep a touching/inverted threshold pair from
    /// resuming and re-entering off the SAME sample; it is gone and stays
    /// gone, now for a structural reason rather than a remembered one:
    /// [`crate::thermal_bands::PauseBands`] cannot be constructed with an
    /// entry band and a recovery band that overlap, so a reading being in
    /// this band already means it is not in the entry band.
    ///
    /// `false` when tier 3 is disarmed, which is vacuous rather than
    /// meaningful — `State::Paused` is unreachable without an entry band.
    fn is_recovery_reading(&self, reading: SoftReading) -> bool {
        self.bands.pause().is_some_and(|p| p.recovery().contains(reading))
    }

    /// (#2774 round-6 MF2) Has THIS pause episode outlasted `max_pause_ms`,
    /// the cap past which tier 3 hands off to the breaker?
    ///
    /// **`0` means UNBOUNDED — rest as long as it takes, never hand off.**
    /// That is darkmux's stated convention for a `0` bound (`redis.maxlen`,
    /// `runtime.step_command_timeout_seconds`, and this block's own
    /// `episode_threshold` all read it that way), and the bare comparison
    /// this replaces read it as the opposite: `pause_episode_ms >= 0` is a
    /// TAUTOLOGY on the first sample, so an operator who set `0` to mean
    /// "never escalate to the breaker" got the breaker on the sample after
    /// the pause, plus a `STOP` file whose reason word (`thermal-critical`)
    /// named a state the machine had never reported.
    ///
    /// Exists as a method rather than an inline clause because the test is
    /// made in TWO places — here in `State::Paused`'s arm and again in the
    /// `None`-thermal arm's `Paused` branch, which accumulates the same
    /// episode across reading gaps. Those two read the same rule by
    /// construction now instead of by a maintainer noticing the twin.
    fn pause_episode_exhausted(&self) -> bool {
        self.config.max_pause_ms != 0 && self.pause_episode_ms >= self.config.max_pause_ms
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

        if self.state == State::Broken || self.state == State::OperatorHold {
            // (finding 2, redesigned for the heartbeat contract; widened
            // #2774 to cover tier 4's OperatorHold identically) Both are
            // terminal for DECISIONS but must keep re-stamping — without
            // an active writer the runtime's pure expiry rule (#2114
            // cf1b1993: no `expires` opt-out) would silently resume the
            // unit on a still-hot/still-escalated machine.
            self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
            if self.ms_since_stamp >= self.restamp_interval_ms() || self.real_age_past_interval() {
                self.mark_stamped();
                write_pace_file(host_out, true, self.terminal_reason(), &self.last_known_state);
            }
            return None;
        }

        // (finding 3) A missing OS thermal reading is "time passed, no new
        // information" — NOT evidence of recovery, and NOT nothing. Only
        // matters while actively `Paused` or `DutyCycle`: accumulate the
        // elapsed time / keep the heartbeat alive, and reset whichever
        // hysteresis hold is in flight (a gap is not a CONTINUOUS hold).
        // `Idle` resets its own duty-cycle-entry hold for the same reason
        // but has no heartbeat to keep alive (it owns no pace-file
        // instruction while `Idle`).
        let thermal = match thermal {
            Some(t) => {
                self.last_known_state = t.state.clone();
                t
            }
            None => {
                // (N4 of the #2110/#2109 review) A missing reading is not
                // evidence the CPU is throttled — reset the consecutive
                // low-sample streak unconditionally, matching how the None
                // arm already resets every other hysteresis hold rather
                // than freezing or extending it.
                self.speed_limit_low_streak = 0;
                match self.state {
                    State::Paused => {
                        self.pause_episode_ms = self.pause_episode_ms.saturating_add(elapsed_ms);
                        self.resume_hold_accum_ms = 0;
                        self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
                        if self.pause_episode_exhausted() {
                            self.state = State::Broken;
                            self.mark_stamped();
                            write_pace_file(host_out, true, "thermal-critical", &self.last_known_state);
                            if let Some(stop) = stop_file {
                                self.last_stop_write_error =
                                    write_stop_file(stop, self.stop_owner.as_deref(), STOP_FILE_REASON).err();
                            }
                            return Some(ThermalEvent::Breaker {
                                state: self.last_known_state.clone(),
                            });
                        }
                        if self.ms_since_stamp >= self.restamp_interval_ms() || self.real_age_past_interval()
                        {
                            self.mark_stamped();
                            write_pace_file(host_out, true, "thermal", &self.last_known_state);
                        }
                    }
                    State::DutyCycle => {
                        // (#2774 tier 2) A reading gap breaks the
                        // continuous-hold claim toward EXITING duty-cycle,
                        // same reasoning `Paused`'s resume hold uses — but
                        // there's no pause episode to accumulate, only the
                        // heartbeat and the exit hold.
                        self.duty_hold_accum_ms = 0;
                        self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
                        if self.ms_since_stamp >= self.restamp_interval_ms() || self.real_age_past_interval()
                        {
                            self.mark_stamped();
                            write_pace_file_with_delay(
                                host_out,
                                "thermal-duty-cycle",
                                &self.last_known_state,
                                self.current_duty_delay_ms,
                            );
                        }
                    }
                    State::Idle => {
                        self.duty_hold_accum_ms = 0;
                    }
                    State::OperatorHold | State::Broken => unreachable!("handled above"),
                }
                return None;
            }
        };

        // (#2774 round-4) `SoftReading::of` is the breaker's state test:
        // `None` is exactly `critical`, or a state name this build does not
        // recognize (which ranks worse than `critical` by the same
        // reasoning `host_probe::thermal_severity` uses — treating an
        // unknown state as mild would hide real thermal pressure). Every
        // `Some` is a reading the soft tiers' bands are defined over, so
        // the two definitions of "breaker-class reading" cannot drift.
        let reading = SoftReading::of(&thermal.state);
        // (finding 7) The speed-limit floor requires N CONSECUTIVE
        // low samples; a single low reading is common DVFS noise. The
        // `critical` state check is untouched — a discrete OS-reported
        // state trips immediately, same as before.
        if thermal.cpu_speed_limit_pct < self.config.min_cpu_speed_limit_pct {
            self.speed_limit_low_streak = self.speed_limit_low_streak.saturating_add(1);
        } else {
            self.speed_limit_low_streak = 0;
        }
        let is_breaker_condition = reading.is_none()
            || self.speed_limit_low_streak >= self.config.speed_limit_hold_samples.max(1);

        if is_breaker_condition {
            self.state = State::Broken;
            self.mark_stamped();
            write_pace_file(host_out, true, "thermal-critical", &thermal.state);
            if let Some(stop) = stop_file {
                self.last_stop_write_error = write_stop_file(stop, self.stop_owner.as_deref(), STOP_FILE_REASON).err();
            }
            return Some(ThermalEvent::Breaker { state: thermal.state.clone() });
        }

        // (#2774 round-3 MF1) The soft tiers (2/3/4) only run when the
        // thresholds describe a real band. The breaker above is compared
        // against its OWN thresholds and therefore ran already — disarming
        // costs the graduated response, never the hardware-danger stop.
        // See `soft_tiers_armed`'s own doc for why a degenerate band has
        // no safe soft behavior to fall back to.
        //
        // (#2774 round-4) This is now only a fast path: each tier below
        // reads its OWN band, so a config where some tiers are armed and
        // others are not behaves correctly without this early return.
        if !self.bands.any_armed() {
            return None;
        }
        // Not `None` — a breaker-class reading returned above.
        let reading = reading.expect("breaker-class readings returned above");

        match self.state {
            State::Idle | State::DutyCycle => {
                if self.bands.pause().is_some_and(|p| p.entry().contains(reading)) {
                    return self.enter_paused(&thermal.state, host_out, stop_file);
                }
                // (#2774 tier 2) `in_duty_band`: inside tier 2's duty band
                // — at/above `resume_at` and, when tier 3 is armed, below
                // `pause_at` (the branch above returned already). Entering
                // FROM `Idle` needs a sustained hold IN this band; leaving
                // FROM `DutyCycle` needs a sustained hold OUTSIDE it —
                // opposite directions, so which reading counts as progress
                // toward the transition flips with `was_duty_cycle`. Both
                // hysteresis holds share `duty_hold_accum_ms`, since a
                // governor is never in both states at once.
                //
                // (#2774 round-4 MF1) `duty()` is `None` when the band
                // would have had no reachable complement — `resume_at =
                // nominal`, where every reading below `pause_at` is inside
                // it and the exit is therefore unreachable. `false` for
                // every reading is then the correct reading of a disarmed
                // tier 2: entry (`in_duty_band`) can never fire, and the
                // exit below stays available so a governor somehow left in
                // `DutyCycle` still leaves it.
                let was_duty_cycle = self.state == State::DutyCycle;
                let in_duty_band = self.bands.duty().is_some_and(|d| d.contains(reading));
                if was_duty_cycle {
                    self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
                }
                let progressing_toward_transition = if was_duty_cycle { !in_duty_band } else { in_duty_band };
                if progressing_toward_transition {
                    self.duty_hold_accum_ms = self.duty_hold_accum_ms.saturating_add(elapsed_ms);
                } else {
                    // Same reset shape `resume_hold_accum_ms` uses: a
                    // sample that isn't progress toward the transition
                    // restarts the clock, it doesn't just pause it —
                    // otherwise a state bouncing at the boundary would
                    // eventually cross the hold on ACCUMULATED good ticks
                    // while still flapping.
                    self.duty_hold_accum_ms = 0;
                }

                if !was_duty_cycle && in_duty_band && self.duty_hold_accum_ms >= self.config.resume_hold_ms
                {
                    self.state = State::DutyCycle;
                    self.duty_hold_accum_ms = 0;
                    self.mark_stamped();
                    write_pace_file_with_delay(
                        host_out,
                        "thermal-duty-cycle",
                        &thermal.state,
                        self.current_duty_delay_ms,
                    );
                    return Some(ThermalEvent::DutyCycleEntered {
                        state: thermal.state.clone(),
                        delay_ms: self.current_duty_delay_ms,
                    });
                }
                if was_duty_cycle && !in_duty_band && self.duty_hold_accum_ms >= self.config.resume_hold_ms
                {
                    self.state = State::Idle;
                    self.duty_hold_accum_ms = 0;
                    self.mark_stamped();
                    write_pace_file(host_out, false, "thermal", &thermal.state);
                    return Some(ThermalEvent::DutyCycleExited { state: thermal.state.clone() });
                }
                // No transition this tick — heartbeat the duty-cycle
                // instruction if that's the state we're still in (`Idle`
                // owns no pace-file instruction, so it heartbeats nothing).
                if was_duty_cycle
                    && (self.ms_since_stamp >= self.restamp_interval_ms() || self.real_age_past_interval())
                {
                    self.mark_stamped();
                    write_pace_file_with_delay(
                        host_out,
                        "thermal-duty-cycle",
                        &thermal.state,
                        self.current_duty_delay_ms,
                    );
                }
                None
            }
            State::Paused => {
                self.pause_episode_ms = self.pause_episode_ms.saturating_add(elapsed_ms);
                self.ms_since_stamp = self.ms_since_stamp.saturating_add(elapsed_ms);
                let recovering = self.is_recovery_reading(reading);
                if recovering {
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
                // (#2774 round-6 MF1) Gated on the PREDICATE as well as the
                // accumulator, the same shape the duty-cycle branch above
                // uses (`in_duty_band && duty_hold_accum_ms >= …`). The
                // accumulator alone encodes "a recovery reading was seen"
                // only while `resume_hold_ms > 0`: at `0` the comparison
                // `0 >= 0` is a TAUTOLOGY, so a machine reading `serious`
                // every sample resumed at full speed on the tick after it
                // paused — and, with the ratchet and the episode count
                // both advancing on each of those phantom recoveries,
                // reached tier 4's terminal operator-gated hold in three
                // samples. `recovering` is the thing the hold was always
                // measuring; the accumulator only says how LONG.
                //
                // This makes `resume_hold_ms = 0` mean "resume on the
                // first recovery reading" — the same reading
                // `speed_limit_hold_samples`'s `.max(1)` floor gives its
                // own `0` — rather than "resume regardless of the
                // reading." Correct at every value, not just zero, which
                // is why the predicate is the fix and a floor on the knob
                // would not have been: a floor leaves the tautology one
                // edit away.
                if recovering && self.resume_hold_accum_ms >= self.config.resume_hold_ms {
                    // (#2774 tier 3) The ratchet: applied on EVERY
                    // successful recovery, one-way for the life of this
                    // governor. Multiplied, never divided; nothing else in
                    // this function ever lowers `current_duty_delay_ms`.
                    self.current_duty_delay_ms = self
                        .current_duty_delay_ms
                        .saturating_mul(u64::from(self.config.ratchet_factor.max(1)));
                    // (#2774 review F5) Persist the ratcheted delay right
                    // away, same reasoning as the episode-count persist in
                    // `enter_paused` — a later unit's governor must never
                    // seed from a pre-ratchet value.
                    self.persist_ladder_state();
                    self.pause_episode_ms = 0;
                    self.resume_hold_accum_ms = 0;
                    self.mark_stamped();
                    // Land in `DutyCycle` if the recovering sample is
                    // still in the duty band, `Idle` if it's below it —
                    // the SAME band the Idle/DutyCycle branch tests, read
                    // off the same value, so "where do we land" cannot
                    // disagree with "when would we leave again." With tier
                    // 2 disarmed there is nowhere to land but `Idle`.
                    if self.bands.duty().is_some_and(|d| d.contains(reading)) {
                        self.state = State::DutyCycle;
                        write_pace_file_with_delay(
                            host_out,
                            "thermal-duty-cycle",
                            &thermal.state,
                            self.current_duty_delay_ms,
                        );
                    } else {
                        self.state = State::Idle;
                        write_pace_file(host_out, false, "thermal", &thermal.state);
                    }
                    return Some(ThermalEvent::Resumed { state: thermal.state.clone() });
                }
                if self.pause_episode_exhausted() {
                    self.state = State::Broken;
                    self.mark_stamped();
                    write_pace_file(host_out, true, "thermal-critical", &thermal.state);
                    if let Some(stop) = stop_file {
                        self.last_stop_write_error = write_stop_file(stop, self.stop_owner.as_deref(), STOP_FILE_REASON).err();
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
            State::OperatorHold | State::Broken => unreachable!("returned early above"),
        }
    }

    /// (#2774 tiers 3/4) Common entry point for BOTH `Idle` and
    /// `DutyCycle` crossing into `serious` (`>= pause_at`). Counts the
    /// EPISODE — exactly once per call, i.e. once per TRANSITION, never
    /// once per sample spent at `serious` (a caller only reaches this
    /// method on the tick severity first crosses the threshold; every
    /// later tick still at `serious` stays inside `State::Paused`'s own
    /// match arm and never calls this again) — and decides whether this
    /// episode is an ordinary tier-3 pause or, having reached
    /// `episode_threshold`, an immediate tier-4 `OperatorHold`.
    fn enter_paused(&mut self, state: &str, host_out: &Path, stop_file: Option<&Path>) -> Option<ThermalEvent> {
        self.serious_episodes = self.serious_episodes.saturating_add(1);
        // (#2774 review F5) Persist the new count immediately — the NEXT
        // unit's governor may construct and seed itself before this
        // dispatch's own next heartbeat, and a mission-scoped count that
        // only updates on some LATER tick would let a fast-following unit
        // read a stale (too-low) episode count.
        self.persist_ladder_state();
        self.pause_episode_ms = 0;
        self.resume_hold_accum_ms = 0;
        self.duty_hold_accum_ms = 0;
        if self.config.tier4_enabled
            && self.config.episode_threshold != 0
            && self.serious_episodes >= self.config.episode_threshold
        {
            self.state = State::OperatorHold;
            self.mark_stamped();
            write_pace_file(host_out, true, STOP_FILE_REASON_EPISODE_LIMIT, state);
            if let Some(stop) = stop_file {
                // (#2774 round-3 C8) The SAME reason the pace file just
                // got, not the breaker's `thermal-critical` — see
                // `STOP_FILE_REASON_EPISODE_LIMIT`.
                self.last_stop_write_error =
                    write_stop_file(stop, self.stop_owner.as_deref(), STOP_FILE_REASON_EPISODE_LIMIT)
                        .err();
            }
            return Some(ThermalEvent::OperatorHold {
                state: state.to_string(),
                episode: self.serious_episodes,
            });
        }
        self.state = State::Paused;
        self.mark_stamped();
        write_pace_file(host_out, true, "thermal", state);
        Some(ThermalEvent::Paused { state: state.to_string() })
    }

    /// The `reason` string for the terminal-state heartbeat re-stamp at
    /// the top of [`ThermalGovernor::on_sample`] — `Broken` and
    /// `OperatorHold` share that re-stamp loop but carry different
    /// reasons so a flow reader can tell a count-based tier-4 escalation
    /// from a hardware-critical tier-5/legacy-breaker one.
    fn terminal_reason(&self) -> &'static str {
        match self.state {
            State::OperatorHold => STOP_FILE_REASON_EPISODE_LIMIT,
            _ => STOP_FILE_REASON,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_probe::thermal::THERMAL_STATES;

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
            duty_delay_ms: 15_000,
            ratchet_factor: 2,
            episode_threshold: 2,
            tier4_enabled: true,
        }
    }

    /// Tier-4 escalation off — for tests exercising the pre-existing
    /// tier-3/breaker behavior in isolation, unaffected by the episode
    /// count.
    fn cfg_tier4_disabled() -> ThermalGovernorConfig {
        ThermalGovernorConfig { tier4_enabled: false, ..cfg() }
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
            // (#2774 round-4 C2) The REASON is part of the value now, and
            // asserted with it: a reader that gets the scope right and the
            // reason wrong is exactly what C2 found.
            Some(StopFileHold {
                scope: StopHold::ThisMission,
                reason: Some(STOP_FILE_REASON.to_string()),
            }),
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
        assert_eq!(
            stop_hold_for_mission(&stop, "anything"),
            Some(StopFileHold {
                scope: StopHold::Unattributed,
                reason: Some(STOP_FILE_REASON.to_string()),
            })
        );
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
        assert_eq!(stop_file_body(Some("   "), STOP_FILE_REASON), "thermal-critical\n");
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
        assert_eq!(
            stop_hold_for_mission(&stop, "crawl-m-1"),
            Some(StopFileHold {
                scope: StopHold::ThisMission,
                reason: Some(STOP_FILE_REASON.to_string()),
            })
        );

        // #2454's reader still works against the new writer, and a
        // second mission's own trip re-stamps the SAME file cleanly.
        let mut gov_b = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-2"));
        gov_b.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        assert_eq!(gov_b.last_stop_write_error(), None);
        assert_eq!(
            stop_hold_for_mission(&stop, "crawl-m-2"),
            Some(StopFileHold {
                scope: StopHold::ThisMission,
                reason: Some(STOP_FILE_REASON.to_string()),
            }),
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
    /// (#2774 review C9) Same shape, one field over: the DUTY-CYCLE key.
    /// Tier 2's whole mechanism is the host writing `turn_delay_ms` and
    /// the runtime reading it, across a crate boundary with no shared type
    /// — a rename on either side would compile, pass both crates' own
    /// suites, and silently stop every duty-cycle delay from being
    /// honored. This writes a REAL pace file through the production writer
    /// and asserts the emitted JSON key, then asserts the runtime declares
    /// a field of that exact name.
    #[test]
    fn duty_cycle_turn_delay_key_matches_the_runtime_reader() {
        let dir = tempfile::tempdir().unwrap();
        write_pace_file_with_delay(dir.path(), "thermal-duty-cycle", "fair", 15_000);
        let raw = std::fs::read_to_string(pace_file_path(dir.path())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["turn_delay_ms"],
            serde_json::json!(15_000),
            "the host must emit the duty-cycle delay under the key `turn_delay_ms`: {raw}"
        );

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime_pace_rs = manifest_dir.join("../../runtime/src/pace.rs");
        let source = std::fs::read_to_string(&runtime_pace_rs)
            .unwrap_or_else(|e| panic!("reading {}: {e}", runtime_pace_rs.display()));
        assert!(
            source.contains("pub turn_delay_ms: Option<u64>"),
            "runtime/src/pace.rs must read the duty-cycle delay from a field named \
             `turn_delay_ms` to match the key the host writes above — a rename on either \
             side compiles and silently disables tier 2 entirely. Got:\n{source}"
        );
    }

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

    // ═══════════════════════════════════════════════════════════════
    // (#2774) The five-tier escalation ladder
    // ═══════════════════════════════════════════════════════════════

    // ── (#2774 review F1) is_pacing vs is_pausing ──

    /// (Mutation self-check target, and the F1 regression) `DutyCycle`
    /// must NOT report `is_pausing() == true` — that is precisely the bug
    /// the review found: the battery governor gates its stand-down on
    /// this, and if `DutyCycle` counted, a real battery-critical pause
    /// would be silently suppressed for the whole duty-cycle episode.
    #[test]
    fn is_pausing_excludes_duty_cycle_but_includes_every_real_pause_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        assert!(!gov.is_pausing(), "Idle is not pausing");
        assert!(!gov.is_pacing(), "Idle is not even pacing");

        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert!(gov.is_pacing(), "DutyCycle DOES own the pace file");
        assert!(
            !gov.is_pausing(),
            "but DutyCycle is NOT a pause — it writes pause:false, and the battery \
             governor must be free to act while this holds"
        );

        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert!(gov.is_pausing(), "Paused IS a real pause");

        let mut hold_gov = ThermalGovernor::new(ThermalGovernorConfig { episode_threshold: 1, ..cfg() });
        hold_gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert!(hold_gov.is_pausing(), "OperatorHold IS a real pause");

        let mut broken_gov = ThermalGovernor::new(cfg());
        broken_gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), None);
        assert!(broken_gov.is_pausing(), "Broken IS a real pause");
    }

    #[test]
    fn f1_regression_battery_governor_still_acts_while_thermal_duty_cycles() {
        // (#2774 review F1) The exact proven scenario: battery below floor,
        // thermal duty-cycling (NOT literally pausing). Before the fix,
        // dispatch_internal.rs passed `thermal_governor.is_pacing()` here,
        // which is `true` for `DutyCycle` — the battery governor stood
        // down and wrote nothing, silently dropping the pause. With
        // `is_pausing()` (false for `DutyCycle`), the battery governor
        // must run its own logic normally and write the pause.
        use crate::power_policy::{BatteryGovernor, PowerPolicyConfig};
        let dir = tempfile::tempdir().unwrap();

        let mut thermal = ThermalGovernor::new(cfg());
        for _ in 0..30 {
            thermal.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert!(thermal.is_pacing(), "thermal owns the file (DutyCycle)");
        assert!(!thermal.is_pausing(), "but is not a real pause — the F1 fix's whole point");

        let battery_sample = crate::host_probe::BatterySample {
            charge_pct: 9,
            on_ac: false,
            charging: false,
            minutes_to_empty: None,
        };
        let mut battery = BatteryGovernor::new(PowerPolicyConfig {
            min_battery_pct: 20,
            refuse_start_below_min: true,
            pause_running_below_min: true,
        });
        let event = battery.on_sample(Some(&battery_sample), 2000, dir.path(), thermal.is_pausing());
        assert!(
            matches!(event, Some(crate::power_policy::BatteryEvent::Paused { .. })),
            "the battery governor must still fire its own pause while thermal merely \
             duty-cycles: {event:?}"
        );
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true), "the file must end up paused");
        assert_eq!(pace["reason"], serde_json::json!("battery"), "battery's own pause, not thermal's");
    }

    // ── Tier 2: duty-cycle entry/exit hysteresis ──

    #[test]
    fn duty_cycle_entry_requires_a_sustained_hold_at_fair() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());

        // 29 ticks * 2000ms = 58000ms < 60000ms hold — never enters.
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        }
        assert!(!pace_file_path(dir.path()).exists(), "no pace file until the hold completes");

        // 30th tick crosses 60000ms — enters, with the CONFIGURED delay
        // (never ratcheted yet).
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::DutyCycleEntered { state: "fair".to_string(), delay_ms: 15_000 })
        );
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false), "duty-cycle is NOT a pause");
        assert_eq!(pace["turn_delay_ms"], serde_json::json!(15_000));
        assert_eq!(pace["reason"], serde_json::json!("thermal-duty-cycle"));
    }

    #[test]
    fn duty_cycle_never_engages_below_resume_at() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        for _ in 0..100 {
            assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
        }
        assert!(!pace_file_path(dir.path()).exists(), "nominal must never engage the duty cycle");
    }

    #[test]
    fn a_tick_back_to_nominal_resets_the_duty_cycle_entry_hold() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        for _ in 0..29 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        // One nominal tick must reset the hold, not just pause it.
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
        for _ in 0..29 {
            assert_eq!(
                gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
                None,
                "the hold restarted at the nominal tick — 58s of fair since then is not enough"
            );
        }
        assert_eq!(
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::DutyCycleEntered { state: "fair".to_string(), delay_ms: 15_000 }),
            "enters only after a FRESH continuous 60s hold"
        );
    }

    #[test]
    fn duty_cycle_exit_requires_a_sustained_hold_at_nominal() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(read_pace(dir.path())["turn_delay_ms"], serde_json::json!(15_000));

        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
        }
        assert_eq!(
            read_pace(dir.path())["turn_delay_ms"],
            serde_json::json!(15_000),
            "still duty-cycling — the exit hold has not completed"
        );
        assert_eq!(
            gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::DutyCycleExited { state: "nominal".to_string() })
        );
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));
        assert!(
            pace.get("turn_delay_ms").is_none(),
            "leaving duty-cycle must not leave a stale turn_delay_ms behind: {pace}"
        );
    }

    #[test]
    fn a_tick_back_to_fair_resets_the_duty_cycle_exit_hold() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        for _ in 0..29 {
            gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None);
        }
        assert_eq!(gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None), None);
        for _ in 0..29 {
            assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
        }
        assert_eq!(
            gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::DutyCycleExited { state: "nominal".to_string() }),
            "exits only after a FRESH continuous 60s hold at nominal"
        );
    }

    #[test]
    fn duty_cycle_heartbeats_the_pace_file_while_sustained() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        let first_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        // restamp_interval_ms = max_pause_ms/4 = 225_000ms. Drive it past
        // that with fair ticks (no transition — still duty-cycling).
        let mut elapsed = 0u64;
        while elapsed < 230_000 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
            elapsed += 2000;
        }
        let later_stamp = read_pace(dir.path())["written_at_ms"].as_u64().unwrap();
        assert!(later_stamp >= first_stamp, "the heartbeat must keep re-stamping while duty-cycling");
        assert_eq!(read_pace(dir.path())["turn_delay_ms"], serde_json::json!(15_000));
    }

    // ── Tier 3: the ratchet is one-way for the life of the run ──

    /// (Mutation self-check target) If the ratchet were applied only ONCE
    /// (e.g. gated on `serious_episodes == 1`) rather than on EVERY
    /// recovery, this test would go red at the second doubling — proving
    /// the "every recovery" clause is load-bearing, not just documented.
    #[test]
    fn the_ratchet_doubles_on_every_recovery_and_never_goes_back_down() {
        let dir = tempfile::tempdir().unwrap();
        // tier 4 disabled and a high threshold so this test can run several
        // full pause/recover cycles without escalating to OperatorHold.
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled());
        assert_eq!(gov.current_duty_delay_ms(), 15_000, "starts at the configured base");

        // Cycle 1: serious -> resume_hold_ms of fair -> resume.
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(gov.current_duty_delay_ms(), 30_000, "doubled after the FIRST recovery");

        // Cycle 2: back to serious, then recover again.
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(gov.current_duty_delay_ms(), 60_000, "doubled AGAIN after the SECOND recovery");

        // Now drop all the way to nominal — the ratchet must NOT reset.
        for _ in 0..30 {
            gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None);
        }
        assert_eq!(
            gov.current_duty_delay_ms(),
            60_000,
            "recovering to full nominal must not restore the pre-doubling delay"
        );
    }

    #[test]
    fn a_ratchet_factor_of_one_holds_the_delay_steady() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThermalGovernorConfig { ratchet_factor: 1, tier4_enabled: false, ..cfg() };
        let mut gov = ThermalGovernor::new(cfg);
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(gov.current_duty_delay_ms(), 15_000, "a factor of 1 holds steady, doesn't grow");
    }

    #[test]
    fn a_ratchet_factor_of_zero_is_coerced_to_one_not_zeroed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThermalGovernorConfig { ratchet_factor: 0, tier4_enabled: false, ..cfg() };
        let mut gov = ThermalGovernor::new(cfg);
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(
            gov.current_duty_delay_ms(),
            15_000,
            "a configured 0 must never zero the delay — that would defeat the ratchet entirely"
        );
    }

    #[test]
    fn resume_lands_in_idle_when_the_recovering_sample_is_fully_nominal() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        // Hold the resume window at NOMINAL (better than fair) so the
        // recovering sample itself reads as fully cool.
        for _ in 0..29 {
            gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None);
        }
        assert_eq!(
            gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "nominal".to_string() })
        );
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));
        assert!(
            pace.get("turn_delay_ms").is_none(),
            "landing in Idle must not carry a stray turn_delay_ms: {pace}"
        );
        // Confirm it's genuinely Idle, not DutyCycle: one more nominal tick
        // must be a pure no-op (no re-stamp, no event).
        assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
    }

    #[test]
    fn resume_lands_in_duty_cycle_when_the_recovering_sample_is_still_fair() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled());
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));
        assert_eq!(
            pace["turn_delay_ms"],
            serde_json::json!(30_000),
            "lands DIRECTLY in duty-cycle, at the already-ratcheted delay"
        );
    }

    // ── Tier 4: the episode count is a TRANSITION, never a sample ──

    /// (Mutation self-check target) If episode counting were driven by
    /// SAMPLES at `serious` instead of the TRANSITION into it, this test
    /// (100 consecutive ticks continuously at `serious`, never recovering)
    /// would report `serious_episodes() == 100`, not `1`, and — with
    /// `episode_threshold: 2` — would incorrectly escalate to
    /// `OperatorHold` on the second tick rather than staying an ordinary
    /// tier-3 pause for the entire sustained stretch.
    #[test]
    fn a_single_sustained_serious_stretch_is_exactly_one_episode() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        let first = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(first, Some(ThermalEvent::Paused { state: "serious".to_string() }));
        assert_eq!(gov.serious_episodes(), 1);
        for _ in 0..100 {
            let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
            assert_eq!(ev, None, "still the SAME episode — no transition, no event");
            assert_eq!(gov.serious_episodes(), 1, "100 samples at serious is still ONE episode");
        }
    }

    #[test]
    fn the_nth_serious_episode_escalates_straight_to_operator_hold() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg()); // episode_threshold: 2

        // Episode 1: ordinary tier-3 pause + resume.
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Paused { state: "serious".to_string() })
        );
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(gov.serious_episodes(), 1);

        // Episode 2 (== episode_threshold): straight to OperatorHold, no
        // ordinary Paused event at all.
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::OperatorHold { state: "serious".to_string(), episode: 2 })
        );
        assert_eq!(gov.serious_episodes(), 2);
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true));
        assert_eq!(pace["reason"], serde_json::json!("thermal-episode-limit"));
    }

    #[test]
    fn operator_hold_never_auto_resumes_even_at_full_nominal() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThermalGovernorConfig { episode_threshold: 1, ..cfg() };
        let mut gov = ThermalGovernor::new(cfg);
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::OperatorHold { state: "serious".to_string(), episode: 1 })
        );
        // Feed a LONG stretch of nominal — far past resume_hold_ms — and
        // confirm it NEVER resumes automatically. Every tick heartbeats
        // the SAME reason.
        for _ in 0..100 {
            assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
        }
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(true), "OperatorHold never releases itself");
        assert_eq!(pace["reason"], serde_json::json!("thermal-episode-limit"));
    }

    #[test]
    fn episode_threshold_zero_means_unbounded_never_escalates() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThermalGovernorConfig { episode_threshold: 0, ..cfg() };
        let mut gov = ThermalGovernor::new(cfg);
        // Three full pause/recover cycles — never once escalates to
        // OperatorHold, no matter how many episodes accumulate.
        for _ in 0..3 {
            let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
            assert!(matches!(ev, Some(ThermalEvent::Paused { .. })), "{ev:?}");
            for _ in 0..30 {
                gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
            }
        }
        assert_eq!(gov.serious_episodes(), 3, "count still accumulates — only the escalation is off");
    }

    #[test]
    fn tier4_disabled_never_escalates_regardless_of_episode_count() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ThermalGovernorConfig { tier4_enabled: false, episode_threshold: 1, ..cfg() };
        let mut gov = ThermalGovernor::new(cfg);
        for _ in 0..3 {
            let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
            assert!(
                matches!(ev, Some(ThermalEvent::Paused { .. })),
                "tier4_enabled=false must keep every episode an ordinary tier-3 pause: {ev:?}"
            );
            for _ in 0..30 {
                gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
            }
        }
    }

    #[test]
    fn ladder_summary_reports_episodes_and_the_live_delay() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled());
        assert_eq!(gov.ladder_summary(), ThermalLadderSummary { serious_episodes: 0, current_duty_delay_ms: 15_000 });
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(
            gov.ladder_summary(),
            ThermalLadderSummary { serious_episodes: 1, current_duty_delay_ms: 30_000 }
        );
    }

    // ── (#2774 round-3 MF1) An incoherent threshold pair disarms the soft
    //    tiers outright — it does not get a patched predicate ──
    //
    //    Round 1 found that a touching/inverted pair manufactured a fresh
    //    EPISODE out of one unchanging reading and reached tier 4's
    //    terminal hold in about a minute. Round 2's fix added "strictly
    //    milder than pause_at" to the recovery predicate, which for
    //    `pause_at = "nominal"` became `sev < 0` on a `usize` —
    //    unsatisfiable — so the governor paused on its FIRST sample, could
    //    never recover, and handed off to the BREAKER at `max_pause_ms`
    //    with `reason: "thermal-critical"` plus a crawl STOP file, on a
    //    machine that read `nominal` the entire time. These tests pin the
    //    third answer: such a pair runs no soft tier at all.

    /// Every incoherent pair, every state, exhaustively — nothing in tiers
    /// 2/3/4 may fire and nothing may be written to the pace file. This is
    /// the 4x4 sweep the MF1 report ran by hand, committed.
    ///
    /// (#2774 round-4 C3) Widened to UNRECOGNIZED names in either slot.
    /// The old sweep ran over the four KNOWN states only, which is why it
    /// missed that `severity()`'s `unwrap_or(THERMAL_STATES.len())` ranked
    /// a typo'd `pause_at = "seroius"` at 4 — above `critical` — so
    /// `4 > severity("fair")` ARMED the ladder with tiers 3/4 unreachable
    /// (the breaker owns every reading at or above `critical`), `serious`
    /// silently degraded to a duty cycle, and the dispatch-start warning,
    /// gated on `!soft_tiers_armed()`, never fired.
    #[test]
    fn every_incoherent_threshold_pair_disarms_the_soft_tiers_entirely() {
        let tokens: Vec<&str> = THERMAL_STATES
            .iter()
            .copied()
            .chain(["seroius", "", "Serious", "unknown-9"])
            .collect();
        let rank = |t: &str| THERMAL_STATES.iter().position(|s| *s == t);
        for pause_at in &tokens {
            for resume_at in &tokens {
                let (pause_at, resume_at) = (*pause_at, *resume_at);
                // Coherent pairs are covered by the sane-gap tests below.
                // An unrecognized token in EITHER slot is incoherent by
                // construction: there is no rank to compare.
                if let (Some(pi), Some(ri)) = (rank(pause_at), rank(resume_at)) {
                    if pi > ri {
                        continue;
                    }
                }
                let gov_cfg = ThermalGovernorConfig {
                    pause_at: pause_at.to_string(),
                    resume_at: resume_at.to_string(),
                    ..cfg()
                };
                let armed = ThermalGovernor::new(gov_cfg.clone());
                assert!(
                    !armed.soft_tiers_armed(),
                    "pause_at={pause_at} / resume_at={resume_at} leaves no band for the holds \
                     to occupy and must not arm the soft tiers"
                );
                // (#2774 round-4 C3) …and SAYS so. A disarm the operator
                // is never told about is how the typo'd-`pause_at` case
                // stayed invisible: the dispatch-start warning renders
                // these notes, so an empty list is a silent disarm.
                assert!(
                    !armed.disarm_notes().is_empty(),
                    "pause_at={pause_at} / resume_at={resume_at}: a disarmed ladder must say why"
                );
                // `nominal` and `fair` only: `serious`/`critical` are real
                // thermal pressure and the breaker's own (threshold-
                // independent) `critical` rule is asserted separately.
                for reading in ["nominal", "fair"] {
                    let dir = tempfile::tempdir().unwrap();
                    let mut gov = ThermalGovernor::new(gov_cfg.clone());
                    // 600 samples x 2000ms = 20 minutes, past the 900s
                    // `max_pause_ms` that turned MF1's wedge into a breaker
                    // trip.
                    for _ in 0..600 {
                        let ev = gov.on_sample(Some(&sample(reading, 100)), 2000, dir.path(), None);
                        assert_eq!(
                            ev, None,
                            "pause_at={pause_at} resume_at={resume_at} reading={reading}: an \
                             incoherent pair must produce no soft-tier event at all, got {ev:?}"
                        );
                    }
                    assert_eq!(gov.serious_episodes(), 0);
                    assert!(
                        !pace_file_path(dir.path()).exists(),
                        "pause_at={pause_at} resume_at={resume_at} reading={reading}: a disarmed \
                         ladder must not write the pace file — it has no instruction to give"
                    );
                }
            }
        }
    }

    /// MF1's exact repro, kept as its own named case because it is the one
    /// that ended at `reason: "thermal-critical"` + a crawl STOP file on a
    /// machine that was cold the whole time.
    #[test]
    fn pause_at_nominal_never_pauses_and_never_reaches_the_breaker() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        for resume_at in THERMAL_STATES {
            let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
                pause_at: "nominal".to_string(),
                resume_at: resume_at.to_string(),
                ..cfg()
            });
            for _ in 0..600 {
                assert_eq!(
                    gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), Some(&stop)),
                    None,
                    "resume_at={resume_at}: 20 minutes of `nominal` is not a thermal event"
                );
            }
        }
        assert!(
            !stop.exists(),
            "a machine reading `nominal` for 20 minutes must never have its crawl stopped"
        );
        assert!(!pace_file_path(dir.path()).exists(), "…nor its run paced");
    }

    /// The second-order half of MF1: with the round-2 predicate, the
    /// `fair`/`fair` pair the F6 guard was actually written for ALSO ended
    /// at the breaker, labeled `thermal-critical` on evidence that only
    /// ever said `fair`.
    #[test]
    fn the_fair_fair_pair_no_longer_ends_at_a_thermal_critical_breaker() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
            pause_at: "fair".to_string(),
            resume_at: "fair".to_string(),
            ..cfg()
        });
        for _ in 0..600 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), Some(&stop));
        }
        assert!(!stop.exists(), "`fair` is not a hardware-critical condition");
        assert!(!pace_file_path(dir.path()).exists());
    }

    /// Disarming the SOFT tiers must not disarm the breaker — the breaker
    /// compares against its own thresholds (`critical`, and the
    /// `cpu_speed_limit_pct` floor), never against `pause_at`/`resume_at`,
    /// so an incoherent pair leaves the hardware-danger stop intact.
    #[test]
    fn the_breaker_still_fires_while_the_soft_tiers_are_disarmed() {
        let incoherent = || ThermalGovernorConfig {
            pause_at: "nominal".to_string(),
            resume_at: "critical".to_string(),
            ..cfg()
        };

        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(incoherent());
        assert_eq!(
            gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop)),
            Some(ThermalEvent::Breaker { state: "critical".to_string() }),
            "an OS-reported `critical` still trips the breaker"
        );
        assert!(stop.exists(), "and still stops the crawl");

        // The speed-limit floor, the breaker's other (threshold-
        // independent) signal — 3 consecutive samples under 50%.
        let dir2 = tempfile::tempdir().unwrap();
        let mut gov2 = ThermalGovernor::new(incoherent());
        assert_eq!(gov2.on_sample(Some(&sample("nominal", 10)), 2000, dir2.path(), None), None);
        assert_eq!(gov2.on_sample(Some(&sample("nominal", 10)), 2000, dir2.path(), None), None);
        assert_eq!(
            gov2.on_sample(Some(&sample("nominal", 10)), 2000, dir2.path(), None),
            Some(ThermalEvent::Breaker { state: "nominal".to_string() }),
            "the sustained speed-limit floor still trips the breaker"
        );
    }

    #[test]
    fn a_sane_threshold_gap_resumes_exactly_as_before() {
        // The half of MF1 that matters most: the disarm must not wedge a
        // LEGITIMATE recovery. With `resume_at` strictly milder than
        // `pause_at`, everything runs as it always did.
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled()); // serious / fair
        assert!(gov.soft_tiers_armed(), "a one-band gap is a coherent pair");
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let mut resumed = None;
        for _ in 0..31 {
            if let Some(ev) = gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None) {
                resumed = Some(ev);
            }
        }
        assert_eq!(
            resumed,
            Some(ThermalEvent::Resumed { state: "fair".to_string() }),
            "a resume AT resume_at (fair, one band below pause_at) must still fire"
        );
        assert_eq!(gov.current_duty_delay_ms(), 30_000, "and still ratchets on the way out");
    }

    // ── (#2774 round-4 MF1) `resume_at = nominal` made tier 2's duty band
    //    a TAUTOLOGY (`sev >= 0`), so `DutyCycle` could be entered and
    //    never left. These drive the governor through the configs the
    //    `thermal_bands` enumeration covers statically. ──

    /// MF1's exact repro, from its own PROVEN report: 900 samples x 2000ms
    /// (30 minutes) of `nominal` on a config the round-3 remedy text
    /// literally recommends. It used to yield `DutyCycleEntered { state:
    /// "nominal", delay_ms: 15000 }` and a pace file still pacing every
    /// turn at the end, on a machine that was cold throughout — a 40-turn
    /// run silently gaining ten minutes, ratcheting to 300s/turn over a
    /// mission and never unwinding.
    #[test]
    fn resume_at_nominal_never_enters_a_duty_cycle() {
        for pause_at in ["fair", "serious", "critical"] {
            let dir = tempfile::tempdir().unwrap();
            let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
                pause_at: pause_at.to_string(),
                resume_at: "nominal".to_string(),
                ..cfg()
            });
            for _ in 0..900 {
                let ev = gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None);
                assert_eq!(
                    ev, None,
                    "pause_at={pause_at}: 30 minutes of `nominal` is not a duty-cycle \
                     condition, got {ev:?}"
                );
            }
            assert!(
                !pace_file_path(dir.path()).exists(),
                "pause_at={pause_at}: a cold machine must not be paced at all"
            );
            assert_eq!(gov.current_duty_delay_ms(), 15_000, "…and nothing may ratchet");
        }
    }

    /// The second half of MF1: recovering from a REAL `serious` episode
    /// under the same config must land in `Idle`, not in a `DutyCycle`
    /// that cannot be exited. This is the path that made the wedge
    /// permanent — the governor landed back in `DutyCycle` on every
    /// recovery, so the ratchet compounded with nothing able to unwind it.
    #[test]
    fn resume_at_nominal_recovers_into_idle_not_a_permanent_duty_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
            pause_at: "serious".to_string(),
            resume_at: "nominal".to_string(),
            ..cfg_tier4_disabled()
        });
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Paused { state: "serious".to_string() }),
            "tiers 3/4 stay armed under this config — only tier 2 is disarmed"
        );
        let mut resumed = None;
        for _ in 0..31 {
            if let Some(ev) = gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None) {
                resumed = Some(ev);
            }
        }
        assert_eq!(resumed, Some(ThermalEvent::Resumed { state: "nominal".to_string() }));
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));
        assert!(
            pace.get("turn_delay_ms").is_none(),
            "recovery must not hand the run a turn delay it can never shed: {pace}"
        );
        // …and it stays shed. Half an hour later, still nothing.
        for _ in 0..900 {
            assert_eq!(gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None), None);
        }
        assert_eq!(read_pace(dir.path())["pause"], serde_json::json!(false));
        assert!(read_pace(dir.path()).get("turn_delay_ms").is_none());
    }

    /// **The behavioral half of the class-regression net.** For EVERY
    /// threshold pair whose tier-2 band survived construction, the band's
    /// own witnesses are driven through the governor: the member enters the
    /// duty cycle and the non-member leaves it. `thermal_bands`'
    /// `every_threshold_pair_yields_bands_that_are_neither_empty_nor_total`
    /// proves the witnesses exist; this proves they do what the band says
    /// they do, so a band that is well-formed but wired to the wrong
    /// predicate is caught too.
    #[test]
    fn every_armed_duty_band_can_be_both_entered_and_exited() {
        let mut exercised = 0;
        for pause_at in THERMAL_STATES {
            for resume_at in THERMAL_STATES {
                let Some(duty) =
                    crate::thermal_bands::ThermalBands::resolve(pause_at, resume_at).duty()
                else {
                    continue;
                };
                exercised += 1;
                let label = format!("pause_at={pause_at} resume_at={resume_at}");
                let dir = tempfile::tempdir().unwrap();
                let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
                    pause_at: pause_at.to_string(),
                    resume_at: resume_at.to_string(),
                    ..cfg()
                });
                let inside = duty.a_member().name();
                let outside = duty.a_non_member().name();

                let mut entered = None;
                for _ in 0..30 {
                    if let Some(ev) = gov.on_sample(Some(&sample(inside, 100)), 2000, dir.path(), None)
                    {
                        entered = Some(ev);
                    }
                }
                assert_eq!(
                    entered,
                    Some(ThermalEvent::DutyCycleEntered {
                        state: inside.to_string(),
                        delay_ms: 15_000
                    }),
                    "{label}: `{inside}` is in the duty band and must enter it"
                );

                let mut exited = None;
                for _ in 0..30 {
                    if let Some(ev) =
                        gov.on_sample(Some(&sample(outside, 100)), 2000, dir.path(), None)
                    {
                        exited = Some(ev);
                    }
                }
                assert_eq!(
                    exited,
                    Some(ThermalEvent::DutyCycleExited { state: outside.to_string() }),
                    "{label}: `{outside}` is outside the duty band and must EXIT it — an exit \
                     that never fires is round 4's defect"
                );
            }
        }
        assert!(exercised >= 3, "the sweep must exercise real armed bands, got {exercised}");
    }

    /// The same, for tiers 3/4: every armed pause pair's entry witness
    /// pauses and its recovery witness resumes. Round 2's defect was
    /// exactly an entry that fired with a recovery that could not.
    #[test]
    fn every_armed_pause_band_can_be_both_entered_and_recovered_from() {
        let mut exercised = 0;
        for pause_at in THERMAL_STATES {
            for resume_at in THERMAL_STATES {
                let Some(pause) =
                    crate::thermal_bands::ThermalBands::resolve(pause_at, resume_at).pause()
                else {
                    continue;
                };
                exercised += 1;
                let label = format!("pause_at={pause_at} resume_at={resume_at}");
                let dir = tempfile::tempdir().unwrap();
                let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
                    pause_at: pause_at.to_string(),
                    resume_at: resume_at.to_string(),
                    ..cfg_tier4_disabled()
                });
                let hot = pause.entry().a_member().name();
                let cool = pause.recovery().a_member().name();
                assert_eq!(
                    gov.on_sample(Some(&sample(hot, 100)), 2000, dir.path(), None),
                    Some(ThermalEvent::Paused { state: hot.to_string() }),
                    "{label}: `{hot}` is in the pause-entry band"
                );
                let mut resumed = None;
                for _ in 0..31 {
                    if let Some(ev) = gov.on_sample(Some(&sample(cool, 100)), 2000, dir.path(), None)
                    {
                        resumed = Some(ev);
                    }
                }
                assert_eq!(
                    resumed,
                    Some(ThermalEvent::Resumed { state: cool.to_string() }),
                    "{label}: `{cool}` is in the recovery band and must clear the pause — a \
                     recovery that can never fire is round 2's defect"
                );
            }
        }
        assert!(exercised >= 3, "the sweep must exercise real armed bands, got {exercised}");
    }

    // ─── (#2774 round-6) The SAME defect shape on the TIME knobs ───
    //
    // Rounds 2-5 chased "a threshold comparison that degenerates at its
    // knob's end value" through three SEVERITY predicates and ended that
    // half structurally (`thermal_bands`). Round 6 found the identical
    // shape alive on the two unfloored TIME knobs, where the degenerate
    // value is `0` and the comparison is `accumulator >= knob`. The tests
    // below are the executable half of that fix: MF1 and MF2 as direct
    // regressions, then one sweep that enumerates every knob's boundary
    // values rather than arguing about them.

    /// MF1. `resume_hold_ms = 0` must not resume a machine that is still
    /// reading `serious`.
    ///
    /// The transition used to be gated on the ACCUMULATOR alone, and
    /// `accum >= 0` is a tautology — so the implication the code relied on
    /// ("a positive accumulator means a recovery reading was seen") broke
    /// at exactly this value. Observed before the fix, on nothing but the
    /// shipped default plus `resume_hold_ms: 0`, feeding `serious` every
    /// 2000ms: `t=2s Paused` -> `t=4s Resumed{serious}` (pace file
    /// `{"pause":false}`, and the ratchet doubled on a recovery that never
    /// happened) -> `t=6s OperatorHold{episode:2}` plus a crawl `STOP`
    /// file. Three samples from "machine is hot" to the terminal,
    /// operator-gated hold, with the machine told to run at full speed in
    /// between.
    #[test]
    fn resume_hold_ms_zero_never_resumes_a_still_hot_machine() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov =
            ThermalGovernor::new(ThermalGovernorConfig { resume_hold_ms: 0, ..cfg() });

        let mut events = Vec::new();
        for _ in 0..12 {
            if let Some(ev) =
                gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop))
            {
                events.push(ev);
            }
        }

        assert_eq!(
            events,
            vec![ThermalEvent::Paused { state: "serious".to_string() }],
            "a machine that reads `serious` on every sample has never produced a recovery \
             reading, so the ONLY event in 24s is the pause that started it"
        );
        let pace = read_pace(dir.path());
        assert_eq!(
            pace["pause"],
            serde_json::json!(true),
            "the pace file must still say PAUSED — a resumed one tells the runtime to run at \
             full speed on a machine reporting `serious`"
        );
        assert!(
            !stop.exists(),
            "no STOP file: tier 4 is reached by COUNTING EPISODES, and one unbroken pause is \
             one episode"
        );
        assert_eq!(gov.serious_episodes, 1, "one entry into `serious` is one episode");
        assert_eq!(
            gov.current_duty_delay_ms,
            15_000,
            "the ratchet is applied on RECOVERY; none happened, so the base delay stands"
        );
    }

    /// MF1's other half — the fix must not turn `0` into "never resume."
    /// `resume_hold_ms = 0` has a coherent meaning: resume on the FIRST
    /// recovery reading, no sustained hold required. (The same reading
    /// `speed_limit_hold_samples`'s `.max(1)` floor gives its own `0`.)
    /// Asserted so a later "fix" that reaches for `.max(1)` on the knob
    /// instead of the predicate has to change a test that states the
    /// intent.
    #[test]
    fn resume_hold_ms_zero_resumes_on_the_first_recovery_reading() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
            resume_hold_ms: 0,
            ..cfg_tier4_disabled()
        });
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Paused { state: "serious".to_string() })
        );
        assert_eq!(
            gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Resumed { state: "nominal".to_string() }),
            "with no hold configured, the first genuinely cool reading clears the pause"
        );
    }

    /// MF2. `max_pause_ms = 0` means UNBOUNDED — never hand off to the
    /// breaker — matching every other `0` bound in darkmux
    /// (`redis.maxlen`, `runtime.step_command_timeout_seconds`, and this
    /// block's own `episode_threshold`).
    ///
    /// The comparison used to be the bare `pause_episode_ms >=
    /// max_pause_ms`, i.e. `0 >= 0` on the first sample. Observed before
    /// the fix, with the shipped default plus `max_pause_ms: 0`: sample 1
    /// `Paused{serious}`, sample 2 `Breaker{serious}` with pace
    /// `{"pause":true,"reason":"thermal-critical","state":"serious"}` and a
    /// `thermal-critical` STOP file — on a machine that had never once
    /// reported `critical`. So the operator who asked for "rest as long as
    /// it takes" got the terminal breaker instead, under a reason word that
    /// misnamed what the hardware said.
    #[test]
    fn max_pause_ms_zero_is_unbounded_not_instant() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov =
            ThermalGovernor::new(ThermalGovernorConfig { max_pause_ms: 0, ..cfg() });

        let mut events = Vec::new();
        for _ in 0..40 {
            if let Some(ev) =
                gov.on_sample(Some(&sample("serious", 100)), 60_000, dir.path(), Some(&stop))
            {
                events.push(ev);
            }
        }

        assert_eq!(
            events,
            vec![ThermalEvent::Paused { state: "serious".to_string() }],
            "40 minutes of `serious` under an UNBOUNDED cap is still one pause and no breaker"
        );
        assert!(!stop.exists(), "an unbounded pause writes no STOP file, ever");
        assert_eq!(read_pace(dir.path())["reason"], serde_json::json!("thermal"));
    }

    /// MF2's twin, in the `None`-thermal arm — the branch that accumulates
    /// the same pause episode across gaps in OS readings, and carried its
    /// own copy of the bare comparison. Both now read
    /// `pause_episode_exhausted`, so they cannot disagree about what `0`
    /// means.
    #[test]
    fn max_pause_ms_zero_is_unbounded_across_reading_gaps_too() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov =
            ThermalGovernor::new(ThermalGovernorConfig { max_pause_ms: 0, ..cfg() });
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop)),
            Some(ThermalEvent::Paused { state: "serious".to_string() })
        );
        for _ in 0..40 {
            assert_eq!(
                gov.on_sample(None, 60_000, dir.path(), Some(&stop)),
                None,
                "a reading gap under an unbounded cap is time passing, not a breaker trip"
            );
        }
        assert!(!stop.exists());
        assert_eq!(read_pace(dir.path())["reason"], serde_json::json!("thermal"));
    }

    /// The BOUNDED reading is untouched: a real `max_pause_ms` still hands
    /// off to the breaker on schedule. Without this, "make `0` unbounded"
    /// could be satisfied by disabling the handoff outright.
    #[test]
    fn a_nonzero_max_pause_ms_still_hands_off_to_the_breaker() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
            max_pause_ms: 10_000,
            ..cfg_tier4_disabled()
        });
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop)),
            Some(ThermalEvent::Paused { state: "serious".to_string() })
        );
        let mut breaker = None;
        for _ in 0..10 {
            if let Some(ev) =
                gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop))
            {
                breaker = Some(ev);
            }
        }
        assert_eq!(
            breaker,
            Some(ThermalEvent::Breaker { state: "serious".to_string() }),
            "a FINITE cap must still hand off once the episode outlasts it"
        );
        assert!(stop.exists());
    }

    /// The class regression, stated as an ENUMERATION rather than an
    /// argument: for every knob in `ThermalGovernorConfig`, at each of its
    /// boundary values, two invariants that follow from the readings alone
    /// must hold — no knob setting may manufacture a transition the
    /// hardware never justified.
    ///
    /// - A machine reading `serious` on every sample has produced NO
    ///   recovery reading, so it may never `Resume`, and (one unbroken
    ///   pause being one episode) may never reach tier 4's
    ///   `OperatorHold`. It MAY hit the breaker — but only where
    ///   `max_pause_ms` is a finite cap the episode genuinely outlasted.
    /// - A machine reading `nominal` on every sample is cold: no pause, no
    ///   breaker, no hold, whatever the knobs say.
    ///
    /// This is the shape `thermal_bands` ended for the severity
    /// comparisons, swept over the TIME and COUNT knobs instead: every
    /// `accumulator >= knob` in the module is exercised at the knob value
    /// where it degenerates. MF1 and MF2 each fail this sweep on their own
    /// (verified by mutation), and so would a future knob that grows the
    /// same shape.
    #[test]
    fn no_knob_boundary_value_manufactures_a_transition_the_readings_never_justified() {
        // Each entry mutates ONE knob off the shipped default. `u64::MAX` /
        // `u32::MAX` are in the list because the saturating arithmetic the
        // accumulators use has a degenerate top end too, not only a zero.
        type Mutate = (&'static str, fn(&mut ThermalGovernorConfig));
        let knobs: Vec<Mutate> = vec![
            ("resume_hold_ms=0", |c| c.resume_hold_ms = 0),
            ("resume_hold_ms=1", |c| c.resume_hold_ms = 1),
            ("resume_hold_ms=MAX", |c| c.resume_hold_ms = u64::MAX),
            ("max_pause_ms=0", |c| c.max_pause_ms = 0),
            ("max_pause_ms=1", |c| c.max_pause_ms = 1),
            ("max_pause_ms=MAX", |c| c.max_pause_ms = u64::MAX),
            ("duty_delay_ms=0", |c| c.duty_delay_ms = 0),
            ("duty_delay_ms=MAX", |c| c.duty_delay_ms = u64::MAX),
            ("ratchet_factor=0", |c| c.ratchet_factor = 0),
            ("ratchet_factor=1", |c| c.ratchet_factor = 1),
            ("ratchet_factor=MAX", |c| c.ratchet_factor = u32::MAX),
            ("episode_threshold=0", |c| c.episode_threshold = 0),
            ("episode_threshold=1", |c| c.episode_threshold = 1),
            ("episode_threshold=MAX", |c| c.episode_threshold = u32::MAX),
            ("speed_limit_hold_samples=0", |c| c.speed_limit_hold_samples = 0),
            ("speed_limit_hold_samples=1", |c| c.speed_limit_hold_samples = 1),
            ("speed_limit_hold_samples=MAX", |c| c.speed_limit_hold_samples = u32::MAX),
            ("min_cpu_speed_limit_pct=0", |c| c.min_cpu_speed_limit_pct = 0),
            ("min_cpu_speed_limit_pct=MAX", |c| c.min_cpu_speed_limit_pct = u64::MAX),
        ];

        const ELAPSED_MS: u64 = 2000;
        const SAMPLES: usize = 24;

        for (label, mutate) in knobs {
            // ── A hot machine: `serious`, forever, at full clock speed. ──
            let mut config = cfg();
            mutate(&mut config);
            // `cpu_speed_limit_pct` is held at 100 so the speed-limit
            // breaker cannot fire on its own signal and confound the
            // reading-driven invariants below. `min_cpu_speed_limit_pct=MAX`
            // is the one entry where 100 is still "below the floor" — it is
            // MEANT to trip the breaker, which the allowance handles.
            let speed_floor_trips = 100 < config.min_cpu_speed_limit_pct;
            let dir = tempfile::tempdir().unwrap();
            let stop = dir.path().join("STOP");
            let mut gov = ThermalGovernor::new(config.clone());
            let mut events = Vec::new();
            for _ in 0..SAMPLES {
                if let Some(ev) =
                    gov.on_sample(Some(&sample("serious", 100)), ELAPSED_MS, dir.path(), Some(&stop))
                {
                    events.push(ev);
                }
            }

            assert!(
                !events.iter().any(|e| matches!(e, ThermalEvent::Resumed { .. })),
                "{label}: a machine that never read a recovery state must never RESUME — \
                 that is MF1's shape. events={events:?}"
            );
            // One unbroken pause is ONE episode, at every knob setting.
            // This is the crispest statement of MF1's defect: the phantom
            // resumes it produced each minted a FRESH episode, which is
            // what walked the run to tier 4 in three samples.
            assert_eq!(
                gov.serious_episodes, 1,
                "{label}: the machine crossed into `serious` exactly once, so the episode \
                 count may not exceed 1 — a higher count means a recovery was counted that \
                 the readings never contained. events={events:?}"
            );
            // Tier 4 MAY fire here, but only as the operator configured it
            // (`episode_threshold = 1`, escalate on the first episode) —
            // and then only ever on episode 1. MF1's run reached it on a
            // fabricated episode 2.
            for ev in &events {
                if let ThermalEvent::OperatorHold { episode, .. } = ev {
                    assert_eq!(
                        *episode, 1,
                        "{label}: tier 4 on an episode past the first, from a single unbroken \
                         pause — that is MF1's shape. events={events:?}"
                    );
                }
            }
            let hold_is_earned = config.tier4_enabled && config.episode_threshold == 1;
            if !hold_is_earned {
                assert!(
                    !events.iter().any(|e| matches!(e, ThermalEvent::OperatorHold { .. })),
                    "{label}: the configured episode threshold is not reachable from one \
                     episode, so tier 4 must not fire. events={events:?}"
                );
            }
            // The breaker IS allowed here, but only where the config asked
            // for it: a finite `max_pause_ms` the episode outlasted, or a
            // speed floor above the sampled 100%.
            let elapsed_total = ELAPSED_MS * SAMPLES as u64;
            let breaker_is_earned = speed_floor_trips
                || (config.max_pause_ms != 0 && config.max_pause_ms <= elapsed_total);
            if !breaker_is_earned {
                assert!(
                    !events.iter().any(|e| matches!(e, ThermalEvent::Breaker { .. })),
                    "{label}: no finite cap elapsed and no speed floor tripped, so a breaker \
                     here is manufactured — that is MF2's shape. events={events:?}"
                );
            }
            if !breaker_is_earned && !hold_is_earned {
                assert!(
                    !stop.exists(),
                    "{label}: neither terminal was earned, so nothing may drop a STOP file — \
                     an unearned breaker's says `thermal-critical`, naming a state the \
                     machine never reported"
                );
            }

            // ── A cold machine: `nominal`, forever. Nothing may fire. ──
            let dir = tempfile::tempdir().unwrap();
            let stop = dir.path().join("STOP");
            let mut gov = ThermalGovernor::new(config.clone());
            let mut cold = Vec::new();
            for _ in 0..SAMPLES {
                if let Some(ev) = gov.on_sample(
                    Some(&sample("nominal", 100)),
                    ELAPSED_MS,
                    dir.path(),
                    Some(&stop),
                ) {
                    cold.push(ev);
                }
            }
            if !speed_floor_trips {
                assert!(
                    cold.iter().all(|e| matches!(
                        e,
                        ThermalEvent::DutyCycleEntered { .. } | ThermalEvent::DutyCycleExited { .. }
                    )),
                    "{label}: a machine reading `nominal` on every sample is COLD — only tier \
                     2's duty cycle may ever fire, never a pause, breaker or hold. \
                     events={cold:?}"
                );
                assert!(!stop.exists(), "{label}: a cold machine never gets a STOP file");
            }
        }
    }

    /// The WIDE-gap case, which the round-2 predicate could also have got
    /// wrong in the other direction: `pause_at = serious`,
    /// `resume_at = nominal` means `fair` is NOT a recovery, and `nominal`
    /// is. Both halves asserted, so a future edit that makes recovery
    /// either too eager or (MF1's failure) unreachable is caught here.
    #[test]
    fn a_two_band_gap_resumes_only_at_the_configured_resume_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(ThermalGovernorConfig {
            pause_at: "serious".to_string(),
            resume_at: "nominal".to_string(),
            ..cfg_tier4_disabled()
        });
        assert!(gov.soft_tiers_armed());
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Paused { state: "serious".to_string() })
        );
        // Five minutes at `fair` — above `resume_at`, so NOT a recovery.
        for _ in 0..150 {
            let ev = gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
            assert_eq!(ev, None, "`fair` is above resume_at=nominal: {ev:?}");
        }
        // …and `nominal` held for `resume_hold_ms` IS.
        let mut resumed = None;
        for _ in 0..31 {
            if let Some(ev) = gov.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None) {
                resumed = Some(ev);
            }
        }
        assert_eq!(
            resumed,
            Some(ThermalEvent::Resumed { state: "nominal".to_string() }),
            "reaching resume_at must still recover — the disarm narrows nothing real"
        );
        // (#2774 round-4 MF1) What this test did NOT assert, and the gap
        // MF1 lived in: where the recovery LANDS. `resume_at = nominal`
        // leaves tier 2 no duty band with a reachable exit, so tier 2 is
        // disarmed and the landing is `Idle`. It used to be `DutyCycle`,
        // permanently.
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));
        assert!(pace.get("turn_delay_ms").is_none(), "no duty cycle to land in: {pace}");
    }

    // ── (#2774 review F5) Mission-scoped ladder state across dispatches ──

    #[test]
    fn a_second_units_governor_seeds_from_the_first_units_persisted_state() {
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("thermal-ladder.json");

        // Unit 1's governor: one full serious episode + recovery.
        let mut unit1 = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        unit1.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            unit1.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(unit1.ladder_summary(), ThermalLadderSummary { serious_episodes: 1, current_duty_delay_ms: 30_000 });
        assert!(ladder_file.exists(), "unit 1 must have persisted its state");

        // Unit 2 is a BRAND NEW governor (simulating the next dispatch()
        // call in the same crawl mission) — it must NOT start over.
        let unit2 = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        assert_eq!(
            unit2.ladder_summary(),
            ThermalLadderSummary { serious_episodes: 1, current_duty_delay_ms: 30_000 },
            "unit 2 must inherit unit 1's episode count and ratcheted delay — this is the \
             WHOLE POINT of F5: the ladder means the mission, not one dispatch"
        );

        // And a SECOND serious episode on unit 2 ratchets from 30_000, not
        // from the base 15_000 — proving the seed isn't just cosmetic.
        let mut unit2 = unit2;
        unit2.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            unit2.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(
            unit2.ladder_summary(),
            ThermalLadderSummary { serious_episodes: 2, current_duty_delay_ms: 60_000 }
        );
    }

    #[test]
    fn tier4_escalation_also_carries_across_dispatches() {
        // episode_threshold: 2 — unit 1 has episode 1 (ordinary pause),
        // unit 2 (a FRESH governor) must escalate straight to OperatorHold
        // on ITS FIRST episode, because the mission is already at 1.
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("thermal-ladder.json");

        let mut unit1 = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        let ev1 = unit1.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(ev1, Some(ThermalEvent::Paused { state: "serious".to_string() }));

        let mut unit2 = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        let ev2 = unit2.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(
            ev2,
            Some(ThermalEvent::OperatorHold { state: "serious".to_string(), episode: 2 }),
            "unit 2's FIRST episode is the mission's SECOND — must escalate immediately"
        );
    }

    #[test]
    fn a_later_mission_never_inherits_an_earlier_missions_ladder_state() {
        // (#2774 review F5, owner stamp) The ladder file's path is keyed on
        // the crawl MANIFEST, not the mission, and nothing removes it — so
        // re-crawling the same workspace lands on the previous mission's
        // file. Without the owner check, mission 2's FIRST `serious`
        // episode would be counted as the mission's second and escalate
        // straight into a terminal, operator-gated hold. Same hazard and
        // same remedy as the STOP file's own owner stamp (#2454).
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("thermal-ladder.json");

        let mut m1 = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        m1.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(m1.serious_episodes(), 1);

        let m2 = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-2"))
            .seeded_from_mission(Some(&ladder_file));
        assert_eq!(
            m2.ladder_summary(),
            ThermalLadderSummary { serious_episodes: 0, current_duty_delay_ms: 15_000 },
            "a DIFFERENT mission must start at zero, not inherit m-1's leftovers"
        );

        let mut m2 = m2;
        let ev = m2.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(
            ev,
            Some(ThermalEvent::Paused { state: "serious".to_string() }),
            "m-2's first episode is an ordinary pause, NOT an immediate OperatorHold"
        );
    }

    #[test]
    fn an_unattributed_run_never_matches_another_unattributed_runs_leftovers() {
        // `None == None` is deliberately NOT an owner match: two bare
        // dispatches with no mission id are not the same run, and treating
        // them as one would carry an episode count between unrelated runs.
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("thermal-ladder.json");

        let mut first = ThermalGovernor::new(cfg()).seeded_from_mission(Some(&ladder_file));
        first.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert!(ladder_file.exists());

        let second = ThermalGovernor::new(cfg()).seeded_from_mission(Some(&ladder_file));
        assert_eq!(
            second.serious_episodes(),
            0,
            "an unattributed run must not inherit another unattributed run's count"
        );
    }

    #[test]
    fn seeding_before_owned_by_never_matches() {
        // Pins the ordering `seeded_from_mission`'s own doc requires: the
        // owner comparison reads `stop_owner`, so chaining the builders the
        // other way round silently loses every carry-forward.
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("thermal-ladder.json");

        let mut unit1 = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        unit1.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        assert_eq!(unit1.serious_episodes(), 1);

        let wrong_order = ThermalGovernor::new(cfg())
            .seeded_from_mission(Some(&ladder_file))
            .owned_by(Some("crawl-m-1"));
        assert_eq!(wrong_order.serious_episodes(), 0, "seeding read an unstamped owner");

        let right_order = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        assert_eq!(right_order.serious_episodes(), 1);
    }

    #[test]
    fn a_seeded_delay_below_the_configured_base_is_floored_not_honored() {
        // A hand-edited or config-changed file must never LOWER the live
        // delay: `0` there would silently disable tier 2 for every
        // remaining unit of the mission.
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("thermal-ladder.json");
        std::fs::write(
            &ladder_file,
            r#"{"owner":"crawl-m-1","serious_episodes":3,"current_duty_delay_ms":0}"#,
        )
        .unwrap();

        let gov = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        assert_eq!(
            gov.ladder_summary(),
            ThermalLadderSummary { serious_episodes: 3, current_duty_delay_ms: 15_000 },
            "the count carries, the delay is floored at the configured base"
        );
    }

    #[test]
    fn seeded_from_mission_with_no_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("does-not-exist.json");
        let gov = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        assert_eq!(gov.ladder_summary(), ThermalLadderSummary { serious_episodes: 0, current_duty_delay_ms: 15_000 });
    }

    #[test]
    fn a_malformed_ladder_state_file_starts_fresh_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let ladder_file = dir.path().join("thermal-ladder.json");
        std::fs::write(&ladder_file, "{ not json at all").unwrap();
        let gov = ThermalGovernor::new(cfg())
            .owned_by(Some("crawl-m-1"))
            .seeded_from_mission(Some(&ladder_file));
        assert_eq!(gov.ladder_summary(), ThermalLadderSummary { serious_episodes: 0, current_duty_delay_ms: 15_000 });
    }

    #[test]
    fn seeded_from_mission_none_keeps_pre_f5_per_dispatch_scoping() {
        // The default (no mission-scoped file) must behave EXACTLY as
        // before F5 — this is the non-crawl / bare-dispatch path.
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled()); // no .seeded_from_mission at all
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert!(
            !dir.path().join("thermal-ladder.json").exists(),
            "a governor never seeded from a mission must never write one either"
        );
    }

    #[test]
    fn ladder_state_file_path_derivation_mirrors_the_stop_file_derivation() {
        let ctx = serde_json::json!({ "workspace": "my-crawl", "unit": "unit-1" });
        let stop = stop_file_path_from_record_context(Some(&ctx)).unwrap();
        let ladder = ladder_state_file_path_from_record_context(Some(&ctx)).unwrap();
        assert_eq!(stop.parent(), ladder.parent(), "same crawl root, different filename");
        assert_eq!(ladder.file_name().unwrap(), "thermal-ladder.json");

        // Non-crawl dispatches derive neither.
        assert_eq!(ladder_state_file_path_from_record_context(None), None);
        let non_crawl = serde_json::json!({ "workspace": "my-crawl" }); // no "unit"
        assert_eq!(ladder_state_file_path_from_record_context(Some(&non_crawl)), None);
    }

    // ── Tier 5: critical is unaffected by the ladder additions ──

    #[test]
    fn critical_still_trips_the_pre_existing_breaker_unconditionally() {
        // The ladder's new tiers must never intercept `critical` — it goes
        // straight to the SAME `Breaker` event tier 3/4 never touch.
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        assert_eq!(
            gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Breaker { state: "critical".to_string() })
        );
        let pace = read_pace(dir.path());
        assert_eq!(pace["reason"], serde_json::json!("thermal-critical"));
        assert!(pace.get("turn_delay_ms").is_none());
    }

    #[test]
    fn critical_trips_immediately_even_mid_duty_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        assert_eq!(
            gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Breaker { state: "critical".to_string() })
        );
    }

    // ── A reading gap (`None`) mid-duty-cycle: heartbeat, not a transition ──

    #[test]
    fn a_reading_gap_while_duty_cycling_keeps_the_heartbeat_alive_without_exiting() {
        let dir = tempfile::tempdir().unwrap();
        let mut gov = ThermalGovernor::new(cfg());
        for _ in 0..30 {
            gov.on_sample(Some(&sample("fair", 100)), 2000, dir.path(), None);
        }
        // A gap must not be read as "recovered to nominal" — the exit hold
        // resets, it doesn't advance toward exit.
        assert_eq!(gov.on_sample(None, 2000, dir.path(), None), None);
        let pace = read_pace(dir.path());
        assert_eq!(pace["pause"], serde_json::json!(false));
        assert_eq!(pace["turn_delay_ms"], serde_json::json!(15_000), "the instruction survives the gap");
    }

    // ── (#2774 round-3 MF3) The battery-governor call site, pinned at the
    //    SOURCE — a unit test cannot reach it ──

    /// Round 1's F1 finding was that `dispatch_internal.rs` gated the
    /// battery governor's stand-down on `is_pacing()` (any non-`Idle`
    /// state, including tier 2's `DutyCycle`, which writes `pause: false`)
    /// instead of `is_pausing()`. Round 2 fixed the call site and added
    /// `f1_regression_battery_governor_still_acts_while_thermal_duty_cycles`
    /// — but that test RE-IMPLEMENTS the call site inside its own body, so
    /// it pins the FUNCTION, not the WIRING. Proven in round 3: reverting
    /// the argument at the call site left the whole crate green at
    /// 1827/1827.
    ///
    /// The commit message claimed that naming the parameter
    /// `thermal_pausing` meant "the call site cannot silently drift back."
    /// Rust does not check argument NAMES, so that is a convention, not a
    /// check. This is the check, in the shape this repo already uses for a
    /// call site no unit test can reach (see
    /// `duty_cycle_turn_delay_key_matches_the_runtime_reader` and
    /// `pace_file_path_matches_runtime_out_base` above).
    #[test]
    fn the_battery_governor_call_site_gates_on_is_pausing() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = manifest_dir.join("src/dispatch_internal.rs");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

        // Narrow to the `battery_governor.on_sample(...)` argument list, so
        // the assertions below are about the ARGUMENT and not about the
        // module's prose (which names both predicates, deliberately).
        let call_start = source
            .find("battery_governor.on_sample(")
            .unwrap_or_else(|| panic!("{} no longer calls battery_governor.on_sample", path.display()));
        let rest = &source[call_start..];
        let call_end = rest
            .find(") {")
            .unwrap_or_else(|| panic!("could not find the end of the on_sample call in {}", path.display()));
        let args = &rest[..call_end];

        assert!(
            args.contains("thermal_governor.is_pausing()"),
            "dispatch_internal.rs must gate the battery governor's stand-down on the thermal \
             governor's `is_pausing()` — `is_pacing()` also covers tier 2's DutyCycle, which \
             writes `pause: false`, and gating on it silently drops a real battery-critical \
             pause for the whole duration of a duty-cycle episode (#2774 F1). Argument list \
             found:\n{args}"
        );
        assert!(
            !args.contains("is_pacing()"),
            "the battery-governor call site must not pass `is_pacing()` — see #2774 F1. \
             Argument list found:\n{args}"
        );
    }

    // ── (#2774 round-3 C8) One event, one reason word, in BOTH artifacts ──

    #[test]
    fn the_tier4_stop_file_says_episode_limit_not_critical() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(ThermalGovernorConfig { episode_threshold: 1, ..cfg() })
            .owned_by(Some("crawl-m-8"));
        let ev = gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), Some(&stop));
        assert!(matches!(ev, Some(ThermalEvent::OperatorHold { .. })), "{ev:?}");

        let body = std::fs::read_to_string(&stop).unwrap();
        assert!(
            body.starts_with(STOP_FILE_REASON_EPISODE_LIMIT),
            "the STOP file is the artifact an operator `cat`s first — it must name the SAME \
             reason the pace file carries (`thermal-episode-limit`), not the breaker's \
             `thermal-critical`, for a machine that never reported `critical`. Got: {body:?}"
        );
        assert_eq!(
            read_pace(dir.path())["reason"],
            serde_json::json!(STOP_FILE_REASON_EPISODE_LIMIT),
            "…and the pace file must agree with it"
        );
        assert!(body.contains("mission=crawl-m-8"), "the owner line survives: {body:?}");

        // (#2774 round-4 C2) …and the READER carries it through. C8 fixed
        // the writer; the one consumer still split the body for `mission=`
        // and dropped the rest, so a tier-4 hold printed "the thermal
        // breaker's STOP file is present (#2109)" and stamped a
        // `UnitOutcome.reason` saying the breaker tripped — on a machine
        // that never reported `critical`. Same artifact disagreement,
        // relocated one consumer out.
        let hold = stop_hold_for_mission(&stop, "crawl-m-8").expect("this mission is held");
        assert_eq!(
            hold,
            StopFileHold {
                scope: StopHold::ThisMission,
                reason: Some(STOP_FILE_REASON_EPISODE_LIMIT.to_string()),
            }
        );
        let what = hold.what_happened();
        assert!(what.contains("episode-count hold"), "{what}");
        assert!(
            !what.contains("breaker"),
            "a count-based escalation must not be described as a breaker trip: {what}"
        );
    }

    /// The other half of C2: a REAL breaker trip still reads as one.
    #[test]
    fn a_breaker_stop_file_reads_as_a_breaker_trip() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg()).owned_by(Some("crawl-m-9"));
        gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        let hold = stop_hold_for_mission(&stop, "crawl-m-9").unwrap();
        assert_eq!(hold.reason.as_deref(), Some(STOP_FILE_REASON));
        assert!(hold.what_happened().contains("thermal breaker tripped"));
    }

    /// A human's bare `touch` carries no reason, and a reason token this
    /// reader will not render (too long, or not lowercase-ASCII-plus-dash)
    /// reads the same way — the stop is still honored, and nothing off disk
    /// is echoed into the operator's terminal. See [`stop_file_reason`].
    #[test]
    fn an_unrenderable_or_absent_reason_still_honors_the_stop() {
        let dir = tempfile::tempdir().unwrap();
        for (body, label) in [
            ("", "an empty file"),
            ("\n", "a bare newline"),
            ("mission=m-1\n", "an owner with no reason"),
            ("THERMAL-CRITICAL mission=m-1\n", "a non-lowercase token"),
            ("\u{1b}[2Kdarkmux: mission=m-1\n", "a terminal-control payload"),
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa mission=m-1\n",
                "an over-long token",
            ),
        ] {
            let stop = dir.path().join(format!("STOP-{}", label.replace(' ', "-")));
            std::fs::write(&stop, body).unwrap();
            let hold = stop_hold_for_mission(&stop, "m-1")
                .unwrap_or_else(|| panic!("{label}: the stop must still be honored"));
            assert_eq!(hold.reason, None, "{label}: must not be rendered");
            assert_eq!(hold.what_happened(), "a stop was recorded with no reason", "{label}");
        }
    }

    #[test]
    fn the_breaker_stop_file_still_says_thermal_critical() {
        let dir = tempfile::tempdir().unwrap();
        let stop = dir.path().join("STOP");
        let mut gov = ThermalGovernor::new(cfg());
        gov.on_sample(Some(&sample("critical", 100)), 2000, dir.path(), Some(&stop));
        let body = std::fs::read_to_string(&stop).unwrap();
        assert!(
            body.starts_with(STOP_FILE_REASON),
            "a hardware-critical trip keeps its own word: {body:?}"
        );
    }

    // ── (#2774 round-3 C6) The ladder file is a read-modify-MAX, so a
    //    concurrent unit cannot walk the mission counter backwards ──

    #[test]
    fn a_concurrent_units_first_episode_cannot_lower_the_mission_ladder() {
        let dir = tempfile::tempdir().unwrap();
        let ladder = dir.path().join("thermal-ladder.json");

        // Unit B is constructed (and seeds from an empty file) BEFORE unit
        // A does any work, and stays alive — the real crawl shape when two
        // rules run on different model identifiers. The clobber window is
        // B's whole LIFETIME, not an instant.
        let mut unit_b = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-9"))
            .seeded_from_mission(Some(&ladder));

        let mut unit_a = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-9"))
            .seeded_from_mission(Some(&ladder));
        for _ in 0..3 {
            unit_a.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
            for _ in 0..31 {
                unit_a.on_sample(Some(&sample("nominal", 100)), 2000, dir.path(), None);
            }
        }
        let after_a: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&ladder).unwrap()).unwrap();
        assert_eq!(after_a["serious_episodes"], serde_json::json!(3), "{after_a}");
        assert_eq!(after_a["current_duty_delay_ms"], serde_json::json!(120_000), "{after_a}");

        // B's FIRST episode. Its own counters are 1 and 15_000 — a blind
        // absolute write would put those on disk and discard A's ratchet.
        unit_b.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let after_b: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&ladder).unwrap()).unwrap();
        assert_eq!(
            after_b["serious_episodes"],
            serde_json::json!(3),
            "the mission's episode count must never go backwards: {after_b}"
        );
        assert_eq!(
            after_b["current_duty_delay_ms"],
            serde_json::json!(120_000),
            "…nor may a concurrent unit discard the mission's ratchet: {after_b}"
        );
    }

    #[test]
    fn a_foreign_owners_ladder_file_is_overwritten_not_merged_into() {
        // The max must not reach ACROSS missions: a stale file from last
        // week's crawl of the same workspace would otherwise escalate this
        // mission through the back door, which is the whole reason the
        // owner is stamped in (#2454 / F5).
        let dir = tempfile::tempdir().unwrap();
        let ladder = dir.path().join("thermal-ladder.json");
        std::fs::write(
            &ladder,
            serde_json::json!({
                "owner": "crawl-LAST-WEEK",
                "serious_episodes": 99,
                "current_duty_delay_ms": 9_000_000u64,
            })
            .to_string(),
        )
        .unwrap();

        let mut gov = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-10"))
            .seeded_from_mission(Some(&ladder));
        assert_eq!(gov.serious_episodes(), 0, "a foreign owner's state is not mine");
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&ladder).unwrap()).unwrap();
        assert_eq!(after["serious_episodes"], serde_json::json!(1), "{after}");
        assert_eq!(after["owner"], serde_json::json!("crawl-m-10"), "{after}");
    }

    // ── (#2774 round-3 C10) The persisted shape carries a schema version ──

    #[test]
    fn the_ladder_state_file_carries_a_schema_version_and_reads_one_without() {
        let dir = tempfile::tempdir().unwrap();
        let ladder = dir.path().join("thermal-ladder.json");
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-11"))
            .seeded_from_mission(Some(&ladder));
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&ladder).unwrap()).unwrap();
        assert_eq!(
            written["schema_version"],
            serde_json::json!(LADDER_STATE_SCHEMA_VERSION),
            "every darkmux persisted shape carries one (cross-system contract 5): {written}"
        );

        // Lenient on read: a pre-#2774-round-3 file has no version and
        // must still seed, or upgrading the binary silently resets every
        // in-flight mission's ladder.
        let old = dir.path().join("old-ladder.json");
        std::fs::write(
            &old,
            serde_json::json!({
                "owner": "crawl-m-11",
                "serious_episodes": 2,
                "current_duty_delay_ms": 60_000u64,
            })
            .to_string(),
        )
        .unwrap();
        let seeded = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-11"))
            .seeded_from_mission(Some(&old));
        assert_eq!(
            seeded.ladder_summary(),
            ThermalLadderSummary { serious_episodes: 2, current_duty_delay_ms: 60_000 },
            "a version-less file must still carry forward"
        );
    }

    // ── (#2774 round-3 C12) The ladder file's own filesystem hazards ──

    #[test]
    fn a_symlink_at_the_ladder_path_seeds_nothing_and_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("planted.json");
        std::fs::write(
            &elsewhere,
            serde_json::json!({
                "owner": "crawl-m-12",
                "serious_episodes": 99,
                "current_duty_delay_ms": 15_000u64,
            })
            .to_string(),
        )
        .unwrap();
        let ladder = dir.path().join("thermal-ladder.json");
        std::os::unix::fs::symlink(&elsewhere, &ladder).unwrap();

        let gov = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-12"))
            .seeded_from_mission(Some(&ladder));
        assert_eq!(
            gov.serious_episodes(),
            0,
            "a symlink at the ladder path must not feed a planted episode count into the ladder"
        );

        // The WRITE refuses it too (the same guard the STOP file next to it
        // uses), so the planted file's contents are left alone.
        let mut gov = gov;
        gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None);
        let planted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&elsewhere).unwrap()).unwrap();
        assert_eq!(
            planted["serious_episodes"],
            serde_json::json!(99),
            "the write must not have followed the symlink either: {planted}"
        );
    }

    #[test]
    fn a_directory_at_the_ladder_path_does_not_wedge_the_governor() {
        // It cannot carry state forward — but it must not panic, must not
        // stop the dispatch, and (see `persist_ladder_state`) says so once.
        let dir = tempfile::tempdir().unwrap();
        let ladder = dir.path().join("thermal-ladder.json");
        std::fs::create_dir_all(&ladder).unwrap();
        let mut gov = ThermalGovernor::new(cfg_tier4_disabled())
            .owned_by(Some("crawl-m-13"))
            .seeded_from_mission(Some(&ladder));
        assert_eq!(
            gov.on_sample(Some(&sample("serious", 100)), 2000, dir.path(), None),
            Some(ThermalEvent::Paused { state: "serious".to_string() }),
            "the governor's own decisions are unaffected by a broken ladder file"
        );
        assert!(ladder.is_dir(), "and the directory is left exactly as found");
    }
}

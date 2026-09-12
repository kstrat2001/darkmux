//! `darkmux mission status` — the global mission-control read (#829).
//!
//! Every other `mission` verb is a mutation or a single-shot op;
//! none answers "show me the whole board, what's drifted, what needs closing
//! out." This is that read — the CLI twin of the viewer's missions lens,
//! headless and scriptable. It completes the `<noun> status` family that
//! `flow status` and `model status` already established; it is deliberately
//! NOT folded into `doctor` (doctor = runtime/substrate health; this = the
//! work-tracking board).
//!
//! READ-ONLY by design (operator-sovereignty, #44): it surfaces drift and
//! prints copy-pasteable reconcile commands, but never MUTATES OPERATOR
//! STATE — no `Mission`/`Phase`/`Task`/`Step` JSON, no config, no profile
//! registry. The operator (or the frontier reading `--json`) runs the
//! suggested commands.
//!
//! The LOCAL board is computed purely from the durable mission + phase JSON
//! (the loader), so it works offline with no Redis/flow dependency — exactly
//! what a session-start housekeeping cue needs. (#1711) On top of that,
//! `run()` ALSO reads the shared flow stream via
//! `darkmux_serve::fleet_records_for_runs()` + the narrow
//! `darkmux_serve::peer_mission_runs()` (the SAME substrate `darkmux run
//! list` already calls — #1905 — itself the CLI twin of the daemon's
//! `/runs` fix, #1705) — so a mission executing on a PEER machine, which has
//! no durable JSON here at all, still surfaces as a thin "observed, not
//! owned" row instead of being structurally invisible. This is best-effort
//! and degrades legibly (`darkmux_serve::source_state::SourceState`): a
//! standalone install with no `DARKMUX_REDIS_URL` gets `Off`, and the LOCAL
//! half of the board — every section, row, and the final rollup line for a
//! board with no peer rows — prints byte-identical output to before #1711.
//!
//! **Honest scope of "read-only" once Redis is configured.** When
//! `config.redis.enabled` (or `DARKMUX_REDIS_URL`) is set, resolving the
//! Redis connection reads the macOS Keychain
//! (`darkmux_flow::redis_url()` → `keychain_redis_password()`), which — like
//! every other Keychain-secret read in this codebase (#1311) — emits a
//! `credential-read:*` liveness marker to `~/.darkmux/liveness/`. That is
//! diagnostic telemetry, not operator state, and it is the SAME cost
//! `darkmux run list` already pays on a fleet-configured machine today —
//! #1711 does not introduce a new mechanism, it makes `mission status`
//! subject to the one `darkmux run list` already accepted. Filed as a
//! separate concern: whether that reaper-less liveness directory should
//! itself be bounded (out of scope here).

use anyhow::Result;
use std::collections::BTreeMap;

use crate::crew;
use crate::crew::types::{Mission, MissionStatus, Phase, PhaseStatus};
use darkmux_serve::{
    source_state::SourceState, AbandonReason, DispatchSessionEvidence, Run, RunStatus,
};
use darkmux_types::{config_access, style};

/// A flagged inconsistency on one mission, with concrete reconcile commands.
/// Kept data-only (no IO) so `detect_drift` is unit-testable.
#[derive(Debug, Clone, PartialEq)]
struct Drift {
    kind: &'static str,
    detail: String,
    suggest: Vec<String>,
}

/// Per-mission rollup the renderer consumes.
struct MissionView<'a> {
    m: &'a Mission,
    total: usize,
    /// Phases that are `Complete` on disk AND clean — a degraded one is
    /// EXCLUDED here and counted in [`MissionView::degraded`] instead
    /// (#2406). See that field for why the two are separable at all.
    complete: usize,
    /// (#2406) Phases the envelope calls `Degraded`: terminal, real output
    /// shipped, some of it did not.
    ///
    /// These are `PhaseStatus::Complete` ON DISK and always will be —
    /// `Degraded` deliberately drives `lifecycle::phase_complete` (a
    /// degraded phase IS terminal-and-produced-output, and `PhaseStatus`
    /// has no third terminal to drive it to). So the fact lives only in
    /// the mission's `envelope.json` (`PhaseOutcome::outcome`), which is
    /// where [`degraded_phase_ids`] reads it from. Without this split the
    /// board reported a phase with 2 completed and 2 cascade-abandoned
    /// steps as plainly `complete`: #2406's display fix moved that phase
    /// from wrong-and-loud (`abandoned`) to wrong-and-quiet.
    degraded: usize,
    running: usize,
    planned: usize,
    abandoned: usize,
    drifts: Vec<Drift>,
    /// (#2299) What the config declared vs what was minted — `None` for a
    /// run minted by a path that never prunes (crew-of-one, review) or one
    /// that predates the report.
    graph: Option<crew::mission_config::prune::PruneReport>,
}

impl MissionView<'_> {
    /// (#2406) Phases that FINISHED AND PRODUCED — the progress column's
    /// numerator. A degraded phase belongs here: it is terminal and it
    /// shipped real output. The mix line beside it is what distinguishes
    /// the two; the bar is a "how far along" reading and must not regress
    /// when a phase turns out to be mixed.
    fn done(&self) -> usize {
        self.complete + self.degraded
    }
}

fn is_terminal(s: PhaseStatus) -> bool {
    matches!(s, PhaseStatus::Complete | PhaseStatus::Abandoned)
}

/// (#2406) The phase ids this mission's `envelope.json` records as
/// `Degraded`. Best-effort by design, exactly like `load_graph_report`
/// beside it in `run()`: a mission with no envelope yet (never finalized),
/// an unreadable one, or one written before `PhaseOutcomeKind::Degraded`
/// existed all yield an empty set, and the board reports what disk says —
/// never an error, never a fabricated bucket.
fn degraded_phase_ids(mission_id: &str) -> std::collections::BTreeSet<String> {
    crew::lifecycle::load_envelope(mission_id)
        .ok()
        .flatten()
        .map(|env| {
            env.phases
                .iter()
                .filter(|p| p.outcome == crew::envelope::PhaseOutcomeKind::Degraded)
                .map(|p| p.phase_id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// (#1569 packet A) The daemon URL a mission id links to.
///
/// The id IS percent-encoded: mission ids are not guaranteed path-safe, and
/// an unencoded one would emit extra path segments that resolve to the wrong
/// route or to nothing.
///
/// **What encoding does and does not buy** (#1593 gate — the first version of
/// this comment implied more): it makes the URL *well-formed*, not
/// *resolvable*. The daemon's `mission_graph_json_handler` gates on
/// `is_valid_catalog_id` (`[A-Za-z0-9-_.:]`, ≤128 chars) AFTER decoding, so an
/// id containing `/`, `@`, `?`, `#`, non-ASCII, or over 128 chars produces a
/// correct-looking link to a guaranteed 400. Encoding is still right — a
/// well-formed link that 400s beats a malformed one that hits an unrelated
/// route — but it is not a fix for out-of-charset ids.
///
/// Every real id on disk today is slugified and passes. The live constraint
/// is for #1563: whatever charset that fix mints for pr-review ids must stay
/// inside `is_valid_catalog_id`, or these links go dead for exactly the ids it
/// introduces. `:` is in the allowed set and round-trips correctly; `/` and
/// `@` are not.
///
/// Encoding is inline rather than a new dependency, per this repo's
/// small-dep convention: the rule needed here is one line of RFC 3986
/// unreserved-set logic, not a crate.
fn mission_url(base: &str, id: &str) -> String {
    let encoded: String = id
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    // `base` always carries its trailing slash (see `viewer_link_base`).
    format!("{base}mission/{encoded}/graph")
}

/// A deep link to another panel, but ONLY when this process is rendering
/// into the console (`DARKMUX_PANEL`, set by the serve daemon's panel
/// spawner) and is not already the target.
///
/// The point is that the ADVICE has to match the surface. "`--all` for every
/// mission" is actionable in a terminal and a dead end in a panel, where
/// there is no prompt to type it at — the operator hit exactly that. The verb
/// fixes it here rather than the viewer pattern-matching this text, because
/// that matching is the twin drift `/panel/:id` exists to kill: the flag and
/// its link are one edit, in one file.
fn panel_deep_link(link_base: &str, target: &str) -> Option<String> {
    let current = std::env::var("DARKMUX_PANEL").ok()?;
    if current == target {
        return None;
    }
    // `link_base` always carries its trailing slash (see `viewer_link_base`).
    Some(format!("{link_base}#lens=console&panel={target}"))
}

/// (#1612) What the row CALLS a mission.
///
/// The id is a mint artifact — `dispatch-code-reviewer-1785589698-5d6a-0` —
/// and on a phone it ate two thirds of the width for the least informative
/// thing on the line, pushing progress off the edge. Meanwhile every mission
/// already carries a `description` (92/92 populated on this board) and it was
/// going unshown.
///
/// The descriptions come in two measured shapes, which is what makes this a
/// lookup rather than a truncation (92 real missions: 59 / 33):
///   - operator-named missions carry prose — "PR review — kstrat2001/darkmux@…";
///   - auto-minted ones carry "dispatch: code-reviewer", i.e. the role.
///
/// Both beat the id, and the `dispatch: ` prefix is noise once the row's own
/// glyph already distinguishes a single-role dispatch from a graph, so it is
/// stripped. The id is not lost: the row is an OSC 8 link, so it is one click
/// away, and any row that needs an id typed carries it verbatim in the drift
/// suggestion printed directly beneath it.
///
/// Falls back to the id when a description is genuinely absent — an id is a
/// poor label but never a wrong one.
///
/// (#2406 CONSIDER 6) For a CONFIG-launched mission (`review`, `crawl`,
/// `coder-phase`, …) `m.description` is never actually operator prose: the
/// generic launcher's mint site
/// (`mission_launch::ensure_mission_and_phases_with_provenance_and_start_payload`)
/// always passes `None` for its own per-launch `description` parameter, so
/// `Mission.description` resolves to `config.description` — the config
/// document's own ~200-word explanation of what the launcher does, meant to
/// be read once in `mission config show`, not truncated into a board title.
/// A live `review` board read that whole paragraph, ellipsized to a handful
/// of words, as the row's name — never "Review." `config_title` resolves
/// the ACTUAL config's declared `name` field instead, when the mission's
/// `spec.config_id` names one; falls through to the description-based
/// lookup for anything without a resolvable config (a `dispatch <role>`
/// crew-of-one, or a hand-authored mission), which is exactly the shape the
/// two-measured-shapes doc above already covers correctly.
#[cfg(test)]
fn display_label(m: &Mission) -> String {
    display_label_cached(m, &mut BTreeMap::new())
}

/// The cached twin of [`display_label`] — see [`config_title_cached`] for
/// why the render loop needs this instead of the plain version.
fn display_label_cached(m: &Mission, cache: &mut BTreeMap<String, Option<String>>) -> String {
    if let Some(name) = config_title_cached(m, cache) {
        return name;
    }
    let d = m.description.trim();
    if d.is_empty() {
        return m.id.clone();
    }
    d.strip_prefix("dispatch: ").unwrap_or(d).to_string()
}

/// (#2406 CONSIDER 6) The originating `MissionConfig`'s declared `name`
/// (e.g. `"Review"`), when `m` was minted from one. `None` for anything
/// without a resolvable config — a `dispatch <role>` crew-of-one names its
/// `spec.config_id` `"dispatch"`, which is not a loadable config id, so
/// `mission_config::load::load` refuses it and this returns `None`,
/// leaving `display_label` on its existing description-based path. A thin,
/// uncached wrapper around [`config_title_cached`] — fine for a one-off
/// call (a unit test, a single mission lookup), never for a board render.
#[cfg(test)]
fn config_title(m: &Mission) -> Option<String> {
    config_title_cached(m, &mut BTreeMap::new())
}

/// (#2406 CONSIDER 6, round 2) The memoized, render-scoped twin of
/// [`config_title`]. A board full of `review` rows used to pay for a fresh
/// `mission_config::load::load("review")` — a disk/embedded read — for
/// EVERY row, up to three times per row (`plan_layout`'s width pass, the
/// row's own name, its description note). `cache` is one `BTreeMap` shared
/// across a whole render (built once in `run()`), so each distinct
/// `config_id` is resolved at most once no matter how many rows share it.
///
/// `"dispatch"` — the crew-of-one sentinel `config_id`, never a real
/// loadable config — is short-circuited BEFORE touching `load` at all:
/// every `dispatch <role>` row used to pay for a full `list_ids()` scan
/// just to build a "not found" error `.ok()` immediately discarded.
fn config_title_cached(m: &Mission, cache: &mut BTreeMap<String, Option<String>>) -> Option<String> {
    let config_id = m.spec.as_ref()?.config_id.as_str();
    if config_id == "dispatch" {
        return None;
    }
    if let Some(cached) = cache.get(config_id) {
        return cached.clone();
    }
    let name = crew::mission_config::load::load(config_id).ok().map(|lc| lc.config.name);
    cache.insert(config_id.to_string(), name.clone());
    name
}

/// (#2406 CONSIDER 6) The mission's own `description` as a one-line note,
/// printed BENEATH the title row — only when `config_title` already won the
/// title above, so this is never a second copy of the same text. Truncated
/// to its first sentence: `review.json`'s description runs to roughly 200
/// words of launcher documentation, and printing it whole on every row would
/// reintroduce the exact wall-of-text this fix exists to keep off the
/// board.
#[cfg(test)]
fn description_note(m: &Mission) -> Option<String> {
    description_note_cached(m, &mut BTreeMap::new())
}

/// The cached twin of [`description_note`] — see [`config_title_cached`].
fn description_note_cached(m: &Mission, cache: &mut BTreeMap<String, Option<String>>) -> Option<String> {
    config_title_cached(m, cache)?; // description is already the title; no second line
    let d = m.description.trim();
    if d.is_empty() {
        return None;
    }
    let sentence = first_sentence(d);
    if sentence.is_empty() {
        return None;
    }
    Some(cap_note(sentence, DESCRIPTION_NOTE_CAP_CHARS))
}

/// Board-row budget for [`description_note`] — well under a normal
/// terminal width even after the ~8-column indent it prints at.
const DESCRIPTION_NOTE_CAP_CHARS: usize = 120;

/// The first SENTENCE of `d` — a `.` counts as a terminator only when it is
/// followed by whitespace or end-of-string, never when the very next
/// character is non-whitespace.
///
/// The real `review.json` description proved why the naive "split on the
/// first '.'" version was wrong: it contains the literal step-kind name
/// `` `review.*` ``, whose embedded `.` sits well before the sentence's
/// actual terminator — so the naive split produced "...its ten Tier-3
/// `review." on a live board, cut off mid-backtick-quoted identifier.
/// Requiring trailing whitespace (or end-of-string) after the `.` treats
/// that as ordinary punctuation inside a token rather than a sentence
/// boundary, and finds the terminator several dozen characters later
/// instead.
fn first_sentence(d: &str) -> &str {
    for (i, ch) in d.char_indices() {
        if ch == '.' {
            let after = &d[i + 1..];
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                return d[..=i].trim();
            }
        }
    }
    d.trim()
}

/// Truncate `s` to at most `max` chars, ending "…", never mid-word. The
/// true first sentence of a config's description (per [`first_sentence`])
/// can still run to hundreds of characters — `review.json`'s is well over
/// 300 — so finding the real sentence boundary is necessary but not
/// sufficient; this is what actually keeps a board row from becoming this
/// feature's own second wall of text. Backs off to the last whitespace
/// inside the cut rather than hard-truncating at `max`, so the ellipsis
/// never lands mid-identifier (e.g. mid backtick-quoted code, mid word).
fn cap_note(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    let cut = match truncated.rfind(char::is_whitespace) {
        Some(i) => &truncated[..i],
        None => &truncated,
    };
    let cut = cut.trim_end_matches(|c: char| c == '.' || c == ',' || c.is_whitespace());
    format!("{cut}…")
}

/// (#1612) Single-role dispatch vs multi-phase graph, in one column-safe glyph.
///
/// The operator asked whether a row could say "graph" versus "role" and floated
/// an emoji. Emoji are DOUBLE-WIDTH, and every column budget in this module is
/// exact arithmetic over `chars().count()` — one emoji would silently overflow
/// every row it appeared on. These two are single-width, so the distinction is
/// free: it costs no column at all, replacing a marker that was already there.
fn kind_glyph(total_phases: usize) -> &'static str {
    if total_phases > 1 {
        "◆"
    } else {
        "•"
    }
}

/// (#1612) A stable short handle for a mission, or `None` when the id has no
/// discriminating token.
///
/// The operator's ask was "a number" — something short that tells two otherwise
/// identical rows apart. Both minting paths already end in one: `mint_run_id`
/// emits `<config>-<secs>-<hex6>` (`review-1785400940-136e76`), and the older
/// dispatch path emits `dispatch-<role>-<secs>-<hex>-<n>`.
///
/// So: scan segments right-to-left for the first all-hex run of at least
/// `MIN_HANDLE_HEX` chars. A HEURISTIC, deliberately — the two formats are not
/// one, and hand-authored ids (`doom-loop-m4`, `104-daemon-observability`) match
/// nothing and correctly yield `None`. It is display-only and drops to nothing
/// on no match, so a wrong guess costs a column, never correctness.
fn short_handle(id: &str) -> Option<&str> {
    id.rsplit('-').find(|seg| {
        seg.len() >= MIN_HANDLE_HEX && seg.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// Shortest run of hex that reads as a deliberate discriminator rather than an
/// accident. Below 4, ordinary id fragments (`-0`, `-5`, a version `-2`) start
/// matching and the column fills with noise.
const MIN_HANDLE_HEX: usize = 4;

/// (#1562) Whether `m` is a machine-minted run instance (a launch of a
/// SHIPPED config, or a `dispatch <role>` crew-of-one) rather than the
/// operator's own named work. The default board hides minted runs behind a
/// summary line — see `hidden_run_summary` — because on a real 59-mission
/// board they outnumber named missions (32 of 59) and drown the work the
/// operator actually cares about.
///
/// (#1717) The predicate itself now lives on [`Mission::is_minted_run`]
/// (`darkmux_crew::types`), next to the `Mission`/`MissionSpecOrigin` types
/// it operates on. It was module-private here (not even `pub(crate)`), so
/// the radio answering seat's grounding block — a sibling command module in
/// the same binary crate, not a submodule of this one — had no way to
/// resolve it and re-derived a divergent copy instead. `darkmux-crew` is a
/// library crate both modules already depend on, so this is the shared
/// home the NEXT consumer resolves from too, rather than a fourth
/// reinvention. See that method's doc for the full discriminator +
/// fallback rules (spec.origin first, id-shape fallback for pre-#1503
/// records).
fn is_minted_run(m: &Mission) -> bool {
    m.is_minted_run()
}

/// (#1612) Compact "how long ago", in at most `AGE_COLS` columns.
///
/// One unit, never two — `3d` not `3d 4h`. The board answers "what needs me
/// now"; the difference between 3d and 3d4h has never changed that answer, and
/// the second unit costs the columns the name needs. Rounds DOWN, so a row
/// never claims to be older than it is.
fn relative_age(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        86_400..=2_591_999 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

/// (#1711) Missions this machine can SEE via the shared flow stream but does
/// not OWN — the CLI twin of #1705's `/runs`/`/flow-missions` fix. A
/// mission's durable `Mission`/`Phase` JSON lives only on the machine that
/// ran it, so a peer's mission is structurally invisible to
/// `crew::loader::load_missions()` no matter how much of its work crosses
/// the flow stream.
///
/// Calls `darkmux_serve::peer_mission_runs` — the NARROW half of #1705's
/// aggregation, taking the caller's own already-loaded `known_mission_ids`
/// rather than reloading `Mission`/`Phase` JSON or rebuilding a `Run` for
/// every LOCAL mission the way the full `darkmux_serve::build_runs` (used
/// by `darkmux run list`) does. `mission status` already has its own local
/// mission set in hand for the board above; paying for `build_runs`'s local
/// half a second time here measured as the dominant cost of this command
/// (#1711 review finding) for no benefit — this board never uses it.
///
/// Sorted here (`darkmux_serve::peer_mission_runs` folds a `HashMap`, so
/// its own order is not stable run to run) by the SAME recency-first key
/// `darkmux run list` sorts by (`run_activity` in `run_list.rs`) — one
/// consistent "what's running on the fleet" ordering across both CLI
/// surfaces, and a `--json` payload that doesn't reshuffle between two
/// identical daemon-panel polls.
fn peer_mission_runs(
    flows_dir: &std::path::Path,
    fleet: &[serde_json::Value],
    known_mission_ids: &std::collections::HashSet<String>,
) -> Vec<Run> {
    let mut peer = darkmux_serve::peer_mission_runs(flows_dir, fleet, known_mission_ids);
    peer.sort_by(|a, b| peer_activity(b).cmp(&peer_activity(a)).then_with(|| a.id.cmp(&b.id)));
    peer
}

/// `updated_ts || completed_ts || started_ts || 0` — the same fallback
/// chain `run_list.rs::run_activity` uses, so a peer row and a local run
/// row agree on what "most recently active" means.
fn peer_activity(r: &Run) -> u64 {
    r.updated_ts.or(r.completed_ts).or(r.started_ts).unwrap_or(0)
}

/// (#1711) The status word for one peer-observed mission row.
///
/// Distinguishes a mission that was DELIBERATELY torn down (a `mission
/// abort` record was seen) from one that simply went quiet with no
/// terminal record at all — the "rostered but silent" case named in the
/// issue is not the same fact as "aborted", and darkmux describes rather
/// than adjudicates: it must not claim a peer mission failed or was
/// abandoned on purpose when all it actually knows is that the stream went
/// quiet (the peer could be asleep, offline, or between heartbeats — this
/// board attempted one read of the shared stream and reports what came
/// back, nothing more).
fn peer_status_word(status: RunStatus, reason: Option<AbandonReason>) -> &'static str {
    match (status, reason) {
        (RunStatus::Running, _) => "running",
        (RunStatus::Complete, _) => "complete",
        (RunStatus::Abandoned, Some(AbandonReason::Aborted)) => "aborted",
        // No terminal record and not currently live — silent, not a verdict.
        (RunStatus::Abandoned, _) => "silent (no terminal record seen)",
        (RunStatus::Error, _) => "error",
        (RunStatus::Planned, _) => "planned",
        (RunStatus::Unparseable, _) => "unparseable",
    }
}

/// (#1711) One line naming an INCOMPLETE fleet read, or `None` when the
/// answer is whole. Mirrors `darkmux run list`'s own `fleet_warning`
/// (`run_list.rs`) state-for-state — kept as a separate small function
/// (this one folds into `style::warn` for this file's renderer; that one is
/// a bare `eprintln`) but deliberately says the SAME thing for the SAME
/// state: two surfaces answering "what's running on the fleet" with
/// different words for the same outage would just be a quieter version of
/// the disagreement #1711 was filed over.
///
/// `Off` and `Ok` are both `None` — `Off` is a correctly-configured
/// standalone machine (warning would be the bug), `Ok` is a complete
/// answer. Never interpolates the source's `detail`: a Redis error can
/// carry the connection string, which may embed a password (#661 Slice 5).
fn fleet_scope_note(state: &SourceState) -> Option<String> {
    match state {
        SourceState::Ok | SourceState::Off => None,
        SourceState::Stale { age_ms, .. } => Some(format!(
            "fleet: could not reach the shared stream; showing a peer-mission snapshot {} old — \
             this board's fleet view may be missing recent work",
            format_age_span(*age_ms / 1000)
        )),
        SourceState::Unavailable { .. } => Some(
            "fleet: could not reach the shared stream and nothing was cached — this board covers \
             this machine's own missions only"
                .to_string(),
        ),
    }
}

/// One-unit span (`Ns`/`Nm`/`Nh`/`Nd`), rounding down. Same shape as
/// `relative_age` above and `run_list.rs::format_span` — kept local rather
/// than shared (four lines, and the two callers format for different
/// renderers; see that module's own precedent for the same call).
fn format_age_span(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// (#1711) The peer-mission section: missions this machine can SEE via the
/// shared flow stream but does not OWN. Thinner than a local row by
/// necessity — no phase graph, no per-task detail, because that structure
/// only exists on the machine that ran it (see [`peer_mission_runs`]'s doc).
/// A no-op when `peer` is empty, which includes every standalone install —
/// this is what keeps the local-only board byte-identical to before #1711.
fn print_peer_missions(peer: &[Run], now: u64, width: Option<usize>) {
    if peer.is_empty() {
        return;
    }
    println!();
    let header = format!(
        "OBSERVED ON THE FLEET ({}) — seen via the shared flow stream, not owned by this machine",
        peer.len()
    );
    for line in wrap_indented(&header, 0, width) {
        println!("{}", style::dim(&line));
    }
    let id_w = peer.iter().map(|r| r.id.chars().count()).max().unwrap_or(0).clamp(1, 40);
    let machine_w = peer
        .iter()
        .map(|r| r.machine.as_deref().unwrap_or("unknown machine").chars().count())
        .max()
        .unwrap_or(0)
        .clamp(1, 24);
    for r in peer {
        let id = ellipsize(&r.id, id_w);
        let machine = ellipsize(r.machine.as_deref().unwrap_or("unknown machine"), machine_w);
        let ts = r.updated_ts.or(r.completed_ts).or(r.started_ts).unwrap_or(now);
        let age = relative_age(now, ts);
        let status = peer_status_word(r.status, r.abandoned_reason);
        println!(
            "  ◇ {id:<id_w$}  {machine:<machine_w$}  {age:>age_w$}  {status}",
            age_w = AGE_COLS,
        );
    }
}

/// Pure drift detection for one mission given its phases. `now` and
/// `stale_days` are passed in (rather than read internally) so the function
/// stays IO-free and unit-testable with fixed timestamps — see the module
/// doc. Load-bearing inconsistencies:
///   - an ACTIVE/PAUSED mission whose phases are ALL terminal with at least
///     one complete — done, just never finalized.
///   - (#1230 Packet 5) an ACTIVE mission with ZERO complete phases whose
///     `started_ts` is older than `stale_days` — the `doom-loop-m4` case
///     (0/4 phases for ~20 days, no drift surfaced by either check above).
///   - (#2682) an ACTIVE mission with a Running phase whose OWN dispatch
///     session shows no evidence of life, by the same staleness rule
///     `darkmux run list` already applies to this exact mission on this
///     exact machine — see [`running_phase_session_drift`]'s own doc.
///
/// (#2406) The former third bullet here — a PLANNED phase with an
/// earlier-in-mission-order Abandoned phase, flagged "can never run" — was
/// RETIRED: see the "RETIRED (#2406)" comment block above `run()` (kept in
/// place as a marker of what was removed and why) for the real mission that
/// proved the linear-order assumption false.
///
/// (#1463) The old "CLOSED mission with a non-terminal phase" arm RETIRED:
/// `mission finalize` / `mission abort` now reconcile EVERY phase to a
/// terminal status as part of closing the mission, so a Finalized mission
/// with an open phase is no longer a reachable state to detect. (Its `phase
/// complete`/`phase abandon` reconcile hints went with the retired `phase`
/// family; the surviving hints point at `mission finalize` / `mission abort`.)
///
/// `local_status`/`local_evidence` (#2682, widened by the #2682 fix-pass)
/// are this mission's own [`RunStatus`] and, when that reads `Abandoned`,
/// WHICH of the three genuinely different situations produced it — both as
/// `darkmux_serve::local_dispatch_status` computed them (the SAME
/// computation `darkmux run list` uses via `mission_to_run`). `local_status`
/// is `None` only when the caller could not classify this mission at all;
/// `local_evidence` is `None` whenever `local_status` isn't `Abandoned`, or
/// when the caller has no mission. Passed in rather than re-derived here for
/// the same IO-free-and-testable reason `now`/`stale_days` already are, and
/// — more importantly — because re-deriving `session_is_live` against this
/// module's own flow read would be a SECOND, independently written liveness
/// rule that could quietly drift from the one `darkmux run list`/the viewer
/// already ship (see `run()`'s call site doc).
fn detect_drift(
    m: &Mission,
    phases: &[&Phase],
    live_steps: &BTreeMap<String, Vec<String>>,
    local_status: Option<RunStatus>,
    local_evidence: Option<DispatchSessionEvidence>,
    now: u64,
    stale_days: u64,
) -> Vec<Drift> {
    let mut out = Vec::new();
    let open: Vec<&&Phase> = phases.iter().filter(|s| !is_terminal(s.status)).collect();
    let complete = phases.iter().filter(|s| s.status == PhaseStatus::Complete).count();
    let all_terminal = !phases.is_empty() && open.is_empty();

    if matches!(m.status, MissionStatus::Active | MissionStatus::Paused)
        && all_terminal
        && complete > 0
    {
        out.push(Drift {
            kind: "done-not-finalized",
            detail: "all phases are terminal — the mission looks done but is still open"
                .to_string(),
            suggest: vec![format!("darkmux mission finalize {}", m.id)],
        });
    }

    if let Some(d) = stale_active_drift(m, complete, now, stale_days) {
        out.push(d);
    }

    if let Some(d) = running_phase_session_drift(m, phases, local_status, local_evidence) {
        out.push(d);
    }

    // (#2406) `unreachable_phase_drifts` retired — see its doc comment
    // above for why the phase-order heuristic it used was simply wrong.
    out.extend(live_step_drifts(m, phases, live_steps));

    out
}

/// (#2310 fix-loop C4 / S4-C4) Drift BELOW the phase level: a step left
/// `Planned`/`Running` under a phase — or a mission — that has already
/// reached a terminal status.
///
/// Every check above stops at the phase, which is why the board reported
/// "clean" through the whole S4-1/S4-2 class: a Finalized mission whose p2
/// read `Complete` on disk while four of its steps were still `Planned`
/// passed every phase-level rule there was. Both halves of that
/// contradiction are now named, because they are reconciled differently in
/// principle even though one command fixes both today: a terminal PHASE
/// holding a live step means the phase closed around work it never
/// accounted for (the #1504 reconcile did not run, or ran before the step
/// existed on disk); a terminal MISSION holding one means the whole run
/// closed over it. A SIGKILLed run leaves exactly this shape too.
///
/// `live_steps` maps a phase id to the ids of its non-terminal steps —
/// passed in rather than loaded here so this function stays IO-free and
/// unit-testable against a hand-built board, the same discipline the rest
/// of this module's drift rules follow.
fn live_step_drifts(
    m: &Mission,
    phases: &[&Phase],
    live_steps: &BTreeMap<String, Vec<String>>,
) -> Vec<Drift> {
    let mut out = Vec::new();
    let mission_terminal =
        matches!(m.status, MissionStatus::Finalized | MissionStatus::Aborted);

    let mut in_terminal_phase: Vec<String> = Vec::new();
    let mut under_terminal_mission: Vec<String> = Vec::new();
    for phase in phases {
        let Some(ids) = live_steps.get(phase.id.as_str()) else { continue };
        if ids.is_empty() {
            continue;
        }
        if is_terminal(phase.status) {
            in_terminal_phase.extend(ids.iter().cloned());
        } else if mission_terminal {
            // Counted ONCE: a live step under a terminal phase is already
            // named above, and on a Finalized mission it is the same
            // instance of the same problem, not two.
            under_terminal_mission.extend(ids.iter().cloned());
        }
    }

    // (#1582) ONE drift per KIND, never one per instance — the same rule
    // the (now-retired, #2406) `unreachable_phase_drifts` used to follow:
    // this is one problem with N instances, and the instances ride
    // `detail`.
    if !in_terminal_phase.is_empty() {
        out.push(Drift {
            kind: "phase-terminal-live-step",
            detail: format!(
                "{} step(s) are still Planned/Running under a phase that already reached a \
                 terminal status: {}",
                in_terminal_phase.len(),
                in_terminal_phase.join(", ")
            ),
            suggest: vec![format!("darkmux mission finalize {}", m.id)],
        });
    }
    if mission_terminal && !under_terminal_mission.is_empty() {
        out.push(Drift {
            kind: "mission-terminal-live-step",
            detail: format!(
                "mission is {:?} but {} step(s) are still Planned/Running: {}",
                m.status,
                under_terminal_mission.len(),
                under_terminal_mission.join(", ")
            ),
            suggest: vec![format!("darkmux mission finalize {}", m.id)],
        });
    }
    out
}

/// (#2310 fix-loop C4) The non-terminal step ids of each of `phases`,
/// keyed by phase id — the input [`live_step_drifts`] judges.
///
/// Deliberately loads only where a rule could FIRE: a phase that is itself
/// terminal, or any phase when the MISSION is terminal. An Active
/// mission's own Running phase is where live steps are supposed to be, and
/// reading every one of them on every board render would buy nothing.
/// Best-effort — a phase whose steps can't be read contributes no entry
/// rather than a false "clean".
fn live_steps_for(m: &Mission, phases: &[&Phase]) -> BTreeMap<String, Vec<String>> {
    let mission_terminal = matches!(m.status, MissionStatus::Finalized | MissionStatus::Aborted);
    let mut out = BTreeMap::new();
    for phase in phases {
        if !(mission_terminal || is_terminal(phase.status)) {
            continue;
        }
        let Ok(steps) = crew::lifecycle::load_steps_for_phase(&m.id, &phase.id) else { continue };
        let live: Vec<String> = steps
            .iter()
            .filter(|s| {
                matches!(s.status, crew::types::NodeStatus::Planned | crew::types::NodeStatus::Running)
            })
            .map(|s| s.id.clone())
            .collect();
        if !live.is_empty() {
            out.insert(phase.id.clone(), live);
        }
    }
    out
}

/// An Active mission with zero Complete phases, stalled for `stale_days`
/// or longer since `started_ts`. A mission that hasn't started yet
/// (`started_ts: None`) can't be judged stale — fails closed, same
/// discipline `reachable` uses for a dangling dependency reference.
fn stale_active_drift(m: &Mission, complete: usize, now: u64, stale_days: u64) -> Option<Drift> {
    if m.status != MissionStatus::Active || complete > 0 {
        return None;
    }
    let started = m.started_ts?;
    let age_days = now.saturating_sub(started) / 86_400;
    if age_days < stale_days {
        return None;
    }
    Some(Drift {
        kind: "stale-active",
        detail: format!(
            "mission has been Active for {age_days} day(s) with zero phases complete \
             (staleness threshold: {stale_days} day(s))"
        ),
        // (#1582) One entry PER COMMAND, not one entry whose rationale
        // mentions two more commands in prose. Only the pre-`#` segment gets
        // the never-wrapped verbatim treatment, so an `abort`/`finalize`
        // buried in the rationale was word-wrapped with a 10-space indent
        // injected mid-command — unpasteable, which is the one thing the
        // #1569 rule exists to prevent. The prose now only explains the
        // CHOICE between them; the commands themselves are copyable lines.
        //
        // (#1665) The first suggestion used to be `mission status --json`,
        // but `board_json`'s per-mission shape carries only status COUNTS
        // (`total`/`complete`/`running`/…) — never phase ids or per-phase
        // statuses. A suggestion promising phase detail that command can't
        // deliver is the same class of overclaim this audit exists to
        // catch. `mission debrief <id> --json` genuinely emits
        // `phases[].{id,description,status,reason}` (`coder_phase::debrief`)
        // for any mission regardless of status, so it's the command that
        // actually answers "what's going on in each phase" before choosing
        // abort vs finalize.
        suggest: vec![
            format!(
                "darkmux mission debrief {id} --json   # inspect the phase details first",
                id = m.id
            ),
            format!(
                "darkmux mission abort {id}   # …then this, to tear the stalled mission down",
                id = m.id
            ),
            format!(
                "darkmux mission finalize {id}   # …or this instead, if the work is actually done",
                id = m.id
            ),
        ],
    })
}

/// (#2682, corrected by the #2682 fix-pass) An ACTIVE mission with a
/// `Running` phase whose board-visible status disagrees with what
/// `darkmux run list` reports for the SAME mission — the "SIGKILLed mission
/// still reads clean" gap between this board and `run list`.
///
/// `darkmux_serve::local_dispatch_status` (the SAME per-mission computation
/// `mission_to_run` runs for `darkmux run list` and the viewer's missions
/// lens — see that function's own doc) already applies `session_is_live`
/// to this exact mission's own flow records, on this exact machine, and
/// — when it lands on `Abandoned` — names WHICH of three genuinely
/// different situations produced it (`local_evidence`,
/// [`DispatchSessionEvidence`]). Both are handed to this function rather
/// than re-derived here — see `detect_drift`'s own doc for why a second,
/// independently-written liveness rule is exactly the drift this issue is
/// about closing, not reopening one level down.
///
/// **Why the evidence matters (the fix-pass review's own finding).** The
/// original version of this rule fired on `local_status ==
/// Some(RunStatus::Abandoned)` alone and printed ONE fixed sentence — "this
/// mission's dispatch session shows no evidence of life" — for every case.
/// That sentence is only true for [`DispatchSessionEvidence::StaleNoTerminal`].
/// It is FALSE for the other two roads `mission_run_status_and_evidence` can
/// take to the same `RunStatus::Abandoned`:
///   - [`DispatchSessionEvidence::NoAttributableSession`] — no session was
///     ever attributed to this mission at all. Fires NOTHING now; see the
///     next paragraph.
///   - [`DispatchSessionEvidence::RecordedEnd`] — darkmux POSITIVELY
///     recorded this mission's session ending (a `session.end` crash/kill/
///     timeout close-edge). "No evidence of life" describes an absence;
///     this is an observed fact. Abort is a reasonable option here — the
///     mission being torn down was actually seen.
///
/// **Why `NoAttributableSession` fires NOTHING (#2682 fix-pass round 2,
/// MUST FIX 1).** Re-wording that arm was not enough: two probes measured
/// at the round-1 head still FIRED, and both were false alarms.
///   - Probe A2 — an Active mission, `started_ts` 25 minutes ago, one
///     Running phase, a step on disk, zero flow records. That is the
///     ORDINARY state of a mission parked at a sign-off gate.
///   - Probe E3 — records written THIS SECOND under a session id that
///     names two missions and is therefore refused as ambiguous (#1918/
///     #2487), mission 90 minutes old. The mission is demonstrably alive.
///
/// Both fired because the firing condition — `Active` + a Running phase +
/// no attributable session + older than `stale_after_ms()` (20 minutes at
/// default config) — never mentions a dispatch at all. Every Active
/// mission whose dispatches fell outside `RUNS_FLOW_SCAN_WINDOW_DAYS`
/// fires it PERMANENTLY, and `attention_rollup` counts drifts
/// kind-agnostically, so each one removes the board's clean checkmark for
/// good.
///
/// `NoAttributableSession` carries ZERO dispatch-liveness information,
/// which is this rule's entire subject — and in Probe E3's shape it is an
/// honest statement about darkmux's own ATTRIBUTION layer, not about the
/// mission, so rendering it as mission drift blames the mission for a
/// flow-emitter defect. It is dropped here rather than re-worded again.
/// (The ambiguity case is still worth surfacing SOMEWHERE — `darkmux
/// doctor` is the right home for a flow-attribution defect, not the
/// mission board. Filed as #2691, deliberately not built here.)
/// `RecordedEnd` and `StaleNoTerminal` are genuine dispatch observations
/// and stay, which is what keeps the `kind` string
/// (`running-phase-session-dead`) accurate for every arm that survives.
///
/// Fires ONLY when:
///   - `m.status == MissionStatus::Active` — checked explicitly by THIS
///     function, not inherited from `mission_run_status_and_evidence`.
///     (#2682 fix-pass review CONSIDER 1 corrected a prior version of this
///     doc that claimed a `Paused` mission's `local_status` can never read
///     `Abandoned` at all — FALSE: the all-terminal/`RecordedEnd` branch
///     there runs BEFORE that function's own Paused early-return, so a
///     Paused mission with a `session.end`-terminated session DOES read
///     `Abandoned`, `DispatchSessionEvidence::RecordedEnd` included. What
///     IS true, and what this guard actually relies on, is narrower: the
///     STALENESS gate specifically (silence read as abandonment) is never
///     applied to a Paused mission — deliberately idle is not the same
///     fact as silent. This rule's own `m.status != Active` check is what
///     keeps it quiet for a Paused mission either way, independent of
///     which branch `local_status` took to get there.
///   - at least one phase reads `Running` — a mission with no Running phase
///     has nothing this rule is about.
///   - `local_status == Some(RunStatus::Abandoned)` — the SAME verdict
///     `darkmux run list` renders for this mission today. `RunStatus::
///     Error`/`Unparseable`/etc. are real terminal signals of their own and
///     are left to whatever surfaces those, not folded into this rule.
///   - `local_evidence` is an actual dispatch OBSERVATION — `RecordedEnd`
///     or `StaleNoTerminal`. `NoAttributableSession` (and a `None` the
///     caller never named) fire nothing; see above.
///
/// Describes, never adjudicates — matching the posture `peer_status_word`'s
/// own doc states: the wording says what was OBSERVED and never claims the
/// mission crashed, failed, or should be torn down. The suggested commands
/// are reconcile OPTIONS, not a verdict — the same "debrief first, then
/// choose" shape `stale_active_drift` already uses above.
///
/// **Scope, stated exactly (#2682 fix-pass review MUST FIX 4; re-derived
/// after round 2's MUST FIX 1).** This rule closes the ONE disagreement
/// #2682 was filed over — an Active mission with a `Running` phase whose
/// `run list` status is `Abandoned` AND whose evidence is an actual
/// dispatch observation — and deliberately does NOT attempt board/`run
/// list` parity for every OTHER shape. Left silent on purpose, because
/// each would need its own reasoning about what "drift" even means for
/// that shape, not a mechanical widen of this rule.
///
/// The numbers below are NOT hand-counted. They are produced by
/// `board_vs_run_list_disagreement_matrix_is_exactly_as_documented` in
/// this module's own test suite, which sweeps 4 `MissionStatus` × 5 phase
/// shapes × 5 flow shapes = 100 rows through the REAL pair
/// (`darkmux_serve::local_dispatch_status` → `detect_drift`) and asserts
/// every figure here, so a change on either side fails that test rather
/// than silently rotting this paragraph. **33 rows** disagree — `run list`
/// reads them `Abandoned` while this rule stays silent:
///   - Active mission, phase Planned/Complete/Abandoned/no-phases, × the 3
///     `Abandoned`-producing flow shapes (12 rows) — this rule only fires
///     for a Running phase. PLUS the Running-phase row whose evidence is
///     `NoAttributableSession` (1 row), silent by round 2's MUST FIX 1
///     above. **13 rows.**
///   - Finalized mission, phase Planned/Running/Abandoned, × all 5 flow
///     shapes (15 rows) — `run list` renders a Finalized mission with no
///     successful envelope as `Abandoned` (`mission_finalized_status`'s
///     `Ok(None)` arm, which ignores flow records entirely); a Finalized
///     mission is CLOSED by construction, and "the board should also flag
///     it as dead" is a different, unexamined claim this PR does not make.
///   - Paused mission with a recorded `session.end`, × all 5 phase shapes
///     (5 rows) — `run list` shows `Abandoned` for a mission the board
///     correctly shows `Paused`; genuinely a display disagreement, but
///     distinct in kind from the "Running phase, dead session" gap this
///     issue named, and not fixed here.
///
/// **Two counting subtleties, so a recount doesn't come out wrong.**
///   1. The naive sweep returns **58**, not 33. The extra 25 are the whole
///      `MissionStatus::Aborted` block (5 phase × 5 flow shapes), and they
///      are NOT disagreements: an aborted mission's row carries
///      `abandoned_reason = Aborted`, which `run_list::subtitle_for`
///      renders as the literal word "aborted" — the same thing the board
///      itself shows for a mission the operator tore down. Splitting on
///      the REASON is what turns 58 into 33.
///   2. These count rows where THIS RULE is silent. The stricter reading —
///      no drift of ANY kind on the row — gives **29** (raw 54), because
///      an Active/Paused mission holding a Complete phase already draws
///      `done-not-finalized`. The matrix test asserts both numbers.
///
/// Narrowing the claim to exactly this, rather than silently shipping a
/// partial fix under the original issue's full title, is a deliberate
/// choice — the remaining rows are real and worth a follow-up, not a
/// gap this PR is unaware of.
fn running_phase_session_drift(
    m: &Mission,
    phases: &[&Phase],
    local_status: Option<RunStatus>,
    local_evidence: Option<DispatchSessionEvidence>,
) -> Option<Drift> {
    if m.status != MissionStatus::Active {
        return None;
    }
    if local_status != Some(RunStatus::Abandoned) {
        return None;
    }
    let running: Vec<&str> =
        phases.iter().filter(|p| p.status == PhaseStatus::Running).map(|p| p.id.as_str()).collect();
    if running.is_empty() {
        return None;
    }

    let cross_check = format!(
        "darkmux run list --json   # cross-check this machine's own dispatch-session read for {id}",
        id = m.id
    );
    let debrief = format!(
        "darkmux mission debrief {id} --json   # inspect what actually happened",
        id = m.id
    );
    let abort =
        format!("darkmux mission abort {id}   # …if the work is in fact dead", id = m.id);

    let (fact, suggest) = match local_evidence {
        // (#2682 fix-pass round 2 MUST FIX 1) STAY SILENT — no drift at
        // all. No session, real or ambiguous, is attributable to this
        // mission, so there is no dispatch-liveness observation here, and
        // dispatch liveness is this rule's ENTIRE subject. See this
        // function's own doc ("Why `NoAttributableSession` fires nothing")
        // for the two measured probes that forced this arm out.
        None | Some(DispatchSessionEvidence::NoAttributableSession) => return None,
        // (#2682 fix-pass MUST FIX 5) darkmux positively recorded this
        // session ending — a fact, not an absence.
        Some(DispatchSessionEvidence::RecordedEnd) => (
            "darkmux recorded this mission's dispatch session ENDING (a crash/kill/timeout \
             close-edge, not a clean finish) — matching the Abandoned verdict `darkmux run list` \
             reports for this mission"
                .to_string(),
            vec![cross_check, debrief, abort],
        ),
        Some(DispatchSessionEvidence::StaleNoTerminal) => (
            "this mission's dispatch session shows no evidence of life — no terminal record seen \
             (the same staleness rule `darkmux run list` reports this mission Abandoned under)"
                .to_string(),
            vec![cross_check, debrief, abort],
        ),
    };

    Some(Drift {
        kind: "running-phase-session-dead",
        detail: format!("phase(s) {} read Running, but {fact}", running.join(", ")),
        suggest,
    })
}

// RETIRED (#2406). `unreachable_phase_drifts` used to flag any Planned
// phase sitting after an Abandoned one, on the theory that phases gate
// strictly linearly by `Mission.phase_ids` order — and it suggested a
// copy-pasteable `mission abort <id> --phase <blocked>` to tear the "dead"
// phase down.
//
// That theory is false. The launcher gates a phase's TASKS only by their
// own `depends_on`/`run_on` declarations (see `scheduler::stranded_reason`
// and `mission_launch::lazy_close_prior_phases`), which are per-TASK and
// document-wide, not "every phase depends on every phase before it in
// sequence." A real run (`review-1788656497-cf872b`, filed against this
// issue) had `review` Abandoned and `deliver` sitting Planned for 545s —
// completely legally, since `deliver`'s tasks named no dependency inside
// `review` — and this rule told the operator `deliver` "can never run"
// with a command that would have aborted it mid-flight. An operator who
// trusted the suggestion would have destroyed a delivery that was, in
// fact, about to complete.
//
// A correct replacement would need to mirror the scheduler's own
// reachability computation: load EVERY task across the WHOLE mission
// (`depends_on` ids are document-wide, not phase-scoped), derive each
// dependency's status from its steps, and evaluate `run_on` acceptance —
// substantial production logic this read-only reporting module has no
// business re-deriving from a snapshot of `Mission`+`Phase` records alone
// (this module doesn't even load `Task`s today). Duplicating that logic
// here risks the exact same class of bug — a false confident answer to a
// question the board doesn't actually have enough data to answer — so
// per the #2406 decision this drift is deleted outright rather than
// reimplemented against phase order. If a real gating signal becomes
// available from the mission store alone (e.g. a persisted reachability
// verdict written by the scheduler itself), reintroduce it keyed on that.

/// Entry from main.rs's dispatch. `--json` emits a structured board for the
/// frontier / CI; otherwise a grouped, colorized human board ending with the
/// aggregated suggested-next-steps.
///
/// `limit` caps rows PER SECTION (not per board) so a long finalized history
/// can't push the active work off screen; `all` lifts the cap. `None` selects
/// the per-section defaults (see `default_limit`) — an explicit `Some(n)`
/// applies uniformly, because a number the operator typed outranks one the
/// system derived (#44). `--json` is never paginated: a machine reader wants
/// the whole board, and trimming it would make the structured output lie about
/// what exists (#1569).
///
/// `missions_only` (#1709) is MEMBERSHIP, not pagination: it filters
/// machine-minted run instances out, leaving the missions the operator
/// named. The default is `false` — the board answers "what's recent" across
/// everything, and the named-only list is the other tab. `all` and
/// `missions_only` are orthogonal: `--missions --all` means every named
/// mission, unpaginated. Like `limit`, it does not touch `--json`.
pub fn run(json: bool, limit: Option<usize>, all: bool, missions_only: bool) -> Result<i32> {
    let unlimited = all || limit == Some(0);
    let missions = crew::loader::load_missions()?;
    let phases = crew::loader::load_phases()?;
    let now = now_unix();
    let stale_days = config_access::mission_stale_active_days();

    // Bucket phases by mission_id once.
    let mut by_mission: BTreeMap<&str, Vec<&Phase>> = BTreeMap::new();
    for s in &phases {
        by_mission.entry(s.mission_id.as_str()).or_default().push(s);
    }

    // (#1711) Fetched BEFORE the per-mission view loop below (moved up from
    // its original position after that loop — see the "known_mission_ids"
    // paragraph there for why the ORIGINAL #1711 design deliberately never
    // re-loaded Mission/Phase JSON here, and `local_run_status`'s own doc
    // just below for why #2682 now pays that cost anyway): missions this
    // machine can SEE via the shared flow stream but does not OWN.
    // `fleet_records_for_runs()` degrades to an empty vec + `SourceState::Off`
    // on a standalone install with no `DARKMUX_REDIS_URL`, so this costs
    // nothing there. See [`peer_mission_runs`]'s doc for why THAT call reuses
    // #1705's narrow aggregation rather than re-deriving it.
    let known_mission_ids: std::collections::HashSet<String> =
        missions.iter().map(|m| m.id.clone()).collect();
    let flows_dir = config_access::flows_dir();
    let fleet = darkmux_serve::fleet_records_for_runs();

    // (#2682, replaced by the #2682 fix-pass) This mission's OWN
    // dispatch-session status — and, when it reads `Abandoned`, WHICH of
    // three genuinely different situations produced it
    // ([`DispatchSessionEvidence`]) — exactly as `darkmux run list`/the
    // viewer's missions lens already compute it (`mission_to_run` →
    // `mission_run_status_and_evidence`, which applies `session_is_live`
    // against this machine's own flow records). Consumed by `detect_drift`
    // below so this board and `run list` read the SAME value for the same
    // mission by construction, rather than two independently-derived
    // opinions that usually — but not always — agree.
    //
    // **Superseeds calling the FULL `darkmux_serve::build_runs` and
    // filtering its output to `known_mission_ids`** (the original #2682
    // shape). That filter turned out to be untested dead weight — the
    // fix-pass review deleted it and 93 tests stayed green — because it
    // isn't what scopes this map to local missions; looping over `missions`
    // (this function's OWN already-loaded snapshot) is what does that,
    // structurally. `darkmux_serve::local_dispatch_status` shares the exact
    // session-pool + verdict code `mission_to_run` uses (see that
    // function's own doc), so this is the SAME judgment, computed more
    // narrowly and more cheaply: it never builds the other `Run` attributes
    // (machine/role/model/timestamps) this board doesn't read, and it takes
    // `&missions` rather than reloading Mission/Phase JSON a second time —
    // one fewer snapshot than the original design, not one more.
    let local_dispatch_status: std::collections::HashMap<
        String,
        (RunStatus, Option<DispatchSessionEvidence>),
    > = darkmux_serve::local_dispatch_status(&missions, &flows_dir, &fleet.records);

    let mut views: Vec<MissionView> = missions
        .iter()
        .map(|m| {
            let ss: Vec<&Phase> = by_mission.get(m.id.as_str()).cloned().unwrap_or_default();
            // (#2406) The envelope's degraded set, applied ONLY over a
            // phase that disk agrees is `Complete` — the same
            // monotone-authority shape the graph lens uses
            // (`mission_graph.rs::phase_display_status`): the envelope may
            // refine a persisted `Complete` into `Degraded`, and may never
            // overwrite any other persisted terminal. Keeps a stale or
            // hand-edited envelope from inventing a bucket disk disagrees
            // with, and keeps the four display buckets summing to `total`.
            let degraded_ids = degraded_phase_ids(&m.id);
            let is_degraded =
                |s: &&&Phase| s.status == PhaseStatus::Complete && degraded_ids.contains(&s.id);
            MissionView {
                total: ss.len(),
                complete: ss
                    .iter()
                    .filter(|s| s.status == PhaseStatus::Complete && !is_degraded(s))
                    .count(),
                degraded: ss.iter().filter(is_degraded).count(),
                running: ss.iter().filter(|s| s.status == PhaseStatus::Running).count(),
                planned: ss.iter().filter(|s| s.status == PhaseStatus::Planned).count(),
                abandoned: ss.iter().filter(|s| s.status == PhaseStatus::Abandoned).count(),
                drifts: {
                    let (local_status, local_evidence) = local_dispatch_status
                        .get(&m.id)
                        .map(|(s, e)| (Some(*s), *e))
                        .unwrap_or((None, None));
                    detect_drift(
                        m,
                        &ss,
                        &live_steps_for(m, &ss),
                        local_status,
                        local_evidence,
                        now,
                        stale_days,
                    )
                },
                graph: crew::lifecycle::load_graph_report(&m.id).ok().flatten(),
                m,
            }
        })
        .collect();
    views.sort_by(board_order);

    let peer = peer_mission_runs(&flows_dir, &fleet.records, &known_mission_ids);
    let fleet_complete = matches!(fleet.state, SourceState::Ok | SourceState::Off);

    // (#1562, restated for #1709) `--json` is deliberately NEVER filtered —
    // not by `--missions`, not by anything — because this branch returns
    // before `board_partition` even runs. A machine reader always gets the
    // whole board (`record exhaustively, display selectively`: the filter is
    // display-only). `--limit`/pagination already followed this same rule.
    if json {
        return run_json(&views, &peer, &fleet.state);
    }

    // Resolved once, above the early return, so every prose line in this
    // renderer — including the empty-board hint — wraps to the same width.
    let width = style::terminal_width();
    // (#1711) A peer mission means there IS something on the board, even
    // with zero local missions — "no missions yet, propose one" would be
    // actively wrong advice while a peer's mission is running. Fall through
    // to the normal renderer instead, which prints an empty local section
    // set, the peer section, and the fleet-scoped rollup.
    if views.is_empty() && peer.is_empty() {
        // (#1582) The prose wraps; the command does not. Same rule the drift
        // suggestions follow, for the same reason — this is the one command a
        // brand-new operator will copy, and it is the worst possible one to
        // break across a line with an indent injected into the middle.
        for line in wrap_indented("no missions yet — propose one with:", 2, width) {
            println!("{}", style::dim(&line));
        }
        println!("  {} darkmux mission propose", style::dim("→"));
        if let Some(note) = fleet_scope_note(&fleet.state) {
            println!();
            for line in wrap_indented(&note, 0, width) {
                println!("{}", style::warn(&line));
            }
        }
        return Ok(0);
    }

    // (#1709) RECENT-FIRST default, filter on request — the inversion of
    // #1562's named-first rule.
    //
    // #1562 was solving a real problem (minted runs outnumber named missions
    // and drown them), but it solved it by answering the wrong question. The
    // board's default now includes run instances, because "what's recent" is
    // the question an operator actually brings to a status board; "which
    // missions did I name" is a FILTER they ask for when they want it
    // (`--missions`), the other tab of the same view.
    //
    // Lived failure that forced the flip: with zero open missions, the
    // FINALIZED section — documented in `default_limit` as "recent-history
    // context, not the question the board answers" — WAS the entire board,
    // and its top row was frozen on the last named mission finalized 8 days
    // earlier. Meanwhile a full day of reviews and panel commands showed up
    // as a single grey "+61 run instances" footer. The board was accurate and
    // useless at the same time.
    let (visible, hidden) = board_partition(&views, missions_only);

    println!(
        "{}",
        style::header(&format!(
            "mission status — {} mission{}",
            visible.len(),
            if visible.len() == 1 { "" } else { "s" }
        ))
    );
    // (#1569 packet A) Resolved ONCE per board, not per row: on a hub/peer
    // this may spawn `tailscale serve status --json`, and doing that 82 times
    // for an 82-mission board would be absurd. It short-circuits to loopback
    // without spawning when the machine declares itself standalone, or when
    // no links will be emitted at all.
    //
    // NB the old "isn't a TTY" spelling of that second case stopped being
    // true in B1: a panel spawn is a pipe but sets CLICOLOR_FORCE, so it DOES
    // resolve — bounded by the daemon's own panel cache.
    let link_base = darkmux_doctor::viewer_link_base(8765);
    let all_link = panel_deep_link(&link_base, "mission-status-all");
    // The link is one affordance for the whole board, not one per section:
    // it goes to the same place from every group, and Active + Paused +
    // Finalized all overflowing would otherwise stack three identical rows.
    let mut all_link_shown = false;

    // Section membership first, so the layout can be planned from exactly the
    // rows that will be printed (and stay aligned across every section).
    let groups: Vec<(MissionStatus, Vec<&MissionView>)> =
        [
            MissionStatus::Active,
            MissionStatus::Paused,
            MissionStatus::Finalized,
            // (#1627) Its own section, last: a torn-down mission is terminal but
            // is NOT a success, and folding it under FINALIZED is what let 6 of
            // 51 phase-bearing missions read as finished work that never ran.
            MissionStatus::Aborted,
        ]
            .into_iter()
            .map(|group| (group, visible.iter().filter(|v| v.m.status == group).copied().collect()))
            .filter(|(_, g): &(_, Vec<&MissionView>)| !g.is_empty())
            .collect();

    let shown_counts: Vec<usize> = groups
        .iter()
        .map(|(group, g)| {
            if unlimited {
                g.len()
            } else {
                limit.unwrap_or_else(|| default_limit(*group)).min(g.len())
            }
        })
        .collect();
    // (#2406 CONSIDER 2) One cache for the WHOLE render — shared between this
    // layout pass and the row-print loop below, which is what actually
    // collapses the redundant per-row `mission_config::load::load` calls a
    // review-heavy board used to pay for (up to 3 per printed row: once
    // here, once for the row's own name, once for its description note).
    let mut config_name_cache: BTreeMap<String, Option<String>> = BTreeMap::new();
    let layout = plan_layout(
        groups.iter().zip(&shown_counts).flat_map(|((_, g), n)| g.iter().take(*n).copied()),
        width,
        &mut config_name_cache,
    );

    // Tracked across sections so the closing rollup can admit that some of the
    // missions it counts had their suggestions paginated away.
    let mut any_drift_hidden = false;

    for ((group, g), &shown) in groups.iter().zip(&shown_counts) {
        println!(
            "\n{}",
            style::dim(&format!("{} ({})", status_word(*group).to_uppercase(), g.len()))
        );
        for v in g.iter().take(shown) {
            // (#2406) The progress numerator is `done()` — complete PLUS
            // degraded. A degraded phase is terminal and produced output;
            // dropping it out of the bar would make a mixed run look less
            // far along than it is. The mix line beside it is what names
            // the difference.
            let prog = format!("{}/{}", v.done(), v.total);
            let bar = progress_bar(v.done(), v.total);
            let name = ellipsize(&display_label_cached(v.m, &mut config_name_cache), layout.name_width);
            // (#1569 packet A) Pad BEFORE linking, and by the VISIBLE width:
            // `{:<width$}` counts the OSC 8 escape bytes, so formatting a
            // linkified name would silently destroy the column alignment the
            // whole layout planner exists to maintain. The link wraps only
            // the name text; the padding stays outside it, so the clickable
            // target is the name rather than a run of trailing whitespace.
            let name_cell = format!(
                "{}{}",
                style::link(&mission_url(&link_base, &v.m.id), &name),
                " ".repeat(layout.name_width.saturating_sub(name.chars().count()))
            );
            // (#1612) Dim, and blank-padded rather than omitted, so a board
            // where only some ids carry a handle keeps one straight column.
            let handle_cell = if layout.show_handle {
                let h = short_handle(&v.m.id).unwrap_or("");
                format!(
                    "  {}{}",
                    style::dim(h),
                    " ".repeat(layout.handle_width.saturating_sub(h.chars().count()))
                )
            } else {
                String::new()
            };
            // Right-aligned by hand for the same reason the name is padded by
            // hand: `{:>width$}` would count `style::dim`'s escape bytes and
            // silently eat the alignment.
            let age = relative_age(now, last_activity(v.m));
            let age_cell = format!(
                "{}{}",
                " ".repeat(AGE_COLS.saturating_sub(age.chars().count())),
                style::dim(&age)
            );
            let row = format!(
                "  {} {}{}  {}  {:>5}  {}",
                kind_glyph(v.total),
                name_cell,
                handle_cell,
                age_cell,
                prog,
                bar,
            );
            if layout.show_mix {
                println!("{row}  {}", style::dim(&phase_mix(v)));
            } else {
                // Narrow terminal: the mix is dropped rather than the name, the
                // age or the progress, because it is the one column whose
                // information the others already carry.
                println!("{row}");
            }
            // (#2299) A run whose config left steps out says so in one dim
            // line; nothing gray is ever drawn for the pruned steps themselves.
            if let Some(g) = v.graph.as_ref().filter(|g| g.pruned_anything()) {
                println!("      {} {}", style::dim("·"), style::dim(&format!("graph: {}", g.summary_line())));
            }
            // (#2300) Growth is the opposite direction from pruning — tasks
            // the config never counted, minted at a phase boundary from a
            // step's output — so it gets its own line rather than being
            // folded into the "N of M steps minted" arithmetic above.
            if let Some(line) = v.graph.as_ref().and_then(|g| g.grown_line()) {
                println!("      {} {}", style::dim("·"), style::dim(&format!("graph: {line}")));
            }
            // (#2406 CONSIDER 6) The description, when the row's title above
            // came from the config's `name` instead — one dim line, one
            // sentence, never the whole ~200-word document.
            if let Some(note) = description_note_cached(v.m, &mut config_name_cache) {
                println!("      {} {}", style::dim("·"), style::dim(&note));
            }
            for d in &v.drifts {
                // The ⚠ marks the warning, not each of its lines — continuation
                // lines get blank space in the marker column so one wrapped
                // warning still reads as one warning.
                for (i, line) in wrap_indented(&d.detail, 8, width).iter().enumerate() {
                    let marker = if i == 0 { style::warn("⚠") } else { " ".to_string() };
                    println!("      {} {}", marker, style::warn(line.trim_start()));
                }
                for cmd in &d.suggest {
                    // The command itself is printed verbatim and never wrapped
                    // or truncated — it exists to be copy-pasted, and a command
                    // broken across lines by a renderer is worse than one that
                    // overflows. Only its trailing rationale is wrapped.
                    let (command, note) = split_suggestion(cmd);
                    println!("        {} {}", style::dim("→"), command);
                    for line in wrap_indented(note, 10, width) {
                        println!("{}", style::dim(&line));
                    }
                }
            }
        }
        if shown < g.len() {
            let hidden_drift = g.iter().skip(shown).filter(|v| !v.drifts.is_empty()).count();
            // In a panel the flag names itself once, as a link, at the end of
            // the block — so the two overflow lines don't each repeat advice
            // the operator cannot take.
            let more = if all_link.is_some() {
                format!("… {} more ({} of {} shown)", g.len() - shown, shown, g.len())
            } else {
                format!(
                    "… {} more ({} of {} shown) — `--all` for every mission",
                    g.len() - shown,
                    shown,
                    g.len()
                )
            };
            for line in wrap_indented(&more, 2, width) {
                println!("{}", style::dim(&line));
            }
            if hidden_drift > 0 {
                any_drift_hidden = true;
                // Never let a limit silently swallow an attention item.
                let warn = format!(
                    "⚠ {} hidden mission{} need{} attention{}",
                    hidden_drift,
                    if hidden_drift == 1 { "" } else { "s" },
                    if hidden_drift == 1 { "s" } else { "" },
                    if all_link.is_some() { "" } else { " — run with `--all`" }
                );
                for line in wrap_indented(&warn, 2, width) {
                    println!("{}", style::warn(&line));
                }
            }
            if let Some(url) = &all_link {
                if !all_link_shown {
                    all_link_shown = true;
                    println!("  {}", style::link(url, "→ show every mission"));
                }
            }
        }
    }

    // (#1562) The named-first default's own footer: names what was collapsed
    // above (count + how many of those need attention) so a hidden actionable
    // run can never read as silently gone — operator sovereignty (#44).
    // `--all` leaves `hidden` empty, so this never prints on a full board.
    let hidden_attention = hidden.iter().filter(|v| !v.drifts.is_empty()).count();
    if let Some(line) = hidden_run_summary(hidden.len(), hidden_attention) {
        println!();
        for l in wrap_indented(&line, 0, width) {
            println!("{}", style::dim(&l));
        }
    }
    // (#1709) The other half of the tab. A filter nobody can find is a
    // filter that doesn't exist — and the default board now MIXES named
    // missions with minted runs, which is exactly when someone wants the
    // named-only list. Printed only when there is something to filter, so a
    // board of purely named work never advertises a no-op.
    // `all_link.is_none()` — the panel surface has no prompt to type a flag
    // at, and this file already learned that the hard way: see
    // `panel_deep_link`'s doc ("the ADVICE has to match the surface … a dead
    // end in a panel"), which is why the `--all` advice above is suppressed
    // the same way. A hint the operator cannot act on is worse than none.
    if !missions_only && all_link.is_none() && visible.iter().any(|v| is_minted_run(v.m)) {
        for l in wrap_indented("→ `--missions` for named missions only", 0, width) {
            println!("{}", style::dim(&l));
        }
    }
    // A hidden run needing attention is exactly the same "some are hidden"
    // situation a per-section `--limit` already warns about below — folded
    // into the same flag rather than a second, competing qualifier.
    let any_drift_hidden = any_drift_hidden || hidden_attention > 0;

    // (#1711) The fleet half of the board — printed after every local
    // section so the operator's own machine stays visually primary, and
    // before the final rollup so the clean-board claim just below can be
    // qualified by what this printed (or admits it could not check). A
    // no-op on a standalone install: `print_peer_missions` is a no-op on an
    // empty slice and `fleet_scope_note` is `None` for `Off`.
    //
    // (#1711 review finding) The scope note prints BEFORE the rows it
    // qualifies, not after — same rule `run_list.rs`'s own `fleet_warning`
    // states: "an incomplete answer has to be qualified where the reader
    // meets it, not in a footnote under rows they have already believed."
    if let Some(note) = fleet_scope_note(&fleet.state) {
        println!();
        for line in wrap_indented(&note, 0, width) {
            println!("{}", style::warn(&line));
        }
    }
    print_peer_missions(&peer, now, width);

    println!();
    // "above" is only true for the drifted missions that were PRINTED as full
    // rows; a section limit or the named-first default can leave others
    // unshown (each warns its own way above), so the rollup admits it rather
    // than pointing at commands that never appeared.
    let visible_attention: usize = visible.iter().filter(|v| !v.drifts.is_empty()).count();
    let (clean, summary) = attention_rollup(
        visible_attention,
        hidden_attention,
        any_drift_hidden,
        all_link.is_some(),
        fleet_complete,
    );
    for line in wrap_indented(&summary, 0, width) {
        println!("{}", if clean { style::success(&line) } else { style::warn(&line) });
    }
    Ok(0)
}

/// Split `views` into (visible, hidden). `include_minted == true` returns
/// every mission visible and nothing hidden; `false` hides machine-minted
/// run instances (`is_minted_run`).
///
/// (#1709) The parameter is no longer "`--all`": the DEFAULT board passes
/// `true` here and `--missions` passes `false` — see [`board_partition`],
/// which owns that mapping. `--all` now only controls pagination.
///
/// Pure and borrowing, so it's unit-testable without any disk I/O.
/// (#1709 gate MF-3) The flag → partition mapping, as a NAMED function the
/// tests can actually reach.
///
/// The mapping itself is one `!`, which is exactly why it needs to live
/// here: `run()` is a printing function no unit test calls, so a mapping
/// written inline is unpinnable, and reverting it to the pre-#1709
/// `partition_visibility(&views, all)` would leave the whole suite green
/// while the default silently re-flipped. This file has already paid for
/// that lesson once — see `board_order`'s doc on shipping an INVERTED
/// comparator because it "lived inside `run()` … so no unit test could
/// reach it".
fn board_partition<'a>(
    views: &'a [MissionView<'a>],
    missions_only: bool,
) -> (Vec<&'a MissionView<'a>>, Vec<&'a MissionView<'a>>) {
    // `--missions` FILTERS minted runs out; the default includes them.
    partition_visibility(views, !missions_only)
}

fn partition_visibility<'a>(
    views: &'a [MissionView<'a>],
    include_minted: bool,
) -> (Vec<&'a MissionView<'a>>, Vec<&'a MissionView<'a>>) {
    if include_minted {
        return (views.iter().collect(), Vec::new());
    }
    views.iter().partition(|v| !is_minted_run(v.m))
}

/// (#1562) The board-footer line naming what the named-first default
/// collapsed — `None` when nothing was hidden (an `--all` board, or a board
/// with no minted runs at all). `hidden_attention` is named explicitly: a
/// hidden run needing `mission abort`/`finalize` must never vanish
/// silently, it just isn't rendered as a full row with its own
/// copy-pasteable commands (operator sovereignty, #44).
fn hidden_run_summary(hidden_len: usize, hidden_attention: usize) -> Option<String> {
    if hidden_len == 0 {
        return None;
    }
    let plural = if hidden_len == 1 { "" } else { "s" };
    // (#1709) This line now only ever prints under `--missions` — the
    // operator ASKED to filter these out, so the advice names the way back
    // rather than `--all` (which would also un-paginate).
    if hidden_attention == 0 {
        return Some(format!(
            "+{hidden_len} run instance{plural} filtered out — drop `--missions` to include them, \
             or see the runs lens"
        ));
    }
    let verb = if hidden_attention == 1 { "needs" } else { "need" };
    Some(format!(
        "+{hidden_len} run instance{plural} filtered out, {hidden_attention} {verb} attention — \
         drop `--missions` to include them, or see the runs lens"
    ))
}

/// (#1562) The final "N missions need attention" line (or the clean
/// checkmark) — extracted so its three distinct cases are each directly
/// testable without a real board:
///   - nothing anywhere → the clean checkmark;
///   - something ONLY among hidden (collapsed) runs → the rollup must still
///     say so, since nothing about that is visible above it;
///   - something on-screen → the existing "run the suggested commands
///     above" wording, with `any_drift_hidden`'s tail unchanged.
///
/// Returns `(is_clean, message)`; the caller picks `style::success` /
/// `style::warn` from `is_clean`.
fn attention_rollup(
    visible_attention: usize,
    hidden_attention: usize,
    any_drift_hidden: bool,
    all_link_present: bool,
    // (#1711) Whether the fleet-wide peer-mission read covered the whole
    // fleet (`SourceState::Ok`/`Off`) or came back degraded
    // (`Stale`/`Unavailable`). The issue's own complaint was specifically
    // about this line: "✓ board is clean" is currently scoped to one
    // machine while claiming to be scoped to everything. `true` on every
    // pre-#1711 call site preserves the exact old wording.
    fleet_complete: bool,
) -> (bool, String) {
    // (#1711) Appended to every branch below so the scope caveat travels
    // with whichever message actually prints, rather than living only in
    // the separate `fleet_scope_note` line above it (which a narrow
    // terminal or a script grepping just this line could miss).
    let fleet_tail =
        if fleet_complete { "" } else { " — the fleet-wide read did not complete; peer missions may be missing" };
    if visible_attention == 0 && hidden_attention == 0 {
        if !fleet_complete {
            // Never the green checkmark here: this machine's own missions
            // are reconciled, but that is not the claim the summary line
            // makes — it says "board", and the board includes the fleet.
            return (
                false,
                format!("this machine's missions are reconciled{fleet_tail} (see note above)"),
            );
        }
        return (true, "✓ board is clean — every mission's phases are reconciled".to_string());
    }
    if visible_attention == 0 {
        // Nothing printed above needs action, but a filtered-out run does.
        // (#1709) This branch is now reachable ONLY under `--missions` —
        // that is the only way anything lands in `hidden` — so the remedy is
        // to DROP the filter, matching `hidden_run_summary`'s advice one line
        // above. Suggesting `--all` here would sit under a footer offering a
        // different cure for the same set, and an operator who ADDED `--all`
        // to their current `--missions` invocation would see nothing new.
        let plural = if hidden_attention == 1 { "" } else { "s" };
        let verb = if hidden_attention == 1 { "needs" } else { "need" };
        let it = if hidden_attention == 1 { "it" } else { "them" };
        return (
            false,
            format!(
                "{hidden_attention} filtered-out run instance{plural} {verb} attention — drop \
                 `--missions` to see {it} and {its} reconcile command{plural}{fleet_tail}",
                its = if hidden_attention == 1 { "its" } else { "their" },
            ),
        );
    }
    let tail = if !any_drift_hidden {
        ""
    } else if all_link_present {
        " (some are hidden — open the full board above)"
    } else {
        " (some are hidden — `--all` to see them)"
    };
    (
        false,
        format!(
            "{visible_attention} mission{s} {verb} attention — run the suggested commands above to \
             reconcile{tail}{fleet_tail}",
            s = if visible_attention == 1 { "" } else { "s" },
            verb = if visible_attention == 1 { "needs" } else { "need" },
        ),
    )
}

/// (#1562) The `--json` payload's data — extracted from `run_json` purely so
/// its COMPLETENESS is directly unit-testable: this takes the same
/// unfiltered `views` slice `run()` builds before it ever computes
/// `partition_visibility`, so a test can assert the mission count here
/// matches the input slice regardless of what a human-board `--all` would
/// show. No I/O, no printing.
fn board_json(views: &[MissionView], peer: &[Run], fleet_state: &SourceState) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = views
        .iter()
        .map(|v| {
            serde_json::json!({
                "id": v.m.id,
                "status": status_word(v.m.status),
                "ticket": v.m.ticket,
                "phases": {
                    // (#2406) `degraded` is a SIBLING bucket, and `complete`
                    // no longer includes it. Safe because nothing parses
                    // `phases.complete`: the only `mission status --json`
                    // consumers in the tree are two `tests/cli.rs` assertions
                    // that read `missions[].drift` and nothing else (enumerated
                    // before the change). The four terminal/live buckets still
                    // sum to `total`, which is the invariant a reader can rely
                    // on.
                    "total": v.total, "complete": v.complete, "degraded": v.degraded,
                    "running": v.running, "planned": v.planned, "abandoned": v.abandoned,
                },
                // (#2299) present only for a config-launched run: what the
                // config declared, what was minted, and what was pruned + why.
                "graph": v.graph,
                "drift": v.drifts.iter().map(|d| serde_json::json!({
                    "kind": d.kind, "detail": d.detail, "suggest": d.suggest,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let attention = views.iter().filter(|v| !v.drifts.is_empty()).count();
    // (#1711) `fleet_complete` mirrors `SourceState::is_complete` (that
    // method is `pub(crate)` inside `darkmux-serve`, not visible from this
    // crate) — `Ok`/`Off` both mean the fleet-wide read covers everything it
    // claims to; `Stale`/`Unavailable` mean a peer's mission could be
    // missing from `peer_missions` below.
    let fleet_complete = matches!(fleet_state, SourceState::Ok | SourceState::Off);
    serde_json::json!({
        "missions": arr,
        // (#1711) Peer missions — observed via the shared flow stream, not
        // owned by this machine (see `peer_mission_runs`'s doc). Always
        // present (possibly empty), same "record exhaustively" posture as
        // `missions` above — a machine reader must be able to tell "no peer
        // missions" from "the fleet read never ran".
        "peer_missions": peer,
        // The SAME wire shape `darkmux run list --json` emits for this
        // field (`run_list.rs::run_json`) — one state vocabulary for
        // "what's running on the fleet" across both CLI surfaces (#1711's
        // own complaint was two surfaces disagreeing).
        "fleet": fleet_state,
        "summary": {
            "total": views.len(),
            "needs_attention": attention,
            "fleet_complete": fleet_complete,
        },
    })
}

fn run_json(views: &[MissionView], peer: &[Run], fleet_state: &SourceState) -> Result<i32> {
    println!("{}", serde_json::to_string_pretty(&board_json(views, peer, fleet_state))?);
    Ok(0)
}

/// Board ordering: drifted first (attention leads), then most-recently-touched,
/// then id as a stable tiebreak.
///
/// Extracted from an inline closure specifically so the DIRECTION is testable.
/// It was previously written `(b.is_empty()).cmp(&(a.is_empty()))` under a
/// comment claiming "drifted first", which sorts clean missions first — the
/// exact inverse of both the comment and the intent. Nothing caught it because
/// the comparator lived inside `run()`, which needs mission JSON on disk to
/// exercise, so no unit test could reach it.
///
/// Direction, stated so it can't silently invert again: a mission WITH drift has
/// `drifts.is_empty() == false == 0u8`, and `Ordering::Less` sorts first, so the
/// drift key must be `a.cmp(b)` — NOT `b.cmp(a)`. Recency is the one key that IS
/// reversed (`b.cmp(a)`), because newest-first means larger-timestamp-first.
///
/// This direction is load-bearing for pagination, not cosmetic: the per-section
/// limit is only safe because truncation drops from the TAIL, so with drift
/// sorted first a cap can only ever hide rows needing no attention.
fn board_order(a: &MissionView, b: &MissionView) -> std::cmp::Ordering {
    (a.drifts.is_empty() as u8)
        .cmp(&(b.drifts.is_empty() as u8))
        .then(last_activity(b.m).cmp(&last_activity(a.m)))
        .then(a.m.id.cmp(&b.m.id))
}

/// Most recent state transition on a mission — the honest "last touched".
///
/// A max over the present timestamps rather than a single field, because which
/// field is newest depends on the mission's path through the state machine
/// (`created` → maybe `started` → maybe `paused` → maybe `finalized`), and a
/// mission can be paused after being started, or finalized without ever having
/// started. Nothing is subtracted, so a mission with only `created_ts` still
/// sorts by that.
/// `pub(crate)` since #1713: `radio_answer`'s grounding block orders the
/// missions it names by the SAME rule, and its verification criterion is
/// literally "matches the CLI board's top row". Two copies of this would
/// drift the first time a new terminal stamp lands (an `aborted_ts`, say):
/// whoever updated one would have no reason to find the other, and radio
/// would quietly start disagreeing with the board.
pub(crate) fn last_activity(m: &Mission) -> u64 {
    m.created_ts
        .max(m.started_ts.unwrap_or(0))
        .max(m.paused_ts.unwrap_or(0))
        .max(m.finalized_ts.unwrap_or(0))
}

/// Rows shown per section when the operator names no `--limit`.
///
/// FINALIZED gets a much smaller budget than the open sections on purpose:
/// closed work is recent-history context, not the question the board answers,
/// and on a real board it dominates by an order of magnitude (measured: 69
/// finalized vs 1 active, 21 of them single-turn `dispatch-*` records minted one
/// per `darkmux dispatch`). Spending equal screen space on both would let
/// exhaust crowd out the work — the failure this pagination exists to fix.
/// An explicit `--limit n` overrides this uniformly (#44: a typed number
/// outranks a derived one).
fn default_limit(group: MissionStatus) -> usize {
    match group {
        // (#1709) Raised 3 → 8. The old budget was tuned when the default
        // board FILTERED run instances out, which made FINALIZED a small
        // recent-history footnote beneath the operator's named work. With
        // the recent-first default, closed work is where nearly everything
        // lands — and when nothing is open it is the whole board, so a
        // 3-row budget answered "what's recent" with one day's tail.
        // Still well under ACTIVE/PAUSED's 10: open work outranks closed.
        MissionStatus::Finalized | MissionStatus::Aborted => 8,
        MissionStatus::Active | MissionStatus::Paused => 10,
    }
}

/// Fixed per-row cost of a row with NEITHER optional column: `"  ◆ "` (4) +
/// the age gap (2) + the age field (3) + a gap (2) + the 5-wide progress field
/// + a gap (2) + the 4-wide bar = 22.
///
/// Exactly this and no more: a row that also shows the mix or the handle pays
/// one ADDITIONAL gap each (`MIX_GAP_COLS` / `HANDLE_GAP_COLS`). Conflating the
/// gap into the base let `plan_layout` judge a with-mix row 2 columns narrower
/// than it renders, so at `COLUMNS=51` an id(12) + mix(22) row measured 51 and
/// printed 53 — hard-wrapping on exactly the terminal width the adaptation
/// exists to respect. The same trap now exists twice; the arithmetic is pinned
/// by `plan_layout_row_fits_every_width_it_can_honor`, which measures the
/// RENDERED string rather than recomputing this budget. It earned its keep
/// immediately: the first draft of this constant said 23.
const ROW_FIXED_COLS: usize = 22;

/// Width of the age field. `now`/`59m`/`23h`/`29d`/`99w` — three columns covers
/// every value `relative_age` can emit below a hundred weeks.
const AGE_COLS: usize = 3;

/// The extra 2-space gap between the bar and the mix column, paid only when the
/// mix is shown. See `ROW_FIXED_COLS`.
const MIX_GAP_COLS: usize = 2;

/// The extra 2-space gap between the name and the handle column, paid only when
/// the handle is shown. See `ROW_FIXED_COLS`.
const HANDLE_GAP_COLS: usize = 2;

/// Never shrink the name column below this — a name truncated to a few
/// characters identifies nothing, which defeats the point of keeping it.
const MIN_NAME_COLS: usize = 12;

/// Below this much room for text, `wrap_indented` stops wrapping and emits one
/// overlong line instead. Wrapping prose into a 3-column gutter produces
/// something less readable than an overflowing line, not more.
///
/// Deliberately its OWN constant rather than reusing `MIN_NAME_COLS`: the two
/// happen to share a value but answer unrelated questions (how short an id may
/// be truncated vs. how narrow a paragraph is worth wrapping), so tying them
/// together would make one silently move when the other is tuned.
const MIN_WRAP_ROOM: usize = 12;

/// How one board row is laid out at the current terminal width.
#[derive(Debug, PartialEq)]
struct Layout {
    name_width: usize,
    handle_width: usize,
    show_handle: bool,
    show_mix: bool,
}

/// Plan the row layout from the rows that will actually be printed.
///
/// Degradation order is deliberate, widest-terminal first:
///   1. the phase mix goes — the progress fraction and bar already carry it;
///   2. then the handle — the age already tells two same-named rows apart, and
///      the full id is a click away on the row's own link;
///   3. only then does the name get truncated.
///
/// The age is never dropped. It is 3 columns, it is the one field that answers
/// "is this still relevant", and it is what makes step 2 survivable.
///
/// `width == None` means output isn't a terminal, so nothing is adapted and
/// nothing is dropped — piped output stays complete.
fn plan_layout<'a>(
    rows: impl Iterator<Item = &'a MissionView<'a>>,
    width: Option<usize>,
    label_cache: &mut BTreeMap<String, Option<String>>,
) -> Layout {
    let (max_name, max_handle, max_mix) = rows.fold((0, 0, 0), |(n, h, x), v| {
        (
            n.max(display_label_cached(v.m, &mut *label_cache).chars().count()),
            h.max(short_handle(&v.m.id).map_or(0, |s| s.chars().count())),
            x.max(phase_mix(v).chars().count()),
        )
    });
    // A handle column is only ever planned if some row actually has one —
    // otherwise every row would pay two gap columns for a run of blanks.
    let handle_cost = if max_handle == 0 { 0 } else { HANDLE_GAP_COLS + max_handle };
    let with_handle = |name_width: usize, show_mix: bool| Layout {
        name_width,
        handle_width: max_handle,
        show_handle: max_handle > 0,
        show_mix,
    };
    let Some(w) = width else {
        return with_handle(max_name, true);
    };
    if max_name + ROW_FIXED_COLS + handle_cost + MIX_GAP_COLS + max_mix <= w {
        with_handle(max_name, true)
    } else if max_name + ROW_FIXED_COLS + handle_cost <= w {
        with_handle(max_name, false)
    } else if max_name + ROW_FIXED_COLS <= w {
        Layout { name_width: max_name, handle_width: 0, show_handle: false, show_mix: false }
    } else {
        Layout {
            name_width: w.saturating_sub(ROW_FIXED_COLS).max(MIN_NAME_COLS),
            handle_width: 0,
            show_handle: false,
            show_mix: false,
        }
    }
}

/// Split a suggested command from its trailing `#` rationale.
///
/// Drift suggestions are authored as `<command>   # <why + caveats>`, and the
/// rationale is where nearly all the length lives (measured: 265-column lines
/// against an 80-column terminal). Splitting lets the command stay verbatim
/// while the prose wraps. A suggestion with no `#` comment yields an empty
/// note, so callers print nothing extra.
fn split_suggestion(s: &str) -> (&str, &str) {
    match s.find("  #") {
        Some(i) => (s[..i].trim_end(), s[i..].trim_start().trim_start_matches('#').trim_start()),
        None => (s, ""),
    }
}

/// Word-wrap `text` to `width` columns, prefixing every line with `indent`
/// spaces. Returns empty for empty text (callers print nothing).
///
/// `width == None` (not a terminal) means no wrapping at all — piped output
/// keeps each logical message on exactly one line, which is what makes it
/// greppable. Words longer than the available room are left overlong rather
/// than hard-split, since the long tokens here are file paths and commands
/// that must survive intact.
///
/// One divergence between the two paths, harmless today but worth knowing: the
/// wrapped path re-joins on `split_whitespace`, so it COLLAPSES internal
/// whitespace runs, while the `None` path emits `text` verbatim. Every string
/// this renders is single-spaced prose, so the paths agree; a future detail
/// carrying deliberate alignment would render differently piped vs. in a
/// terminal.
fn wrap_indented(text: &str, indent: usize, width: Option<usize>) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let pad = " ".repeat(indent);
    let Some(w) = width.filter(|w| w.saturating_sub(indent) >= MIN_WRAP_ROOM) else {
        return vec![format!("{pad}{text}")];
    };
    let room = w - indent;
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let add = if line.is_empty() { word.chars().count() } else { line.chars().count() + 1 + word.chars().count() };
        if !line.is_empty() && add > room {
            out.push(format!("{pad}{line}"));
            line = word.to_string();
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(format!("{pad}{line}"));
    }
    out
}

/// Truncate to `max` CHARS, eliding the MIDDLE with a `…`.
///
/// Chars, not bytes (so a multi-byte id can't be cut mid-character) and not
/// display columns: for the ASCII machine-minted ids this renders, one char is
/// one column, but a double-width glyph would be counted as one and drawn as
/// two. The same char-as-column assumption underlies `wrap_indented` and
/// `plan_layout`; it holds for every string this module renders today.
///
/// The elision is in the middle, not the tail, because darkmux's machine-minted
/// ids carry their discriminator as a SUFFIX
/// (`dispatch-code-reviewer-1785386551-4b71-0`): tail-truncating a screenful of
/// those renders every row as the identical string
/// `dispatch-code-reviewer-17853…`, which destroys exactly the identity the id
/// column exists to preserve. Keeping both ends costs nothing and tells the
/// rows apart. Observed directly at 46 columns while building #1569.
fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let keep = max - 1; // one column for the `…`
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let front: String = chars[..head].iter().collect();
    let back: String = chars[n - tail..].iter().collect();
    format!("{front}…{back}")
}

fn status_word(s: MissionStatus) -> &'static str {
    match s {
        MissionStatus::Active => "active",
        MissionStatus::Paused => "paused",
        MissionStatus::Finalized => "finalized",
        MissionStatus::Aborted => "aborted",
    }
}

fn phase_mix(v: &MissionView) -> String {
    if v.total == 0 {
        return "no phases".to_string();
    }
    let mut parts = Vec::new();
    if v.complete > 0 { parts.push(format!("{} complete", v.complete)); }
    // (#2406) Between complete and running on purpose: a degraded phase is
    // terminal-and-productive, so it reads next to `complete`, not down
    // beside `abandoned`.
    if v.degraded > 0 { parts.push(format!("{} degraded", v.degraded)); }
    if v.running > 0 { parts.push(format!("{} running", v.running)); }
    if v.planned > 0 { parts.push(format!("{} planned", v.planned)); }
    if v.abandoned > 0 { parts.push(format!("{} abandoned", v.abandoned)); }
    parts.join(" · ")
}

fn progress_bar(done: usize, total: usize) -> String {
    if total == 0 {
        return "····".to_string();
    }
    let width = 4;
    let filled = (done * width + total / 2) / total;
    let filled = filled.min(width);
    format!("{}{}", "▓".repeat(filled), "░".repeat(width - filled))
}

/// The only IO/clock touch in this module — kept to one call site in `run()`
/// so `detect_drift` itself stays pure and unit-testable with fixed
/// timestamps.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::types::MissionSpec;
    use darkmux_serve::RunKind;

    /// (#2682 fix-pass review CONSIDER: test hygiene) RAII guard for a test
    /// that needs a scratch `DARKMUX_HOME` — restores the PREVIOUS value on
    /// `Drop`, including when the test body panics partway through. The
    /// prior version of `cli_board_and_run_list_agree_on_a_crashed_local_mission`
    /// restored the env var by hand AFTER several `unwrap()`/`unwrap_or_else
    /// (|| panic!(..))` calls, so a single real failure in that test left
    /// `DARKMUX_HOME` pointed at a `TempDir` about to be dropped — every
    /// subsequent `#[serial]` test in the process would then read/write
    /// through a directory that no longer exists. Caller must hold
    /// `#[serial_test::serial]` — this guard does not itself serialize.
    struct DarkmuxHomeGuard {
        tmp: tempfile::TempDir,
        prev: Option<String>,
    }
    impl DarkmuxHomeGuard {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_HOME").ok();
            // SAFETY: caller holds #[serial_test::serial].
            unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
            Self { tmp, prev }
        }
        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }
    }
    impl Drop for DarkmuxHomeGuard {
        fn drop(&mut self) {
            // SAFETY: caller holds #[serial_test::serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    /// (#2682 fix-pass round 2, MUST FIX 2) RAII pin for the staleness
    /// budget every liveness verdict in this module is measured against.
    /// `stale_after_ms()` is `config_access::inactivity_timeout_seconds()
    /// * 2`, whose TOP tier is `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` — a
    /// documented operator knob. A fixture that places a mission "25
    /// minutes ago" and expects that to read stale is therefore asserting
    /// against a threshold the ENVIRONMENT owns: with the knob exported at
    /// `7200`, the budget becomes 4 hours and the fixture's own premise
    /// evaporates. That is the clock rule one axis over — freeze the
    /// distance's DENOMINATOR, not just its numerator. Caller must hold
    /// `#[serial_test::serial]`.
    struct InactivityBudgetGuard {
        prev: Option<String>,
    }
    impl InactivityBudgetGuard {
        fn seconds(secs: u64) -> Self {
            let prev = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
            // SAFETY: caller holds #[serial_test::serial].
            unsafe { std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", secs.to_string()) };
            Self { prev }
        }
    }
    impl Drop for InactivityBudgetGuard {
        fn drop(&mut self) {
            // SAFETY: caller holds #[serial_test::serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                    None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
                }
            }
        }
    }

    fn mission(id: &str, status: MissionStatus) -> Mission {
        Mission {
            id: id.into(),
            // (#1612) Mirror the id: the row labels from `description` now, so
            // a fixed "d" would make every width test measure a 1-column name
            // and silently stop exercising the arithmetic it exists to pin.
            description: id.into(),
            status,
            phase_ids: vec![],
            created_ts: 0,
            started_ts: None,
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
            machine: None,
        }
    }
    fn phase(id: &str, mid: &str, status: PhaseStatus) -> Phase {
        Phase {
            id: id.into(),
            mission_id: mid.into(),
            description: "d".into(),
            display_name: None,
            status,
            created_ts: 0,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        }
    }

    /// A `MissionView` carrying just enough to exercise the layout planner:
    /// the id sets the identity column, the phase counts set the mix column.
    fn view<'a>(m: &'a Mission, complete: usize, running: usize) -> MissionView<'a> {
        MissionView {
            m,
            total: complete + running,
            complete,
            degraded: 0,
            running,
            planned: 0,
            abandoned: 0,
            drifts: Vec::new(),
            graph: None,
        }
    }

    /// (#2406) `view` plus a degraded bucket. A separate constructor rather
    /// than a fourth positional `usize` on `view` — every existing layout
    /// test reads better without a `, 0` it does not care about.
    fn view_with_degraded<'a>(
        m: &'a Mission,
        complete: usize,
        degraded: usize,
        running: usize,
    ) -> MissionView<'a> {
        MissionView { total: complete + degraded + running, degraded, ..view(m, complete, running) }
    }

    #[test]
    fn split_suggestion_separates_the_command_from_its_rationale() {
        let (cmd, note) = split_suggestion(
            "darkmux mission abort m1 --phase p2   # abandon just this blocked phase",
        );
        assert_eq!(cmd, "darkmux mission abort m1 --phase p2");
        assert_eq!(note, "abandon just this blocked phase");
    }

    #[test]
    fn split_suggestion_leaves_a_bare_command_whole() {
        // No `#` rationale: the whole string is the command and the note is
        // empty, so the renderer prints no extra line.
        let (cmd, note) = split_suggestion("darkmux mission finalize m1");
        assert_eq!(cmd, "darkmux mission finalize m1");
        assert_eq!(note, "");
    }

    #[test]
    fn split_suggestion_keeps_a_shell_comment_inside_the_command() {
        // A single `#` with no preceding double-space is part of the command
        // (e.g. a `--message '#1569'` argument), not a rationale separator.
        let (cmd, note) = split_suggestion("darkmux flow note --text 'fixes #1569'");
        assert_eq!(cmd, "darkmux flow note --text 'fixes #1569'");
        assert_eq!(note, "");
    }

    #[test]
    fn wrap_indented_wraps_to_width_and_indents_every_line() {
        let lines = wrap_indented("alpha beta gamma delta", 4, Some(16));
        // room = 16 - 4 = 12 columns of text per line.
        assert_eq!(lines, vec!["    alpha beta".to_string(), "    gamma delta".to_string()]);
        assert!(lines.iter().all(|l| l.chars().count() <= 16));
    }

    #[test]
    fn wrap_indented_does_not_wrap_when_output_is_not_a_terminal() {
        // Piped output keeps one logical message on one line so it stays
        // greppable — the same reason plan_layout adapts nothing at None.
        let long = "alpha beta gamma delta epsilon zeta eta theta";
        assert_eq!(wrap_indented(long, 4, None), vec![format!("    {long}")]);
    }

    #[test]
    fn wrap_indented_leaves_an_overlong_word_intact() {
        // Long tokens here are paths and commands; hard-splitting one would
        // corrupt it, so it overflows instead.
        let path = "/very/long/path/that/exceeds/the/room";
        let lines = wrap_indented(&format!("run {path} now"), 4, Some(20));
        // The overlong word gets its OWN line (it can't share one), so assert
        // it survives somewhere intact rather than assuming which line.
        assert!(lines.iter().any(|l| l.contains(path)), "path was split: {lines:?}");
        assert!(lines.iter().all(|l| l.starts_with("    ")));
    }

    #[test]
    fn wrap_indented_is_empty_for_empty_text() {
        assert!(wrap_indented("", 4, Some(80)).is_empty());
    }

    #[test]
    fn last_activity_takes_the_newest_present_timestamp() {
        let mut m = mission("m1", MissionStatus::Finalized);
        m.created_ts = 100;
        assert_eq!(last_activity(&m), 100, "created_ts alone is the floor");

        m.started_ts = Some(200);
        m.paused_ts = Some(400);
        m.finalized_ts = Some(300);
        // Deliberately out of chronological order: a mission can be paused
        // after being finalized on hand-edited data, and the sort must still
        // pick the newest stamp rather than trusting a field precedence.
        assert_eq!(last_activity(&m), 400);
    }

    /// A `MissionView` with drift attached, for ordering tests.
    fn drifted<'a>(m: &'a Mission) -> MissionView<'a> {
        let mut v = view(m, 1, 0);
        v.drifts.push(Drift { kind: "test", detail: "d".into(), suggest: vec![] });
        v
    }

    #[test]
    fn board_order_puts_drifted_missions_first() {
        // THE regression. The comparator previously read
        // `(b.is_empty()).cmp(&(a.is_empty()))` under a "drifted first" comment,
        // which sorted CLEAN first — inverting the one property that makes the
        // per-section limit safe (truncation drops from the tail, so drift must
        // lead or a cap hides exactly the rows that needed attention).
        let (ma, mb, mc, md) = (
            mission("m-clean-a", MissionStatus::Active),
            mission("m-drift-b", MissionStatus::Active),
            mission("m-clean-c", MissionStatus::Active),
            mission("m-drift-d", MissionStatus::Active),
        );
        let mut views = [view(&ma, 1, 0), drifted(&mb), view(&mc, 1, 0), drifted(&md)];
        views.sort_by(board_order);
        let ids: Vec<&str> = views.iter().map(|v| v.m.id.as_str()).collect();
        assert_eq!(ids, vec!["m-drift-b", "m-drift-d", "m-clean-a", "m-clean-c"]);
    }

    #[test]
    fn board_order_sorts_newest_first_within_equal_drift() {
        // Ids sort ascending b < c, activity sorts c newer — recency must win,
        // since mission ids are largely machine-minted and carry no work order.
        let mut older = mission("m-bbb", MissionStatus::Active);
        older.started_ts = Some(100);
        let mut newer = mission("m-ccc", MissionStatus::Active);
        newer.started_ts = Some(900);
        let mut views = [view(&older, 1, 0), view(&newer, 1, 0)];
        views.sort_by(board_order);
        assert_eq!(
            views.iter().map(|v| v.m.id.as_str()).collect::<Vec<_>>(),
            vec!["m-ccc", "m-bbb"],
            "newest-touched must lead despite sorting later by id"
        );
    }

    #[test]
    fn board_order_falls_back_to_id_when_drift_and_recency_tie() {
        let mut a = mission("m-zzz", MissionStatus::Active);
        let mut b = mission("m-aaa", MissionStatus::Active);
        a.created_ts = 500;
        b.created_ts = 500;
        let mut views = [view(&a, 1, 0), view(&b, 1, 0)];
        views.sort_by(board_order);
        assert_eq!(
            views.iter().map(|v| v.m.id.as_str()).collect::<Vec<_>>(),
            vec!["m-aaa", "m-zzz"],
            "identical drift + timestamps must order stably by id"
        );
    }

    #[test]
    fn drift_leads_so_a_section_limit_cannot_hide_an_attention_item() {
        // The pagination-safety property stated in run()'s docs, asserted
        // directly: after sorting, taking the first N rows (what the limit
        // does) retains every drifted mission while N >= the drift count.
        let ms: Vec<Mission> =
            (0..8).map(|i| mission(&format!("m-{i}"), MissionStatus::Finalized)).collect();
        let mut views: Vec<MissionView> = ms
            .iter()
            .enumerate()
            .map(|(i, m)| if i % 3 == 0 { drifted(m) } else { view(m, 1, 0) })
            .collect();
        let total_drift = views.iter().filter(|v| !v.drifts.is_empty()).count();
        views.sort_by(board_order);
        let kept = views.iter().take(total_drift).filter(|v| !v.drifts.is_empty()).count();
        assert_eq!(kept, total_drift, "a tail-truncating limit must not drop drifted rows");
    }

    #[test]
    fn plan_layout_shows_everything_when_output_is_not_a_terminal() {
        // `None` = piped/redirected. Nothing adapts and nothing is dropped, so
        // `mission status | grep` is byte-predictable regardless of the window
        // it ran in.
        let m = mission("a-very-long-machine-minted-mission-id-0001", MissionStatus::Active);
        let v = view(&m, 3, 1);
        let l = plan_layout([v].iter(), None, &mut BTreeMap::new());
        assert_eq!(l.name_width, 42);
        assert!(l.show_mix);
    }

    #[test]
    fn plan_layout_sizes_the_id_column_to_the_widest_shown_row() {
        let short = mission("m1", MissionStatus::Active);
        let long = mission("m-longer-id", MissionStatus::Active);
        let rows = [view(&short, 1, 0), view(&long, 1, 0)];
        let l = plan_layout(rows.iter(), Some(200), &mut BTreeMap::new());
        // Natural width, not the old hardcoded 30 — narrow boards stay narrow.
        assert_eq!(l.name_width, "m-longer-id".len());
        assert!(l.show_mix);
    }

    /// Render one board row exactly as `run()` does, so tests can assert the
    /// width of what is actually PRINTED rather than of the plan.
    ///
    /// Layout bugs here are off-by-N in a column budget, and a test that
    /// recomputes the same budget it is checking will agree with a wrong one —
    /// which is how a 2-column undercount survived its own unit test.
    fn render_row(v: &MissionView, layout: &Layout) -> String {
        let prog = format!("{}/{}", v.done(), v.total);
        let bar = progress_bar(v.done(), v.total);
        let name = ellipsize(&display_label(v.m), layout.name_width);
        let handle = if layout.show_handle {
            let h = short_handle(&v.m.id).unwrap_or("");
            format!("  {:<width$}", h, width = layout.handle_width)
        } else {
            String::new()
        };
        // Any age of the right WIDTH exercises the same budget — the row's
        // column cost is `AGE_COLS`, never the particular value.
        let age = format!("{:>width$}", "9d", width = AGE_COLS);
        let row = format!(
            "  {} {:<width$}{}  {}  {:>5}  {}",
            kind_glyph(v.total),
            name,
            handle,
            age,
            prog,
            bar,
            width = layout.name_width
        );
        if layout.show_mix {
            format!("{row}  {}", phase_mix(v))
        } else {
            row
        }
    }

    /// The narrowest terminal a row can honor: the fixed columns plus the id
    /// floor. Below this the row overflows BY DESIGN (see `MIN_NAME_COLS`).
    const NARROWEST_HONORABLE: usize = ROW_FIXED_COLS + MIN_NAME_COLS;

    #[test]
    fn plan_layout_row_fits_every_width_it_can_honor() {
        // THE off-by-two regression, asserted against the RENDERED string
        // rather than a recomputed budget: at COLUMNS=51 an id(12) + mix(22)
        // row used to measure 51 and print 53. Sweeping widths also pins the
        // mix-shown/mix-dropped/id-truncated boundaries all at once.
        let m = mission("m-0123456789", MissionStatus::Active);
        for w in NARROWEST_HONORABLE..=90 {
            let layout = plan_layout([view(&m, 2, 1)].iter(), Some(w), &mut BTreeMap::new());
            let row = render_row(&view(&m, 2, 1), &layout);
            let cols = row.chars().count();
            assert!(
                cols <= w,
                "at COLUMNS={w} the row rendered {cols} cols (show_mix={}): {row:?}",
                layout.show_mix
            );
        }
    }

    #[test]
    fn below_the_id_floor_the_row_overflows_by_design_but_stays_bounded() {
        // Deliberate, and worth pinning so it can't drift into unbounded
        // overflow: under ~29 columns the id floor wins over fitting the row,
        // because an id truncated to 3 chars identifies nothing. The row is
        // still exactly the floor row — never wider.
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        for w in 1..NARROWEST_HONORABLE {
            let layout = plan_layout([view(&m, 2, 1)].iter(), Some(w), &mut BTreeMap::new());
            assert_eq!(layout.name_width, MIN_NAME_COLS, "at COLUMNS={w}");
            assert!(!layout.show_mix, "at COLUMNS={w} the mix must be gone before this point");
            let cols = render_row(&view(&m, 2, 1), &layout).chars().count();
            assert_eq!(cols, NARROWEST_HONORABLE, "at COLUMNS={w} overflow must stay bounded");
        }
    }

    #[test]
    fn plan_layout_drops_the_mix_before_truncating_the_id() {
        let m = mission("m-0123456789", MissionStatus::Active);
        let v = view(&m, 2, 1); // mix = "2 complete · 1 running"
        let mix_cols = phase_mix(&v).chars().count();
        let name_cols = display_label(&m).chars().count();
        // (#1612) This id's trailing `0123456789` is all hex, so the row also
        // carries a handle — and the mix is only droppable AFTER the widest
        // row it must sit beside is accounted for. Omitting this term is what
        // made this test fail when the handle column landed.
        let handle_cols =
            HANDLE_GAP_COLS + short_handle(&m.id).map_or(0, |h| h.chars().count());
        let base = name_cols + ROW_FIXED_COLS + handle_cols;

        // One column short of fitting the mix: the mix goes, the name AND the
        // handle survive intact — the fraction and bar already carry the mix's
        // information, so it is the first thing worth losing.
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(base + MIX_GAP_COLS + mix_cols - 1), &mut BTreeMap::new());
        assert_eq!(l.name_width, name_cols, "the name must not shrink while the mix is droppable");
        assert!(l.show_handle, "the handle must not go before the mix");
        assert!(!l.show_mix);

        // And one column MORE than the widest with-mix row does fit it.
        assert!(plan_layout([view(&m, 2, 1)].iter(), Some(base + MIX_GAP_COLS + mix_cols), &mut BTreeMap::new()).show_mix);
    }

    #[test]
    fn plan_layout_truncates_the_id_only_when_even_that_cannot_fit() {
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        let l = plan_layout([view(&m, 1, 0)].iter(), Some(ROW_FIXED_COLS + 15), &mut BTreeMap::new());
        assert_eq!(l.name_width, 15);
        assert!(!l.show_mix);
    }

    #[test]
    fn plan_layout_never_shrinks_the_id_below_the_legible_floor() {
        // An absurdly narrow terminal overflows the row rather than rendering
        // an id too short to identify anything.
        let m = mission("m-0123456789-0123456789", MissionStatus::Active);
        let l = plan_layout([view(&m, 1, 0)].iter(), Some(10), &mut BTreeMap::new());
        assert_eq!(l.name_width, MIN_NAME_COLS);
    }

    // ── (#1612) What a row actually calls a mission ─────────────────────────

    /// The two description shapes measured on a real 92-mission board, and the
    /// fallback. The `dispatch: ` prefix goes because `kind_glyph` already
    /// carries "this is one role, not a graph".
    #[test]
    fn display_label_prefers_the_description_and_drops_the_dispatch_prefix() {
        let mut m = mission("dispatch-code-reviewer-1785589698-5d6a-0", MissionStatus::Finalized);
        m.description = "dispatch: code-reviewer".into();
        assert_eq!(display_label(&m), "code-reviewer");

        m.description = "PR review — kstrat2001/darkmux@38031a5".into();
        assert_eq!(display_label(&m), "PR review — kstrat2001/darkmux@38031a5");

        // No description at all: an id is a poor label, never a wrong one.
        m.description = "   ".into();
        assert_eq!(display_label(&m), "dispatch-code-reviewer-1785589698-5d6a-0");
    }

    /// (#2406 CONSIDER 6) A `review`-launched mission's `description` is the
    /// config's own ~200-word launcher documentation (see
    /// `ensure_mission_and_phases_with_provenance_and_start_payload` — the
    /// per-launch `description` argument is always `None` on this path, so
    /// `Mission.description` falls to `config.description`). The title must
    /// be the config's declared `name` ("Review"), never that paragraph.
    ///
    /// `#[serial]` — resolves "review" through `mission_config::load::load`,
    /// which reads `DARKMUX_HOME`-scoped user config dirs before falling
    /// back to the compiled-in embedded copy; scoped to a fresh tempdir
    /// here (round 2, #2434) so this never reads whatever the OPERATOR'S
    /// real `~/.darkmux/mission-configs/` happens to hold on the machine
    /// running the suite.
    #[test]
    #[serial_test::serial]
    fn display_label_prefers_the_config_name_over_a_config_launched_missions_own_long_description() {
        // (#2682 fix-pass round 2, MUST FIX 4) `DarkmuxHomeGuard` instead
        // of a hand-rolled save/restore: every `unwrap()`/`assert!` below
        // used to sit BETWEEN the set and the restore, so one real failure
        // left `DARKMUX_HOME` pointing at a `TempDir` about to be dropped
        // and every subsequent serial test in the process read through a
        // deleted directory — one failure rendering as a wall of them.
        let _home = DarkmuxHomeGuard::new();

        let mut m = mission("review-1788656497-cf872b", MissionStatus::Active);
        m.description = "(#2310 P4d) The code review, built on the shared mission building \
             blocks rather than a pipeline of its own — and, since P4d, the ONLY `review`: \
             the bespoke funnel launcher and its ten Tier-3 step kinds are deleted."
            .into();
        m.spec = Some(crate::crew::types::MissionSpec {
            config_id: "review".to_string(),
            inputs_fingerprint: "x".to_string(),
            origin: None,
        });

        let label = display_label(&m);

        assert_eq!(
            label, "Review",
            "the board must show the config's declared name, not its long description"
        );
    }

    /// (#2406 CONSIDER 6) A mission with no resolvable config (a
    /// `dispatch <role>` crew-of-one, whose `spec.config_id` is the literal
    /// `"dispatch"` sentinel, not a loadable config id) keeps the existing
    /// description-based label — `config_title` must refuse rather than
    /// silently returning some other config's name. No `DARKMUX_HOME`
    /// scoping needed: `"dispatch"` is short-circuited before `load` ever
    /// runs (round 2, #2434), so this never touches disk regardless of what
    /// the ambient home holds.
    #[test]
    fn display_label_falls_back_to_description_when_no_config_resolves() {
        let mut m = mission("dispatch-code-reviewer-1785589698-5d6a-0", MissionStatus::Active);
        m.description = "dispatch: code-reviewer".into();
        m.spec = Some(minted_spec()); // config_id: "dispatch" — not a real config

        assert_eq!(display_label(&m), "code-reviewer");
        assert_eq!(config_title(&m), None);
    }

    /// (#2406 CONSIDER 6, round 2) The description only ever earns a SECOND
    /// line — never printed at all when the config name didn't win the
    /// title (so a `dispatch <role>` mission, or one with no spec, prints
    /// nothing extra) — and, when it does, it is cut at a real sentence
    /// boundary and hard-capped, never mid-identifier.
    ///
    /// Uses the mission config's REAL, compiled-in `review.json`
    /// description as its fixture (via `mission_config::load::load`, the
    /// exact path `Mission.description` is populated from at launch) rather
    /// than a hand-typed copy — a hand-edited fixture is exactly what let
    /// the original bug (splitting on the FIRST bare `.`, which lands
    /// inside the literal step-kind name `` `review.*` `` and produces
    /// "...its ten Tier-3 `review.") stay green: the shortened fixture
    /// simply didn't carry that clause. This one can't drift out of sync
    /// with what the board actually prints, because it IS what the board
    /// prints from.
    ///
    /// `#[serial]` for the same `DARKMUX_HOME` reason as the test above.
    #[test]
    #[serial_test::serial]
    fn description_note_is_the_first_sentence_capped_and_never_mid_identifier() {
        // (MUST FIX 4) See the sibling test above for why this is a guard
        // and no longer a hand-rolled save/restore.
        let _home = DarkmuxHomeGuard::new();

        let real_description =
            crew::mission_config::load::load("review").unwrap().config.description.unwrap();

        let mut m = mission("review-1788656497-cf872b", MissionStatus::Active);
        m.description = real_description.clone();

        // No spec yet: the description IS the title, so no second line.
        let note_without_config = description_note(&m);

        m.spec = Some(crate::crew::types::MissionSpec {
            config_id: "review".to_string(),
            inputs_fingerprint: "x".to_string(),
            origin: None,
        });
        let note = description_note(&m);

        assert_eq!(note_without_config, None);

        let note = note.expect("a config-launched mission with a non-empty description gets a note");
        assert!(
            note.chars().count() <= DESCRIPTION_NOTE_CAP_CHARS + 1, // +1 for the trailing "…"
            "must respect the board-row cap: {note:?} ({} chars)",
            note.chars().count()
        );
        assert!(note.ends_with('…'), "the true first sentence exceeds the cap, so it must be marked cut: {note:?}");
        assert!(
            !note.ends_with("review.…") && !note.ends_with("review."),
            "must never reproduce the original defect (cut inside the literal `review.*` step-kind name): {note:?}"
        );
        assert_eq!(
            note.matches('`').count() % 2,
            0,
            "an odd backtick count means the cut landed INSIDE a backtick-quoted identifier: {note:?}"
        );
        assert!(
            real_description.starts_with(note.trim_end_matches('…').trim_end()),
            "the note's text must be a verbatim PREFIX of the real description, not a rephrasing: {note:?}"
        );
    }

    /// (#2406 CONSIDER 6, round 2) Isolates the sentence-boundary defect from
    /// the cap: the real `review.json` description's mid-token dot (inside
    /// `` `review.*` ``) sits at char ~198, well past the 120-char cap, so
    /// an end-to-end test through `description_note` never actually
    /// exercises the naive "split on the first '.'" bug — the cap happens
    /// to truncate before reaching it either way. This crafts a SHORT
    /// string with the identical defect shape (a mid-token dot before the
    /// real terminator) that sits entirely inside the cap, so it fails
    /// under the naive split regardless of any cap interaction.
    #[test]
    fn first_sentence_treats_a_mid_token_dot_as_punctuation_not_a_terminator() {
        let d = "This mentions `review.*` inline. Full stop.";
        assert_eq!(
            first_sentence(d),
            "This mentions `review.*` inline.",
            "a `.` immediately followed by a non-whitespace character is punctuation INSIDE a token, not a sentence boundary"
        );
    }

    /// Both minting formats seen in the wild yield a handle; hand-authored ids
    /// correctly yield none rather than a meaningless word fragment.
    #[test]
    fn short_handle_finds_the_mint_discriminator_or_nothing() {
        // `mint_run_id`: <config>-<secs>-<hex6>
        assert_eq!(short_handle("review-1785400940-136e76"), Some("136e76"));
        // the older dispatch path: <config>-<role>-<secs>-<hex>-<n>
        assert_eq!(short_handle("dispatch-code-reviewer-1785589698-5d6a-0"), Some("5d6a"));
        // Hand-authored ids have no discriminator — a column of word fragments
        // would be worse than an empty column.
        assert_eq!(short_handle("doom-loop-m4"), None);
        assert_eq!(short_handle("104-daemon-observability"), None);
        // The `-0` counter is below the hex floor, so it can never be picked as
        // the handle in preference to the real one.
        assert!(short_handle("dispatch-x-1785589698-5d6a-0") != Some("0"));
    }

    /// Rounds DOWN and emits exactly one unit, so the column is `AGE_COLS` wide
    /// for every value it can produce short of a hundred weeks.
    #[test]
    fn relative_age_is_one_unit_and_fits_its_column() {
        assert_eq!(relative_age(30, 0), "now");
        assert_eq!(relative_age(59, 0), "now");
        assert_eq!(relative_age(60, 0), "1m");
        assert_eq!(relative_age(3_599, 0), "59m");
        assert_eq!(relative_age(3_600, 0), "1h");
        assert_eq!(relative_age(86_399, 0), "23h");
        assert_eq!(relative_age(86_400, 0), "1d");
        assert_eq!(relative_age(2_591_999, 0), "29d");
        assert_eq!(relative_age(2_592_000, 0), "4w");
        // A clock that went backwards must not underflow into a huge age.
        assert_eq!(relative_age(0, 5_000), "now");
        for secs in [0u64, 61, 4_000, 90_000, 3_000_000, 60_000_000] {
            assert!(
                relative_age(secs, 0).chars().count() <= AGE_COLS,
                "{secs}s rendered wider than AGE_COLS"
            );
        }
    }

    /// Single-width, both of them — an emoji here would overflow every row it
    /// appeared on, because every budget in this module is exact `chars()` math.
    #[test]
    fn kind_glyph_separates_graphs_from_single_role_dispatches() {
        assert_eq!(kind_glyph(3), "◆");
        assert_eq!(kind_glyph(1), "•");
        assert_eq!(kind_glyph(0), "•");
        for total in [0, 1, 2, 9] {
            assert_eq!(kind_glyph(total).chars().count(), 1);
        }
    }

    /// The new rung in the ladder. Between "everything fits" and "truncate the
    /// name" the handle goes — the age still tells two same-named rows apart,
    /// and the full id is on the row's own link.
    #[test]
    fn plan_layout_drops_the_handle_before_truncating_the_name() {
        let m = mission("review-1785400940-136e76", MissionStatus::Active);
        let name_cols = display_label(&m).chars().count();
        let handle_cols = short_handle(&m.id).unwrap().chars().count();

        // Exactly wide enough for the handle but not the mix: handle stays.
        let with_handle = name_cols + ROW_FIXED_COLS + HANDLE_GAP_COLS + handle_cols;
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(with_handle), &mut BTreeMap::new());
        assert!(l.show_handle && !l.show_mix);
        assert_eq!(l.name_width, name_cols);

        // One column short: the handle goes, the name survives INTACT.
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(with_handle - 1), &mut BTreeMap::new());
        assert!(!l.show_handle);
        assert_eq!(l.name_width, name_cols, "the name must not shrink while the handle is droppable");

        // Only below the no-handle row does the name finally truncate.
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(name_cols + ROW_FIXED_COLS - 1), &mut BTreeMap::new());
        assert!(!l.show_handle);
        assert!(l.name_width < name_cols);
    }

    /// A board whose ids carry no discriminator must not pay two gap columns
    /// for a column of blanks.
    #[test]
    fn plan_layout_plans_no_handle_column_when_no_row_has_one() {
        let m = mission("doom-loop-m4", MissionStatus::Active);
        let l = plan_layout([view(&m, 2, 1)].iter(), Some(200), &mut BTreeMap::new());
        assert!(!l.show_handle);
        assert_eq!(l.handle_width, 0);
    }

    /// (#1569 packet A) Mission ids are NOT guaranteed path-safe — `pr-review`
    /// ids embed a full TMPDIR path (#1563) — so an unencoded id would emit a
    /// URL with extra path segments pointing at the wrong route, or none.
    #[test]
    #[serial_test::serial] // mutates DARKMUX_PANEL, a process-global
    fn panel_deep_link_only_fires_inside_a_panel_and_never_at_itself() {
        let base = "http://127.0.0.1:8765/";
        // A terminal has a prompt to type `--all` at, so the hint stays a
        // hint and no link is emitted.
        std::env::remove_var("DARKMUX_PANEL");
        assert_eq!(panel_deep_link(base, "mission-status-all"), None);

        // Rendering into the base panel: the flag becomes reachable.
        std::env::set_var("DARKMUX_PANEL", "mission-status");
        assert_eq!(
            panel_deep_link(base, "mission-status-all").as_deref(),
            Some("http://127.0.0.1:8765/#lens=console&panel=mission-status-all")
        );

        // Already the unlimited panel — a link to where you are is noise, and
        // it is the one case the caller's `shown < len` guard would not catch
        // if the section limit ever applied under `--all`.
        std::env::set_var("DARKMUX_PANEL", "mission-status-all");
        assert_eq!(panel_deep_link(base, "mission-status-all"), None);
        std::env::remove_var("DARKMUX_PANEL");
    }

    #[test]
    fn mission_url_percent_encodes_ids_that_are_not_path_safe() {
        let base = "http://127.0.0.1:8765/";
        assert_eq!(mission_url(base, "doom-loop-m4"), "http://127.0.0.1:8765/mission/doom-loop-m4/graph");
        // The #1563 shape: a slash would otherwise open a new path segment.
        assert_eq!(
            mission_url(base, "review-/tmp/x"),
            "http://127.0.0.1:8765/mission/review-%2Ftmp%2Fx/graph"
        );
        // `?`/`#` would truncate the path into a query/fragment.
        assert_eq!(
            mission_url(base, "a?b#c"),
            "http://127.0.0.1:8765/mission/a%3Fb%23c/graph"
        );
        // RFC 3986 unreserved characters survive unescaped.
        assert_eq!(
            mission_url(base, "a-b_c.d~e"),
            "http://127.0.0.1:8765/mission/a-b_c.d~e/graph"
        );
    }

    /// (#1569 packet A) The id column must stay aligned once ids carry OSC 8
    /// escapes. `{:<width$}` counts the escape BYTES, so formatting a
    /// linkified id directly would silently destroy every column to its
    /// right — the failure the layout planner exists to prevent, reintroduced
    /// by the feature. Padding is therefore computed from the VISIBLE text.
    // Mutates the process-global colorize override — see the note on
    // `style::set_colorize_override`. Without this a concurrent test flips it
    // mid-assertion and `link()` returns plain text (the #1544 class, and I
    // reproduced it writing these).
    #[test]
    #[serial_test::serial]
    fn linkified_id_cell_pads_by_visible_width_not_byte_length() {
        style::set_colorize_override(Some(true));
        let id = ellipsize("m1", 12);
        let cell = format!(
            "{}{}",
            style::link(&mission_url("http://127.0.0.1:8765/", "m1"), &id),
            " ".repeat(12usize.saturating_sub(id.chars().count()))
        );
        style::set_colorize_override(None);

        // The cell carries the escape…
        assert!(cell.contains("\x1b]8;;"), "{cell:?}");
        // …and exactly the padding the plain form would have had.
        let visible: String = strip_ansi(&cell);
        assert_eq!(visible, format!("{:<12}", "m1"), "visible width must match the plain cell");
    }

    /// (#2310 fix-loop C2 / C2-3) The disk→rule wiring itself, pinned
    /// directly: `live_steps_for` is what turns persisted step records into
    /// the map `live_step_drifts` judges, and it was unpinned — mutating it
    /// to return an empty map left the whole suite green while the board
    /// silently stopped seeing this class of drift. `#[serial]` — mutates
    /// DARKMUX_HOME, a process-global.
    #[test]
    #[serial_test::serial]
    fn live_steps_for_reads_the_live_steps_of_a_terminal_phase() {
        // (MUST FIX 4) A guard, not a hand-rolled save/restore — the
        // `save_step` unwraps below sat between the set and the restore.
        let _home = DarkmuxHomeGuard::new();

        let m = mission("m1", MissionStatus::Finalized);
        let closed = phase("m1-p1", "m1", PhaseStatus::Complete);
        let step = |id: &str, status: crew::types::NodeStatus| {
            let s = crew::types::Step {
                id: id.to_string(),
                task_id: format!("{id}-task"),
                gate: None,
                kind: "procedural.noop".to_string(),
                status,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: None,
            };
            crew::lifecycle::save_step("m1", "m1-p1", &s).unwrap();
        };
        step("s-done", crew::types::NodeStatus::Complete);
        step("s-planned", crew::types::NodeStatus::Planned);
        step("s-running", crew::types::NodeStatus::Running);

        let live = live_steps_for(&m, &[&closed]);

        assert_eq!(
            live.get("m1-p1").cloned(),
            Some(vec!["s-planned".to_string(), "s-running".to_string()]),
            "only the NON-terminal steps, and they must actually be read off disk: {live:?}"
        );
        // …and the rule this feeds fires on exactly that map.
        let drifts = live_step_drifts(&m, &[&closed], &live);
        assert!(
            drifts.iter().any(|d| d.kind == "phase-terminal-live-step"),
            "{drifts:?}"
        );
    }

    /// (#1569 packet A) A narrow terminal elides the id for DISPLAY, but the
    /// link must still target the FULL id — otherwise every row on a narrow
    /// terminal links to a mission that doesn't exist, and the failure is a
    /// 404 the operator would reasonably blame on the daemon rather than on
    /// the renderer. The display text and the URL come from different values
    /// on purpose; this pins that they stay different.
    // Mutates the process-global colorize override — see the note on
    // `style::set_colorize_override`. Without this a concurrent test flips it
    // mid-assertion and `link()` returns plain text (the #1544 class, and I
    // reproduced it writing these).
    #[test]
    #[serial_test::serial]
    fn a_narrowed_id_still_links_to_the_full_mission() {
        style::set_colorize_override(Some(true));
        let full = "dispatch-code-reviewer-1785570518-17301-0";
        let shown = ellipsize(full, 18);
        assert_ne!(shown, full, "precondition: this width must actually elide");
        assert!(shown.contains('…'), "{shown}");

        let cell = style::link(&mission_url("http://127.0.0.1:8765/", full), &shown);
        style::set_colorize_override(None);

        // The visible text is the elided form…
        assert!(strip_ansi(&cell).contains('…'), "{cell:?}");
        // …while the target carries the whole id, unelided.
        assert!(cell.contains(&format!("mission/{full}/graph")), "{cell:?}");
        assert!(!cell.contains(&format!("mission/{shown}/graph")), "elided id must never be the target: {cell:?}");
    }

    /// Minimal ANSI/OSC stripper for the alignment assertion above — enough
    /// for the two sequences this renderer emits (SGR and OSC 8), not a
    /// general terminal parser.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match chars.next() {
                // OSC: consume through ST (ESC \).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // CSI: consume through the final byte (@..~).
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn ellipsize_preserves_short_ids_and_marks_truncated_ones() {
        assert_eq!(ellipsize("m1", 10), "m1");
        assert_eq!(ellipsize("m-0123456789", 12), "m-0123456789", "exact fit is untouched");
        let cut = ellipsize("m-0123456789", 6);
        assert_eq!(cut, "m-0…89");
        assert_eq!(cut.chars().count(), 6, "must not exceed the budget");
        // Char-counted, not byte-counted: a multi-byte id must not be cut
        // mid-character (which would emit invalid UTF-8 to the terminal).
        assert_eq!(ellipsize("mécanique", 4), "mé…e");
        assert_eq!(ellipsize("m-0123456789", 1), "…");
    }

    #[test]
    fn ellipsize_keeps_suffix_discriminated_ids_distinguishable() {
        // The real regression this guards: darkmux's machine-minted ids differ
        // only in their SUFFIX, so tail-truncation collapses a whole screenful
        // into one indistinguishable string. Middle elision keeps them apart.
        let a = ellipsize("dispatch-code-reviewer-1785386551-4b71-0", 20);
        let b = ellipsize("dispatch-code-reviewer-1785384819-157-0", 20);
        assert_ne!(a, b, "rows differing only by suffix must not render identically");
        assert!(a.ends_with("4b71-0"), "the discriminating suffix survives: {a}");
        assert!(a.starts_with("dispatch-"), "the identifying prefix survives: {a}");
        assert_eq!(a.chars().count(), 20);
    }

    #[test]
    fn finalized_mission_with_open_phase_no_longer_drifts() {
        // (#1463) The "finalized-with-open-phase" arm retired: `mission
        // finalize` / `mission abort` reconcile every phase to terminal as
        // part of closing, so this is no longer a reachable state — and a
        // Finalized mission never surfaces a drift on this axis anymore,
        // even if a hand-edited JSON produced one. (Legacy on-disk data is a
        // `mission finalize`/`abort` re-run away from clean.)
        let m = mission("m1", MissionStatus::Finalized);
        let running = phase("s1", "m1", PhaseStatus::Running);
        let planned = phase("s2", "m1", PhaseStatus::Planned);
        assert!(detect_drift(&m, &[&running, &planned], &BTreeMap::new(), None, None, 0, 14).is_empty());
    }

    #[test]
    fn finalized_mission_all_terminal_is_clean() {
        let m = mission("m1", MissionStatus::Finalized);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        assert!(detect_drift(&m, &[&s], &BTreeMap::new(), None, None, 0, 14).is_empty());
    }

    #[test]
    fn active_mission_all_terminal_suggests_finalize() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        let d = detect_drift(&m, &[&s], &BTreeMap::new(), None, None, 0, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "done-not-finalized");
        assert!(d[0].suggest[0].contains("mission finalize m1"));
    }

    #[test]
    fn active_mission_with_running_phase_is_clean() {
        // Work in flight is normal, not drift.
        let m = mission("m1", MissionStatus::Active);
        let s = phase("s1", "m1", PhaseStatus::Running);
        assert!(detect_drift(&m, &[&s], &BTreeMap::new(), None, None, 0, 14).is_empty());
    }

    #[test]
    fn active_mission_only_abandoned_is_not_done() {
        // All terminal but nothing COMPLETE → not "done", don't nag to close.
        let m = mission("m1", MissionStatus::Active);
        let s = phase("s1", "m1", PhaseStatus::Abandoned);
        assert!(detect_drift(&m, &[&s], &BTreeMap::new(), None, None, 0, 14).is_empty());
    }

    #[test]
    fn mission_with_no_phases_is_clean() {
        let m = mission("m1", MissionStatus::Active);
        assert!(detect_drift(&m, &[], &BTreeMap::new(), None, None, 0, 14).is_empty());
    }

    // ─── stale-active (#1230 Packet 5) ─────────────────────────────────

    #[test]
    fn stale_active_mission_past_threshold_drifts() {
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        // No phases at all — zero complete either way.
        let now = 15 * 86_400; // 15 days later
        let d = detect_drift(&m, &[], &BTreeMap::new(), None, None, now, 14);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "stale-active");
        assert!(d[0].detail.contains("15 day"));
    }

    /// (#1582) Every command the operator might actually RUN is its own
    /// suggestion, because only the pre-`#` segment of a suggestion is
    /// protected from wrapping. A command left inside rationale prose gets
    /// word-wrapped with the rationale's indent injected mid-command and
    /// will not survive a copy-paste — the exact failure the #1569
    /// verbatim-command rule exists to prevent.
    #[test]
    fn stale_active_actionable_commands_are_each_their_own_suggestion() {
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let d = detect_drift(&m, &[], &BTreeMap::new(), None, None, 15 * 86_400, 14);
        let stale = d.iter().find(|dr| dr.kind == "stale-active").expect("stale-active drift");

        for want in ["darkmux mission abort m1", "darkmux mission finalize m1"] {
            let is_own_command = stale
                .suggest
                .iter()
                .any(|s| split_suggestion(s).0 == want);
            assert!(is_own_command, "`{want}` must be a suggestion's own verbatim command, not prose");
        }

        // …and no suggestion's RATIONALE smuggles a runnable command back in.
        for s in &stale.suggest {
            let note = split_suggestion(s).1;
            assert!(
                !note.contains("darkmux mission abort") && !note.contains("darkmux mission finalize"),
                "rationale must not embed a runnable command (it would wrap): {note}"
            );
        }
    }

    /// (#1665) The "inspect the phase details first" suggestion used to
    /// name `mission status --json`, whose per-mission JSON (`board_json`)
    /// carries only status COUNTS — never a phase id or per-phase status
    /// (see `board_json_is_complete_regardless_of_what_a_human_board_would_
    /// hide` below for that shape). An operator following the suggestion
    /// verbatim got a command that could not deliver what it promised.
    /// `mission debrief <id> --json` is the command that actually emits
    /// `phases[].{id,status,reason}` — this pins the fix and guards against
    /// a future edit reverting the pointer without noticing why.
    #[test]
    fn stale_active_phase_detail_suggestion_names_a_command_that_can_deliver_it() {
        let mut m = mission("m9", MissionStatus::Active);
        m.started_ts = Some(0);
        let d = detect_drift(&m, &[], &BTreeMap::new(), None, None, 15 * 86_400, 14);
        let stale = d.iter().find(|dr| dr.kind == "stale-active").expect("stale-active drift");
        let first_cmd = split_suggestion(&stale.suggest[0]).0;
        assert_eq!(
            first_cmd, "darkmux mission debrief m9 --json",
            "the phase-detail suggestion must be a real per-phase read, not `mission status --json` \
             (whose JSON carries only counts): got `{first_cmd}`"
        );
    }

    #[test]
    fn active_mission_within_staleness_threshold_is_clean() {
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let now = 5 * 86_400; // only 5 days in — under the 14-day default
        assert!(detect_drift(&m, &[], &BTreeMap::new(), None, None, now, 14).is_empty());
    }

    #[test]
    fn active_mission_never_started_is_not_flagged_stale() {
        // started_ts: None (never actually kicked off) — can't judge
        // staleness, fails closed rather than flagging.
        let m = mission("m1", MissionStatus::Active);
        assert!(m.started_ts.is_none());
        assert!(detect_drift(&m, &[], &BTreeMap::new(), None, None, 999 * 86_400, 14).is_empty());
    }

    #[test]
    fn active_mission_with_a_complete_phase_is_not_flagged_stale() {
        // Old started_ts, but at least one phase completed — progress is
        // happening, this is `done-not-finalized`/normal territory, not stale.
        let mut m = mission("m1", MissionStatus::Active);
        m.started_ts = Some(0);
        let s = phase("s1", "m1", PhaseStatus::Complete);
        let d = detect_drift(&m, &[&s], &BTreeMap::new(), None, None, 30 * 86_400, 14);
        // `done-not-finalized` fires (all terminal + complete>0), but NOT
        // `stale-active`.
        assert!(!d.iter().any(|dr| dr.kind == "stale-active"));
    }

    // ─── running-phase-session-dead (#2682) ────────────────────────────

    #[test]
    fn running_phase_session_drift_fires_when_local_status_is_abandoned() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("p1", "m1", PhaseStatus::Running);
        let d = detect_drift(&m, &[&s], &BTreeMap::new(), Some(RunStatus::Abandoned), Some(DispatchSessionEvidence::StaleNoTerminal), 0, 14);
        let kinds: Vec<&str> = d.iter().map(|x| x.kind).collect();
        let hit = d
            .iter()
            .find(|dr| dr.kind == "running-phase-session-dead")
            .unwrap_or_else(|| panic!("no running-phase-session-dead drift: {kinds:?}"));
        assert!(hit.detail.contains("p1"), "{}", hit.detail);
        // Describes, never adjudicates (#2682's own doctrine, matching
        // `peer_status_word`'s posture) — must say what was OBSERVED, never
        // assert the mission crashed/failed/should be torn down.
        assert!(
            hit.detail.contains("no evidence of life") || hit.detail.contains("no terminal record")
        );
        assert!(
            !hit.detail.to_lowercase().contains("crash")
                && !hit.detail.to_lowercase().contains("failed"),
            "must describe, not adjudicate: {}",
            hit.detail
        );
        // `StaleNoTerminal` is a genuine liveness judgment (a real session
        // existed and went stale), so abort stays a real option here.
        assert!(
            hit.suggest.iter().any(|s| s.contains("mission abort")),
            "a genuinely stale session should still offer abort: {:?}",
            hit.suggest
        );
    }

    /// (#2682 fix-pass round 2, MUST FIX 1) A mission with NO attributable
    /// session at all must fire NOTHING. Round 1 re-worded this arm and
    /// left it firing; two measured probes then showed the two shapes that
    /// reach it — a mission parked at a sign-off gate past the 20-minute
    /// default budget, and a mission emitting records THIS SECOND under a
    /// session id refused as ambiguous — are both false alarms.
    /// `NoAttributableSession` carries no dispatch-liveness information,
    /// which is this rule's entire subject.
    #[test]
    fn running_phase_session_drift_stays_silent_with_no_attributable_session() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("p1", "m1", PhaseStatus::Running);
        let d = detect_drift(
            &m,
            &[&s],
            &BTreeMap::new(),
            Some(RunStatus::Abandoned),
            Some(DispatchSessionEvidence::NoAttributableSession),
            0,
            14,
        );
        assert!(
            !d.iter().any(|dr| dr.kind == "running-phase-session-dead"),
            "a mission with no attributable dispatch session must not be flagged as one whose \
             dispatch session died: {d:?}"
        );
    }

    /// `local_evidence: None` alongside `local_status: Some(Abandoned)` — a
    /// caller that classified the mission Abandoned but could not (or did
    /// not) name a reason — must take the SAME silent road as
    /// `NoAttributableSession`. An unnamed reason is not an observation.
    #[test]
    fn running_phase_session_drift_stays_silent_when_evidence_is_missing() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("p1", "m1", PhaseStatus::Running);
        let d = detect_drift(&m, &[&s], &BTreeMap::new(), Some(RunStatus::Abandoned), None, 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "running-phase-session-dead"), "{d:?}");
    }

    /// (#2682 fix-pass MUST FIX 5) darkmux POSITIVELY recorded the session
    /// ending (`session.end`) — an observation, not an absence. The wording
    /// must say so, never "no evidence of life", and abort stays offered.
    #[test]
    fn running_phase_session_drift_recorded_end_describes_an_observed_stop() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("p1", "m1", PhaseStatus::Running);
        let d = detect_drift(
            &m,
            &[&s],
            &BTreeMap::new(),
            Some(RunStatus::Abandoned),
            Some(DispatchSessionEvidence::RecordedEnd),
            0,
            14,
        );
        let hit = d
            .iter()
            .find(|dr| dr.kind == "running-phase-session-dead")
            .unwrap_or_else(|| panic!("no running-phase-session-dead drift: {d:?}"));
        assert!(
            hit.detail.to_lowercase().contains("recorded") && hit.detail.to_lowercase().contains("ending"),
            "must describe the POSITIVE observation, not an absence: {}",
            hit.detail
        );
        assert!(
            !hit.detail.to_lowercase().contains("no evidence of life"),
            "a recorded end is a fact, not the same claim as silence: {}",
            hit.detail
        );
        assert!(
            hit.suggest.iter().any(|s| s.contains("mission abort")),
            "a positively recorded end is a reasonable abort case: {:?}",
            hit.suggest
        );
    }

    /// (#2682 fix-pass CONSIDER 2) Multiple Running phases must ALL be named
    /// in the detail, joined — not just the first one. Mutating
    /// `running.join(", ")` down to `running[0]` must fail this test.
    #[test]
    fn running_phase_session_drift_names_every_running_phase_not_just_the_first() {
        let m = mission("m1", MissionStatus::Active);
        let p1 = phase("p1", "m1", PhaseStatus::Running);
        let p2 = phase("p2", "m1", PhaseStatus::Running);
        let d = detect_drift(
            &m,
            &[&p1, &p2],
            &BTreeMap::new(),
            Some(RunStatus::Abandoned),
            Some(DispatchSessionEvidence::StaleNoTerminal),
            0,
            14,
        );
        let hit = d
            .iter()
            .find(|dr| dr.kind == "running-phase-session-dead")
            .unwrap_or_else(|| panic!("no running-phase-session-dead drift: {d:?}"));
        assert!(hit.detail.contains("p1"), "{}", hit.detail);
        assert!(hit.detail.contains("p2"), "{}", hit.detail);
    }

    /// Invariant 2 (issue #2682): a Running phase whose session IS live must
    /// stay clean — no new false positive from this rule.
    #[test]
    fn running_phase_session_drift_stays_clean_when_session_is_live() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("p1", "m1", PhaseStatus::Running);
        let d = detect_drift(&m, &[&s], &BTreeMap::new(), Some(RunStatus::Running), None, 0, 14);
        assert!(
            !d.iter().any(|dr| dr.kind == "running-phase-session-dead"),
            "a live session must never fire this drift: {d:?}"
        );
    }

    /// `local_status: None` (the caller could not classify the mission at
    /// all) must never be treated as "dead" — no evidence either way stays
    /// quiet rather than guessing.
    #[test]
    fn running_phase_session_drift_stays_clean_when_status_is_unknown() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("p1", "m1", PhaseStatus::Running);
        let d = detect_drift(&m, &[&s], &BTreeMap::new(), None, None, 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "running-phase-session-dead"), "{d:?}");
    }

    /// (#2682 fix-pass review CONSIDER 1) A `Paused` mission's
    /// `local_status` CAN read `Abandoned` in production — a `session.end`
    /// terminal (`DispatchSessionEvidence::RecordedEnd`) lands regardless
    /// of `mission.status`. What actually keeps this rule quiet for a
    /// Paused mission is its OWN `m.status != Active` guard, independent of
    /// which road `local_status` took to reach `Abandoned` — pinned here
    /// with `RecordedEnd` specifically (the one evidence value a Paused
    /// mission can genuinely carry) rather than the fixture's previous
    /// `StaleNoTerminal`, which a Paused mission can never actually produce
    /// (the staleness gate itself IS skipped for `Paused` — see
    /// `mission_run_status_and_evidence`'s own doc) and so was pinning a
    /// state that could never occur, not the guard that matters.
    #[test]
    fn running_phase_session_drift_stays_clean_for_a_paused_mission() {
        let m = mission("m1", MissionStatus::Paused);
        let s = phase("p1", "m1", PhaseStatus::Running);
        let d = detect_drift(
            &m,
            &[&s],
            &BTreeMap::new(),
            Some(RunStatus::Abandoned),
            Some(DispatchSessionEvidence::RecordedEnd),
            0,
            14,
        );
        assert!(!d.iter().any(|dr| dr.kind == "running-phase-session-dead"), "{d:?}");
    }

    #[test]
    fn running_phase_session_drift_stays_clean_with_no_running_phase() {
        let m = mission("m1", MissionStatus::Active);
        let s = phase("p1", "m1", PhaseStatus::Planned);
        let d = detect_drift(&m, &[&s], &BTreeMap::new(), Some(RunStatus::Abandoned), Some(DispatchSessionEvidence::StaleNoTerminal), 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "running-phase-session-dead"), "{d:?}");
    }

    // ─── the board-vs-`run list` disagreement matrix (#2682 round 2) ───

    /// The five flow-record shapes the matrix below sweeps, one per row of
    /// its third axis. Each is a genuinely different road
    /// `mission_run_status_and_evidence` can take, NOT five spellings of
    /// the same one.
    #[derive(Clone, Copy, Debug)]
    enum MatrixFlow {
        /// No records at all — the session pool is empty, so the verdict
        /// rests on the mission's own AGE (`NoAttributableSession`).
        NoRecords,
        /// One open session whose last activity is NOW — live.
        LiveSession,
        /// One open session whose last activity is far past any budget and
        /// which never reached a terminal (`StaleNoTerminal`).
        StaleOpenSession,
        /// One session darkmux positively saw END (`session.end` →
        /// `RecordedEnd`).
        RecordedEnd,
        /// One session that reached a `dispatch error` terminal — a real
        /// terminal signal of its own, deliberately NOT `Abandoned`.
        ErrorTerminal,
    }

    /// (#2682 fix-pass round 2, CONSIDER 1) The scope claim in
    /// [`running_phase_session_drift`]'s own doc — "N rows where `darkmux
    /// run list` reads a mission `Abandoned` while this board stays
    /// silent" — DERIVED by running the real pair over the whole matrix,
    /// never hand-counted. Any future change to either side moves these
    /// numbers and fails here, which is the only way a prose count in a
    /// doc comment can be kept honest.
    ///
    /// The matrix is 4 `MissionStatus` × 5 phase shapes (none, plus each
    /// `PhaseStatus`) × 5 [`MatrixFlow`] shapes = 100 rows. For each row it
    /// asks `darkmux_serve::local_dispatch_status` (the SAME computation
    /// `darkmux run list` renders from) for a verdict, hands that verdict
    /// to this module's own `detect_drift`, and records the rows where the
    /// two disagree.
    ///
    /// **The counting subtlety, stated so a recount doesn't come out
    /// wrong.** The naive "`run list` says `Abandoned`, board is silent"
    /// predicate returns 58, not 33. Twenty-five of those are the whole
    /// `MissionStatus::Aborted` block (5 × 5), and they are NOT
    /// disagreements: an aborted mission's row carries `abandoned_reason =
    /// Aborted`, which `run_list::subtitle_for` renders as the literal word
    /// "aborted" (its sibling `AbandonReason::NoTerminal` is the one that
    /// reads "no ending recorded") — the same thing the board itself shows
    /// for a mission the operator tore down. Splitting on the REASON is
    /// what turns the raw count into
    /// the real one. A future reader recounting without that split will
    /// get 58 and think this doc drifted.
    ///
    /// Pins `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` (MUST FIX 2): the
    /// `NoRecords` row's verdict is the mission's age measured against
    /// `stale_after_ms()`, so the matrix would otherwise re-shape itself
    /// under an operator's exported budget.
    #[test]
    #[serial_test::serial]
    fn board_vs_run_list_disagreement_matrix_is_exactly_as_documented() {
        let _home = DarkmuxHomeGuard::new();
        // 60s knob → a 120s staleness budget: the fixtures below sit 90
        // minutes back, unambiguously outside it.
        let _budget = InactivityBudgetGuard::seconds(60);

        let now = now_unix();
        let mission_statuses = [
            MissionStatus::Active,
            MissionStatus::Paused,
            MissionStatus::Aborted,
            MissionStatus::Finalized,
        ];
        let phase_shapes = [
            None,
            Some(PhaseStatus::Planned),
            Some(PhaseStatus::Running),
            Some(PhaseStatus::Complete),
            Some(PhaseStatus::Abandoned),
        ];
        let flow_shapes = [
            MatrixFlow::NoRecords,
            MatrixFlow::LiveSession,
            MatrixFlow::StaleOpenSession,
            MatrixFlow::RecordedEnd,
            MatrixFlow::ErrorTerminal,
        ];

        let mut rows = 0usize;
        // Predicate 1 — "THIS RULE stayed silent": `Abandoned` per `run
        // list`, no `running-phase-session-dead` drift. This is the one the
        // scope claim in `running_phase_session_drift`'s doc is about
        // ("this rule only fires for Running").
        let mut raw_rule_silent = 0usize;
        let mut real_rule_silent = 0usize;
        let mut per_status: BTreeMap<String, usize> = BTreeMap::new();
        // Predicate 2 — "the WHOLE BOARD stayed silent": `Abandoned` per
        // `run list` and NO drift of any kind. Strictly narrower, because
        // an Active/Paused mission with a Complete phase already draws
        // `done-not-finalized`. Reported so a recount under either reading
        // lands on a number this test names.
        let mut raw_board_silent = 0usize;
        let mut real_board_silent = 0usize;
        let mut detail: Vec<String> = Vec::new();

        for (mi, ms) in mission_statuses.iter().enumerate() {
            for (pi, ps) in phase_shapes.iter().enumerate() {
                for (fi, fs) in flow_shapes.iter().enumerate() {
                    rows += 1;
                    let id = format!("matrix-{mi}-{pi}-{fi}");
                    let mut m = mission(&id, *ms);
                    // 90 minutes old: past the pinned budget, so the
                    // `NoRecords` age branch genuinely fires.
                    m.started_ts = Some(now.saturating_sub(90 * 60));
                    let phases: Vec<Phase> = ps
                        .map(|status| phase(&format!("{id}-p"), &id, status))
                        .into_iter()
                        .collect();
                    m.phase_ids = phases.iter().map(|p| p.id.clone()).collect();
                    crew::lifecycle::save_mission(&m).unwrap();
                    for p in &phases {
                        crew::lifecycle::save_phase(p).unwrap();
                    }

                    let flows = tempfile::TempDir::new().unwrap();
                    let session = format!("{id}-s");
                    let rec = |action: &str, ts: &str| {
                        serde_json::json!({
                            "ts": ts,
                            "action": action,
                            "session_id": session,
                            "mission_id": id,
                            "handle": "coder",
                        })
                        .to_string()
                    };
                    // A fixed past stamp rather than "an hour ago": no
                    // arithmetic, and nothing that can land on the wrong
                    // side of a UTC midnight while the suite runs.
                    let old = "2024-01-01T09:00:00Z";
                    let live = darkmux_flow::ts_utc_now();
                    let lines: Vec<String> = match fs {
                        MatrixFlow::NoRecords => Vec::new(),
                        MatrixFlow::LiveSession => vec![rec("dispatch start", &live)],
                        MatrixFlow::StaleOpenSession => vec![rec("dispatch start", old)],
                        MatrixFlow::RecordedEnd => {
                            vec![rec("dispatch start", old), rec("session.end", old)]
                        }
                        MatrixFlow::ErrorTerminal => {
                            vec![rec("dispatch start", old), rec("dispatch error", old)]
                        }
                    };
                    if !lines.is_empty() {
                        std::fs::write(
                            flows.path().join(format!("{}.jsonl", darkmux_flow::day_utc_now())),
                            format!("{}\n", lines.join("\n")),
                        )
                        .unwrap();
                    }

                    let local = darkmux_serve::local_dispatch_status(
                        std::slice::from_ref(&m),
                        flows.path(),
                        &[],
                    );
                    let (status, evidence) = local
                        .get(&id)
                        .copied()
                        .unwrap_or_else(|| panic!("no local_dispatch_status entry for {id}"));

                    let phase_refs: Vec<&Phase> = phases.iter().collect();
                    // `now: 0` keeps `stale_active_drift` (a DAY-scale rule
                    // on a different clock) out of the measurement — this
                    // matrix is about the `run list` pair, not that rule.
                    let d = detect_drift(
                        &m,
                        &phase_refs,
                        &BTreeMap::new(),
                        Some(status),
                        evidence,
                        0,
                        14,
                    );

                    if status != RunStatus::Abandoned {
                        continue;
                    }
                    // The `abandoned_reason = Aborted` split described in
                    // this test's doc: a torn-down mission reads "aborted"
                    // on BOTH surfaces, so it is not a disagreement.
                    let counts_as_disagreement = *ms != MissionStatus::Aborted;
                    if !d.iter().any(|dr| dr.kind == "running-phase-session-dead") {
                        raw_rule_silent += 1;
                        if counts_as_disagreement {
                            real_rule_silent += 1;
                            *per_status.entry(format!("{ms:?}")).or_default() += 1;
                            detail.push(format!("{ms:?}/{ps:?}/{fs:?} evidence={evidence:?}"));
                        }
                    }
                    if d.is_empty() {
                        raw_board_silent += 1;
                        if counts_as_disagreement {
                            real_board_silent += 1;
                        }
                    }
                }
            }
        }

        // Printed so the numbers in `running_phase_session_drift`'s doc can
        // be re-derived by RUNNING this (`-- --nocapture`), never by
        // editing prose.
        println!("matrix rows: {rows}");
        println!("rule-silent, raw (incl. Aborted): {raw_rule_silent}");
        println!("rule-silent, real disagreements:  {real_rule_silent}");
        println!("board-silent, raw (incl. Aborted): {raw_board_silent}");
        println!("board-silent, real disagreements:  {real_board_silent}");
        println!("per mission status (rule-silent): {per_status:?}");
        for line in &detail {
            println!("  · {line}");
        }

        assert_eq!(rows, 100, "the matrix must stay 4 x 5 x 5");
        assert_eq!(
            raw_rule_silent, 58,
            "naive count changed — see this test's doc on the Aborted split: {detail:?}"
        );
        assert_eq!(real_rule_silent, 33, "the documented disagreement count moved: {detail:?}");
        assert_eq!(
            per_status.get("Active").copied(),
            Some(13),
            "Active breakdown moved: {per_status:?}"
        );
        assert_eq!(
            per_status.get("Finalized").copied(),
            Some(15),
            "Finalized breakdown moved: {per_status:?}"
        );
        assert_eq!(
            per_status.get("Paused").copied(),
            Some(5),
            "Paused breakdown moved: {per_status:?}"
        );
        assert_eq!(raw_board_silent, 54, "whole-board-silent raw count moved");
        assert_eq!(real_board_silent, 29, "whole-board-silent disagreement count moved");
    }

    /// (#2682) The invariant the issue exists to close, pinned directly
    /// against the REAL wiring rather than two independently hand-typed
    /// expectations: build a mission whose dispatch session crashed
    /// (bookend `dispatch start` with no terminal, its own last activity far
    /// past the staleness budget), and feed `darkmux_serve::build_runs`'s
    /// OWN computed status for that mission straight into `detect_drift`.
    /// If a future change ever made these two disagree, this test — which
    /// never hand-types `RunStatus::Abandoned` as the middle value, only as
    /// the final assertion on what `build_runs` itself produced — would
    /// need `build_runs`'s real output to already be wrong before this test
    /// could pass, which is a stronger guarantee than two separately-written
    /// tests that merely happen to agree today.
    #[test]
    #[serial_test::serial]
    fn cli_board_and_run_list_agree_on_a_crashed_local_mission() {
        let guard = DarkmuxHomeGuard::new();
        let tmp_path = guard.path();

        let mut m = mission("dispatch-crashed-2682", MissionStatus::Active);
        m.phase_ids = vec!["p-crash".to_string()];
        // Far enough in the past that it is stale under ANY reasonable
        // inactivity budget, without pinning this test to a literal "now" —
        // matching darkmux-serve's own precedent for this exact scenario
        // (`build_runs_crashed_active_mission_reports_abandoned_not_eternal_running`).
        m.started_ts = Some(1_700_000_000);
        crew::lifecycle::save_mission(&m).unwrap();
        let mut running_phase = phase("p-crash", "dispatch-crashed-2682", PhaseStatus::Running);
        running_phase.task_ids = vec!["t-crash".to_string()];
        crew::lifecycle::save_phase(&running_phase).unwrap();
        let task = crew::types::Task {
            run_on: crew::types::default_run_on(),
            id: "t-crash".to_string(),
            phase_id: "p-crash".to_string(),
            description: "d".to_string(),
            display_name: None,
            step_ids: vec!["s-crash".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: Some("coder".to_string()),
            profile_name: None,
            workdir: None,
            image: None,
        };
        crew::lifecycle::save_task("dispatch-crashed-2682", &task).unwrap();
        let step = crew::types::Step {
            id: "s-crash".to_string(),
            task_id: "t-crash".to_string(),
            gate: None,
            kind: "dispatch.internal".to_string(),
            status: crew::types::NodeStatus::Running,
            config: serde_json::json!({ "session_id": "crew-dispatch-coder-2682" }),
            started_ts: Some(1_700_000_000),
            completed_ts: None,
            output: None,
        };
        crew::lifecycle::save_step("dispatch-crashed-2682", "p-crash", &step).unwrap();

        let flows_dir = tmp_path.join("flows");
        std::fs::create_dir_all(&flows_dir).unwrap();
        let day = darkmux_flow::day_utc_now();
        let mut f = std::fs::File::create(flows_dir.join(format!("{day}.jsonl"))).unwrap();
        use std::io::Write as _;
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "ts": "2024-01-01T09:00:00Z",
                "action": "dispatch start",
                "session_id": "crew-dispatch-coder-2682",
                "handle": "coder",
            })
        )
        .unwrap();
        drop(f);

        let runs = darkmux_serve::build_runs(&flows_dir, None, &[]);
        let run = runs
            .iter()
            .find(|r| r.id == "dispatch-crashed-2682")
            .unwrap_or_else(|| panic!("no Run for the crashed mission: {runs:?}"));

        // What `darkmux run list` reports for this exact mission today.
        assert_eq!(
            run.status,
            RunStatus::Abandoned,
            "fixture must actually reproduce the crashed-session shape: {run:?}"
        );

        // (#2682 fix-pass) What `mission status`'s OWN narrower entry point
        // — the one `run()` actually calls now — reports for the SAME
        // fixture. Must agree with `build_runs` above; if these two ever
        // diverge, that is exactly the two-independently-derived-opinions
        // bug this whole rule exists to prevent.
        let local = darkmux_serve::local_dispatch_status(std::slice::from_ref(&m), &flows_dir, &[]);
        let (local_status, local_evidence) = local
            .get(&m.id)
            .copied()
            .unwrap_or_else(|| panic!("no local_dispatch_status entry for {}", m.id));
        assert_eq!(
            local_status,
            RunStatus::Abandoned,
            "local_dispatch_status disagreed with build_runs for the same crashed fixture"
        );
        assert_eq!(
            local_evidence,
            Some(darkmux_serve::DispatchSessionEvidence::StaleNoTerminal),
            "a session with a start but no terminal, past the staleness budget, must read \
             StaleNoTerminal — got {local_evidence:?}"
        );

        // What the board's OWN drift check does when handed that SAME value.
        let d = detect_drift(
            &m,
            &[&running_phase],
            &BTreeMap::new(),
            Some(local_status),
            local_evidence,
            0,
            14,
        );

        assert!(
            d.iter().any(|dr| dr.kind == "running-phase-session-dead"),
            "`darkmux run list` reads this mission Abandoned but the board stayed clean: {d:?}"
        );
        // `guard` restores DARKMUX_HOME on drop — including if an assert
        // above already panicked, since Drop still runs during unwind.
        drop(guard);
    }

    // ─── step-level drift (#2310 fix-loop C4 / S4-C4) ──────────────────

    /// The S4-1 board shape, hand-built: a Finalized mission whose phase
    /// closed `Complete` while steps under it were still `Planned`. Before
    /// this rule the board printed "board is clean, drift: []" for exactly
    /// this — every check stopped at the phase.
    #[test]
    fn terminal_phase_holding_a_non_terminal_step_drifts() {
        let done = phase("p2", "m1", PhaseStatus::Complete);
        let mut m = mission("m1", MissionStatus::Finalized);
        m.phase_ids = vec!["p2".to_string()];
        let live = BTreeMap::from([(
            "p2".to_string(),
            vec!["s-dep".to_string(), "s-chain".to_string()],
        )]);

        let d = detect_drift(&m, &[&done], &live, None, None, 0, 14);
        let hit = d
            .iter()
            .find(|dr| dr.kind == "phase-terminal-live-step")
            .unwrap_or_else(|| panic!("no step-level drift: {:?}", d.iter().map(|x| x.kind).collect::<Vec<_>>()));
        assert!(hit.detail.contains("s-dep") && hit.detail.contains("s-chain"), "{}", hit.detail);
        assert!(hit.suggest.iter().any(|c| c.contains("mission finalize m1")), "{:?}", hit.suggest);
    }

    /// The SIGKILL shape: the mission closed but a phase it left open still
    /// holds live steps. Named as a MISSION-level contradiction, distinct
    /// from the phase-level one above.
    #[test]
    fn terminal_mission_holding_a_non_terminal_step_drifts() {
        let open = phase("p1", "m1", PhaseStatus::Running);
        let mut m = mission("m1", MissionStatus::Aborted);
        m.phase_ids = vec!["p1".to_string()];
        let live = BTreeMap::from([("p1".to_string(), vec!["s-1".to_string()])]);

        let d = detect_drift(&m, &[&open], &live, None, None, 0, 14);
        assert!(
            d.iter().any(|dr| dr.kind == "mission-terminal-live-step" && dr.detail.contains("s-1")),
            "{:?}",
            d.iter().map(|x| x.kind).collect::<Vec<_>>()
        );
        // Not double-counted as the phase-level kind — the phase is Running.
        assert!(!d.iter().any(|dr| dr.kind == "phase-terminal-live-step"));
    }

    /// An Active mission's Running phase with live steps is exactly where
    /// live steps belong — no drift.
    #[test]
    fn live_steps_under_a_running_phase_of_an_active_mission_are_not_drift() {
        let open = phase("p1", "m1", PhaseStatus::Running);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = vec!["p1".to_string()];
        let live = BTreeMap::from([("p1".to_string(), vec!["s-1".to_string()])]);

        let d = detect_drift(&m, &[&open], &live, None, None, 0, 14);
        assert!(
            !d.iter().any(|dr| dr.kind.ends_with("live-step")),
            "{:?}",
            d.iter().map(|x| x.kind).collect::<Vec<_>>()
        );
    }

    // ─── unreachable-phase, RETIRED (#2406) ────────────────────────────
    //
    // Every test in this section used to assert that `detect_drift` flagged
    // a Planned phase sitting after an Abandoned one as `"unreachable-phase"`
    // and suggested `mission abort <id> --phase <name>`. That rule is gone —
    // see `detect_drift`'s doc and the (now-deleted) `unreachable_phase_drifts`
    // doc comment for the real mission (`review-1788656497-cf872b`) that
    // proved phase-order was never the launcher's actual gating rule. Each
    // test below is KEPT and RENAMED, rather than silently deleted, to pin
    // that its exact old-trigger shape no longer produces the retired kind.

    #[test]
    fn planned_phase_after_abandoned_phase_no_longer_flags_unreachable() {
        // (renamed from `planned_phase_after_abandoned_phase_drifts`)
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let blocked = phase("blocked", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = vec!["dead".to_string(), "blocked".to_string()];

        let d = detect_drift(&m, &[&dead, &blocked], &BTreeMap::new(), None, None, 0, 14);
        assert!(
            !d.iter().any(|dr| dr.kind == "unreachable-phase"),
            "the phase-order rule is retired (#2406): {d:?}"
        );
    }

    #[test]
    fn siblings_blocked_by_one_dead_ancestor_no_longer_flags_unreachable() {
        // (renamed from `siblings_blocked_by_one_dead_ancestor_state_the_rationale_once`)
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let a = phase("blocked-a", "m1", PhaseStatus::Planned);
        let b = phase("blocked-b", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = ["dead", "blocked-a", "blocked-b"].map(String::from).to_vec();

        let d = detect_drift(&m, &[&dead, &a, &b], &BTreeMap::new(), None, None, 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-phase"), "{d:?}");
    }

    #[test]
    fn bare_abort_caveat_no_longer_offered_because_the_drift_is_gone() {
        // (renamed from `bare_abort_caveat_appears_only_when_a_phase_would_be_lost`)
        let healthy = phase("healthy", "m1", PhaseStatus::Planned);
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let blocked = phase("blocked", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = ["healthy", "dead", "blocked"].map(String::from).to_vec();

        let d = detect_drift(&m, &[&healthy, &dead, &blocked], &BTreeMap::new(), None, None, 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-phase"), "{d:?}");
    }

    #[test]
    fn a_running_phase_after_the_dead_ancestor_still_not_flagged() {
        // (renamed from `a_running_phase_after_the_dead_ancestor_counts_as_collateral`)
        let mut dead = phase("dead", "m1", PhaseStatus::Abandoned);
        dead.abandoned_ts = Some(1);
        let running = phase("in-flight", "m1", PhaseStatus::Running);
        let blocked = phase("blocked", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = ["dead", "in-flight", "blocked"].map(String::from).to_vec();

        let d = detect_drift(&m, &[&dead, &running, &blocked], &BTreeMap::new(), None, None, 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-phase"), "{d:?}");
    }

    #[test]
    fn planned_phase_after_healthy_phase_is_not_flagged_unreachable() {
        let done = phase("done", "m1", PhaseStatus::Complete);
        let next = phase("next", "m1", PhaseStatus::Planned);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = vec!["done".to_string(), "next".to_string()];

        let d = detect_drift(&m, &[&done, &next], &BTreeMap::new(), None, None, 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-phase"));
    }

    #[test]
    fn non_planned_phase_after_abandoned_phase_is_not_flagged() {
        // Only PLANNED phases get flagged — a phase that already
        // completed/abandoned/started isn't "stuck", it already resolved.
        let dead = phase("dead", "m1", PhaseStatus::Abandoned);
        let done = phase("done", "m1", PhaseStatus::Complete);
        let mut m = mission("m1", MissionStatus::Active);
        m.phase_ids = vec!["dead".to_string(), "done".to_string()];

        let d = detect_drift(&m, &[&dead, &done], &BTreeMap::new(), None, None, 0, 14);
        assert!(!d.iter().any(|dr| dr.kind == "unreachable-phase"));
    }

    /// (renamed from `doom_loop_m4_mission_status_fixture_flags_both_drift_variants`,
    /// #2406) Reproduces the REAL `doom-loop-m4` mission that used to be this
    /// rule's own acceptance fixture: `mission.json` (Active,
    /// `started_ts: 1782141824`) + its four phases IN ORDER —
    /// `runtime-capture` (Planned), `file-match` (Abandoned),
    /// `sovereignty-verbs` (Planned), `validate-cure` (Planned). Under the
    /// retired rule, both `sovereignty-verbs` and `validate-cure` were
    /// flagged "can never run" purely from sitting after `file-match` in
    /// list order — with no regard for whether their own tasks actually
    /// depended on anything inside it. `stale-active` is unaffected (it
    /// never depended on phase order) and still fires.
    #[test]
    fn doom_loop_m4_mission_status_fixture_no_longer_flags_unreachable() {
        let m = Mission {
            id: "doom-loop-m4".to_string(),
            description: "M4 doom-loop arc".to_string(),
            status: MissionStatus::Active,
            phase_ids: vec![
                "runtime-capture".to_string(),
                "file-match".to_string(),
                "sovereignty-verbs".to_string(),
                "validate-cure".to_string(),
            ],
            created_ts: 1_782_141_824,
            started_ts: Some(1_782_141_824),
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
            machine: None,
        };
        let runtime_capture = phase("runtime-capture", "doom-loop-m4", PhaseStatus::Planned);
        let mut file_match = phase("file-match", "doom-loop-m4", PhaseStatus::Abandoned);
        file_match.started_ts = Some(1_782_141_937);
        file_match.abandoned_ts = Some(1_782_147_136);
        let sovereignty_verbs =
            phase("sovereignty-verbs", "doom-loop-m4", PhaseStatus::Planned);
        let validate_cure = phase("validate-cure", "doom-loop-m4", PhaseStatus::Planned);
        let phases: Vec<&Phase> =
            vec![&runtime_capture, &file_match, &sovereignty_verbs, &validate_cure];

        let now = now_unix(); // real elapsed time since the real started_ts
        let d = detect_drift(&m, &phases, &BTreeMap::new(), None, None, now, 14);

        assert!(
            d.iter().any(|dr| dr.kind == "stale-active"),
            "doom-loop-m4 has sat at 0/4 phases for weeks — must still flag stale-active: {d:?}"
        );
        assert!(
            !d.iter().any(|dr| dr.kind == "unreachable-phase"),
            "the phase-order rule is retired (#2406) — no phase in this fixture should be \
             flagged unreachable any more: {d:?}"
        );
        assert_eq!(d.len(), 1, "only stale-active should fire now: {d:?}");
    }

    /// (#2406) The actual repro that killed the retired rule: a real mission
    /// (`review-1788656497-cf872b`) with `review` Abandoned and `deliver`
    /// sitting Planned — legitimately, since `deliver`'s tasks named no
    /// dependency inside `review` — for 545s before it went on to run and
    /// complete. The retired rule read this shape as "`deliver` can never
    /// run" and suggested `mission abort review-1788656497-cf872b --phase
    /// deliver`, which would have destroyed the delivery mid-flight had the
    /// operator trusted it. This pins that the board now says nothing of the
    /// kind for this exact shape: no `"unreachable-phase"` drift, and no
    /// `mission abort ... --phase deliver` suggestion anywhere on the board.
    #[test]
    fn a_planned_phase_after_an_abandoned_one_that_actually_goes_on_to_run_is_never_told_to_abort() {
        let mut review = phase("review", "review-1788656497-cf872b", PhaseStatus::Abandoned);
        review.abandoned_ts = Some(1_788_657_000);
        let deliver = phase("deliver", "review-1788656497-cf872b", PhaseStatus::Planned);
        let mut m = mission("review-1788656497-cf872b", MissionStatus::Active);
        m.phase_ids = vec!["review".to_string(), "deliver".to_string()];
        m.started_ts = Some(1_788_656_497);

        let d = detect_drift(&m, &[&review, &deliver], &BTreeMap::new(), None, None, 1_788_657_100, 14);

        assert!(
            !d.iter().any(|dr| dr.kind == "unreachable-phase"),
            "must never flag `deliver` unreachable while it is legitimately about to run: {d:?}"
        );
        assert!(
            !d.iter().any(|dr| dr.suggest.iter().any(|c| c.contains("--phase deliver"))),
            "must never suggest aborting the phase that is about to complete: {d:?}"
        );
    }

    #[test]
    fn progress_bar_rounds_sensibly() {
        assert_eq!(progress_bar(0, 1), "░░░░");
        assert_eq!(progress_bar(1, 1), "▓▓▓▓");
        assert_eq!(progress_bar(1, 2), "▓▓░░");
        assert_eq!(progress_bar(0, 0), "····");
    }

    // ── (#1562) Named-first default: minted-run classification + collapse ──
    //
    // (#1717) The semantic tests for the predicate itself (spec-over-id-shape
    // trust, the `1616-compactor-fix` counterexample, the pre-#1503 id-shape
    // fallback, hand-authored ids with no spec) moved with it to
    // `darkmux-crew/src/types.rs`'s `is_minted_run_*` test group, next to
    // `Mission::is_minted_run`. What stays here is this MODULE's own
    // composition of that predicate — the board partition/collapse — which
    // is what would actually break if a future edit here re-diverged from
    // the shared method.

    fn minted_spec() -> MissionSpec {
        MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "x".to_string(), origin: None }
    }

    #[test]
    fn partition_visibility_keeps_named_and_collapses_minted_when_filtering() {
        let named = mission("doom-loop-m4", MissionStatus::Active);
        let mut minted = mission("dispatch-code-reviewer-1785589698-5d6a-0", MissionStatus::Active);
        minted.spec = Some(minted_spec());
        let views = [view(&named, 1, 0), view(&minted, 1, 0)];

        let (visible, hidden) = partition_visibility(&views, false);
        assert_eq!(visible.iter().map(|v| v.m.id.as_str()).collect::<Vec<_>>(), vec!["doom-loop-m4"]);
        assert_eq!(
            hidden.iter().map(|v| v.m.id.as_str()).collect::<Vec<_>>(),
            vec!["dispatch-code-reviewer-1785589698-5d6a-0"]
        );
    }

    #[test]
    fn partition_visibility_all_shows_everything_and_hides_nothing() {
        let named = mission("doom-loop-m4", MissionStatus::Active);
        let mut minted = mission("dispatch-code-reviewer-1785589698-5d6a-0", MissionStatus::Active);
        minted.spec = Some(minted_spec());
        let views = [view(&named, 1, 0), view(&minted, 1, 0)];

        let (visible, hidden) = partition_visibility(&views, true);
        assert_eq!(visible.len(), 2, "`--all` must show every mission, minted or named");
        assert!(hidden.is_empty(), "`--all` must hide nothing");
    }

    /// (#1709) The DEFAULT board passes `true` here (`!missions_only`), so a
    /// minted run instance is on the board unless the operator filters it
    /// out. This pins the inversion itself: before #1709 the default passed
    /// `all` (false), which is what buried a day of real work under an
    /// 8-day-old finished mission.
    #[test]
    fn the_default_board_includes_minted_run_instances() {
        let named = mission("doom-loop-m4", MissionStatus::Finalized);
        let mut minted = mission("review-1786150410-209398", MissionStatus::Finalized);
        minted.spec = Some(minted_spec());
        let views = [view(&named, 1, 0), view(&minted, 1, 0)];

        // `board_partition` is the function `run()` itself calls (#1709 gate
        // MF-3) — testing `partition_visibility` directly would pass even if
        // the call site reverted to the pre-#1709 `!`-less mapping.
        let (visible, hidden) = board_partition(&views, false);
        assert!(
            visible.iter().any(|v| v.m.id == "review-1786150410-209398"),
            "today's run instance must be ON the default board, not in a footer"
        );
        assert!(hidden.is_empty(), "nothing is filtered out unless --missions asks for it");

        // …and the filter still works when asked for.
        let (visible, hidden) = board_partition(&views, true);
        assert_eq!(visible.iter().map(|v| v.m.id.as_str()).collect::<Vec<_>>(), vec!["doom-loop-m4"]);
        assert_eq!(hidden.len(), 1, "--missions filters the minted run out");
    }

    #[test]
    fn hidden_run_summary_is_none_when_nothing_is_hidden() {
        assert_eq!(hidden_run_summary(0, 0), None);
    }

    #[test]
    fn hidden_run_summary_names_the_count_and_pluralizes() {
        assert_eq!(
            hidden_run_summary(1, 0).unwrap(),
            "+1 run instance filtered out — drop `--missions` to include them, or see the runs lens"
        );
        assert_eq!(
            hidden_run_summary(32, 0).unwrap(),
            "+32 run instances filtered out — drop `--missions` to include them, or see the runs lens"
        );
    }

    #[test]
    fn hidden_run_summary_surfaces_hidden_actionable_runs() {
        // THE needs-attention requirement: a hidden stalled run that needs
        // `mission abort` must not become invisible just because it was
        // collapsed out of the section it would have rendered in.
        assert_eq!(
            hidden_run_summary(32, 2).unwrap(),
            "+32 run instances filtered out, 2 need attention — drop `--missions` to include them, \
             or see the runs lens"
        );
        assert_eq!(
            hidden_run_summary(3, 1).unwrap(),
            "+3 run instances filtered out, 1 needs attention — drop `--missions` to include them, \
             or see the runs lens"
        );
    }

    #[test]
    fn attention_rollup_is_clean_only_when_both_counts_are_zero() {
        let (clean, msg) = attention_rollup(0, 0, false, false, true);
        assert!(clean);
        assert_eq!(msg, "✓ board is clean — every mission's phases are reconciled");
    }

    #[test]
    fn attention_rollup_names_a_hidden_only_attention_item() {
        // Nothing printed above needs action, but a filtered-out run does —
        // the board must not read as clean, and (#1709) must point at
        // DROPPING `--missions`, the only thing that could have hidden it,
        // matching the footer's advice rather than competing with it.
        let (clean, msg) = attention_rollup(0, 1, true, false, true);
        assert!(!clean, "a hidden actionable run must never look like a clean board");
        assert!(msg.contains("1 filtered-out run instance needs attention"), "{msg}");
        assert!(msg.contains("--missions"), "{msg}");

        let (clean, msg) = attention_rollup(0, 2, true, false, true);
        assert!(!clean);
        assert!(msg.contains("2 filtered-out run instances need attention"), "{msg}");
        assert!(msg.contains("--missions"), "{msg}");
    }

    #[test]
    fn attention_rollup_uses_the_existing_wording_when_visible_missions_need_attention() {
        let (clean, msg) = attention_rollup(3, 0, false, false, true);
        assert!(!clean);
        assert_eq!(msg, "3 missions need attention — run the suggested commands above to reconcile");

        let (_, msg) = attention_rollup(1, 0, false, false, true);
        assert_eq!(msg, "1 mission needs attention — run the suggested commands above to reconcile");
    }

    #[test]
    fn attention_rollup_tail_reflects_hidden_drift_and_panel_presence() {
        let (_, msg) = attention_rollup(3, 2, true, false, true);
        assert!(msg.ends_with("(some are hidden — `--all` to see them)"), "{msg}");

        let (_, msg) = attention_rollup(3, 2, true, true, true);
        assert!(msg.ends_with("(some are hidden — open the full board above)"), "{msg}");

        let (_, msg) = attention_rollup(3, 0, false, false, true);
        assert!(!msg.contains("hidden"), "{msg}");
    }

    // ─── #1711: the clean-board claim must cover — or admit the scope of —
    // the fleet, not just this machine ───────────────────────────────────

    #[test]
    fn attention_rollup_never_shows_the_green_checkmark_when_the_fleet_read_is_incomplete() {
        // This machine's own missions are perfectly reconciled (0, 0), but
        // the fleet-wide read never completed — the summary line says
        // "board", and the board is supposed to include the fleet. The
        // green checkmark is a claim of full coverage this run cannot make.
        let (clean, msg) = attention_rollup(0, 0, false, false, false);
        assert!(!clean, "an incomplete fleet read must never render as the clean checkmark");
        assert!(!msg.starts_with('✓'), "{msg}");
        assert!(
            msg.contains("did not complete") || msg.contains("fleet"),
            "the message must name the fleet gap, got: {msg}"
        );
    }

    #[test]
    fn attention_rollup_still_says_board_is_clean_when_fleet_is_off_or_ok() {
        // `Off` (no fleet substrate configured) and `Ok` (a complete read)
        // are both real "nothing is missing" answers — a standalone
        // install must see the EXACT pre-#1711 wording, unchanged.
        let (clean, msg) = attention_rollup(0, 0, false, false, true);
        assert!(clean);
        assert_eq!(msg, "✓ board is clean — every mission's phases are reconciled");
    }

    #[test]
    fn attention_rollup_names_the_fleet_gap_even_when_local_missions_need_attention() {
        // The fleet-incomplete caveat must travel with whichever branch
        // actually renders, not just the all-clean one — an operator
        // reconciling local drift should also know the fleet half of the
        // board could not be verified.
        let (_, msg) = attention_rollup(3, 0, false, false, false);
        assert!(msg.contains("3 missions need attention"), "{msg}");
        assert!(msg.contains("fleet"), "the local-attention branch must still name the fleet gap: {msg}");
    }

    #[test]
    fn board_json_is_complete_regardless_of_what_a_human_board_would_hide() {
        // The JSON path is built from the SAME unfiltered `views` slice
        // `run()` passes it, before `partition_visibility` ever runs — this
        // pins that a mix of named + minted missions all survive into the
        // payload, and that a minted mission's drift is still counted.
        let named = mission("doom-loop-m4", MissionStatus::Active);
        let mut minted = mission("dispatch-code-reviewer-1785589698-5d6a-0", MissionStatus::Active);
        minted.spec = Some(minted_spec());
        let views = vec![view(&named, 1, 0), drifted(&minted)];

        let payload = board_json(&views, &[], &SourceState::Off);
        let ids: Vec<&str> =
            payload["missions"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 2, "both named and minted missions must be present");
        assert!(ids.contains(&"doom-loop-m4"));
        assert!(ids.contains(&"dispatch-code-reviewer-1785589698-5d6a-0"));
        assert_eq!(payload["summary"]["total"], 2);
        // The minted mission carries drift (via `drifted`) — `--json` must
        // never hide an actionable item behind the display-only filter.
        assert_eq!(payload["summary"]["needs_attention"], 1);
    }

    // ─── #2406: the board can say `degraded` ────────────────────────────

    #[test]
    fn phase_mix_names_a_degraded_phase_instead_of_folding_it_into_complete() {
        // (#2406) The defect this fixes: a degraded phase is
        // `PhaseStatus::Complete` ON DISK (Degraded drives
        // `lifecycle::phase_complete` deliberately — see `MissionView::
        // degraded`), so before the split the board printed "3 complete"
        // for a mission where one of those three shipped only part of its
        // work. The #2406 display fix had moved that phase from
        // wrong-and-loud (`abandoned`) to wrong-and-quiet.
        let m = mission("m1", MissionStatus::Finalized);
        let v = view_with_degraded(&m, 2, 1, 0);
        assert_eq!(phase_mix(&v), "2 complete · 1 degraded");

        // And a board with none reads exactly as it always did — no empty
        // bucket appears on a clean mission.
        assert_eq!(phase_mix(&view(&m, 3, 0)), "3 complete");
    }

    #[test]
    fn a_degraded_phase_still_counts_as_progress() {
        // (#2406) The other half of the split: a degraded phase is
        // TERMINAL and PRODUCED OUTPUT, so the progress column must keep
        // counting it. Splitting the bucket without this makes a mixed run
        // look less far along than it is — a second wrong reading traded
        // for the first.
        let m = mission("m1", MissionStatus::Finalized);
        let v = view_with_degraded(&m, 2, 1, 0);
        assert_eq!(v.done(), 3, "2 clean + 1 degraded = 3 phases that finished and produced");
        assert_eq!(v.total, 3);
        assert_eq!(progress_bar(v.done(), v.total), "▓▓▓▓", "a fully-terminal mission reads full");
    }

    #[test]
    fn board_json_carries_degraded_as_its_own_bucket_and_still_sums_to_total() {
        // (#2406) `--json` is part of the contract — the defect was pulled
        // straight out of `mission status --json --all`. `degraded` is a
        // NEW SIBLING key and `complete` no longer includes it; the four
        // buckets still sum to `total`, which is the invariant a reader
        // can rely on.
        let m = mission("m1", MissionStatus::Finalized);
        let v = view_with_degraded(&m, 2, 1, 0);
        let payload = board_json(std::slice::from_ref(&v), &[], &SourceState::Off);
        let phases = &payload["missions"][0]["phases"];
        assert_eq!(phases["complete"], 2, "the degraded phase must NOT be folded in here");
        assert_eq!(phases["degraded"], 1);
        assert_eq!(phases["total"], 3);
        let sum = ["complete", "degraded", "running", "planned", "abandoned"]
            .iter()
            .map(|k| phases[*k].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(sum, phases["total"].as_u64().unwrap(), "the buckets must partition `total`");
    }

    // ─── #1711: peer missions — the CLI board is no longer fleet-blind ────

    /// One fabricated flow record — a plain `serde_json::Value`, the SAME
    /// shape `darkmux_serve::build_runs`/`build_flow_mission_index` consume
    /// whether it came from a local day-file or the shared Redis stream.
    /// Used to simulate "what a peer wrote to the fleet" with zero network
    /// or Redis involvement — the whole point of testing at this layer.
    fn flow_record(
        mission_id: &str,
        machine_id: &str,
        action: &str,
        ts: &str,
        session_id: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "mission_id": mission_id,
            "machine_id": machine_id,
            "action": action,
            "ts": ts,
            "session_id": session_id,
        })
    }

    /// Isolates `DARKMUX_HOME` to a fresh tempdir for the duration of `f`,
    /// restoring the previous value afterward — the SAME pattern
    /// `display_label_prefers_the_config_name_over_a_config_launched_
    /// missions_own_long_description` above uses, applied here because
    /// `peer_mission_runs` → `darkmux_serve::peer_mission_runs` →
    /// `resolve_machine_id` → `config_access::machine_id()` reads
    /// `DARKMUX_HOME`-scoped config (#1711 review finding: this file's own
    /// tests must never read whatever the OPERATOR'S real `~/.darkmux/`
    /// happens to hold on the machine running the suite). Callers must be
    /// `#[serial_test::serial]` — this mutates process-global env.
    fn with_isolated_darkmux_home<R>(f: impl FnOnce() -> R) -> R {
        // (#2682 fix-pass round 2, MUST FIX 4) `DarkmuxHomeGuard` rather
        // than a hand-rolled save/restore. `f` is a test body full of
        // assertions: when one of them panicked, the restore below it never
        // ran, `DARKMUX_HOME` stayed pointed at a `TempDir` about to drop,
        // and every subsequent serial test in the process read through a
        // deleted directory. The guard's `Drop` still runs during unwind,
        // so one real failure now reports as one failure.
        let _home = DarkmuxHomeGuard::new();
        f()
    }

    #[test]
    #[serial_test::serial]
    fn peer_mission_runs_surfaces_a_mission_seen_only_via_the_shared_stream() {
        // The exact defect #1711 was filed over: a mission with real flow
        // records and NO local durable `Mission` JSON (because it ran on a
        // peer) must still produce a row here, `tracked: false`, carrying
        // the peer's machine id.
        with_isolated_darkmux_home(|| {
            let flows = tempfile::tempdir().unwrap();
            let fleet = vec![
                flow_record("review-peer-1", "hub", "mission start", "2026-09-01T00:00:00Z", "s1"),
                flow_record("review-peer-1", "hub", "mission close", "2026-09-01T00:05:00Z", "s1"),
            ];
            let known = std::collections::HashSet::new();
            let peer = peer_mission_runs(flows.path(), &fleet, &known);
            assert_eq!(peer.len(), 1, "a peer-only mission must produce exactly one row: {peer:?}");
            assert_eq!(peer[0].id, "review-peer-1");
            assert_eq!(peer[0].machine.as_deref(), Some("hub"));
            assert!(!peer[0].tracked, "an observed-not-owned row must be marked untracked");
            assert_eq!(peer[0].status, RunStatus::Complete);
        });
    }

    #[test]
    #[serial_test::serial]
    fn peer_mission_runs_is_empty_on_a_standalone_install_with_no_fleet_records() {
        // (#1711 hard requirement) The local-only case must be UNCHANGED:
        // no fleet records at all (the `Off`/standalone shape) must yield
        // zero peer rows, not a synthesized one.
        with_isolated_darkmux_home(|| {
            let flows = tempfile::tempdir().unwrap();
            let known = std::collections::HashSet::new();
            let peer = peer_mission_runs(flows.path(), &[], &known);
            assert!(peer.is_empty(), "a standalone install must see no peer missions: {peer:?}");
        });
    }

    #[test]
    #[serial_test::serial]
    fn peer_mission_runs_a_still_running_peer_reads_running() {
        // "A live peer" — the issue's own contrast case against "rostered
        // but silent" below. No terminal record, but the session is
        // recent enough to read as live.
        with_isolated_darkmux_home(|| {
            let flows = tempfile::tempdir().unwrap();
            let now = now_unix();
            let recent = chrono_like_ts(now.saturating_sub(5));
            let fleet = vec![flow_record("review-peer-2", "peer-2", "mission start", &recent, "s2")];
            let known = std::collections::HashSet::new();
            let peer = peer_mission_runs(flows.path(), &fleet, &known);
            assert_eq!(peer.len(), 1, "{peer:?}");
            assert_eq!(
                peer[0].status,
                RunStatus::Running,
                "a fresh, non-terminal record must read live"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn peer_mission_runs_excludes_a_locally_known_mission_id() {
        // (#1711 review finding) A caller's `known_mission_ids` must be
        // honored even when the flow stream also carries records for that
        // same mission — this is the SAME de-dup `darkmux_serve::build_runs`
        // gives its own callers, now exercised through the narrow entry
        // point `mission status` actually calls.
        with_isolated_darkmux_home(|| {
            let flows = tempfile::tempdir().unwrap();
            let fleet = vec![flow_record(
                "review-local-1",
                "hub",
                "mission start",
                "2026-09-01T00:00:00Z",
                "s1",
            )];
            let mut known = std::collections::HashSet::new();
            known.insert("review-local-1".to_string());
            let peer = peer_mission_runs(flows.path(), &fleet, &known);
            assert!(
                peer.is_empty(),
                "a mission in `known_mission_ids` must never surface as a peer row: {peer:?}"
            );
        });
    }

    /// A minimal RFC3339 stamp from a Unix-seconds value — just enough for
    /// `flow_record`'s `ts` field and the staleness clock the aggregation
    /// reads it against. Not a general-purpose formatter; this file's tests
    /// need exactly one shape.
    fn chrono_like_ts(secs: u64) -> String {
        // `1970-01-01T00:00:00Z` plus `secs` — computed by hand (no chrono
        // dependency in this crate) via civil-from-days, good enough for
        // dates in the 2020s this test actually uses.
        let days = secs / 86_400;
        let rem = secs % 86_400;
        let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        // 1970-01-01 is a Thursday; civil_from_days below is the standard
        // Howard Hinnant algorithm, days since epoch -> (y, m, d).
        let z = days as i64 + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mth = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if mth <= 2 { y + 1 } else { y };
        format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
    }

    #[test]
    fn peer_status_word_distinguishes_silent_from_aborted_from_running() {
        // The issue's own load-bearing distinction: "a rostered-but-silent
        // machine and a live peer are different states and should read
        // differently" — and neither is the same claim as a deliberate
        // teardown.
        assert_eq!(peer_status_word(RunStatus::Running, None), "running");
        assert_eq!(peer_status_word(RunStatus::Complete, None), "complete");
        assert_eq!(
            peer_status_word(RunStatus::Abandoned, Some(AbandonReason::Aborted)),
            "aborted",
            "a real `mission abort` record must read as a deliberate teardown"
        );
        assert_eq!(
            peer_status_word(RunStatus::Abandoned, Some(AbandonReason::NoTerminal)),
            "silent (no terminal record seen)",
            "no terminal record and not live must NOT be worded as a verdict — darkmux describes, \
             never adjudicates"
        );
        assert_eq!(
            peer_status_word(RunStatus::Abandoned, None),
            "silent (no terminal record seen)",
            "the reason-less Abandoned case (defensive) must fall to the same honest wording"
        );
    }

    #[test]
    fn fleet_scope_note_is_silent_for_ok_and_off() {
        // `Off` (no fleet substrate) must never warn — that's a correctly
        // configured standalone machine, and warning about it would be the
        // bug (`source_state`'s own doctrine). `Ok` is a complete answer.
        assert_eq!(fleet_scope_note(&SourceState::Ok), None);
        assert_eq!(fleet_scope_note(&SourceState::Off), None);
    }

    #[test]
    fn fleet_scope_note_names_an_unreachable_fleet_without_hanging_or_adjudicating() {
        // (#1711 degraded case: "a peer that is unreachable") `Unavailable`
        // is also what a Redis AUTH failure collapses to — the substrate
        // deliberately never distinguishes "network unreachable" from "bad
        // credential" this far up (a Redis error can embed the connection
        // URL/password — #661 Slice 5), so this ONE rendering path is the
        // honest answer for both underlying causes. The note must say what
        // was attempted, never render a verdict about the operator's
        // network.
        let note = fleet_scope_note(&SourceState::Unavailable { detail: "could not reach Redis" })
            .expect("an unavailable fleet read must be disclosed, not swallowed");
        assert!(note.contains("could not reach the shared stream"), "{note}");
        assert!(!note.contains("could not reach Redis"), "the raw `detail` must never leak: {note}");
    }

    #[test]
    fn fleet_scope_note_names_a_stale_fleet_snapshot_with_its_age() {
        let note = fleet_scope_note(&SourceState::Stale { age_ms: 125_000, detail: "x" })
            .expect("a stale fleet read must be disclosed");
        assert!(note.contains("2m"), "the age must be legible: {note}");
        assert!(!note.contains('x'), "the raw `detail` must never leak: {note}");
    }

    #[test]
    fn board_json_carries_peer_missions_and_fleet_state() {
        // (#1711) `--json` must stay coherent: a peer mission is additive
        // (never mixed into `missions`), and the fleet's own coverage state
        // rides alongside it so a script can tell "no peer missions" from
        // "the fleet read never ran".
        let peer_run = Run {
            id: "review-peer-3".to_string(),
            kind: RunKind::Mission,
            status: RunStatus::Running,
            machine: Some("peer-3".to_string()),
            route: None,
            role: None,
            model: None,
            started_ts: Some(100),
            completed_ts: None,
            updated_ts: Some(100),
            tracked: false,
            session_id: None,
            abandoned_reason: None,
        };
        let payload = board_json(&[], std::slice::from_ref(&peer_run), &SourceState::Ok);
        assert_eq!(payload["peer_missions"][0]["id"], "review-peer-3");
        assert_eq!(payload["peer_missions"][0]["tracked"], false);
        assert_eq!(payload["fleet"]["state"], "ok");
        assert_eq!(payload["summary"]["fleet_complete"], true);
        assert!(
            payload["missions"].as_array().unwrap().is_empty(),
            "a peer mission must never land in the local `missions` array"
        );
    }

    #[test]
    fn board_json_fleet_complete_is_false_only_for_stale_and_unavailable() {
        let ok = board_json(&[], &[], &SourceState::Ok);
        assert_eq!(ok["summary"]["fleet_complete"], true);
        let off = board_json(&[], &[], &SourceState::Off);
        assert_eq!(off["summary"]["fleet_complete"], true, "`Off` is a correct standalone machine");
        let unavailable = board_json(&[], &[], &SourceState::Unavailable { detail: "x" });
        assert_eq!(unavailable["summary"]["fleet_complete"], false);
        let stale = board_json(&[], &[], &SourceState::Stale { age_ms: 1, detail: "x" });
        assert_eq!(stale["summary"]["fleet_complete"], false);
    }
}

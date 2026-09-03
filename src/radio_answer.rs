//! The radio interpreter's ANSWERING seat (#1698 Packet B2) — the last
//! packet of the radio arc. Dispatched only when the ROUTING seat
//! (`src/radio.rs`) refuses free text: instead of the panel/CLI printing a
//! bare refusal + command listing, the same text goes here for a grounded,
//! in-persona answer. Never invoked for a routed (`RouteDecision::Route`)
//! exchange — that path executes unchanged.
//!
//! # Wall 5 — the answer seat has no hands (issue #1698)
//!
//! This module NEVER dispatches, reads a file, or runs a command on the
//! answering seat's behalf. Every fact the seat can cite is gathered
//! DETERMINISTICALLY, zero-model, by [`assemble_grounding`] BEFORE the one
//! model call this module makes — the "the observer must not join the
//! observed" discipline applied to the answering seat: grounding gathers
//! read kernel/registry/config state, never dispatch a model, and the
//! seat's own reply is single-exchange prose, never a tool call.
//!
//! # Three pieces
//!
//! - **The grounding assembler** ([`assemble_grounding`]) — pure,
//!   deterministic, zero-model. Compiles the catalog, the live config
//!   surface, a compact mission-board summary, the session's artifact shelf,
//!   and (when the question names one) one deep artifact, enforcing the
//!   pinned context budget (issue #1698, "B2 context budget" comment):
//!   target 4-8K tokens, hard cap ~10K, dropping in reverse priority.
//! - **The artifact shelf** ([`ArtifactShelf`]) — a per-session ring buffer
//!   of the last few rendered command outputs, owned by the ACP session map
//!   (`src/acp.rs`) and read (never written) by the assembler.
//! - **The answering dispatch** ([`answer`] / [`dispatch_answerer_call_with`]) —
//!   a single tool-less exchange through the `radio-host` role, whose
//!   persona template (`templates/builtin/roles/radio-host.md`) carries a
//!   `{{humor}}` placeholder substituted here from `radio.humor` config.

use crate::radio::CatalogEntry;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::Path;

// ── C. The artifact shelf ────────────────────────────────────────────────

/// One rendered command execution, kept on a session's shelf for the
/// answering seat to reference ("what did I just run?"). Process-RAM only —
/// dies with the process, deliberately (issue #1698: "process-RAM only,
/// dies with the process — deliberate").
#[derive(Debug, Clone)]
pub struct ShelfEntry {
    pub command: String,
    pub args: String,
    pub rendered: String,
    pub timestamp_unix: u64,
}

/// How many rendered outputs the shelf keeps — "last ~3 entries" (issue
/// #1698, scope C).
pub const SHELF_CAPACITY: usize = 3;

/// A per-session ring buffer of the last [`SHELF_CAPACITY`] rendered command
/// executions — written on every command execution (slash-routed AND
/// no-slash routed), read only by [`assemble_grounding`]. Lives in the ACP
/// session map (`src/acp.rs`'s `Sessions` value type); the CLI verb
/// (`src/radio_cli.rs`) has no persistent session, so it always answers
/// against a fresh, empty shelf — a documented, deliberate limitation (one
/// CLI invocation is one process; there is nothing to shelve between calls).
#[derive(Debug, Clone, Default)]
pub struct ArtifactShelf {
    entries: VecDeque<ShelfEntry>,
}

impl ArtifactShelf {
    /// Push a newly rendered command execution, evicting the oldest entry
    /// once capacity is exceeded.
    pub fn push(&mut self, entry: ShelfEntry) {
        self.entries.push_back(entry);
        while self.entries.len() > SHELF_CAPACITY {
            self.entries.pop_front();
        }
    }

    /// Oldest-first iteration — the same order [`Sections::render`] renders
    /// in (most-recent last, so a truncating reader sees the OLDEST entries
    /// drop from view first if it stops early).
    pub fn entries(&self) -> impl Iterator<Item = &ShelfEntry> {
        self.entries.iter()
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a [`ShelfEntry`] stamped with the current time — the one
/// construction site both `src/acp.rs` call sites (slash-routed execution
/// and no-slash routed execution) use, so the timestamp convention can't
/// drift between them.
///
/// Truncates at WRITE time, not just at assembly (#1698 Packet B2 gate):
/// a `/review` render is unbounded, the shelf holds 3 of them per session,
/// sessions are never evicted, and the process is long-lived — so an
/// assembly-time-only cap would let RAM grow with everything the operator
/// ever ran. `SHELF_ENTRY_CAP_CHARS` is exactly what assembly can read
/// back, so nothing storable beyond it was ever reachable anyway.
pub fn shelf_entry(command: &str, args: &str, rendered: &str) -> ShelfEntry {
    ShelfEntry {
        command: command.to_string(),
        args: args.to_string(),
        rendered: truncate_chars(rendered, SHELF_ENTRY_CAP_CHARS),
        timestamp_unix: now_unix(),
    }
}

// ── B. The grounding assembler — budget knobs ────────────────────────────
//
// Char-based caps, NOT a real tokenizer (this codebase deliberately keeps
// its dep set small — CLAUDE.md's "don't add dependencies casually" — and
// every existing length cap in the tree, e.g. `radio::SOURCE_TEXT_RECORD_CAP`
// / `dispatch::capped_prompt`, is char-based too). Approximated at ~4
// chars/token, a common rough heuristic — the issue's own numbers are
// TOKEN targets, so every constant below divides that target by 4. This is
// deliberately conservative-leaning (a cap that trims a little early beats
// one that silently overflows).

/// Catalog block cap: ~800 tokens.
const CATALOG_CAP_CHARS: usize = 3_200;
/// Live config-surface block cap: ~800 tokens.
const CONFIG_CAP_CHARS: usize = 3_200;
/// Mission-board summary cap: ~400 tokens (within the issue's "300-1K when
/// relevant" range).
const BOARD_CAP_CHARS: usize = 1_600;
/// Top-level `--help` block cap: ~400 tokens.
/// (#1784/#1862) The verb index's cap. Sized from the measurement in
/// `radio_index::tests::rendered_index_fits_its_cap` with headroom; the
/// whole tree, one line per verb, is what makes "how do I..." a lookup, so
/// this is the largest section by design and the LAST generic one dropped
/// under the hard cap (see `enforce_budget`).
pub const VERB_INDEX_CAP_CHARS: usize = 16_000;
/// Per-shelf-entry truncation: ~1.5K tokens (issue #1698's own number).
const SHELF_ENTRY_CAP_CHARS: usize = 6_000;
/// One named deep artifact: ~1.5K tokens (issue's "1-2K" range, midpoint).
const DEEP_ARTIFACT_CAP_CHARS: usize = 6_000;
/// Hard cap on the WHOLE assembled grounding message: ~10K tokens (issue's
/// own hard number). Excludes the persona system prompt (a separate,
/// small, fixed-size message) and the user's own raw question text.
const HARD_CAP_CHARS: usize = 40_000;

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str(" …[truncated]");
    out
}

/// One compiled grounding source — present or absent, independently
/// droppable so [`enforce_budget`] can trim without re-deriving anything.
#[derive(Debug, Clone, Default)]
struct Sections {
    catalog: Option<String>,
    config: Option<String>,
    board: Option<String>,
    help: Option<String>,
    shelf: Vec<String>,
    deep_artifact: Option<String>,
}

impl Sections {
    fn total_chars(&self) -> usize {
        [&self.catalog, &self.config, &self.board, &self.help, &self.deep_artifact]
            .into_iter()
            .flatten()
            .map(|s| s.chars().count())
            .sum::<usize>()
            + self.shelf.iter().map(|s| s.chars().count()).sum::<usize>()
    }

    /// Drop sections in reverse priority until under [`HARD_CAP_CHARS`] —
    /// "dropping in reverse priority (shelf tail and help yield before a
    /// named artifact)" (issue #1698's own B2 context-budget comment). Drop
    /// order: help (whole section) → shelf, oldest entry first → board
    /// (whole section) → deep artifact → config → catalog.
    ///
    /// **Currently unreachable from [`assemble_grounding`] in practice**
    /// (a fresh-review finding worth naming honestly, not hiding): the SUM
    /// of every per-section cap (`CATALOG_CAP_CHARS` + `CONFIG_CAP_CHARS` +
    /// `BOARD_CAP_CHARS` + `HELP_CAP_CHARS` + 3×`SHELF_ENTRY_CAP_CHARS` +
    /// `DEEP_ARTIFACT_CAP_CHARS`) is comfortably under `HARD_CAP_CHARS`, so
    /// a real call can never actually accumulate enough to trigger this
    /// loop today. It is exercised directly (not through `assemble_grounding`)
    /// by this module's own tests, which construct an over-budget `Sections`
    /// by hand — proving the ORDER and TERMINATION are correct even though
    /// no live input reaches it yet. It becomes reachable the moment any
    /// per-section cap is raised, or a new section is added, without a
    /// matching hard-cap increase — kept live (not deleted) for that reason.
    fn enforce_budget(&mut self) {
        while self.total_chars() > HARD_CAP_CHARS {
            // Session history first, then the board, then the verb index:
            // radio is interactive help, so "how do I" grounding outlives
            // "what am I working on" grounding. A NAMED artifact still
            // outlives all three: the user asked about it by name.
            if !self.shelf.is_empty() {
                self.shelf.remove(0);
                continue;
            }
            if self.board.take().is_some() {
                continue;
            }
            if self.help.take().is_some() {
                continue;
            }
            if self.deep_artifact.take().is_some() {
                continue;
            }
            if self.config.take().is_some() {
                continue;
            }
            if self.catalog.take().is_some() {
                continue;
            }
            // Nothing left to drop — stop; the message is as lean as it can
            // get without dropping the user's own question (not a section
            // this struct owns).
            break;
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.catalog {
            out.push_str("Available commands:\n");
            out.push_str(c);
            out.push('\n');
        }
        if let Some(c) = &self.config {
            out.push_str("\nCurrent config (darkmux config list):\n");
            out.push_str(c);
            out.push('\n');
        }
        if let Some(b) = &self.board {
            out.push_str("\nMission board summary:\n");
            out.push_str(b);
            out.push('\n');
        }
        if let Some(h) = &self.help {
            out.push_str(
                "\ndarkmux command index (every runnable verb, its options, one line each; \
                 when the answer is a command, name it exactly as listed here):\n",
            );
            out.push_str(h);
            out.push('\n');
        }
        if !self.shelf.is_empty() {
            out.push_str("\nRecent command outputs this session (oldest first):\n");
            for (i, s) in self.shelf.iter().enumerate() {
                out.push_str(&format!("--- shelf entry {} ---\n", i + 1));
                out.push_str(s);
                out.push('\n');
            }
        }
        if let Some(a) = &self.deep_artifact {
            out.push_str("\nDeep artifact (named in the user's message):\n");
            out.push_str(a);
            out.push('\n');
        }
        out
    }
}

fn render_catalog_block(catalog: &[CatalogEntry]) -> String {
    let mut out = String::new();
    for entry in catalog {
        out.push_str("- ");
        out.push_str(&entry.id);
        out.push_str(": ");
        out.push_str(&entry.description);
        if let Some(hint) = &entry.hint {
            out.push_str(" (hint: ");
            out.push_str(hint);
            out.push(')');
        }
        out.push('\n');
    }
    truncate_chars(&out, CATALOG_CAP_CHARS)
}

fn render_config_block(cfg_json: &str) -> String {
    truncate_chars(cfg_json, CONFIG_CAP_CHARS)
}

/// (#1784/#1862) The verb index in place of top-level `--help`: every
/// runnable verb with its options and one sentence, so a help question is
/// answered from the tree rather than guessed at (an invented `/machine`
/// was #1861's first defect; `darkmux machine status` was in the tree).
fn render_help_block() -> String {
    use clap::CommandFactory;
    let index = crate::radio_index::render_verb_index(&crate::radio_index::build_verb_index(&crate::cli::Cli::command()));
    truncate_chars(&index, VERB_INDEX_CAP_CHARS)
}

/// How many recent missions the board block names (#1713). Small on
/// purpose: this is grounding for one answer, not a listing — the operator
/// asking "what's recent" needs the top of the list, and `darkmux mission
/// status` is the surface that shows the rest.
const RECENT_MISSIONS_IN_BOARD_BLOCK: usize = 5;

/// (#1717) The named-mission floor — the crowding half of the fix. On a
/// board dominated by machine-minted runs, the top
/// `RECENT_MISSIONS_IN_BOARD_BLOCK` most-recently-touched missions can be
/// (and, on the board this issue measured — 61 runs vs 32 named — plausibly
/// are) ALL run instances, so the operator's own named work never reaches
/// the bundle. This many of the most-recently-touched NAMED missions
/// (`Mission::is_minted_run() == false`) are always represented, on top of
/// whatever named missions already made the "Most recent" cut — see the
/// second list this backs in `render_board_block_from`.
///
/// Kept smaller than `RECENT_MISSIONS_IN_BOARD_BLOCK`: this is a FLOOR, not
/// a second listing — enough to answer "what am I working on" with more
/// than one name without doubling the block's token cost on a board that's
/// already all named (where the extra list is empty and costs nothing) or
/// already representative (where most of the floor is deduplicated away).
const NAMED_MISSION_FLOOR_IN_BOARD_BLOCK: usize = 3;

/// Per-id cap in the recent-missions rows (#1714 gate C-4). `truncate_chars`
/// cuts the WHOLE block at a char count, which can slice an id in half and
/// leave `review-17860` looking like a complete mission id a model can
/// confidently cite. Capping each id first means an over-long name is
/// visibly elided (`…`) instead of silently forged. Comfortably above real
/// ids: the longest observed are ~40 chars.
const BOARD_ID_CAP_CHARS: usize = 56;

/// The lowercase status word for a mission, for the grounding block. Kept
/// local rather than borrowed from `mission_status` — that module's copy is
/// board-rendering detail, and a model-facing string should not silently
/// change when a board's presentation does. The exhaustive `match` means a
/// new `MissionStatus` variant breaks the build rather than drifting quietly.
fn status_word(s: crate::crew::types::MissionStatus) -> &'static str {
    use crate::crew::types::MissionStatus as M;
    match s {
        M::Active => "active",
        M::Paused => "paused",
        M::Finalized => "finalized",
        M::Aborted => "aborted",
    }
}

/// Compact mission-board summary — always-on, cheap (issue #1698: "always-on
/// cheap summaries (the board), deep artifacts only when the question names
/// one"). Never the full board render `mission status` itself produces
/// (that's an operator-facing table, not grounding text).
///
/// Emits counts by status, the open (Active/Paused) mission ids when there
/// are any, and — since #1713 — the most RECENT missions whatever their
/// status. The last line is the one that matters most in practice: the
/// questions an operator asks a console are disproportionately about what
/// just happened, and before #1713 this block filtered exactly that out,
/// leaving a machine with nothing open unable to name a single mission.
///
/// (#1717) That recency list mixes machine-minted run instances with the
/// operator's own named missions and, before this, marked neither — a
/// fresh-context model had no way to tell `review-1786081556-0eea32
/// (finalized)` apart from a mission the operator actually planned, and on
/// a board dominated by runs the top-N cap could crowd named missions out
/// of the bundle entirely. Two independent fixes, both keyed on
/// `Mission::is_minted_run`: every minted row in the recency list now
/// carries an `auto` marker (defined inline, once, for a model with no
/// darkmux history), and a small floor of the most-recently-touched named
/// missions is always represented even when none of them made the
/// recency-list cut.
///
/// The loader half only; [`render_board_block_from`] is the pure core.
fn render_board_block() -> Option<String> {
    let missions = crate::crew::loader::load_missions().ok()?;
    render_board_block_from(&missions)
}

/// The pure core of [`render_board_block`] (#1714 gate MF-2).
///
/// Split out so the ordering, the cap, and the status-inclusion rule are
/// reachable by a test. They were not: the only entry point read
/// `~/.darkmux` off disk, so on CI (no crew dir) `load_missions` returned
/// empty and every line below the early return NEVER EXECUTED under test,
/// while on a developer machine the same tests silently read the real board.
/// That is the setup `mission_status::board_order`'s doc records as how an
/// INVERTED comparator once shipped — reachable only through a printing
/// function, so no unit test could pin it.
fn render_board_block_from(missions: &[crate::crew::types::Mission]) -> Option<String> {
    if missions.is_empty() {
        return Some("no missions yet.".to_string());
    }
    use crate::crew::types::MissionStatus;
    let count = |s: MissionStatus| missions.iter().filter(|m| m.status == s).count();
    let mut out = format!(
        "{} mission(s) total — {} active, {} paused, {} finalized, {} aborted.\n",
        missions.len(),
        count(MissionStatus::Active),
        count(MissionStatus::Paused),
        count(MissionStatus::Finalized),
        count(MissionStatus::Aborted)
    );

    // (#1717, and its own follow-up) A fresh-context model has no way to
    // tell a machine-minted run instance (`review-1786081556-0eea32`) from
    // an id the operator typed — both are just strings on a line. `auto` is
    // defined inline, ONCE, here — before EITHER id-bearing line below it
    // (model-facing-prompt-construction provenance: option 2, "supplied
    // conceptual definition before first use") — then used as a compact
    // per-row marker on every line in this block that can emit a mission
    // id, so the seat can weigh a marked row as exhaust rather than intent.
    // A partially-marked block is worse than an unmarked one: once this
    // definition exists anywhere, an id with NO marker reads as a positive
    // claim the operator typed it, not merely "unknown" — so completeness
    // across every id-bearing line matters as much as the marker itself.
    // Uses the SAME predicate the CLI board's named-first default hides
    // behind (`Mission::is_minted_run`, shared as of #1717 so this marker
    // and that default cannot classify the same mission two different
    // ways).
    out.push_str(
        "(\"auto\" marks a run the darkmux CLI launched by itself, not something the user \
         typed.)\n",
    );

    // Kept as mission refs (not just formatted strings) so the floor below
    // can exclude exactly what actually got emitted on this line — see its
    // own comment for why (#1717 follow-up #2).
    let live_missions: Vec<&crate::crew::types::Mission> = missions
        .iter()
        .filter(|m| matches!(m.status, MissionStatus::Active | MissionStatus::Paused))
        .take(5)
        .collect();
    let live: Vec<String> = live_missions
        .iter()
        .map(|m| {
            let marker = if m.is_minted_run() { " (auto)" } else { "" };
            format!("{}{marker}", elide(&m.id, BOARD_ID_CAP_CHARS))
        })
        .collect();
    if !live.is_empty() {
        out.push_str("Active/paused: ");
        out.push_str(&live.join(", "));
        out.push('\n');
    }

    // (#1713) The MOST RECENT missions, whatever their status.
    //
    // This block used to name only the active/paused ones, on the assumption
    // that open work is the interesting work. That is the same assumption
    // #1709 removed from the CLI board, and it failed the same way: an
    // operator with nothing open (every mission finalized — the ordinary
    // state on a machine whose recent work is all run instances) got a
    // grounding bundle containing zero mission NAMES, and the answering seat
    // correctly declined a question the machine could trivially answer.
    //
    // Ordered by `mission_status::last_activity` — the SAME rule the board
    // sorts by, shared rather than copied (#1714 gate C-1) so radio's answer
    // and the board's top row cannot drift apart.
    let mut recent: Vec<&crate::crew::types::Mission> = missions.iter().collect();
    recent.sort_by_key(|m| std::cmp::Reverse(crate::mission_status::last_activity(m)));
    let top: Vec<&crate::crew::types::Mission> =
        recent.iter().take(RECENT_MISSIONS_IN_BOARD_BLOCK).copied().collect();

    // The `auto` marker is defined once, above, before this line — see the
    // comment at its definition site for the full #1717 provenance.
    out.push_str("Most recent (newest first): ");
    let rows: Vec<String> = top
        .iter()
        .map(|m| {
            let marker = if m.is_minted_run() { ", auto" } else { "" };
            format!("{} ({}{marker})", elide(&m.id, BOARD_ID_CAP_CHARS), status_word(m.status))
        })
        .collect();
    out.push_str(&rows.join(", "));
    out.push('\n');

    // (#1717) The named-mission floor — the crowding half of the fix. The
    // list above is capped at `RECENT_MISSIONS_IN_BOARD_BLOCK` and NOT
    // filtered by kind, so on a board dominated by runs it can legitimately
    // be all `auto` rows (the issue's own measured case: 61 runs vs 32
    // named). Top up to `NAMED_MISSION_FLOOR_IN_BOARD_BLOCK` of the most-
    // recently-touched NAMED missions not already shown above, so the
    // operator's own engagement work always has a floor in the bundle
    // regardless of run volume. Emitted only when there's something to add
    // — a board that's already named-heavy leaves this line out entirely,
    // same as `hidden_run_summary`'s no-op convention on the CLI board.
    //
    // (#1717 follow-up #2) "not already shown above" means EVERY list
    // above, not just the recent top-5 — this line's own header says "not
    // in the list above," and a named mission that is Active/Paused is
    // already on the `Active/paused:` line. Before this fix the filter only
    // excluded `top`, so an open named mission crowded out of the top-5
    // could be emitted a SECOND time here: a genuine duplicate that also
    // makes the header's own claim false. Excluding `live_missions` too
    // closes that gap. Because this is `filter().take(N)`, not `take(N)`
    // THEN filter, an excluded active mission doesn't shrink the floor —
    // the scan simply continues past it to the next-most-recent named
    // mission not yet shown anywhere, so a slot the active mission didn't
    // need still reaches someone who does.
    //
    // Cost: on a run-dominated board this doubles the mission-id surface
    // the model has to parse (two disjoint lists instead of one) and adds
    // a bounded number of extra characters — the floor is capped, so the
    // cost never scales with how many runs exist, only with how many named
    // missions do.
    let extra_named: Vec<String> = recent
        .iter()
        .filter(|m| {
            !m.is_minted_run()
                && !top.iter().any(|t| t.id == m.id)
                && !live_missions.iter().any(|l| l.id == m.id)
        })
        .take(NAMED_MISSION_FLOOR_IN_BOARD_BLOCK)
        .map(|m| format!("{} ({})", elide(&m.id, BOARD_ID_CAP_CHARS), status_word(m.status)))
        .collect();
    if !extra_named.is_empty() {
        out.push_str("Also tracking (named work not in the list above): ");
        out.push_str(&extra_named.join(", "));
        out.push('\n');
    }

    Some(truncate_chars(&out, BOARD_CAP_CHARS))
}

/// Shorten `s` to `max_chars` with a trailing `…` when it doesn't fit —
/// unlike [`truncate_chars`], which is for whole blocks and whose verbose
/// marker would be absurd per-id.
fn elide(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Deterministic pre-retrieval heuristic (issue #1698): a case-insensitive
/// `mission <token>` phrase in the user's text names a mission id to fetch
/// in full. Local, read-only, zero-model, zero network — the ONE deep
/// artifact this packet ships; a PR-number-shaped heuristic (`gh pr view`)
/// is a documented follow-up (see this module's own doc / the PR body),
/// deferred because it needs a network round trip the grounding assembler
/// otherwise never makes.
fn detect_mission_mention(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("mission ")?;
    let rest = &text[idx + "mission ".len()..];
    let token: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!token.is_empty()).then_some(token)
}

/// Render one mission's full detail (id, status, phase roll-up) — the
/// deep-artifact payload for [`detect_mission_mention`]'s match. `None` when
/// no mission with that id (case-insensitively) exists.
fn render_mission_deep_artifact(mission_token: &str) -> Option<String> {
    let missions = crate::crew::loader::load_missions().ok()?;
    let mission = missions.iter().find(|m| m.id.eq_ignore_ascii_case(mission_token))?;
    let phases = crate::crew::loader::load_phases().ok().unwrap_or_default();
    use crate::crew::types::PhaseStatus;
    let mine: Vec<_> = phases.iter().filter(|p| p.mission_id == mission.id).collect();
    let mut out = format!(
        "mission `{}` — status: {:?}, {} phase(s).\n{}\n",
        mission.id,
        mission.status,
        mine.len(),
        mission.description
    );
    for p in &mine {
        out.push_str(&format!("  - phase `{}` ({:?})\n", p.id, p.status));
    }
    let complete = mine.iter().filter(|p| p.status == PhaseStatus::Complete).count();
    out.push_str(&format!("  {complete}/{} phases complete.\n", mine.len()));
    Some(truncate_chars(&out, DEEP_ARTIFACT_CAP_CHARS))
}

/// How much of the machine's own state may go into one grounding bundle
/// (#1698 Packet B2 gate — the data boundary).
///
/// The answering seat is the first darkmux surface that COMPOSES a payload
/// out of local state and hands it to a model the operator picks at
/// runtime. The "radio host" picker offers every profile in the registry,
/// and on a remote-only machine (no local models) `default_profile` is
/// remote — so "this bundle might leave the machine" is the ordinary path
/// there, not an exotic misconfiguration.
///
/// The precedent this follows is one function away: `identity.md` is
/// withheld from EVERY remote endpoint, approved ones included
/// (`dispatch_internal::identity_augmentation_allowed`, #1405). A grounding
/// bundle is strictly more sensitive than `identity.md` — after a
/// `/review` the artifact shelf holds rendered review output over the
/// operator's private diff, and the config block carries machine ids, urls,
/// and directory layout.
///
/// (#1714 gate C-3) The board block belongs in that list too, and since
/// #1713 it always carries mission ids — which encode repository names and
/// commit SHAs (`zed-<repo>-<sha>`, `review-<epoch>-<hash>`) and
/// ticket-shaped operator names. When the approved-endpoint allowlist
/// mentioned below gets written, the board must stay withheld (or its ids
/// redacted) even for an approved endpoint: an allowlist decides WHOSE
/// server may see a bundle, not whether repo names stop being work-derived
/// identifiers.
///
/// So: what leaves the machine is what the machine already publishes.
/// `RemoteSafe` keeps the command catalog (already sent to the client on
/// every `session/new`) and the binary's own `--help` text; it drops the
/// config surface, the mission board, the artifact shelf, and any deep
/// artifact. "Is this darkmux?" and "what can I run here?" still answer
/// correctly on a remote seat — only questions about THIS machine's private
/// state lose their grounding, and the seat says so honestly rather than
/// guessing (its persona forbids inventing facts it wasn't handed).
///
/// This is the conservative default, not a final ruling: an
/// approved-endpoint allowlist (Azure yes, personal-key vendors no) is the
/// obvious refinement if the operator wants one. Widening later costs a
/// config field; un-sending a bundle costs nothing less than a rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroundingScope {
    /// The resolved answering seat is served locally — every source is in
    /// scope.
    Full,
    /// The resolved answering seat is a remote endpoint — public surfaces
    /// only (catalog + help).
    RemoteSafe,
}

/// Assemble the answering seat's grounding block for one ask — the pure(-
/// ish; every source is a read-only local call, never a dispatch) core of
/// scope B. `cwd` is accepted for a future cwd-scoped grounding source
/// (none needed yet — every source today is process/registry-global); kept
/// as an explicit parameter rather than added later as a breaking change.
pub fn assemble_grounding(
    text: &str,
    catalog: &[CatalogEntry],
    shelf: &ArtifactShelf,
    _cwd: &Path,
    scope: GroundingScope,
) -> String {
    let machine_local = scope == GroundingScope::Full;
    let mut sections = Sections {
        // Always safe: the catalog is the advertised command surface (it is
        // already sent to the CLIENT on every `session/new`), and `--help`
        // is the shipped binary's own public text.
        catalog: Some(render_catalog_block(catalog)),
        help: Some(render_help_block()),
        // Machine-local only — see `GroundingScope`.
        config: machine_local.then(config_block).flatten(),
        board: machine_local.then(render_board_block).flatten(),
        shelf: if machine_local {
            shelf
                .entries()
                .map(|e| {
                    truncate_chars(
                        &format!(
                            "command: /{} {} (t={})\noutput: {}",
                            e.command, e.args, e.timestamp_unix, e.rendered
                        ),
                        SHELF_ENTRY_CAP_CHARS,
                    )
                })
                .collect()
        } else {
            Vec::new()
        },
        deep_artifact: machine_local
            .then(|| detect_mission_mention(text).and_then(|m| render_mission_deep_artifact(&m)))
            .flatten(),
    };
    sections.enforce_budget();
    sections.render()
}

fn config_block() -> Option<String> {
    let path = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config;
    crate::config_cmd::list_at(&path).ok().map(|s| render_config_block(&s))
}

// ── A/D. The answering dispatch ──────────────────────────────────────────

/// The injectable model-call seam — mirrors `radio::ModelCall`, but takes
/// the FULLY ASSEMBLED user message (grounding + question); the persona
/// system prompt (with `{{humor}}` substituted) is baked in by
/// [`dispatch_answerer_call_with`] before the call, since tests inject a canned
/// closure and never dispatch a real model.
pub type AnswererCall<'a> = dyn FnMut(&str) -> Result<String> + 'a;

/// The answering seat's reply.
#[derive(Debug, Clone)]
pub struct AnswerOutcome {
    /// The seat's own prose, verbatim.
    pub text: String,
    /// `text` plus the live command listing, appended ONLY when `text`
    /// itself names an advertised `/command` (issue #1698: "the command
    /// listing becomes the last resort ... and always appends after
    /// answers that reference commands"). This is the field callers render.
    pub rendered: String,
}

/// `true` iff `text` names one of `catalog`'s ids with the exact
/// `/<id>` slash syntax the persona's own prompt instructs it to use —
/// a cheap, deterministic heuristic (no NLP, no second model call).
fn answer_references_a_command(text: &str, catalog: &[CatalogEntry]) -> bool {
    let lower = text.to_ascii_lowercase();
    catalog.iter().any(|c| lower.contains(&format!("/{}", c.id.to_ascii_lowercase())))
}

/// Build the ANSWERING seat's user message: the assembled grounding, then
/// the user's own text verbatim. Byte-locked contract for the ORDER (facts
/// before the question — same "provenance/context first" shape
/// `radio::build_router_message` uses) but not golden-tested char-for-char
/// like the router's own message: unlike the router, this message embeds
/// live process state (config, board, help), which is expected to drift
/// run-to-run — a golden test here would be a golden test of the WHOLE
/// runtime's current state, not of this function's own logic.
pub fn build_answer_message(text: &str, grounding: &str) -> String {
    format!(
        "{grounding}\nThe user's message did not match any advertised command exactly, so \
         you're answering it directly:\n---\n{}\n---\n",
        text.trim()
    )
}

/// Route `text` to the answering seat: assemble grounding, dispatch once,
/// post-process. `call` is the injected [`AnswererCall`] — production wires
/// [`dispatch_answerer_call_with`]; tests inject a canned closure (no live model
/// ever runs under test).
pub fn answer(
    text: &str,
    catalog: &[CatalogEntry],
    shelf: &ArtifactShelf,
    cwd: &Path,
    scope: GroundingScope,
    call: &mut AnswererCall<'_>,
) -> Result<AnswerOutcome> {
    let grounding = assemble_grounding(text, catalog, shelf, cwd, scope);
    let message = build_answer_message(text, &grounding);
    let raw = call(&message)?;
    let reply = raw.trim().to_string();
    // (#1698 Packet B2 gate) The bare LISTING, not `not_a_command_message`
    // — appending "darkmux acp doesn't recognize that as a command" under
    // an answer that just helpfully named `/pr-list` tells the operator
    // their message failed, immediately after RADIO answered it.
    let listing = crate::acp_panel::command_listing(&crate::acp_panel::list_panel_commands());
    let rendered = if answer_references_a_command(&reply, catalog) && !listing.is_empty() {
        format!("{reply}\n\n{listing}")
    } else {
        reply.clone()
    };
    Ok(AnswerOutcome { text: reply, rendered })
}

/// Session-scoped overrides of the `radio.answerer_profile` / `radio.humor`
/// config values (#1698 Packet B2, scope F — the session config-option
/// pickers). `Default` (both `None`) falls through to the global
/// `config.json` tier exactly, so a session that never touches the pickers
/// behaves identically to before this struct existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswererOverrides {
    /// Session-selected answering-seat profile (the "radio host" picker).
    pub profile_name: Option<String>,
    /// Session-selected humor value (the "humor" picker).
    pub humor: Option<u8>,
}

/// The production [`AnswererCall`] implementation, parameterized by session
/// overrides. Loads the `radio-host` persona template (honoring an
/// operator-tier override, per `crate::crew::loader::role_prompt`'s own
/// precedence — "operator overrides are sovereign"), substitutes
/// `{{humor}}` from the resolved humor value, and dispatches through the
/// SAME container-free single-shot path (`dispatch_local_single_shot`) the
/// router uses — via `DispatchOpts.system_prompt_override` so the
/// substituted persona text is sent VERBATIM rather than re-resolved by the
/// loader (see that field's own doc on `DispatchOpts`).
/// The answering seat's explicitly-selected profile, if any: the session
/// picker wins over `radio.answerer_profile`; `None` means "no explicit
/// selection" and lets dispatch's own `role_profiles.radio-host` →
/// `default_profile` precedence decide.
///
/// Factored so [`dispatch_answerer_call_with`] and [`grounding_scope_for`]
/// resolve the SAME name. Two copies of this two-line precedence would be
/// a data-boundary bug waiting to happen: the gate would be deciding about
/// one profile while the dispatch went to another.
fn resolved_answerer_profile(overrides: &AnswererOverrides) -> Option<String> {
    overrides
        .profile_name
        .clone()
        .or_else(darkmux_types::config_access::radio_answerer_profile)
}

/// The grounding scope this dispatch is allowed — [`GroundingScope::RemoteSafe`]
/// when the answering seat resolves to a remote endpoint. Fails closed via
/// `crew::dispatch::dispatch_resolves_remote`.
pub fn grounding_scope_for(overrides: &AnswererOverrides) -> GroundingScope {
    let profile = resolved_answerer_profile(overrides);
    if crate::crew::dispatch::dispatch_resolves_remote("radio-host", profile.as_deref(), None) {
        GroundingScope::RemoteSafe
    } else {
        GroundingScope::Full
    }
}

/// The answering seat's per-call completion budget when the operator has
/// not set `runtime.max_tokens_per_call`. The single-shot path's own
/// default is 4096, and a 35B thinking model spent exactly that reasoning
/// about "how do I see what is loaded?" and returned no text (2026-08-28).
/// 16,384 is the figure the same path already uses when reasoning effort
/// is set; a thinking model is the radio-host's normal staffing.
pub const RADIO_ANSWER_TOKEN_CAP: u32 = 16_384;

/// `runtime.max_tokens_per_call` when set (env or config.json), else
/// [`RADIO_ANSWER_TOKEN_CAP`]. The knob's documented meaning is exactly this
/// budget (reasoning + content of one call), so radio honors it rather than
/// growing a knob of its own.
pub fn answer_token_cap() -> u32 {
    darkmux_types::config_access::max_tokens_per_call().unwrap_or(RADIO_ANSWER_TOKEN_CAP)
}

/// The seat's text, or an error when there is none. `single_shot` returns
/// empty content as `Ok("")` on purpose (it is a transport, not a judge);
/// radio is the judge, and an answer with no text is a failed answer that
/// names the likeliest cause and the knob.
pub fn answer_text(stdout: &str, cap: u32) -> Result<String> {
    let text = stdout.trim();
    if text.is_empty() {
        anyhow::bail!(
            "the answering seat returned no text. A reasoning model can spend its whole \
             {cap}-token per-call budget thinking and emit nothing; retry, or raise \
             `runtime.max_tokens_per_call` (`darkmux config set runtime.max_tokens_per_call N`)."
        );
    }
    Ok(text.to_string())
}

pub fn dispatch_answerer_call_with(user_message: &str, overrides: &AnswererOverrides) -> Result<String> {
    let persona = crate::crew::loader::role_prompt("radio-host").ok_or_else(|| {
        anyhow::anyhow!("radio-host role has no readable .md persona template — cannot dispatch the answering seat")
    })?;
    let humor = overrides.humor.unwrap_or_else(darkmux_types::config_access::radio_humor);
    let system_prompt = persona.replace("{{humor}}", &humor.to_string());
    let profile_name = resolved_answerer_profile(overrides);

    let opts = crate::crew::dispatch::DispatchOpts {
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        role_id: "radio-host".to_string(),
        message: user_message.to_string(),
        session_id: None,
        timeout_seconds: 300,
        skip_preflight: false,
        json: false,
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        compaction: crate::crew::dispatch::CompactionDispatchArgs::default(),
        // (#1698 Packet B2, scope E/F) session override (the "radio host"
        // picker) wins over `radio.answerer_profile`, which wins over
        // `role_profiles.radio-host`/`default_profile` — see
        // `RadioConfig::answerer_profile`'s doc for the base-tier precedence.
        profile_name,
        config_path: None,
        force_container: false,
        max_completion_tokens: Some(answer_token_cap()),
        image: None,
        model_base_url_override: None,
        step_id: None,
        system_prompt_override: Some(system_prompt),
    };
    let result = crate::fleet::dispatch_routed_via(opts, crate::crew::dispatch::dispatch_local_single_shot)?;
    answer_text(&result.stdout, answer_token_cap())
}

/// Convenience wrapper: [`answer`] wired to the production call, with
/// optional session overrides (empty for the CLI verb, which has none) —
/// the one call site both `src/acp.rs`'s no-slash channel and
/// `src/radio_cli.rs`'s CLI refusal path use.
pub fn answer_live(
    text: &str,
    catalog: &[CatalogEntry],
    shelf: &ArtifactShelf,
    cwd: &Path,
    overrides: &AnswererOverrides,
) -> Result<AnswerOutcome> {
    // (#1698 Packet B2 gate) The boundary is decided HERE, before assembly
    // — not inside the dispatch, which only ever sees the finished message.
    let scope = grounding_scope_for(overrides);
    if scope == GroundingScope::RemoteSafe {
        eprintln!(
            "[darkmux-acp] radio answering seat resolves to a REMOTE endpoint — grounding limited \
             to the command catalog and `--help`; the config surface, mission board, artifact \
             shelf, and any deep artifact are withheld (they never leave this machine)."
        );
    }
    answer(text, catalog, shelf, cwd, scope, &mut |m: &str| {
        dispatch_answerer_call_with(m, overrides)
    })
    .context("dispatching the radio answering seat")
}

/// The profile names available for the "radio host" session config-option
/// picker (#1698 Packet B2, scope F) — every profile the operator's own
/// registry declares, read-only (the same registry `resolve_dispatch_model_internal`
/// resolves against). Empty on a registry load failure rather than erroring
/// — the picker degrades to "no choices" instead of breaking `session/new`.
pub fn available_profile_names() -> Vec<String> {
    darkmux_profiles::profiles::load_registry(None)
        .map(|loaded| loaded.registry.profiles.keys().cloned().collect())
        .unwrap_or_default()
}

/// Preset humor values offered by the "humor" session config-option picker
/// (#1698 Packet B2, scope F). The vendored ACP v1 schema has no numeric/
/// slider config-option kind — only `select` (dropdown) and `boolean` — so
/// a continuous 0-100 dial is exposed as a small preset ladder instead. See
/// this packet's PR body for the schema finding in full.
pub const HUMOR_PRESETS: &[u8] = &[10, 50, 75, 100];

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, description: &str) -> CatalogEntry {
        CatalogEntry { id: id.to_string(), description: description.to_string(), hint: None }
    }

    fn fixture_catalog() -> Vec<CatalogEntry> {
        vec![entry("pr-list", "List open pull requests."), entry("review", "Run the review pipeline.")]
    }

    // ── ArtifactShelf ─────────────────────────────────────────────────

    #[test]
    fn shelf_evicts_oldest_beyond_capacity() {
        let mut shelf = ArtifactShelf::default();
        for i in 0..(SHELF_CAPACITY + 2) {
            shelf.push(shelf_entry("pr-list", "", &format!("output {i}")));
        }
        let rendered: Vec<String> = shelf.entries().map(|e| e.rendered.clone()).collect();
        assert_eq!(rendered.len(), SHELF_CAPACITY);
        // The two OLDEST (0, 1) must be gone; the most recent SHELF_CAPACITY survive.
        assert!(!rendered.iter().any(|r| r == "output 0"));
        assert!(!rendered.iter().any(|r| r == "output 1"));
        assert!(rendered.iter().any(|r| r.contains("output 4")));
    }

    // ── Sections::enforce_budget — trim order ────────────────────────────

    #[test]
    fn enforce_budget_drops_help_and_shelf_tail_before_a_named_artifact() {
        // Construct a Sections whose sections individually respect their own
        // per-section caps but whose TOTAL blows the hard cap — proving the
        // trim order names in the issue's own B2 context-budget comment:
        // help and shelf tail yield BEFORE a named artifact.
        let big = |n: usize| "x".repeat(n);
        let mut sections = Sections {
            catalog: Some(big(1_000)),
            config: Some(big(1_000)),
            board: Some(big(1_000)),
            help: Some(big(1_000)),
            shelf: vec![big(1_000), big(1_000), big(1_000)],
            deep_artifact: Some("THE NAMED ARTIFACT".to_string()),
        };
        // Force a tiny hard cap via direct manipulation isn't possible (the
        // constant is private + fixed) — instead inflate every section well
        // past HARD_CAP_CHARS so the real constant's trim logic actually
        // fires end to end.
        sections.help = Some(big(HARD_CAP_CHARS));
        sections.shelf = vec![big(20_000), big(20_000), big(20_000)];
        sections.enforce_budget();
        assert!(sections.help.is_none(), "help must be dropped first");
        assert!(sections.shelf.len() < 3, "shelf tail must shrink");
        assert_eq!(
            sections.deep_artifact.as_deref(),
            Some("THE NAMED ARTIFACT"),
            "a named deep artifact must survive trimming that only needed to drop help + shelf"
        );
        assert!(sections.total_chars() <= HARD_CAP_CHARS);
    }

    #[test]
    fn enforce_budget_is_a_noop_when_already_under_cap() {
        let mut sections = Sections {
            catalog: Some("small".to_string()),
            config: Some("small".to_string()),
            board: Some("small".to_string()),
            help: Some("small".to_string()),
            shelf: vec!["small".to_string()],
            deep_artifact: Some("small".to_string()),
        };
        sections.enforce_budget();
        assert!(sections.help.is_some());
        assert_eq!(sections.shelf.len(), 1);
        assert!(sections.deep_artifact.is_some());
    }

    // ── The board block (#1713 / #1714 gate MF-2) ────────────────────────
    //
    // Against the PURE core, so these actually execute on CI. Before the
    // extraction the only entry point read `~/.darkmux` off disk: CI has no
    // crew dir, so `load_missions` returned empty, the early return fired,
    // and every line below it went untested — while locally the same tests
    // silently read the developer's real board.

    fn board_mission(
        id: &str,
        status: crate::crew::types::MissionStatus,
        created: u64,
        finalized: Option<u64>,
    ) -> crate::crew::types::Mission {
        crate::crew::types::Mission {
            id: id.into(),
            description: id.into(),
            status,
            phase_ids: vec![],
            created_ts: created,
            started_ts: None,
            finalized_ts: finalized,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
        }
    }

    /// THE #1713 regression. Every mission finalized — the ordinary state on
    /// a machine whose recent work is all run instances — must still put
    /// mission NAMES in the bundle. Before the fix this block named only
    /// active/paused missions, so the answering seat was handed counts and
    /// nothing else, and correctly refused to say which was most recent.
    #[test]
    fn the_board_block_names_recent_missions_even_when_nothing_is_open() {
        use crate::crew::types::MissionStatus as M;
        let missions = vec![
            board_mission("review-old", M::Finalized, 100, Some(200)),
            board_mission("review-newest", M::Finalized, 100, Some(9_000)),
        ];
        let block = render_board_block_from(&missions).expect("block renders");
        assert!(
            block.contains("review-newest"),
            "a finalized mission must still be nameable — this is #1713: {block}"
        );
        let recent_line = block.lines().find(|l| l.starts_with("Most recent")).expect("line");
        // …and it must be FIRST, not merely present.
        let newest = recent_line.find("review-newest").unwrap();
        let older = recent_line.find("review-old").unwrap();
        assert!(newest < older, "newest-first ordering: {recent_line}");
    }

    /// The sort is `Reverse(last_activity)`, and `last_activity` is a max over
    /// whichever stamps are present — which field is newest depends on the
    /// mission's path through the state machine. A mission finalized long
    /// after creation must outrank one merely created later.
    #[test]
    fn board_block_orders_by_last_touched_not_by_creation() {
        use crate::crew::types::MissionStatus as M;
        let missions = vec![
            board_mission("created-later-never-finished", M::Active, 5_000, None),
            board_mission("created-early-finalized-late", M::Finalized, 10, Some(8_000)),
        ];
        let block = render_board_block_from(&missions).expect("block renders");
        // Scoped to the recent LINE on purpose: an open mission also appears
        // on the `Active/paused:` line above it, so a whole-block `find`
        // measures which line comes first, not the ordering under test.
        let recent_line = block.lines().find(|l| l.starts_with("Most recent")).expect("line");
        let finalized_at = recent_line.find("created-early-finalized-late").unwrap();
        let created_at = recent_line.find("created-later-never-finished").unwrap();
        assert!(
            finalized_at < created_at,
            "a mission finalized at 8000 was touched more recently than one created at 5000: {recent_line}"
        );
    }

    #[test]
    fn board_block_caps_the_recent_list() {
        use crate::crew::types::MissionStatus as M;
        let missions: Vec<_> = (0..12)
            .map(|i| board_mission(&format!("m-{i}"), M::Finalized, 100, Some(1_000 + i as u64)))
            .collect();
        let block = render_board_block_from(&missions).expect("block renders");
        let recent_line = block.lines().find(|l| l.starts_with("Most recent")).expect("line");
        let named = (0..12).filter(|i| recent_line.contains(&format!("m-{i} ("))).count();
        assert_eq!(
            named, RECENT_MISSIONS_IN_BOARD_BLOCK,
            "the recent list is capped, not the whole board: {recent_line}"
        );
        // The cap keeps the NEWEST, which is the whole point.
        assert!(recent_line.contains("m-11"), "{recent_line}");
        assert!(!recent_line.contains("m-0 ("), "{recent_line}");
    }

    // ── (#1717) Minted-run marking + the named-mission crowding floor ────

    /// A minted run's row carries the `auto` marker; a named mission's row
    /// does not. Both ids are real observed shapes: the epoch-stamped one
    /// is a pre-#1503 run-instance pattern (`spec: None`, id-shape
    /// fallback), the other is exactly the `1616-compactor-fix` operator-
    /// naming-convention counterexample `Mission::is_minted_run`'s own doc
    /// names — proof the marker isn't just "contains digits."
    #[test]
    fn the_board_block_marks_minted_runs_and_leaves_named_missions_unmarked() {
        use crate::crew::types::MissionStatus as M;
        let missions = vec![
            board_mission("review-1785400940-136e76", M::Finalized, 100, Some(9_000)),
            board_mission("1616-compactor-fix", M::Active, 100, Some(8_000)),
        ];
        let block = render_board_block_from(&missions).expect("block renders");
        let recent_line = block.lines().find(|l| l.starts_with("Most recent")).expect("line");
        assert!(
            recent_line.contains("review-1785400940-136e76 (finalized, auto)"),
            "a machine-minted run must carry the auto marker: {recent_line}"
        );
        assert!(
            recent_line.contains("1616-compactor-fix (active)")
                && !recent_line.contains("1616-compactor-fix (active, auto)"),
            "an operator-named mission must NOT carry the auto marker: {recent_line}"
        );
    }

    /// THE #1717 regression. A board where every one of the top
    /// `RECENT_MISSIONS_IN_BOARD_BLOCK` most-recently-touched missions is a
    /// machine-minted run (the issue's own measured shape: runs vastly
    /// outnumber named missions) must still surface the operator's named
    /// work somewhere in the bundle — never silently crowded out entirely.
    ///
    /// Both fixtures are non-open (`Finalized`) on purpose: an `Active` or
    /// `Paused` floor fixture would also land on the `Active/paused:` line,
    /// which would satisfy a whole-block `contains` check regardless of
    /// whether the floor logic under test ever ran. A prior version of this
    /// test used `M::Active` for `doom-loop-m4` and asserted only
    /// `block.contains(...)`, so it passed for the wrong reason on that
    /// half of its coverage — the `1616-compactor-fix` assertion (reachable
    /// only through the floor) is what actually caught #1717. Scoping both
    /// assertions to the `Also tracking` line specifically closes that gap.
    #[test]
    fn the_board_block_surfaces_named_missions_even_when_runs_dominate_the_recent_list() {
        use crate::crew::types::MissionStatus as M;
        // More minted runs than the recent-list cap, all touched more
        // recently than either named mission below.
        let mut missions: Vec<_> = (0..(RECENT_MISSIONS_IN_BOARD_BLOCK + 3))
            .map(|i| {
                board_mission(
                    &format!("review-178540{i:04}-136e76"),
                    M::Finalized,
                    100,
                    Some(10_000 + i as u64),
                )
            })
            .collect();
        missions.push(board_mission("doom-loop-m4", M::Finalized, 1, Some(2)));
        missions.push(board_mission("1616-compactor-fix", M::Finalized, 1, Some(50)));

        let block = render_board_block_from(&missions).expect("block renders");
        let recent_line = block.lines().find(|l| l.starts_with("Most recent")).expect("line");
        assert!(
            !recent_line.contains("doom-loop-m4") && !recent_line.contains("1616-compactor-fix"),
            "precondition: the recent-list cap alone must NOT already include either named \
             mission, or this test isn't exercising the floor: {recent_line}"
        );
        let floor_line = block
            .lines()
            .find(|l| l.starts_with("Also tracking"))
            .expect("the floor line must be present when named missions are crowded out");
        assert!(
            floor_line.contains("doom-loop-m4"),
            "a named mission must reach the bundle via the floor line specifically, not \
             merely somewhere in the block: {floor_line}"
        );
        assert!(
            floor_line.contains("1616-compactor-fix"),
            "a second named mission should also surface within the floor line: {floor_line}"
        );
    }

    /// (#1717 follow-up, MUST FIX) A minted run that drops out of the "Most
    /// recent" top-5 — because five OTHER missions were touched more
    /// recently — still appears on the `Active/paused:` line if it's
    /// Active or Paused. Before this fix that line never marked minted
    /// runs, and the block's own inline definition ("`auto` marks a run
    /// the darkmux CLI launched by itself, not something the user typed")
    /// applies block-wide once stated — so an unmarked id on THIS line now
    /// reads as a positive claim the user typed it, on exactly the row an
    /// operator asking "what's open" is most likely to read.
    #[test]
    fn the_active_paused_line_also_marks_minted_runs_dropped_from_the_recent_list() {
        use crate::crew::types::MissionStatus as M;
        let mut missions =
            vec![board_mission("dispatch-code-reviewer-1785589698-abc123", M::Active, 100, None)];
        // Five more-recently-touched named missions push the minted run out
        // of the top-5 "Most recent" list without changing its Active
        // status — it can ONLY still surface via the Active/paused line.
        missions.extend((0..RECENT_MISSIONS_IN_BOARD_BLOCK).map(|i| {
            board_mission(&format!("named-mission-{i}"), M::Finalized, 100, Some(9_000 + i as u64))
        }));

        let block = render_board_block_from(&missions).expect("block renders");
        let recent_line = block.lines().find(|l| l.starts_with("Most recent")).expect("line");
        assert!(
            !recent_line.contains("dispatch-code-reviewer-1785589698-abc123"),
            "precondition: the minted run must be crowded out of the recent list, or this \
             test isn't exercising the bug: {recent_line}"
        );
        let live_line = block.lines().find(|l| l.starts_with("Active/paused")).expect("line");
        assert!(
            live_line.contains("dispatch-code-reviewer-1785589698-abc123 (auto)"),
            "a minted run that only surfaces via Active/paused must still carry the auto \
             marker — an unmarked id here reads as a positive claim the user typed it: \
             {live_line}"
        );
    }

    /// (#1717 follow-up #2, coordinator finding) A named mission that is
    /// itself Active/Paused — and therefore already visible on the
    /// `Active/paused:` line — must not ALSO be re-emitted by the floor.
    /// The floor's own header claims "named work not in the list above";
    /// before this fix the floor only excluded ids already in the `Most
    /// recent` top-5, not ids already on the `Active/paused` line, so an
    /// active named mission crowded out of the top-5 (but still open) got
    /// a genuine duplicate: once on `Active/paused`, again on the floor.
    /// Two lines both asserting something true about the same mission
    /// reads to a model as two DIFFERENT pieces of evidence about it, not
    /// one restated — the same failure class the MUST FIX above closed on
    /// the marking side, now on the dedup side.
    #[test]
    fn the_floor_excludes_ids_already_on_the_active_paused_line() {
        use crate::crew::types::MissionStatus as M;
        // Active named mission, touched a while ago — old enough to be
        // crowded out of the RECENT_MISSIONS_IN_BOARD_BLOCK top-5 by the
        // finalized runs below, but still Active (so it's on Active/paused).
        let mut missions = vec![board_mission("1616-compactor-fix", M::Active, 100, None)];
        missions.extend((0..RECENT_MISSIONS_IN_BOARD_BLOCK).map(|i| {
            board_mission(
                &format!("review-178540{i:04}-136e76"),
                M::Finalized,
                100,
                Some(10_000 + i as u64),
            )
        }));

        let block = render_board_block_from(&missions).expect("block renders");
        let recent_line = block.lines().find(|l| l.starts_with("Most recent")).expect("line");
        assert!(
            !recent_line.contains("1616-compactor-fix"),
            "precondition: the active mission must be crowded out of the recent list: \
             {recent_line}"
        );
        let live_line = block.lines().find(|l| l.starts_with("Active/paused")).expect("line");
        assert!(
            live_line.contains("1616-compactor-fix"),
            "precondition: it must be on Active/paused: {live_line}"
        );

        let occurrences = block.matches("1616-compactor-fix").count();
        assert_eq!(
            occurrences, 1,
            "an active named mission must appear exactly ONCE across the whole block, not \
             once on Active/paused AND again on the floor's Also-tracking line: {block}"
        );
    }

    /// (#1717 follow-up #2) Excluding an already-visible active mission from
    /// the floor must not simply shrink the floor by one slot — the floor's
    /// whole point is that named work reaches the bundle, so a slot the
    /// active mission didn't need goes to the next-most-recent named
    /// mission that ISN'T visible anywhere else yet, not left unfilled.
    #[test]
    fn the_floor_reaches_deeper_when_an_active_mission_is_excluded_from_it() {
        use crate::crew::types::MissionStatus as M;
        // Crowds `1616-compactor-fix` out of the top-5, same as the test
        // above.
        let mut missions = vec![board_mission("1616-compactor-fix", M::Active, 100, None)];
        missions.extend((0..RECENT_MISSIONS_IN_BOARD_BLOCK).map(|i| {
            board_mission(
                &format!("review-178540{i:04}-136e76"),
                M::Finalized,
                100,
                Some(10_000 + i as u64),
            )
        }));
        // Three more named missions, each OLDER than `1616-compactor-fix`
        // (last_activity 100) — without the exclusion fix these are exactly
        // the ones a naive "skip it, shrink by one" fix would leave out,
        // since `1616-compactor-fix` itself would otherwise occupy one of
        // the floor's `NAMED_MISSION_FLOOR_IN_BOARD_BLOCK` (3) slots ahead
        // of them in recency order.
        missions.push(board_mission("older-named-a", M::Finalized, 1, Some(3)));
        missions.push(board_mission("older-named-b", M::Finalized, 1, Some(2)));
        missions.push(board_mission("older-named-c", M::Finalized, 1, Some(1)));

        let block = render_board_block_from(&missions).expect("block renders");
        let floor_line =
            block.lines().find(|l| l.starts_with("Also tracking")).expect("floor line present");
        assert!(
            !floor_line.contains("1616-compactor-fix"),
            "the active mission must not consume a floor slot it doesn't need — it's \
             already visible on Active/paused: {floor_line}"
        );
        assert!(floor_line.contains("older-named-a"), "{floor_line}");
        assert!(floor_line.contains("older-named-b"), "{floor_line}");
        assert!(
            floor_line.contains("older-named-c"),
            "the freed slot must go to the NEXT-most-recent named mission, not sit empty: \
             {floor_line}"
        );
    }

    /// (#1717 follow-up, CONSIDER) The `Also tracking` line is positionally
    /// LAST in the assembled block, so `truncate_chars` — which cuts the
    /// WHOLE block at `BOARD_CAP_CHARS` with no per-section awareness —
    /// would eat it FIRST if the worst case ever grew past budget. Nobody
    /// had pinned the arithmetic that keeps today's worst case (5 live + 5
    /// recent + 3 floor rows, each at `BOARD_ID_CAP_CHARS`) under that
    /// budget; this test does, so the next constant bump that breaks it
    /// fails loudly instead of silently dropping the floor's guarantee.
    #[test]
    fn the_also_tracking_line_survives_truncation_at_the_worst_case_width() {
        use crate::crew::types::MissionStatus as M;

        // Exactly `BOARD_ID_CAP_CHARS` long, so `elide` never touches these
        // — this test is about the OUTER `truncate_chars`, not per-id
        // elision (that's covered separately by `elide_marks_what_it_cut...`).
        fn wide_id(prefix: &str) -> String {
            let base = format!("{prefix}-");
            let pad = BOARD_ID_CAP_CHARS.saturating_sub(base.chars().count());
            format!("{base}{}", "z".repeat(pad))
        }

        let mut missions = Vec::new();
        // 5 live (Active) rows — lowest recency, but always shown on the
        // Active/paused line regardless of where recency puts them.
        for i in 0..5 {
            missions.push(board_mission(&wide_id(&format!("live{i}")), M::Active, 1, None));
        }
        // `NAMED_MISSION_FLOOR_IN_BOARD_BLOCK` floor-only named rows — mid
        // recency: higher than the live rows (excluded from the top-5
        // cleanly), lower than the recent rows below (excluded from the
        // top-5 by recency, which is exactly what stands them up the floor).
        for i in 0..NAMED_MISSION_FLOOR_IN_BOARD_BLOCK {
            missions.push(board_mission(
                &wide_id(&format!("floor{i}")),
                M::Finalized,
                100,
                Some(50_000 + i as u64),
            ));
        }
        // `RECENT_MISSIONS_IN_BOARD_BLOCK` recent rows — highest recency,
        // dominate the "Most recent" top-5 list outright.
        for i in 0..RECENT_MISSIONS_IN_BOARD_BLOCK {
            missions.push(board_mission(
                &wide_id(&format!("recent{i}")),
                M::Finalized,
                100,
                Some(100_000 + i as u64),
            ));
        }

        let block = render_board_block_from(&missions).expect("block renders");
        assert!(
            !block.ends_with("…[truncated]"),
            "today's worst case must fit under BOARD_CAP_CHARS — if this fails, the \
             arithmetic across BOARD_CAP_CHARS / BOARD_ID_CAP_CHARS / \
             RECENT_MISSIONS_IN_BOARD_BLOCK / NAMED_MISSION_FLOOR_IN_BOARD_BLOCK has \
             drifted and the floor is no longer guaranteed to reach the model: {block}"
        );
        let floor_line = block
            .lines()
            .find(|l| l.starts_with("Also tracking"))
            .expect("the floor line must survive truncation at today's worst-case width");
        for i in 0..NAMED_MISSION_FLOOR_IN_BOARD_BLOCK {
            let id = wide_id(&format!("floor{i}"));
            assert!(
                floor_line.contains(&id),
                "floor id {i} must appear IN FULL on the Also-tracking line, not truncated \
                 or dropped: {floor_line}"
            );
        }
    }

    /// The floor line is a no-op cost on a board that's already
    /// representative — every named mission worth floor-listing is already
    /// in the "Most recent" rows, so nothing is left to add.
    #[test]
    fn the_named_floor_adds_nothing_when_the_recent_list_is_already_all_named() {
        use crate::crew::types::MissionStatus as M;
        let missions: Vec<_> = (0..RECENT_MISSIONS_IN_BOARD_BLOCK)
            .map(|i| board_mission(&format!("m-{i}"), M::Finalized, 100, Some(1_000 + i as u64)))
            .collect();
        let block = render_board_block_from(&missions).expect("block renders");
        assert!(
            !block.contains("Also tracking"),
            "no named missions were left to add — the floor line must not appear: {block}"
        );
    }

    #[test]
    fn board_block_counts_every_status_not_just_the_open_ones() {
        use crate::crew::types::MissionStatus as M;
        let missions = vec![
            board_mission("a", M::Active, 1, None),
            board_mission("f", M::Finalized, 1, Some(2)),
            board_mission("x", M::Aborted, 1, None),
        ];
        let block = render_board_block_from(&missions).expect("block renders");
        assert!(block.contains("1 active"), "{block}");
        assert!(block.contains("1 finalized"), "{block}");
        assert!(block.contains("1 aborted"), "{block}");
    }

    #[test]
    fn board_block_is_the_no_missions_line_when_there_are_none() {
        assert_eq!(render_board_block_from(&[]).unwrap(), "no missions yet.");
    }

    /// An elided id must not read as a whole one — a model that cites
    /// `review-17860` as a mission id has been handed a forgery by the
    /// harness, which is exactly what this seat's honesty rests on not
    /// happening.
    #[test]
    fn elide_marks_what_it_cut_and_leaves_short_ids_alone() {
        assert_eq!(elide("review-1786081556-0eea32", BOARD_ID_CAP_CHARS), "review-1786081556-0eea32");
        let long = "x".repeat(BOARD_ID_CAP_CHARS + 20);
        let cut = elide(&long, BOARD_ID_CAP_CHARS);
        assert!(cut.ends_with('…'), "an elided id must be visibly partial: {cut}");
        assert_eq!(cut.chars().count(), BOARD_ID_CAP_CHARS);
    }

    // ── The data boundary (#1698 Packet B2 gate) ─────────────────────────

    /// A shelf entry with content distinctive enough that finding it in the
    /// assembled bundle can't be a coincidence.
    fn shelf_with_private_output() -> ArtifactShelf {
        let mut shelf = ArtifactShelf::default();
        shelf.push(shelf_entry("review", "", "SECRET-DIFF-CONTENT-e7f1a2 leaked from a /review"));
        shelf
    }

    #[test]
    fn remote_safe_grounding_withholds_the_shelf_config_and_board() {
        let grounding = assemble_grounding(
            "is this darkmux?",
            &fixture_catalog(),
            &shelf_with_private_output(),
            Path::new("/tmp"),
            GroundingScope::RemoteSafe,
        );
        assert!(
            !grounding.contains("SECRET-DIFF-CONTENT-e7f1a2"),
            "a remote-resolved answering seat must never be handed the artifact shelf — \
             after a /review it holds rendered output over the operator's private diff. \
             Got: {grounding}"
        );
        // The public surfaces still ship, or the seat couldn't answer
        // "is this darkmux?" / "what can I run?" at all on a remote machine.
        assert!(grounding.contains("pr-list"), "the command catalog is public and must survive: {grounding}");
    }

    /// The inverted case — without this, the assertion above would pass just
    /// as happily if `assemble_grounding` returned the empty string, or if
    /// the shelf were broken everywhere rather than withheld on purpose.
    #[test]
    fn full_grounding_does_include_the_shelf() {
        let grounding = assemble_grounding(
            "is this darkmux?",
            &fixture_catalog(),
            &shelf_with_private_output(),
            Path::new("/tmp"),
            GroundingScope::Full,
        );
        assert!(
            grounding.contains("SECRET-DIFF-CONTENT-e7f1a2"),
            "a LOCAL answering seat must still get the shelf — otherwise the RemoteSafe test \
             above proves nothing about the boundary. Got: {grounding}"
        );
    }

    #[test]
    fn shelf_entries_are_truncated_at_write_time_not_only_at_assembly() {
        let huge = "x".repeat(SHELF_ENTRY_CAP_CHARS * 4);
        let entry = shelf_entry("review", "", &huge);
        assert!(
            entry.rendered.chars().count() <= SHELF_ENTRY_CAP_CHARS + 32,
            "stored {} chars — an unbounded store grows process RAM with every command the \
             operator ever runs, even though assembly can only ever read back {}",
            entry.rendered.chars().count(),
            SHELF_ENTRY_CAP_CHARS
        );
    }

    // ── detect_mission_mention (deep-artifact heuristic) ─────────────────

    #[test]
    fn detect_mission_mention_extracts_the_token_after_mission() {
        assert_eq!(detect_mission_mention("what's up with mission foo-bar-2?"), Some("foo-bar-2".to_string()));
    }

    #[test]
    fn detect_mission_mention_case_insensitive_on_the_keyword() {
        assert_eq!(detect_mission_mention("check on Mission alpha please"), Some("alpha".to_string()));
        assert_eq!(detect_mission_mention("MISSION baz status"), Some("baz".to_string()));
    }

    #[test]
    fn detect_mission_mention_absent_returns_none() {
        assert_eq!(detect_mission_mention("is this darkmux?"), None);
    }

    // ── answer_references_a_command ──────────────────────────────────────

    #[test]
    fn answer_referencing_a_slash_command_gets_the_listing_appended() {
        let mut call = |_msg: &str| -> Result<String> { Ok("Try running /pr-list to see them.".to_string()) };
        let shelf = ArtifactShelf::default();
        let outcome = answer("anything mergeable?", &fixture_catalog(), &shelf, Path::new("/tmp"), GroundingScope::Full, &mut call).unwrap();
        assert!(outcome.rendered.len() > outcome.text.len(), "the listing must be appended: {outcome:?}");
    }

    #[test]
    fn answer_not_referencing_a_command_stays_bare() {
        let mut call = |_msg: &str| -> Result<String> { Ok("darkmux is a local-AI orchestrator CLI.".to_string()) };
        let shelf = ArtifactShelf::default();
        let outcome = answer("is this darkmux?", &fixture_catalog(), &shelf, Path::new("/tmp"), GroundingScope::Full, &mut call).unwrap();
        assert_eq!(outcome.text, outcome.rendered, "no command referenced — no listing appended: {outcome:?}");
    }

    #[test]
    fn answer_dispatch_error_propagates_as_err() {
        let mut call = |_msg: &str| -> Result<String> { Err(anyhow::anyhow!("no model loaded")) };
        let shelf = ArtifactShelf::default();
        let result = answer("is this darkmux?", &fixture_catalog(), &shelf, Path::new("/tmp"), GroundingScope::Full, &mut call);
        assert!(result.is_err(), "a dispatch failure must propagate, not be swallowed into a bogus answer");
    }

    // ── build_answer_message ─────────────────────────────────────────────

    #[test]
    fn build_answer_message_puts_grounding_before_the_question() {
        let msg = build_answer_message("is this darkmux?", "GROUNDING HERE");
        let g_pos = msg.find("GROUNDING HERE").unwrap();
        let q_pos = msg.find("is this darkmux?").unwrap();
        assert!(g_pos < q_pos, "grounding must precede the question: {msg}");
    }

    // ── radio-host.md frozen golden (contract 6) ─────────────────────────

    #[test]
    fn radio_host_role_prompt_matches_frozen_golden() {
        // Compared against the SHIPPED template directly (`include_str!`),
        // never `crate::crew::loader::role_prompt`, which would resolve an
        // operator's own override at `~/.darkmux/crew/roles/radio-host.md`
        // instead — see `radio.rs`'s sibling golden test for why.
        const SHIPPED_TEMPLATE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/templates/builtin/roles/radio-host.md"
        ));
        let expected = "# RADIO\n\
            \n\
            You are RADIO, the voice on the operator's local-AI console. Speak like a NASA flight-controller on an open channel: calm, precise, dry wit, never filler. Honesty is not negotiable — you never invent a fact, a command, or a capability that wasn't handed to you. Humor is the one dial that moves.\n\
            \n\
            Humor setting: {{humor}}%\n\
            \n\
            ## Why you're being asked\n\
            \n\
            Every message you see already failed to match a known command exactly. Someone typed a sentence instead of a command, and now it's your turn on the mic: read what they said, read the mission facts below, and answer like a person who actually knows this system — not a search engine reciting them back.\n\
            \n\
            ## What you were handed\n\
            \n\
            Every call gives you a compiled grounding bundle assembled BEFORE you were dispatched — the command catalog, the darkmux command index (every runnable `darkmux` verb with its options, one line each), the current config surface, a short status board, and (when the user's message named something) recent history and one deep artifact. This is the entire truth you have access to. You have no tools, no memory of other exchanges, and no way to look anything up yourself — everything you can honestly say comes from what's in this message.\n\
            \n\
            ## Your job\n\
            \n\
            1. Answer the user's message using only the grounding you were given. If the grounding doesn't cover it, say so plainly — never guess or pad with generic AI filler.\n\
            2. If the honest answer points at a command the user could run, name it exactly as it appears in your grounding: a panel command by its slash id from the catalog (e.g. `/pr-list`), any other darkmux verb as the full line from the command index (e.g. `darkmux machine status`), with the option that matters if one does. Never invent a command or an option that is not listed in the grounding you were given.\n\
            3. If a config value is the right lever, tell them the exact invocation to run themselves (e.g. \"run `darkmux config set radio.humor 80`\") — you never execute anything, you only ever say what to run. Suggest, never do.\n\
            4. If the message is genuinely outside what you can ground an answer in — open-ended, off-topic, or asking you to reason about something no grounding source covers — say so honestly and hand it off: \"That's outside what I can answer from here — worth raising with your frontier orchestrator directly.\" Never fake an answer to avoid saying no.\n\
            \n\
            ## Output\n\
            \n\
            Plain prose. No JSON, no fenced blocks, no headers, no bullet-point dumps unless the answer genuinely needs a short list. A few sentences is usually the right length — this is one exchange, not a report.\n";
        assert_eq!(
            SHIPPED_TEMPLATE, expected,
            "templates/builtin/roles/radio-host.md drifted from the frozen model-facing text \
             (contract 6) — a deliberate edit updates both this golden and the file together."
        );
    }

    /// (#1784) The bundle carries the verb index, so a "how do I" question
    /// finds the exact invocation instead of top-level help's verb names.
    #[test]
    fn grounding_carries_the_verb_index_with_subverbs_and_options() {
        let shelf = ArtifactShelf::default();
        let bundle = assemble_grounding("how do I see what is loaded?", &[], &shelf, Path::new("/tmp"), GroundingScope::Full);
        assert!(bundle.contains("darkmux machine status"), "{bundle}");
        assert!(bundle.contains("darkmux machine list [") && bundle.contains("--deep"), "{bundle}");
        assert!(bundle.contains("darkmux mission launch"), "{bundle}");
        assert!(!bundle.contains("Top-level darkmux --help"), "the old block is gone: {bundle}");
    }

    // ── an empty answer is a failure, not an answer (2026-08-28) ─────────

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: caller's test is #[serial_test::serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }


    /// The answering seat ran a 35B thinking model against a one-line
    /// question and returned "" after 4095 completion tokens: it reasoned
    /// its whole 4096 budget away. `single_shot` calls that Ok(""), radio
    /// printed "radio: " and exited 0. The text must be non-empty to count.
    #[test]
    fn an_empty_answer_names_the_budget_and_the_knob() {
        let err = answer_text("   \n", 4096).unwrap_err().to_string();
        assert!(err.contains("4096"), "{err}");
        assert!(err.contains("runtime.max_tokens_per_call"), "{err}");
        assert!(err.to_lowercase().contains("reason"), "{err}");
        assert_eq!(answer_text("Run `darkmux machine status`.", 4096).unwrap(), "Run `darkmux machine status`.");
    }

    /// The seat's per-call budget honors the operator's knob and otherwise
    /// gives a reasoning model room: the single-shot default of 4096 is what
    /// produced the empty answer.
    #[test]
    #[serial_test::serial]
    fn answer_token_cap_honors_the_config_knob_and_defaults_above_4096() {
        let _g = EnvGuard::set("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "12000");
        assert_eq!(answer_token_cap(), 12000);
        drop(_g);
        let _g = EnvGuard::set("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "");
        assert!(answer_token_cap() > 4096, "{}", answer_token_cap());
        assert_eq!(answer_token_cap(), RADIO_ANSWER_TOKEN_CAP);
    }
}


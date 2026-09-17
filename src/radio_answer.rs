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

use crate::radio::{CatalogEntry, RadioSurface};
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
    /// (#1861 defect 1) Which surface the seat is actually speaking to —
    /// present on every real call ([`assemble_grounding`] always sets it),
    /// `None` only in hand-built test fixtures that predate this field and
    /// don't care about it. Deliberately excluded from
    /// [`Sections::enforce_budget`]'s drop cascade: it is a few dozen
    /// characters and the one fact that decides which command-reference
    /// SYNTAX is even real, so it is never a candidate for trimming.
    surface: Option<String>,
    catalog: Option<String>,
    config: Option<String>,
    board: Option<String>,
    help: Option<String>,
    shelf: Vec<String>,
    deep_artifact: Option<String>,
}

impl Sections {
    fn total_chars(&self) -> usize {
        [&self.surface, &self.catalog, &self.config, &self.board, &self.help, &self.deep_artifact]
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
        if let Some(s) = &self.surface {
            out.push_str(s);
            out.push('\n');
        }
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
    let index = crate::radio_index::render_verb_index(&command_verb_index());
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
/// `surface` is #1861 defect 1's fix: which command-reference SYNTAX is
/// even real depends on where the seat is talking, so that fact is handed
/// over as DATA (a grounding section, below) rather than left to prompt
/// wording alone.
pub fn assemble_grounding(
    text: &str,
    catalog: &[CatalogEntry],
    shelf: &ArtifactShelf,
    _cwd: &Path,
    scope: GroundingScope,
    surface: RadioSurface,
) -> String {
    let machine_local = scope == GroundingScope::Full;
    let mut sections = Sections {
        surface: Some(render_surface_block(surface)),
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

/// (#1861 defect 1) The one fact that decides which command-reference
/// syntax is real: on the CLI, no `/anything` is ever typed by the user (a
/// catalog command is invoked BY radio, never by a shell); inside the
/// editor panel, `/id` is the real, running syntax. Mirrors the wording
/// substituted into the persona's own `{{surface_instructions}}`
/// placeholder (`dispatch_answerer_call_with`) — same source of truth,
/// stated twice (grounding + persona) so the seat is told the same thing
/// both ways rather than left to infer one from the other.
fn render_surface_block(surface: RadioSurface) -> String {
    match surface {
        RadioSurface::Cli => "Surface: command line (`darkmux radio`). There is no shell here \
             that runs `/anything` — a catalog command is invoked by radio itself, never typed \
             by the user. Never write a bare `/id`, and never name a catalog command by its id \
             alone: `review` on its own is not something the user can run. Name a catalog \
             command as `darkmux mission launch <id>`, which runs that exact command directly; \
             name any other darkmux verb as the full line from the command index below."
            .to_string(),
        RadioSurface::Panel => "Surface: editor panel. A catalog command runs by its exact \
             slash id (e.g. `/pr-list`). Any other darkmux verb is a command the user types in \
             a separate darkmux CLI shell, not in this panel — name it as the full line from \
             the command index below."
            .to_string(),
    }
}

/// The `{{surface_instructions}}` substitution for the persona's rule 2
/// (`templates/builtin/roles/radio-host.md`) — the SAME per-surface fact
/// [`render_surface_block`] states in the grounding bundle, phrased to
/// slot into that rule's own sentence. Two statements of one fact, from
/// one source of truth (this module), rather than the grounding and the
/// instruction drifting independently.
fn surface_instructions(surface: RadioSurface) -> String {
    match surface {
        RadioSurface::Cli => "on the command line, a catalog command is `darkmux mission launch \
             <id>` — never a bare `/id` and never the id on its own, since there is no shell \
             here that runs `/anything` and no such subcommand either; any other darkmux verb \
             is the full line from the command index (e.g. `darkmux machine status`)."
            .to_string(),
        RadioSurface::Panel => "in this panel, a catalog command runs by its exact slash id \
             (e.g. `/pr-list`); any other darkmux verb is a command the user types in a \
             separate darkmux CLI shell, cited as the full line from the command index (e.g. \
             `darkmux machine status`)."
            .to_string(),
    }
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
    /// The seat's own prose, VERBATIM (#2050 restored this, after #1861
    /// defect 2 had made it a rewritten copy). Nothing is edited out of a
    /// reply any more, because a reply naming a command that cannot be run
    /// on this surface never becomes an `AnswerOutcome` at all — see
    /// [`names_an_unrunnable_command`] and [`answer`]'s own fallback.
    pub text: String,
    /// `text` plus the live command listing, appended ONLY when `text`
    /// itself names an advertised `/command` AND the seat is speaking to
    /// the [`RadioSurface::Panel`] (issue #1698: "the command listing
    /// becomes the last resort ... and always appends after answers that
    /// reference commands"; issue #1861 defect 1: a slash listing is
    /// meaningless — and wrong — on the CLI, which has no shell that runs
    /// `/anything`). This is the field callers render.
    pub rendered: String,
}

/// `true` iff `text` names one of `catalog`'s ids with the exact
/// `/<id>` slash syntax the persona's own prompt instructs it to use —
/// a cheap, deterministic heuristic (no NLP, no second model call).
fn answer_references_a_command(text: &str, catalog: &[CatalogEntry]) -> bool {
    let lower = text.to_ascii_lowercase();
    catalog.iter().any(|c| lower.contains(&format!("/{}", c.id.to_ascii_lowercase())))
}

/// The mechanical backstop for #1861 defects 1 and 2, as #2050 rebuilt it
/// and its review sweep generalized it: a DETECTOR, not a repair. `true`
/// iff `reply` names a command that cannot actually be run on `surface`.
///
/// The persona's own "never invent a command" rule (radio-host.md rule 2)
/// is honored only as well as whichever model is loaded that day honors an
/// instruction — model-dependent, and not provable by any test. So this
/// scans the seat's raw reply for the command-reference SHAPES rule 2
/// tells it to produce and checks each against what is ACTUALLY runnable:
///
/// - `/id` — valid ONLY on [`RadioSurface::Panel`] (on the CLI there is no
///   shell that runs `/anything` — a routed id is executed by radio
///   itself, never typed by the operator; defect 1) AND only when `id` is
///   one of `catalog`'s advertised ids (defect 2).
/// - `darkmux <path...>` — valid on EITHER surface when `<path...>` is a
///   real leaf path in `verb_index` (#1784's introspected index — the
///   single grounding source this validates against, per the issue's own
///   fix-shape note), or names any node of that tree with `--help` /
///   `--version`, which are real at every node including the root.
/// - a bare catalog id handed over as something to type (`` `review` ``),
///   which is runnable on NEITHER surface — see [`is_a_bare_catalog_id`].
///
/// **Decoration is not the input format (the #2050 review sweep).** Rule 2
/// says "name it exactly ... in the syntax that's actually real" — a
/// constraint on the COMMAND, with no requirement to put backticks round
/// it. The first build of this detector nonetheless scrutinized
/// backtick-quoted spans and barely checked bare prose, which cost three
/// findings at once: a bare-prose `darkmux machine roster` was never
/// checked at all, ONE unmatched backtick moved every later reference into
/// an "inside" part that only an exact catalog-id match could trip (and a
/// backtick where an apostrophe belongs is this seat's demonstrated
/// failure pattern, not a hypothesis), and `Darkmux ...` failed a
/// case-sensitive prefix. So the scan now runs on
/// [`decoration_stripped`] text, where backticks, emphasis, brackets and
/// quotes are all spaces: parity cannot be broken by an unmatched
/// delimiter, because nothing depends on parity any more.
///
/// **What the quoted half still buys.** Two shapes are judged ONLY inside
/// a closed inline-code span, because the markup is the whole signal that
/// the seat is handing over something to TYPE rather than describing
/// something: a bare catalog id (an id is frequently an ordinary English
/// word), and a `darkmux ...` reference whose first word is not a real
/// top-level verb. In bare prose those two are read as description. The
/// symmetric part — the part the sweep was about — is that every reference
/// shape is now scanned in BOTH contexts; only the threshold for calling
/// one an instruction differs, and it differs on purpose.
///
/// **Why a detector and not a repair (#2050, second measurement).** Two
/// repairs were tried and both shipped broken prose, for the same reason.
/// Substituting the reference inline left a stub — *"Run (not an available
/// command) to kick off the local-model diff review."* Dropping the
/// SENTENCE the reference stood in left a dangling remainder, measured
/// live on this branch for `run the review pipeline`:
///
/// > `It will scan your current working-tree diff for bugs and report
/// > back. Let me know if it flags anything worth fixing.`
///
/// That is fluent, future-tense, and has no instruction in it, so it reads
/// as though the review is already running and the user can wait for a
/// verdict that never comes. **Fluent and misleading is worse than
/// visibly broken**, which is the severity ordering this project applies
/// everywhere else. The remainder of such a reply is ABOUT the reference
/// that was removed, and no amount of text surgery can see that — so the
/// whole reply is discarded and [`answer`] falls back to the plain
/// refusal, which both callers already render together with the live
/// command listing. That listing is exactly the "what can I actually run"
/// content the discarded reply was getting wrong.
///
/// **The cost, stated rather than buried:** a long, otherwise-good answer
/// that names one invented verb in passing is replaced wholesale by the
/// refusal plus the listing. Accepted deliberately — the alternative is a
/// surface that renders cleanly and misleads, and the fallback is the
/// output the operator measured as the best line of the original run.
/// That cost is also why widening the scan to bare prose came WITH a
/// narrowing ([`FILESYSTEM_ROOT_SEGMENTS`]) and two framing gates: a
/// discarded reply is invisible — it looks exactly like an ordinary
/// refusal — so the false-positive direction gets the same pinning as the
/// false-negative one.
///
/// **Known coverage limits, pinned by tests so they are decisions.**
///
/// - **A fenced code block is not scanned at all, and by likelihood this
///   is the DOMINANT residual — not a narrow one.** `index % 2 == 0` drops
///   every fence body, closed or unclosed, and handing over a command
///   inside a fence is the most idiomatic way a model answers "how do I do
///   X". The exclusion predates this detector's rebuild and is kept here
///   deliberately rather than revisited in passing: a fence is quoted
///   material (sample output, a transcript), and a truncated fence — the
///   seat hitting its token cap mid-block — is a real output whose body
///   read as prose would discard good replies. It wants its own change
///   with its own measurement; earlier wording in this doc called it "one
///   documented coverage limit", which understated it.
/// - An invented `darkmux <verb>` in bare prose with NO invocation cue and
///   no markup — *"darkmux machine roster sounds like a good feature"* — is
///   read as description and passes.
/// - A bare-prose reference whose first word is not a real top-level verb
///   is not a candidate, so a WHOLLY-invented verb — *"run darkmux
///   telepathy."* — is missed. Nothing separates it from the English word
///   in *"use darkmux instead of driving lms by hand"*, which the same gate
///   protects. Quoted (`` `darkmux telepathy --help` ``) it is caught.
/// - An unadvertised `/id` that nothing frames — *"the command is
///   /machine"* — is read as prose. That is the price of not discarding
///   *"mounted at /workspace"*; see [`token_is_framed`].
fn names_an_unrunnable_command(
    reply: &str,
    catalog: &[CatalogEntry],
    verb_index: &[crate::radio_index::VerbEntry],
    surface: RadioSurface,
) -> bool {
    // Fences first, so a fence body is excluded DETERMINISTICALLY rather
    // than by whatever parity the inline scan happens to land on.
    reply
        .split("```")
        .enumerate()
        .filter(|(i, _)| i % 2 == 0)
        .any(|(_, chunk)| chunk_names_an_unrunnable_command(chunk, catalog, verb_index, surface))
}

/// One outside-a-fence chunk. Two passes, and the split between them is
/// the sweep's whole point:
///
/// 1. **Decoration-independent** ([`decoration_stripped`]) — every `/id`
///    candidate, and every `darkmux ...` reference that bare prose FRAMES
///    as an instruction. No backtick parity is consulted, so an unmatched
///    delimiter cannot hide anything from this pass.
/// 2. **Closed inline-code spans only** — the two shapes whose only signal
///    that they are an instruction IS the markup: a bare catalog id, and a
///    `darkmux ...` reference with any first word. A span is a span only
///    when it CLOSES ([`closed_code_spans`]); an unmatched trailing
///    backtick simply opens nothing, and pass 1 has already covered the
///    text either way.
fn chunk_names_an_unrunnable_command(
    chunk: &str,
    catalog: &[CatalogEntry],
    verb_index: &[crate::radio_index::VerbEntry],
    surface: RadioSurface,
) -> bool {
    let undecorated = decoration_stripped(chunk);
    let tokens = tokenize(&undecorated);
    if slash_reference_is_unrunnable(&undecorated, &tokens, catalog, surface) {
        return true;
    }
    if prose_darkmux_reference_is_unrunnable(&undecorated, &tokens, verb_index) {
        return true;
    }
    for span in closed_code_spans(chunk) {
        let span = span.trim();
        if let Some(rest) = strip_darkmux_prefix(span) {
            if !darkmux_reference_is_valid(rest, verb_index) {
                return true;
            }
        } else if is_a_bare_catalog_id(span, catalog) {
            return true;
        }
    }
    false
}

/// Decoration that FRAMES a reference — inline-code backticks, markdown
/// emphasis, brackets, and quotes (straight and curly). Each becomes a
/// [`DECORATION_MARK`], so a scanner sees the reference undressed AND can
/// still tell that the seat marked it as a token rather than writing prose.
///
/// Two deliberate absences, each load-bearing:
///
/// - `_` is a legal character inside a command id, and in this seat's
///   output an id carrying an underscore is likelier than markdown
///   underscore-emphasis.
/// - `~` must stay glued to the `/` after it. Most home-relative paths
///   survive its removal anyway — `~/.darkmux/profiles.json` stops at the
///   `.`, and any two-segment path at the second `/` — but a
///   SINGLE-segment one, `~/runs`, would become a word-initial `/runs`
///   and read as an invented command.
const FRAMING_DECORATION: &[char] =
    &['`', '*', '(', ')', '[', ']', '{', '}', '"', '\'', '\u{2018}', '\u{2019}'];

/// What a [`FRAMING_DECORATION`] character becomes. U+000B is whitespace to
/// `char::is_whitespace`, so it ends a token exactly like a space does,
/// while staying distinguishable from one — which is the whole trick: the
/// scan is decoration-INDEPENDENT (no parity, nothing hidden) and still
/// decoration-AWARE (it knows the seat put markup there).
const DECORATION_MARK: char = '\u{000B}';

/// `chunk` with decoration replaced per the two classes above.
fn decoration_stripped(chunk: &str) -> Vec<char> {
    chunk
        .chars()
        .map(|c| {
            if FRAMING_DECORATION.contains(&c) {
                DECORATION_MARK
            } else {
                c
            }
        })
        .collect()
}

/// One whitespace-delimited token of a [`decoration_stripped`] chunk, with
/// the character offsets that let [`token_is_framed`] look at what sat on
/// either side of it.
struct Token {
    text: String,
    start: usize,
    end: usize,
}

/// Split on whitespace, keeping offsets. A [`DECORATION_MARK`] counts as
/// whitespace here, so `(/machine)` and `` `darkmux machine status` ``
/// tokenize exactly like their bare equivalents.
fn tokenize(chars: &[char]) -> Vec<Token> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        out.push(Token { text: chars[start..i].iter().collect(), start, end: i });
    }
    out
}

/// Every CLOSED inline-code span in `chunk`, in order.
///
/// A scanner rather than `split('`')`, because split alternates in/out by
/// backtick COUNT PARITY: one unmatched backtick — a backtick typed where
/// an apostrophe belongs, this seat's demonstrated slip — inverted the
/// classification of everything after it and made a huge run of prose the
/// "inside" part. Here an unmatched trailing backtick simply opens a span
/// that never closes, and yields nothing.
///
/// Pairing is first-to-next, which is what markdown itself does. With an
/// ODD number of backticks that still leaves one real span unpaired —
/// `` It`s easy: `darkmux machine roster` lists your crew `` pairs the
/// apostrophe-slip with the command's OPENING backtick — and nothing in
/// the text says which pairing was meant. That case is caught by the
/// decoration-aware pass instead: the command's own backticks are
/// [`DECORATION_MARK`]s there, so it is framed whether or not it pairs.
fn closed_code_spans(chunk: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = chunk;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else { break };
        out.push(&after[..close]);
        rest = &after[close + 1..];
    }
    out
}

/// Characters legal inside a command id — the id half of `/id`, and of a
/// `darkmux` verb token.
fn is_command_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Sentence punctuation that can trail a reference written as prose. It is
/// trimmed BEFORE any shape test, and it also ENDS a multi-word candidate:
/// a clause boundary is a command-path boundary.
///
/// (#2050 sweep finding 1.) An earlier revision deleted this helper on the
/// grounds that [`slash_candidate_id`]'s character scan stops at the first
/// non-id character anyway. That was true of the slash path and false of
/// the bare-prose `darkmux` path, which splits on whitespace and so
/// inherits the period: `"Try darkmux machine roster."` truncated to the
/// valid parent node `machine` and passed, while the same sentence with a
/// trailing clause — `"... roster for that."` — was caught. The bare-prose
/// pass was therefore near-inert in ordinary prose, which ends in a full
/// stop.
fn is_trailing_punctuation(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '?' | '!' | '"' | '\'' | '\u{2026}')
}

/// Words that FRAME a reference as an INSTRUCTION rather than a
/// description, plus the shell-prompt characters that do the same job in a
/// transcript.
///
/// This is prose's counterpart to markup, and one of the two things
/// [`token_is_framed`] accepts. Without it the scan eats the seat's
/// ordinary sentences about the tool it exists to talk about —
/// *"darkmux config lives at ~/.darkmux/config.json"* names a real node
/// followed by an English verb, which is structurally identical to
/// *"darkmux machine roster"*, and no amount of tree lookup separates
/// them. What separates them is that one is framed as something to do.
const REFERENCE_INVOCATION_CUES: &[&str] = &[
    "run", "runs", "running", "ran", "type", "types", "typing", "try", "tries", "trying", "use",
    "uses", "using", "execute", "executes", "executing", "invoke", "invokes", "invoking", "call",
    "calls", "calling", "launch", "launches", "launching", "start", "starts", "enter", "enters",
    "issue", "issues", "do", "does", "want", "wants", "simply", "just", "via", "$", ">", "%",
];

/// English function words a bare-prose reference STOPS at, so the judged
/// candidate is the command path and not the rest of the sentence.
///
/// Closed and small on purpose, and every entry is a word that can never be
/// a darkmux verb token — guarded mechanically against the LIVE verb index
/// by `no_stop_word_collides_with_a_real_verb_token`, so a verb added later
/// fails a test rather than silently shortening every candidate.
const REFERENCE_STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "am", "to", "of", "in", "on", "at",
    "by", "for", "from", "with", "into", "onto", "and", "or", "but", "so", "if", "then", "when",
    "while", "because", "it", "its", "this", "that", "these", "those", "there", "here", "you",
    "your", "we", "our", "they", "their", "i", "my", "will", "would", "could", "should", "may",
    "might", "must", "has", "have", "had", "did", "not", "no", "as", "than",
];

/// How many pure-punctuation tokens [`token_is_framed`] steps back over
/// looking for a cue. An em dash is its own token — *"Run — darkmux
/// machine roster"* — and so is a lone bullet; two is enough for those
/// without letting a cue five words back frame something it does not
/// govern.
const MAX_PUNCTUATION_TOKENS_BEFORE_A_CUE: usize = 2;

/// `true` iff `word` could be part of a verb path: a long or short flag, or
/// a token of nothing but [`is_command_id_char`]. Punctuation ends a
/// candidate, which is what makes a sentence boundary a path boundary.
///
/// Case is NOT a signal here, and an earlier draft that rejected any
/// capitalized word was measurably worse in both directions: it missed
/// *"try darkmux machine Roster"* (the candidate collapsed to the valid
/// group node `machine`) while buying nothing the case-INSENSITIVE stop
/// list below does not already buy.
fn looks_like_a_verb_token(word: &str) -> bool {
    !word.is_empty() && (word.starts_with('-') || word.chars().all(is_command_id_char))
}

/// `true` iff `word` is a [`REFERENCE_STOP_WORDS`] entry, case-insensitively
/// — a line break or a list item routinely capitalizes one (*"run darkmux
/// machine\nAnd then check the board"*), and a case-sensitive match would
/// let the candidate run on into the sentence.
fn is_a_reference_stop_word(word: &str) -> bool {
    REFERENCE_STOP_WORDS.iter().any(|stop| word.eq_ignore_ascii_case(stop))
}

/// `true` iff token `k` is FRAMED as something to type rather than
/// something to read: the seat put decoration against it, or an invocation
/// cue governs it.
///
/// **This is the single judgement that replaced a hardcoded list of
/// filesystem roots** (#2050 sweep finding 4). That list tried to name the
/// single-segment absolute paths a seat may legitimately quote — `/tmp`,
/// `/etc`, `/var` — and the set is OPEN, so it missed darkmux's own
/// container mounts: `"the sandbox is mounted at /workspace"` and
/// `"bookkeeping goes to /darkmux-out"` (`darkmux_types::workdir`, the
/// literal paths this project mounts) were each discarded whole, as were
/// `/scratch` and `/Makefile`. "Where does my code live inside the
/// runtime?" is an ordinary radio question.
///
/// Framing closes that without an allowlist, because the distinction is
/// structural rather than lexical: *"mounted at /workspace"* is a location,
/// *"run /workspace"* and `` `/workspace` `` are claims about something to
/// type. The residual cost, unchanged from before this change and stated
/// so it stays a decision: a BACKTICKED single-segment path — `` `/tmp` ``
/// — still reads as a command and costs the reply.
fn token_is_framed(chars: &[char], tokens: &[Token], k: usize) -> bool {
    let token = &tokens[k];
    if (token.start > 0 && chars[token.start - 1] == DECORATION_MARK)
        || chars.get(token.end).is_some_and(|c| *c == DECORATION_MARK)
    {
        return true;
    }
    let mut skipped = 0;
    let mut j = k;
    while j > 0 {
        j -= 1;
        let core = tokens[j].text.trim_matches(|c: char| !c.is_alphanumeric() && !matches!(c, '$' | '>' | '%'));
        if core.is_empty() {
            // A pure-punctuation token (an em dash, a bullet) does not
            // break the cue's reach, but only a couple of them.
            skipped += 1;
            if skipped > MAX_PUNCTUATION_TOKENS_BEFORE_A_CUE {
                return false;
            }
            continue;
        }
        return REFERENCE_INVOCATION_CUES.iter().any(|cue| core.eq_ignore_ascii_case(cue));
    }
    false
}

/// The bare id of a `/`-prefixed COMMAND-SHAPED token, or `None`.
///
/// A candidate is a token that STARTS with `/`, followed by at least one
/// [`is_command_id_char`], NOT followed by a second path segment or a file
/// extension. That last clause is what keeps absolute paths out —
/// `/Users/kain/.darkmux/config.json` and `/etc/hosts` stop at the second
/// `/`, `/tmp/x.json` at the `.` before an alphanumeric — while a
/// sentence-final `/machine.` still yields `machine`, because a `.` with
/// no alphanumeric after it is punctuation, not an extension.
///
/// Requiring the token to START with `/` is what keeps
/// `https://example.com`, `and/or`, `9/12` and `~/.darkmux/profiles.json`
/// out without special-casing any of them; decoration is already
/// whitespace by the time this runs, so `(/machine)` and `*/machine*` do
/// start tokens (#2050 sweep finding 5).
fn slash_candidate_id(token: &str) -> Option<String> {
    let chars: Vec<char> = token.chars().collect();
    if chars.first() != Some(&'/') {
        return None;
    }
    let mut end = 1;
    while end < chars.len() && is_command_id_char(chars[end]) {
        end += 1;
    }
    if end == 1 {
        return None;
    }
    let next = chars.get(end).copied();
    let is_a_path =
        next == Some('/') || (next == Some('.') && chars.get(end + 1).is_some_and(|c| c.is_ascii_alphanumeric()));
    (!is_a_path).then(|| chars[1..end].iter().collect())
}

/// `true` iff any `/`-shaped candidate names something the user cannot run
/// here.
///
/// The two branches are ORDERED, and the order is the point. An ADVERTISED
/// id is a command reference however it was written, and gets the surface
/// check — telling a CLI user to type `/tmp` is the #1861 defect whether or
/// not `tmp` also names a directory. Anything else has to be
/// [`token_is_framed`] before it counts as a reference at all.
fn slash_reference_is_unrunnable(
    chars: &[char],
    tokens: &[Token],
    catalog: &[CatalogEntry],
    surface: RadioSurface,
) -> bool {
    tokens.iter().enumerate().any(|(k, token)| {
        let Some(id) = slash_candidate_id(&token.text) else { return false };
        if is_an_advertised_id(&id, catalog) {
            // Valid on the panel only: the CLI has no shell that runs
            // `/anything` — a routed id is executed by radio itself.
            surface != RadioSurface::Panel
        } else {
            token_is_framed(chars, tokens, k)
        }
    })
}

/// `true` iff `id` names one of `catalog`'s advertised commands.
/// Case-insensitive, matching `acp_panel::route_command`'s own rule — a
/// mixed-case spelling of a real id is a real command, not an invention.
fn is_an_advertised_id(id: &str, catalog: &[CatalogEntry]) -> bool {
    catalog.iter().any(|c| c.id.eq_ignore_ascii_case(id))
}

/// `Some(rest)` iff `span` opens with the binary name followed by
/// whitespace, matched case-INSENSITIVELY (#2050 sweep finding 3).
/// Sentence-initial capitalization is ordinary for a model and nothing in
/// the persona says the name is always lowercase; a case-sensitive
/// `strip_prefix("darkmux ")` sent `Darkmux machine roster` straight to the
/// bare-id test, which a multi-word span can never trip.
///
/// Only the NAME folds. The verb path after it stays case-sensitive,
/// because clap's subcommand matching is: `darkmux Machine status` really
/// does fail.
fn strip_darkmux_prefix(span: &str) -> Option<&str> {
    let (head, rest) = span.split_at_checked(7)?;
    if !head.eq_ignore_ascii_case("darkmux") {
        return None;
    }
    let trimmed = rest.trim_start();
    (trimmed.len() < rest.len()).then_some(trimmed)
}

/// `true` iff `word` is the first segment of some path in the tree — the
/// candidacy gate for bare prose. *"darkmux uses LMStudio"* fails it and is
/// left alone; *"darkmux machine roster"* passes it and gets judged.
///
/// The gate's cost, pinned as a test rather than left implicit: a
/// WHOLLY-invented top-level verb in bare prose — *"run darkmux telepathy"*
/// — is not a candidate, because nothing distinguishes it from the English
/// word that follows `darkmux` in *"use darkmux instead of driving lms by
/// hand"*. Quoted, the same text is caught.
fn is_a_top_level_verb_token(word: &str, verb_index: &[crate::radio_index::VerbEntry]) -> bool {
    verb_index.iter().any(|v| v.path.split(' ').next() == Some(word))
}

/// `true` iff `path` names a node of the tree — a leaf, or a group with
/// children under it.
///
/// Accepted in bare prose and NOT in a code span, which is the same
/// instruction-vs-description split the rest of this module runs on:
/// *"you can use darkmux machine to manage models"* describes a real part
/// of the CLI, while `` `darkmux machine` `` hands over something to type
/// that exits with a usage error.
fn names_a_verb_node(path: &str, verb_index: &[crate::radio_index::VerbEntry]) -> bool {
    verb_index.iter().any(|v| v.path == path || v.path.starts_with(&format!("{path} ")))
}

/// `true` iff some framed `darkmux ...` reference names something the
/// binary will not accept (#2050 sweep findings 1 and 2).
///
/// The proven miss this closes: *"You can run darkmux machine roster to see
/// your crew from here."* — `machine roster` does not exist, and before
/// this the unquoted half of the scan looked ONLY for `/`-prefixed tokens,
/// so the sentence shipped verbatim. The tokens arrive decoration-aware, so
/// the same sentence with a stray backtick anywhere in front of it is the
/// identical input.
fn prose_darkmux_reference_is_unrunnable(
    chars: &[char],
    tokens: &[Token],
    verb_index: &[crate::radio_index::VerbEntry],
) -> bool {
    for (k, token) in tokens.iter().enumerate() {
        let name = token.text.trim_end_matches(is_trailing_punctuation);
        if !name.eq_ignore_ascii_case("darkmux") || !token_is_framed(chars, tokens, k) {
            continue;
        }
        let mut candidate: Vec<&str> = Vec::new();
        for following in &tokens[k + 1..] {
            let core = following.text.trim_end_matches(is_trailing_punctuation);
            if core.is_empty() || !looks_like_a_verb_token(core) || is_a_reference_stop_word(core) {
                break;
            }
            candidate.push(core);
            // Trailing punctuation ENDED a clause, and a clause boundary is
            // a command-path boundary (finding 1).
            //
            // No length cap: an earlier revision carried one and mutation
            // showed it could not change any verdict, because
            // `verb_path_matches` validates on a PREFIX — a longer
            // candidate whose first words name a real leaf stays valid, and
            // one whose first words do not was already invalid. An
            // unprovable constant is worse than no constant.
            if core.len() < following.text.len() {
                break;
            }
        }
        let Some(first) = candidate.first() else { continue };
        if !first.starts_with('-') && !is_a_top_level_verb_token(first, verb_index) {
            continue;
        }
        let path = candidate.join(" ");
        if !darkmux_reference_is_valid(&path, verb_index) && !names_a_verb_node(&path, verb_index) {
            return true;
        }
    }
    false
}
/// `true` iff `span` is EXACTLY an advertised command id, with no
/// `/` and no `darkmux ` in front of it (#2050, third measurement).
///
/// A bare id is runnable on NEITHER surface, so this needs no `surface`
/// argument. The panel's own parser requires the slash
/// (`acp_panel::parse_command` returns `None` without it) and the CLI has
/// no such clap subcommand — `darkmux review` exits with `unrecognized
/// subcommand 'review'` and helpfully suggests `serve`. The seat produced
/// exactly this, measured live:
///
/// > ``Run `review` to execute the code review pipeline against your
/// > current working-tree diff.``
///
/// which the detector passed, because a bare id is neither of the two
/// shapes the persona instructs the seat to write, so nothing looked at
/// it. The runnable forms are `darkmux mission launch review` and
/// `/review`, and both are still accepted by the arms above.
///
/// **The one reading under which it is not wrong**, checked and rejected:
/// `darkmux radio "review"` DOES route, so "say `review` to radio" would
/// be true. But the sentence carries no such framing, the user is already
/// talking to radio, and this project already picked the canonical CLI
/// form in `render_surface_block`. So the text is wrong, not merely
/// ambiguous.
///
/// **Deliberately requires the backticks.** Only a whole inline-code span
/// is tested, never a word in prose — an id is frequently an ordinary
/// English word (`review`), and the catalog is operator-authored, so
/// scanning prose for ids would discard replies wholesale. "I'll review
/// your diff" and "the review config" are untouched; only `` `review` ``
/// is a claim about something to type.
///
/// The residual false positive, stated: a legitimate MENTION in backticks
/// — "`review` takes no arguments" — is read as an instruction and costs
/// the reply. Accepted on the same ordering as the rest of this module:
/// the fallback is honest and unhelpful, and the alternative is telling a
/// user to run something that errors.
fn is_a_bare_catalog_id(span: &str, catalog: &[CatalogEntry]) -> bool {
    let id = span.trim();
    !id.is_empty() && catalog.iter().any(|c| c.id.eq_ignore_ascii_case(id))
}

/// `true` iff `rest` (the text right after `"darkmux "`) names something
/// the binary will actually accept.
///
/// Two ways to qualify. Either it names a real leaf verb at a word
/// boundary (`machine status --json` matches the `machine status` leaf;
/// `machine statuses` does not — the boundary check is what stops a real,
/// shorter path from validating a longer INVENTED one that merely shares a
/// prefix); or it asks for `--help` / `--version` at any node of the tree.
/// The second case exists because the verb index holds LEAVES only, so
/// `darkmux --help` — the one command guaranteed real on every build —
/// and `darkmux machine --help` (`machine` only groups subverbs) would
/// otherwise both be rejected as invented.
fn darkmux_reference_is_valid(rest: &str, verb_index: &[crate::radio_index::VerbEntry]) -> bool {
    if verb_index.iter().any(|v| verb_path_matches(&v.path, rest)) {
        return true;
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    let verb_words: Vec<&str> = words.iter().copied().take_while(|w| !w.starts_with('-')).collect();
    let asks_for_help =
        words[verb_words.len()..].iter().any(|w| matches!(*w, "--help" | "-h" | "--version" | "-V"));
    if !asks_for_help {
        return false;
    }
    let prefix = verb_words.join(" ");
    prefix.is_empty() || verb_index.iter().any(|v| verb_path_matches(&v.path, &prefix) || v.path.starts_with(&format!("{prefix} ")))
}

/// `true` iff `content` names `path` at a word boundary — `content =
/// "machine status --json"`, `path = "machine status"` matches; `content =
/// "machine statuses"` does not.
fn verb_path_matches(path: &str, content: &str) -> bool {
    content == path || content.strip_prefix(path).is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// (#1784/#1861) The full introspected `darkmux` verb tree, walked from
/// clap at call time — the same tree [`render_help_block`] renders into
/// the grounding bundle and [`sanitize_command_references`] validates
/// against. Factored out so both read the SAME index rather than building
/// two clap trees that could drift from each other.
fn command_verb_index() -> Vec<crate::radio_index::VerbEntry> {
    use clap::CommandFactory;
    crate::radio_index::build_verb_index(&crate::cli::Cli::command())
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
    surface: RadioSurface,
    call: &mut AnswererCall<'_>,
) -> Result<AnswerOutcome> {
    let grounding = assemble_grounding(text, catalog, shelf, cwd, scope, surface);
    let message = build_answer_message(text, &grounding);
    let raw = call(&message)?;
    let reply = raw.trim().to_string();
    // (#1861 defects 1 + 2, rebuilt by #2050) The mechanical backstop.
    // A reply that names a command the operator cannot actually run here
    // is discarded WHOLE, never edited: every attempt to salvage the rest
    // of it shipped broken prose — first a mid-sentence stub, then (after
    // sentence-level suppression) a fluent, future-tense remainder that
    // read as though the command had already started. See
    // `names_an_unrunnable_command`'s own doc for both transcripts.
    //
    // An Err — not an empty `Ok` — because both callers
    // (`src/radio_cli.rs`, `src/acp.rs`'s `answer_no_slash_refusal`)
    // already treat an Err as "print the plain refusal + the live command
    // listing". That is the measurably good output: on `review this
    // branch` against 3.7.1 the seat failed outright and that same
    // fallback produced the most useful line of the whole run.
    if reply.is_empty() {
        anyhow::bail!("the answering seat returned no usable text");
    }
    if names_an_unrunnable_command(&reply, catalog, &command_verb_index(), surface) {
        anyhow::bail!(
            "the answering seat's reply named a command that cannot be run on this surface, \
             and the rest of the reply was written around it"
        );
    }
    // (#1698 Packet B2 gate) The bare LISTING, not `not_a_command_message`
    // — appending "darkmux acp doesn't recognize that as a command" under
    // an answer that just helpfully named `/pr-list` tells the operator
    // their message failed, immediately after RADIO answered it.
    // (#1861 defect 1) Panel-only: the listing is a slash-id list, which
    // is meaningless — and exactly the shape of the defect — on the CLI.
    let listing = crate::acp_panel::command_listing(&crate::acp_panel::list_panel_commands());
    let rendered = if surface == RadioSurface::Panel && answer_references_a_command(&reply, catalog) && !listing.is_empty()
    {
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

/// Substitute every placeholder in the `radio-host` persona template.
/// Split out of [`dispatch_answerer_call_with`] as a PURE function so the
/// substitution is assertable on the FINISHED text (#1861): the persona
/// golden below pins the TEMPLATE, and a template golden structurally
/// cannot catch a substitution that stops firing and ships a raw
/// `{{surface_instructions}}` to the model. Takes `persona` rather than
/// loading it, so a test can pin the SHIPPED template without resolving an
/// operator's own `~/.darkmux/crew/roles/radio-host.md` override.
fn substitute_persona(persona: &str, humor: u8, surface: RadioSurface) -> String {
    persona
        .replace("{{humor}}", &humor.to_string())
        .replace("{{surface_instructions}}", &surface_instructions(surface))
}

pub fn dispatch_answerer_call_with(
    user_message: &str,
    overrides: &AnswererOverrides,
    surface: RadioSurface,
) -> Result<String> {
    let persona = crate::crew::loader::role_prompt("radio-host").ok_or_else(|| {
        anyhow::anyhow!("radio-host role has no readable .md persona template — cannot dispatch the answering seat")
    })?;
    let humor = overrides.humor.unwrap_or_else(darkmux_types::config_access::radio_humor);
    let system_prompt = substitute_persona(&persona, humor, surface);
    let profile_name = resolved_answerer_profile(overrides);

    let opts = crate::crew::dispatch::DispatchOpts {
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None, // (#2480)
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
    surface: RadioSurface,
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
    answer(text, catalog, shelf, cwd, scope, surface, &mut |m: &str| {
        dispatch_answerer_call_with(m, overrides, surface)
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
        CatalogEntry { id: id.to_string(), description: description.to_string(), hint: None, accepts_args: true }
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
            surface: Some("small".to_string()),
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
            surface: Some("small".to_string()),
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
            machine: None,
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
            RadioSurface::Panel,
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
            RadioSurface::Panel,
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
        let outcome =
            answer("anything mergeable?", &fixture_catalog(), &shelf, Path::new("/tmp"), GroundingScope::Full, RadioSurface::Panel, &mut call)
                .unwrap();
        assert!(outcome.rendered.len() > outcome.text.len(), "the listing must be appended: {outcome:?}");
    }

    #[test]
    fn answer_not_referencing_a_command_stays_bare() {
        let mut call = |_msg: &str| -> Result<String> { Ok("darkmux is a local-AI orchestrator CLI.".to_string()) };
        let shelf = ArtifactShelf::default();
        let outcome = answer(
            "is this darkmux?",
            &fixture_catalog(),
            &shelf,
            Path::new("/tmp"),
            GroundingScope::Full,
            RadioSurface::Panel,
            &mut call,
        )
        .unwrap();
        assert_eq!(outcome.text, outcome.rendered, "no command referenced — no listing appended: {outcome:?}");
    }

    #[test]
    fn answer_dispatch_error_propagates_as_err() {
        let mut call = |_msg: &str| -> Result<String> { Err(anyhow::anyhow!("no model loaded")) };
        let shelf = ArtifactShelf::default();
        let result =
            answer("is this darkmux?", &fixture_catalog(), &shelf, Path::new("/tmp"), GroundingScope::Full, RadioSurface::Panel, &mut call);
        assert!(result.is_err(), "a dispatch failure must propagate, not be swallowed into a bogus answer");
    }

    // ── sanitize_command_references (#1861 defects 1 + 2) ────────────────

    fn fixture_verb_index() -> Vec<crate::radio_index::VerbEntry> {
        vec![
            crate::radio_index::VerbEntry {
                path: "machine status".to_string(),
                summary: "Show loaded models.".to_string(),
                options: Vec::new(),
            },
            crate::radio_index::VerbEntry {
                path: "mission launch".to_string(),
                summary: "Launch a mission.".to_string(),
                options: vec!["<config_id>".to_string()],
            },
        ]
    }

    /// Every detector test reads the same way: does this reply name
    /// something the operator cannot run HERE? `true` costs the whole
    /// reply (see `names_an_unrunnable_command`'s doc), so the false
    /// cases below are load-bearing, not filler.
    fn detects(reply: &str, surface: RadioSurface) -> bool {
        names_an_unrunnable_command(reply, &fixture_catalog(), &fixture_verb_index(), surface)
    }

    #[test]
    fn detects_an_invented_slash_command_on_the_panel_surface() {
        // #1861 defect 2: `/machine` was never in the catalog (only
        // `pr-list` and `review` are). A real check must catch what the
        // persona's own "never invent" rule cannot prove.
        assert!(detects("Run `/machine` to see your crew.", RadioSurface::Panel));
    }

    #[test]
    fn detects_a_real_slash_command_on_the_cli_surface() {
        // #1861 defect 1: `/pr-list` IS a real catalog id, but the CLI has
        // no shell that runs `/anything` — surface-inappropriate, not
        // invented, and must still not ship.
        assert!(detects("Run `/pr-list` to see them.", RadioSurface::Cli));
    }

    #[test]
    fn leaves_a_real_slash_command_alone_on_the_panel_surface() {
        // The inverted case: a LEGITIMATE reference on the surface it is
        // actually valid on must pass — a detector that fired on
        // everything would satisfy the two tests above just as happily,
        // and would replace every panel answer with the refusal.
        assert!(!detects("Run `/pr-list` to see them.", RadioSurface::Panel));
    }

    #[test]
    fn leaves_a_real_darkmux_verb_alone_on_either_surface() {
        for surface in [RadioSurface::Cli, RadioSurface::Panel] {
            assert!(
                !detects("Run `darkmux machine status` to see what's loaded.", surface),
                "a real darkmux verb must not trip the fallback on {surface:?}"
            );
        }
    }

    #[test]
    fn accepts_a_real_darkmux_verb_with_a_placeholder_argument() {
        assert!(!detects("Run `darkmux mission launch <config_id>` to start it.", RadioSurface::Cli));
    }

    #[test]
    fn detects_an_invented_darkmux_subcommand_on_either_surface() {
        // `machine roster` is exactly issue #1861's own example of a
        // doubly-invented reference (no such subcommand exists at all).
        for surface in [RadioSurface::Cli, RadioSurface::Panel] {
            assert!(
                detects("Run `darkmux machine roster` to see your crew.", surface),
                "an invented subcommand must be caught on {surface:?}"
            );
        }
    }

    #[test]
    fn leaves_non_command_backtick_content_alone() {
        assert!(!detects("Set `n_ctx` and `radio.humor` as you like.", RadioSurface::Cli));
    }

    #[test]
    fn answer_falls_back_rather_than_shipping_an_invented_command() {
        let mut call = |_msg: &str| -> Result<String> { Ok("Run `/machine` to see your crew.".to_string()) };
        let shelf = ArtifactShelf::default();
        let err = answer(
            "how do I see my crew?",
            &fixture_catalog(),
            &shelf,
            Path::new("/tmp"),
            GroundingScope::Full,
            RadioSurface::Panel,
            &mut call,
        )
        .expect_err("a reply naming an invented command must never reach the operator");
        assert!(format!("{err:#}").contains("cannot be run on this surface"), "{err:#}");
    }

    #[test]
    fn answer_never_appends_the_slash_listing_on_the_cli_surface() {
        // #1698 Packet B2 appends the panel-command listing under an
        // answer that names a command — but that listing is itself a list
        // of `/id`s, meaningless on a surface with no shell that runs
        // `/anything` (#1861 defect 1). The reply here names a REAL verb,
        // so it is shipped rather than discarded, and the question is only
        // whether the listing gets bolted on.
        let mut call =
            |_msg: &str| -> Result<String> { Ok("Run `darkmux machine status` to see them.".to_string()) };
        let shelf = ArtifactShelf::default();
        let outcome = answer(
            "anything loaded?",
            &fixture_catalog(),
            &shelf,
            Path::new("/tmp"),
            GroundingScope::Full,
            RadioSurface::Cli,
            &mut call,
        )
        .unwrap();
        assert_eq!(
            outcome.text, outcome.rendered,
            "the panel-only slash listing must never append on the CLI surface: {outcome:?}"
        );
    }

    // ── the path/command boundary (#1861 review blocker 1) ───────────────
    //
    // Now MORE load-bearing than when it was written: a false positive
    // used to cost one span, and since #2050 costs the entire answer.

    #[test]
    fn leaves_a_backticked_absolute_path_alone() {
        // A blanket `starts_with('/')` rule treated every absolute path
        // the seat quoted as an invented command — and the grounding
        // bundle is full of them.
        for surface in [RadioSurface::Cli, RadioSurface::Panel] {
            assert!(
                !detects("Your config lives at `/Users/kain/.darkmux/config.json` — edit it there.", surface),
                "an absolute path is not a command reference ({surface:?})"
            );
        }
    }

    #[test]
    fn a_path_next_to_an_invented_command_does_not_rescue_the_reply() {
        // The reply still goes, because the invented verb is in it — but
        // the PATH is not what caught it. Paired with the test above,
        // which proves the path alone is inert.
        assert!(detects(
            "Run `darkmux machine roster` then check `/Users/me/.darkmux/config.json`.",
            RadioSurface::Cli
        ));
    }

    #[test]
    fn leaves_a_home_relative_path_alone() {
        assert!(!detects("The registry is `~/.darkmux/profiles.json` on this machine.", RadioSurface::Cli));
        // The SINGLE-segment case is the one that actually needs `~` kept
        // out of `REFERENCE_DECORATION`: every longer path stops at its
        // second `/` or its extension regardless, while `~/runs` would
        // become a word-initial `/runs` and read as an invented command.
        assert!(!detects("Your runs land in `~/runs` under the darkmux root.", RadioSurface::Cli));
    }

    #[test]
    fn leaves_urls_dates_and_prose_slashes_alone() {
        assert!(!detects(
            "See https://darkmux.com/docs, filed 9/12, and either read/write works.",
            RadioSurface::Cli
        ));
    }

    #[test]
    fn accepts_darkmux_help_and_version() {
        // The verb index holds LEAVES, so a bare top-level flag matches
        // nothing in it — yet `darkmux --help` is the one command
        // guaranteed real on every build.
        for reply in [
            "Run `darkmux --help` to see everything.",
            "Run `darkmux -h` for the list.",
            "Check `darkmux --version` first.",
            "Try `darkmux machine --help` for that group.",
        ] {
            assert!(!detects(reply, RadioSurface::Cli), "help/version is real at every node: {reply}");
        }
    }

    #[test]
    fn still_rejects_an_invented_group_asking_for_help() {
        assert!(detects("Try `darkmux telepathy --help`.", RadioSurface::Cli));
    }

    // ── the unbackticked half of the backstop (#1861 review) ─────────────

    #[test]
    fn detects_a_bare_unbackticked_invented_slash_command() {
        // Issue #1861's own wording. A backtick-only scan never saw it.
        assert!(detects("Try running /machine to see them.", RadioSurface::Panel));
    }

    #[test]
    fn detects_a_bare_slash_command_with_trailing_punctuation() {
        // Sentence punctuation must be trimmed BEFORE the shape test, or
        // `/machine.` fails the character check and sails through.
        //
        // The cue is load-bearing and was NOT here before the sweep: an
        // unadvertised slash token now has to be FRAMED (see
        // `token_is_framed`), which is what stops `"mounted at /workspace"`
        // discarding a good reply. This test's original text — "The command
        // is /machine." — is consequently no longer judged. That is the
        // trade, taken deliberately, not an oversight.
        assert!(detects("Run /machine.", RadioSurface::Panel));
        assert!(detects("Run `/machine`, then look at the board.", RadioSurface::Panel));
    }

    #[test]
    fn leaves_a_bare_real_slash_command_alone_on_the_panel_surface() {
        assert!(!detects("Try running /pr-list to see them.", RadioSurface::Panel));
    }

    #[test]
    fn leaves_a_fenced_code_block_alone() {
        // Documented coverage limit: a fence is quoted material, so it is
        // excluded — including an id that would have tripped the detector
        // in prose. Pinned so the limit is a decision, not a surprise.
        assert!(!detects("Like this:\n```\n/machine\n```\nThat is the shape.", RadioSurface::Cli));
    }

    // ── (#2050, third measurement) a bare catalog id is not an invocation ──

    #[test]
    fn detects_a_bare_catalog_id_named_as_something_to_run() {
        // Measured live on `355ebda8` for `run the review pipeline`:
        //   "Run `review` to execute the code review pipeline against your
        //    current working-tree diff. It will report any bugs it finds."
        // followed by, in the operator's terminal:
        //   $ darkmux review
        //   error: unrecognized subcommand 'review'
        //     tip: a similar subcommand exists: 'serve'
        //
        // A bare id is runnable on NEITHER surface: the panel's parser
        // requires the slash, and there is no such clap subcommand.
        for surface in [RadioSurface::Cli, RadioSurface::Panel] {
            assert!(
                detects("Run `review` to execute the code review pipeline.", surface),
                "a bare catalog id is not an invocation on {surface:?}"
            );
        }
    }

    #[test]
    fn accepts_the_runnable_forms_of_the_same_catalog_command() {
        // The inverted case, and the one that matters most: if these were
        // caught, the seat could never name the command CORRECTLY, and
        // every reply about `review` would collapse to the fallback.
        assert!(
            !detects("Run `darkmux mission launch review` to start it.", RadioSurface::Cli),
            "the CLI's own canonical form must pass"
        );
        assert!(
            !detects("Run `/review` to start it.", RadioSurface::Panel),
            "the panel's own canonical form must pass"
        );
    }

    #[test]
    fn leaves_a_catalog_id_alone_when_it_is_ordinary_prose() {
        // The backticks are the whole signal. An id is often an ordinary
        // English word and the catalog is operator-authored, so scanning
        // prose for ids would discard replies wholesale.
        for reply in [
            "I'll review your working-tree diff for bugs.",
            "The review config takes no arguments.",
            "Your review ran twice yesterday.",
        ] {
            assert!(!detects(reply, RadioSurface::Cli), "prose must never trip the fallback: {reply}");
        }
    }

    #[test]
    fn leaves_a_backticked_span_that_is_not_a_catalog_id_alone() {
        // Only an EXACT id is a candidate — a config key or a near-miss
        // word must not be read as a command.
        for reply in ["Set `reviewer` in your profile.", "Check `radio.humor` first."] {
            assert!(!detects(reply, RadioSurface::Cli), "{reply}");
        }
    }

    // ── (#2050 sweep) decoration is not the detector's input format ──────
    //
    // The persona never requires backticks — rule 2 says "name it exactly
    // ... in the syntax that's actually real", which is a constraint on the
    // COMMAND, not on the markup around it. Everything below feeds crafted
    // reply strings straight to the predicate; no model runs under test.

    #[test]
    fn detects_an_invented_darkmux_verb_written_as_bare_prose() {
        // Sweep finding 1. `machine roster` does not exist, and before this
        // the outside-a-backtick half of the scan looked ONLY for
        // `/`-prefixed tokens — so this shipped verbatim.
        for surface in [RadioSurface::Cli, RadioSurface::Panel] {
            assert!(
                detects("You can run darkmux machine roster to see your crew from here.", surface),
                "an invented verb in bare prose must be caught on {surface:?}"
            );
        }
    }

    #[test]
    fn a_stray_backtick_does_not_blind_the_detector() {
        // Sweep finding 2. The old scan alternated in/out of code by
        // backtick COUNT PARITY, so one unmatched backtick moved every
        // later reference into an "inside" part that was only ever tested
        // for exact equality against a catalog id — which a multi-word span
        // never satisfies. This seat has three documented rounds of
        // producing malformed markdown, and the natural trigger is a
        // backtick typed where an apostrophe belongs.
        assert!(
            detects("It`s set up — run darkmux machine roster to check.", RadioSurface::Cli),
            "an apostrophe typo must not hide the invented verb after it"
        );
        assert!(
            detects("Check `/Users/kain/.darkmux — then try running /machine.", RadioSurface::Panel),
            "an unclosed span must not hide the invented slash command after it"
        );
        assert!(
            detects("- first `bullet\n- then run darkmux machine roster\n", RadioSurface::Cli),
            "a formatting slip in an early bullet must not hide a later bullet's bad command"
        );
    }

    #[test]
    fn detects_a_capitalized_darkmux_reference() {
        // Sweep finding 3. Sentence-initial capitalization is ordinary for
        // a model, and nothing in the persona says the binary name is
        // always lowercase. A case-sensitive `strip_prefix("darkmux ")`
        // dropped the span straight through to the bare-id test, which a
        // multi-word span can never trip.
        assert!(detects("Run `Darkmux machine roster` to see your crew.", RadioSurface::Cli));
        assert!(detects("Try DARKMUX machine roster for that.", RadioSurface::Cli));
        // Span-ONLY: `telepathy` is not a real top-level verb, so the bare-
        // prose pass does not treat this as a candidate at all. Only the
        // quoted half can catch it, and only if the name folds — which is
        // what keeps this assertion honest about where the fix lives.
        assert!(detects("Try `Darkmux telepathy --help`.", RadioSurface::Cli));
    }

    #[test]
    fn accepts_a_capitalized_real_darkmux_verb() {
        // The inverted case for finding 3: case-insensitivity must not turn
        // a REAL verb into a detection. Only the binary name is folded —
        // the verb path stays case-sensitive, because clap's own
        // subcommand matching is.
        assert!(!detects("Run `Darkmux machine status` to see what's loaded.", RadioSurface::Cli));
    }

    #[test]
    fn detects_a_decorated_bare_slash_command() {
        // Sweep finding 5. The word-boundary test required literal
        // whitespace before the `/`, so a parenthetical or an emphasis
        // marker walked straight past it.
        for reply in [
            "See your crew (/machine) for details.",
            "Run */machine* to see your crew.",
            "Check [/machine] for that.",
            "Use \"/machine\" instead.",
        ] {
            assert!(detects(reply, RadioSurface::Panel), "decoration must not hide a slash command: {reply}");
        }
    }

    // ── the false-positive direction: what the detector must NOT eat ─────

    #[test]
    fn leaves_an_unframed_single_segment_path_alone() {
        // Sweep finding 4, and the relaxation this change pays for the
        // three tightenings above with. "Explain where a file lives" is an
        // ordinary answer shape for this seat, and a discarded reply is
        // INVISIBLE — it looks exactly like an ordinary refusal.
        //
        // The first revision of this fix used a hardcoded list of
        // filesystem roots, and the review sweep proved the set is OPEN by
        // naming darkmux's OWN container mounts — `/workspace` and
        // `/darkmux-out` (`darkmux_types::workdir`) — plus `/scratch` and
        // `/Makefile`, none of which any allowlist would have held. The
        // rule is structural instead: an unadvertised slash token is a
        // command reference only when something FRAMES it as one.
        for reply in [
            "The sandbox is mounted at /workspace in the container.",
            "Bookkeeping goes to /darkmux-out inside the container.",
            "It writes to /scratch on this box.",
            "Read /Makefile at the root.",
            "Logs are written to /tmp for now.",
            "It reads /etc for that.",
            "The daemon runs as /root here.",
            "Binaries land in /usr for a brew install.",
        ] {
            for surface in [RadioSurface::Cli, RadioSurface::Panel] {
                assert!(!detects(reply, surface), "an unframed path is not a command ({surface:?}): {reply}");
            }
        }
    }

    #[test]
    fn a_framed_single_segment_path_is_still_judged_and_that_is_the_residual_cost() {
        // The other side of the rule above, stated as a decision rather
        // than discovered later: framing cannot tell `` `/tmp` `` from
        // `` `/machine` ``, so a backticked single-segment path still
        // costs the reply. Unchanged from before this PR (it was the
        // module's original documented limit) and narrower than it was,
        // since the unframed form — by far the commoner one — is now free.
        assert!(detects("Your logs live in `/tmp` for now.", RadioSurface::Panel));
    }

    #[test]
    fn an_unframed_invented_slash_command_is_not_judged() {
        // The false-negative this rule buys the false-positive fix with.
        // Both halves in one place so the boundary is visible: same id,
        // same surface, framing is the only difference.
        assert!(!detects("The command is /machine.", RadioSurface::Panel));
        assert!(detects("Run /machine.", RadioSurface::Panel));
        assert!(detects("It is `/machine`.", RadioSurface::Panel));
    }

    #[test]
    fn leaves_a_multi_segment_path_alone_whatever_its_first_segment() {
        // The path SHAPE guard, independent of anything lexical: a second
        // segment or a file extension is what makes these paths, and
        // nothing else has to know their names. None of them carries an
        // invocation cue either, so this and
        // `leaves_an_unframed_single_segment_path_alone` protect the same
        // replies by two different mechanisms — which is why both exist.
        for reply in [
            "The build lands in /workspace/darkmux/target/debug/darkmux here.",
            "It wrote /scratch/run-42/trajectory.jsonl for that run.",
            "Read `/data/darkmux/flow.jsonl` for the raw records.",
            "The manifest is /manifest.json in the run directory.",
        ] {
            for surface in [RadioSurface::Cli, RadioSurface::Panel] {
                assert!(!detects(reply, surface), "a path is not a command reference ({surface:?}): {reply}");
            }
        }
    }

    #[test]
    fn an_underscore_in_a_command_id_survives_the_decoration_strip() {
        // `_` is deliberately NOT a decoration character. Markdown uses it
        // for emphasis, but it is also legal inside a command id, and
        // stripping it would split `/pr_list` into `/pr` plus a stray word
        // — turning a REAL command into an invented one, which costs the
        // whole reply.
        let catalog = vec![entry("pr_list", "List open pull requests.")];
        assert!(
            !names_an_unrunnable_command("Try /pr_list for that.", &catalog, &fixture_verb_index(), RadioSurface::Panel),
            "an underscored id must survive decoration stripping intact"
        );
    }

    #[test]
    fn an_advertised_id_is_judged_as_a_command_however_it_is_written() {
        // Ordering pin for finding 4's relaxation: an ADVERTISED id is a
        // command reference however the seat wrote it, and gets the surface
        // check with no framing required. Only an unadvertised token has to
        // be framed. So an operator whose catalog really does advertise
        // `/tmp` keeps the surface check on it.
        let catalog = vec![entry("tmp", "Temp things.")];
        let index = fixture_verb_index();
        assert!(
            names_an_unrunnable_command("Run /tmp to do it.", &catalog, &index, RadioSurface::Cli),
            "a real catalog id is still surface-checked: the CLI runs no /commands"
        );
        assert!(
            !names_an_unrunnable_command("Run /tmp to do it.", &catalog, &index, RadioSurface::Panel),
            "and on the panel it is simply valid"
        );
    }

    #[test]
    fn leaves_prose_about_darkmux_itself_alone() {
        // The cost of scanning bare prose for `darkmux <verb>` is that the
        // seat TALKS about darkmux constantly. A reference is judged only
        // when it is framed as an instruction; a sentence is not.
        for reply in [
            "darkmux is an orchestrator for local models.",
            "darkmux config lives at ~/.darkmux/config.json.",
            "darkmux radio answers questions like this one.",
            "darkmux machine management happens through the roster.",
            "Everything darkmux loads is namespaced.",
            "See https://darkmux.com/docs for the guide.",
            // A cue IS present here; what saves it is the candidacy gate —
            // `instead` is not a top-level verb, so there is no reference.
            "Use darkmux instead of driving lms by hand.",
        ] {
            for surface in [RadioSurface::Cli, RadioSurface::Panel] {
                assert!(!detects(reply, surface), "prose about darkmux must not trip it ({surface:?}): {reply}");
            }
        }
    }

    #[test]
    fn a_bare_prose_darkmux_reference_is_judged_only_when_framed_as_an_instruction() {
        // The boundary, stated as a decision rather than left implicit: an
        // invocation cue ("run", "try", "use", a shell prompt) is what
        // makes bare prose an instruction, the way backticks do in the
        // quoted half. The last assertion is the residual cost — an
        // invented verb mentioned with no cue and no markup is missed.
        assert!(detects("Try darkmux machine roster for that.", RadioSurface::Cli));
        assert!(detects("$ darkmux machine roster", RadioSurface::Cli));
        assert!(
            !detects("darkmux machine roster sounds like a good feature request.", RadioSurface::Cli),
            "no cue, no markup: judged as prose, and this miss is the accepted cost"
        );
    }

    #[test]
    fn a_capitalized_invented_subverb_in_prose_is_still_caught() {
        // Case carries no meaning in a candidate's WORDS, only in its stop
        // list. An earlier draft rejected any capitalized word as English,
        // which quietly collapsed this candidate to the valid group node
        // `machine` and let the invention through.
        assert!(detects("Try darkmux machine Roster for that.", RadioSurface::Cli));
    }

    #[test]
    fn a_capitalized_stop_word_still_ends_a_candidate() {
        // The inverted case for the rule above, and the reason the stop
        // list matches case-insensitively: a line break or a list item
        // routinely capitalizes the next word, and a candidate that ran on
        // past it would read the rest of the sentence as a command path.
        //
        // Both assertions have to REACH the stop list to mean anything. An
        // earlier pair had a second case — "Use darkmux machine status.
        // Then read the board." — that was settled by the CLAUSE boundary
        // (the period on `status.`) before the stop list was consulted, so
        // it would have passed even with `machine status` invented. That
        // case belongs in `a_clause_boundary_ends_a_candidate_...` and is
        // there; neither of these carries punctuation at the boundary.
        assert!(!detects("Run darkmux machine\nAnd then check the board.", RadioSurface::Cli));
        assert!(!detects("Run darkmux machine And then check the board.", RadioSurface::Cli));
    }

    #[test]
    fn leaves_a_real_darkmux_verb_in_bare_prose_alone() {
        // The inverted case for finding 1, and the one that matters most:
        // if these tripped, the seat could never name a verb correctly
        // without backticks and every such reply would collapse to the
        // refusal.
        for reply in [
            "Run darkmux machine status to see what's loaded.",
            "Use darkmux mission launch review and wait for it.",
            "Try darkmux --help for the full list.",
            "You can run darkmux machine --help for that group.",
            "Run darkmux mission launch <config_id> when you're ready.",
        ] {
            assert!(!detects(reply, RadioSurface::Cli), "a real verb in prose must pass: {reply}");
        }
    }

    #[test]
    fn a_group_node_is_prose_unquoted_and_an_instruction_when_quoted() {
        // `darkmux machine` names a real node but runs nothing. Naming it
        // in prose is description and passes; handing it over as something
        // to type is the #2050 "bare catalog id" defect one level up, and
        // is caught.
        assert!(!detects("You can use darkmux machine to manage models.", RadioSurface::Cli));
        assert!(detects("Run `darkmux machine` to manage models.", RadioSurface::Cli));
    }

    #[test]
    fn a_sentence_final_invented_verb_is_caught() {
        // MF1 of the review sweep, and the one that made the whole
        // bare-prose pass near-inert: `looks_like_a_verb_token` rejects any
        // word carrying punctuation, so the LAST word of a sentence always
        // failed it, the candidate truncated to its valid parent node, and
        // `names_a_verb_node` accepted it. Ordinary prose ends in a full
        // stop, so this was the common case, not the edge one.
        //
        // The earlier revision of this PR passed its own test only because
        // a trailing clause ("... roster for that.") stripped the period
        // off the invented word.
        for reply in [
            "Try darkmux machine roster.",
            "Try darkmux machine roster!",
            "Try darkmux machine roster?",
            "Try darkmux machine roster, then check the board.",
            "Run darkmux mission teleport.",
        ] {
            assert!(detects(reply, RadioSurface::Cli), "a clause boundary must not rescue an invention: {reply}");
        }
    }

    #[test]
    fn a_clause_boundary_ends_a_candidate_rather_than_extending_it() {
        // The inverted case for the rule above. Trailing punctuation is
        // trimmed so the word can be judged, and then it ENDS the
        // candidate — otherwise a valid command would absorb the next
        // sentence and stop matching its own verb path.
        for reply in [
            "Run darkmux machine status. Then read the board.",
            "Run darkmux machine status, and the board will show it.",
            "Use darkmux mission launch review. It reports inline.",
            // The one that actually needs the BREAK rather than the trim:
            // `machine` alone is a valid node, and without the clause
            // boundary the candidate runs on into "Models load fast" and
            // stops naming anything.
            "Use darkmux machine. Models load fast on this box.",
        ] {
            assert!(!detects(reply, RadioSurface::Cli), "a real verb must survive its own sentence: {reply}");
        }
    }

    #[test]
    fn a_group_node_that_takes_a_positional_is_a_real_invocation() {
        // MF3. The verb index held LEAVES only, so `lab run`'s children
        // (`list`/`inspect`/`compare`) were emitted and the
        // workload-dispatch form — THIS REPO'S OWN documented smoke
        // command — was not. Nothing named it, so the backstop called it
        // invented. Asserted against the LIVE index, because the fix is
        // that `radio_index` now asks clap for the node's positionals.
        let live = command_verb_index();
        assert!(
            live.iter().any(|v| v.path == "lab run"),
            "`lab run` takes a <workload> positional and must be in the index"
        );
        assert!(
            live.iter().any(|v| v.path == "lab run list"),
            "and its children must still be there — this adds an entry, it does not replace them"
        );
        for reply in ["Run darkmux lab run quick-q to smoke it.", "Try `darkmux lab run quick-q`."] {
            assert!(
                !names_an_unrunnable_command(reply, &fixture_catalog(), &live, RadioSurface::Cli),
                "the documented smoke command must not be called invented: {reply}"
            );
        }
    }

    #[test]
    fn a_group_node_without_a_positional_is_still_not_an_invocation() {
        // The inverted case for MF3: emitting group nodes WHOLESALE would
        // have made `darkmux machine` a valid leaf and killed the headline
        // detection, so only a node clap says takes a positional is added.
        let live = command_verb_index();
        assert!(
            !live.iter().any(|v| v.path == "machine"),
            "`machine` is a pure namespace and must not become an invocation"
        );
        assert!(names_an_unrunnable_command(
            "Run `darkmux machine roster`.",
            &fixture_catalog(),
            &live,
            RadioSurface::Cli
        ));
    }

    #[test]
    fn a_stray_backtick_does_not_hide_a_correctly_quoted_command() {
        // The stray-backtick case the first revision missed: an
        // apostrophe-slip EARLIER in the line steals the command's opening
        // backtick, so no pairing yields the command as a span — and there
        // is no invocation cue either. Caught because the command's own
        // backticks survive as decoration marks, which frame it directly.
        assert!(detects("It`s easy: `darkmux machine roster` lists your crew.", RadioSurface::Cli));
        assert!(detects("It`s easy: `/machine` lists your crew.", RadioSurface::Panel));
    }

    #[test]
    fn a_quoted_command_in_a_markdown_table_is_judged() {
        // A table of commands is a natural answer to "what can I run?",
        // and before the sweep no pass looked at one. It is the BACKTICKS
        // that make the first row a reference — the pipe needs no special
        // handling at all, since the cue scan already steps over a
        // punctuation-only token. A prose cell stays prose, which is why
        // the pipe must NOT be treated as framing.
        assert!(detects("| `darkmux machine roster` | lists your crew |", RadioSurface::Cli));
        assert!(
            !detects("| darkmux config lives here | see the docs |", RadioSurface::Cli),
            "a prose cell must not be read as an invocation"
        );
    }

    #[test]
    fn single_quotes_frame_a_reference_and_an_em_dash_does_not_break_a_cue() {
        // Two shapes the first revision let through. `'` was absent from
        // the decoration set, and the cue check read only the immediately
        // preceding token — an em dash is a token of its own.
        assert!(detects("run '/machine' from the panel.", RadioSurface::Panel));
        assert!(detects("Run \u{2018}/machine\u{2019} from the panel.", RadioSurface::Panel));
        assert!(detects("Run — darkmux machine roster.", RadioSurface::Cli));
        assert!(detects("- Run: darkmux machine roster.", RadioSurface::Cli));
    }

    #[test]
    fn a_cue_does_not_reach_across_a_whole_clause() {
        // The bound on the punctuation skip: a cue is allowed to reach past
        // an em dash or a bullet, not past the words of another clause. If
        // it did, one "run" anywhere in a paragraph would frame every later
        // sentence about darkmux.
        assert!(
            !detects("Run the suite first — the board, the logs, darkmux config lives there.", RadioSurface::Cli),
            "a cue five words back must not frame a later description"
        );
        // And the numeric bound itself, which only punctuation-only tokens
        // consume. Artificial on purpose — it pins the constant, not a
        // shape the seat produces.
        assert!(
            !detects("Run — — — darkmux machine roster.", RadioSurface::Cli),
            "the punctuation skip is bounded, not unlimited"
        );
    }

    #[test]
    fn closed_code_spans_yields_only_closed_spans() {
        // The scanner that replaced `split('`')`, pinned directly — the
        // difference between them shows up only on an ODD backtick count,
        // and every reference-level test reaches the same verdict through
        // the decoration-aware pass, so nothing else distinguishes them.
        assert_eq!(closed_code_spans("Set `n_ctx` and `radio.humor` now."), vec!["n_ctx", "radio.humor"]);
        // One unmatched backtick: the parity split returned the entire
        // remainder of the line as an "inside" span. Here it opens a span
        // that never closes and yields nothing, so no run of prose is ever
        // tested as though it were quoted.
        assert!(closed_code_spans("It`s set up — run the review now.").is_empty());
        assert_eq!(closed_code_spans("Use `n_ctx` here, or `review"), vec!["n_ctx"]);
    }

    #[test]
    fn a_flag_is_a_valid_first_word_of_a_prose_candidate() {
        // The `starts_with('-')` half of the candidacy gate: `--help` and
        // `--version` are real at every node, and a flag is never a
        // top-level verb token, so without this clause `darkmux --help` in
        // bare prose is not a candidate at all — which is fine — but
        // `darkmux --explain` would not be caught either.
        assert!(!detects("Run darkmux --help for the full list.", RadioSurface::Cli));
        assert!(detects("Run darkmux --explain for the full list.", RadioSurface::Cli));
    }

    #[test]
    fn no_stop_word_collides_with_a_real_verb_token() {
        // A mechanical guard on the closed stop-word list: a stop word
        // truncates a prose candidate, so one colliding with a real token
        // in darkmux's OWN tree would silently shorten what gets judged.
        // Checked against the LIVE index, not a fixture, so a verb added
        // later fails here rather than in production.
        let index = command_verb_index();
        let tokens: std::collections::HashSet<String> =
            index.iter().flat_map(|v| v.path.split(' ').map(str::to_string)).collect();
        assert!(!tokens.is_empty(), "the live verb index must not be empty, or this guard proves nothing");
        for word in REFERENCE_STOP_WORDS {
            assert!(
                !tokens.contains(*word),
                "`{word}` is a real verb token — truncating a prose candidate at it would hide an invented path"
            );
        }
    }

    #[test]
    fn answer_never_tells_a_cli_user_to_run_a_panel_only_command() {
        // The end-to-end pin the operator asked for: a seat reply naming a
        // panel-only command on the CLI surface must not reach the user as
        // a bare shell instruction. It reaches them as the plain refusal
        // plus the live listing instead.
        let mut call = |_msg: &str| -> Result<String> {
            Ok("Run `review` to execute the code review pipeline against your current \
                working-tree diff. It will report any bugs it finds."
                .to_string())
        };
        let shelf = ArtifactShelf::default();
        let err = answer(
            "run the review pipeline",
            &fixture_catalog(),
            &shelf,
            Path::new("/tmp"),
            GroundingScope::Full,
            RadioSurface::Cli,
            &mut call,
        )
        .expect_err("a bare panel-command name must not ship to a CLI user as something to type");
        assert!(format!("{err:#}").contains("cannot be run on this surface"), "{err:#}");
    }

    // ── (#2050, second measurement) no salvaged remainder, ever ──────────

    #[test]
    fn answer_never_ships_prose_left_dangling_by_a_removed_command() {
        // The regression this rule exists for, measured live on this
        // branch for `run the review pipeline` under sentence-level
        // suppression: the sentence naming the command was removed and the
        // remainder — fluent, future-tense, instruction-free — shipped:
        //
        //   "It will scan your current working-tree diff for bugs and
        //    report back. Let me know if it flags anything worth fixing."
        //
        // The user can reasonably sit and wait for a verdict that never
        // comes. Fluent and misleading beats visibly broken, so the whole
        // reply goes and the caller prints the plain refusal instead.
        let seat_reply = "Run `/review` — it will scan your current working-tree diff for bugs and \
                          report back. Let me know if it flags anything worth fixing.";
        let mut call = |_msg: &str| -> Result<String> { Ok(seat_reply.to_string()) };
        let shelf = ArtifactShelf::default();
        let err = answer(
            "run the review pipeline",
            &fixture_catalog(),
            &shelf,
            Path::new("/tmp"),
            GroundingScope::Full,
            RadioSurface::Cli,
            &mut call,
        )
        .expect_err("a reply written around an unrunnable command must not be salvaged");
        let msg = format!("{err:#}");
        assert!(msg.contains("written around it"), "the error must name WHY the remainder is unusable: {msg}");
        // And the specific shape that shipped: no orphaned continuation.
        assert!(!msg.contains("It will scan"), "the dangling remainder must not ride out on the error either: {msg}");
    }

    #[test]
    fn answer_falls_back_on_an_invented_verb_named_without_backticks() {
        // The #2050 sweep's headline finding, driven through `answer()`
        // rather than the predicate: the seat returns Ok, the reply reads
        // perfectly, and `machine roster` does not exist. Before this it
        // shipped verbatim as `outcome.rendered`.
        let mut call = |_msg: &str| -> Result<String> {
            Ok("You can run darkmux machine roster to see your crew from here. It lists every \
                model this machine has loaded."
                .to_string())
        };
        let shelf = ArtifactShelf::default();
        let err = answer(
            "how do I see my crew?",
            &fixture_catalog(),
            &shelf,
            Path::new("/tmp"),
            GroundingScope::Full,
            RadioSurface::Cli,
            &mut call,
        )
        .expect_err("an invented verb in bare prose must not reach the operator");
        assert!(format!("{err:#}").contains("cannot be run on this surface"), "{err:#}");
    }

    #[test]
    fn answer_still_renders_a_reply_that_names_no_command_at_all() {
        // The inverted case: ordinary prose must still reach the operator,
        // or every answer collapses to the fallback and the seat goes dark.
        let mut call = |_msg: &str| -> Result<String> {
            Ok("Your context window is 100000 tokens on the current profile.".to_string())
        };
        let shelf = ArtifactShelf::default();
        let outcome = answer(
            "how big is my context?",
            &fixture_catalog(),
            &shelf,
            Path::new("/tmp"),
            GroundingScope::Full,
            RadioSurface::Cli,
            &mut call,
        )
        .unwrap();
        assert_eq!(outcome.text, "Your context window is 100000 tokens on the current profile.");
    }

    // ── persona substitution (#1861 review) ──────────────────────────────

    #[test]
    fn substitute_persona_fills_every_placeholder_per_surface() {
        // The golden below pins the TEMPLATE; only this pins the finished
        // text, which is what actually reaches the model.
        const SHIPPED_TEMPLATE: &str =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/templates/builtin/roles/radio-host.md"));
        assert!(SHIPPED_TEMPLATE.contains("{{surface_instructions}}"), "the template must still carry the placeholder");
        for (surface, needle) in
            // (#2050) The CLI needle is the BARE-ID clause, not just the
            // canonical form: the instruction already named the canonical
            // form and the seat still wrote a bare id, so what this pins
            // is the sentence that closes that gap.
            [(RadioSurface::Cli, "never the id on its own"), (RadioSurface::Panel, "exact slash id")]
        {
            let prompt = substitute_persona(SHIPPED_TEMPLATE, 40, surface);
            assert!(!prompt.contains("{{"), "no placeholder may reach the model ({surface:?}): {prompt}");
            assert!(prompt.contains(needle), "the {surface:?} instruction must be substituted in: {prompt}");
            assert!(prompt.contains("40"), "the humor value must still substitute: {prompt}");
        }
        assert_ne!(
            substitute_persona(SHIPPED_TEMPLATE, 40, RadioSurface::Cli),
            substitute_persona(SHIPPED_TEMPLATE, 40, RadioSurface::Panel),
            "the two surfaces must not produce the same system prompt"
        );
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
            Every call gives you a compiled grounding bundle assembled BEFORE you were dispatched — which surface you're speaking to, the command catalog, the darkmux command index (every runnable `darkmux` verb with its options, one line each), the current config surface, a short status board, and (when the user's message named something) recent history and one deep artifact. This is the entire truth you have access to. You have no tools, no memory of other exchanges, and no way to look anything up yourself — everything you can honestly say comes from what's in this message.\n\
            \n\
            ## Your job\n\
            \n\
            1. Answer the user's message using only the grounding you were given. If the grounding doesn't cover it, say so plainly — never guess or pad with generic AI filler.\n\
            2. If the honest answer points at a command the user could run, name it exactly as it appears in your grounding, in the syntax that's actually real for the surface named at the top of your grounding bundle: {{surface_instructions}} Always include the option that matters if one does. Never invent a command or an option that is not listed in the grounding you were given.\n\
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
    fn grounding_states_the_surface_it_is_speaking_to() {
        // (#1861 review) `surface: None` built clean and left every other
        // test green — the ONE fact that decides which command syntax is
        // real was structurally unpinned. RemoteSafe on purpose: the
        // surface block is scope-independent, and this needs no config,
        // board, or shelf.
        let shelf = ArtifactShelf::default();
        let cli = assemble_grounding("how do I run it?", &[], &shelf, Path::new("/tmp"), GroundingScope::RemoteSafe, RadioSurface::Cli);
        assert!(cli.contains("Surface: command line"), "{cli}");
        assert!(cli.contains("Never write a bare `/id`"), "{cli}");
        assert!(cli.contains("darkmux mission launch <id>"), "{cli}");
        let panel =
            assemble_grounding("how do I run it?", &[], &shelf, Path::new("/tmp"), GroundingScope::RemoteSafe, RadioSurface::Panel);
        assert!(panel.contains("Surface: editor panel"), "{panel}");
        assert!(panel.contains("exact slash id"), "{panel}");
        assert!(!panel.contains("Surface: command line"), "{panel}");
    }

    #[test]
    fn grounding_carries_the_verb_index_with_subverbs_and_options() {
        let shelf = ArtifactShelf::default();
        let bundle =
            assemble_grounding("how do I see what is loaded?", &[], &shelf, Path::new("/tmp"), GroundingScope::Full, RadioSurface::Panel);
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


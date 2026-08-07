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
pub fn shelf_entry(command: &str, args: &str, rendered: &str) -> ShelfEntry {
    ShelfEntry {
        command: command.to_string(),
        args: args.to_string(),
        rendered: rendered.to_string(),
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
const HELP_CAP_CHARS: usize = 1_600;
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
            if self.help.take().is_some() {
                continue;
            }
            if !self.shelf.is_empty() {
                self.shelf.remove(0);
                continue;
            }
            if self.board.take().is_some() {
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
            out.push_str("\nTop-level darkmux --help:\n");
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

fn render_help_block() -> String {
    use clap::CommandFactory;
    let help = crate::cli::Cli::command().render_help().to_string();
    truncate_chars(&help, HELP_CAP_CHARS)
}

/// Compact mission-board summary — always-on, cheap (issue #1698: "always-on
/// cheap summaries (the board), deep artifacts only when the question names
/// one"). Counts by status plus up to 5 non-terminal (Active/Paused)
/// mission ids, never the full board render `mission status` itself
/// produces (that's an operator-facing table, not grounding text).
fn render_board_block() -> Option<String> {
    let missions = crate::crew::loader::load_missions().ok()?;
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
    let live: Vec<&str> = missions
        .iter()
        .filter(|m| matches!(m.status, MissionStatus::Active | MissionStatus::Paused))
        .map(|m| m.id.as_str())
        .take(5)
        .collect();
    if !live.is_empty() {
        out.push_str("Active/paused: ");
        out.push_str(&live.join(", "));
        out.push('\n');
    }
    Some(truncate_chars(&out, BOARD_CAP_CHARS))
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

/// Assemble the answering seat's grounding block for one ask — the pure(-
/// ish; every source is a read-only local call, never a dispatch) core of
/// scope B. `cwd` is accepted for a future cwd-scoped grounding source
/// (none needed yet — every source today is process/registry-global); kept
/// as an explicit parameter rather than added later as a breaking change.
pub fn assemble_grounding(text: &str, catalog: &[CatalogEntry], shelf: &ArtifactShelf, _cwd: &Path) -> String {
    let mut sections = Sections {
        catalog: Some(render_catalog_block(catalog)),
        config: config_block(),
        board: render_board_block(),
        help: Some(render_help_block()),
        shelf: shelf
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
            .collect(),
        deep_artifact: detect_mission_mention(text).and_then(|m| render_mission_deep_artifact(&m)),
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
pub fn answer(text: &str, catalog: &[CatalogEntry], shelf: &ArtifactShelf, cwd: &Path, call: &mut AnswererCall<'_>) -> Result<AnswerOutcome> {
    let grounding = assemble_grounding(text, catalog, shelf, cwd);
    let message = build_answer_message(text, &grounding);
    let raw = call(&message)?;
    let reply = raw.trim().to_string();
    let rendered = if answer_references_a_command(&reply, catalog) {
        format!("{reply}\n\n{}", crate::acp_panel::not_a_command_message(&crate::acp_panel::list_panel_commands()))
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
pub fn dispatch_answerer_call_with(user_message: &str, overrides: &AnswererOverrides) -> Result<String> {
    let persona = crate::crew::loader::role_prompt("radio-host").ok_or_else(|| {
        anyhow::anyhow!("radio-host role has no readable .md persona template — cannot dispatch the answering seat")
    })?;
    let humor = overrides.humor.unwrap_or_else(darkmux_types::config_access::radio_humor);
    let system_prompt = persona.replace("{{humor}}", &humor.to_string());
    let profile_name = overrides
        .profile_name
        .clone()
        .or_else(darkmux_types::config_access::radio_answerer_profile);

    let opts = crate::crew::dispatch::DispatchOpts {
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
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
        step_id: None,
        system_prompt_override: Some(system_prompt),
    };
    let result = crate::fleet::dispatch_routed_via(opts, crate::crew::dispatch::dispatch_local_single_shot)?;
    Ok(result.stdout)
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
    answer(text, catalog, shelf, cwd, &mut |m: &str| dispatch_answerer_call_with(m, overrides))
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
pub const HUMOR_PRESETS: &[u8] = &[10, 35, 65, 90];

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
        let outcome = answer("anything mergeable?", &fixture_catalog(), &shelf, Path::new("/tmp"), &mut call).unwrap();
        assert!(outcome.rendered.len() > outcome.text.len(), "the listing must be appended: {outcome:?}");
    }

    #[test]
    fn answer_not_referencing_a_command_stays_bare() {
        let mut call = |_msg: &str| -> Result<String> { Ok("darkmux is a local-AI orchestrator CLI.".to_string()) };
        let shelf = ArtifactShelf::default();
        let outcome = answer("is this darkmux?", &fixture_catalog(), &shelf, Path::new("/tmp"), &mut call).unwrap();
        assert_eq!(outcome.text, outcome.rendered, "no command referenced — no listing appended: {outcome:?}");
    }

    #[test]
    fn answer_dispatch_error_propagates_as_err() {
        let mut call = |_msg: &str| -> Result<String> { Err(anyhow::anyhow!("no model loaded")) };
        let shelf = ArtifactShelf::default();
        let result = answer("is this darkmux?", &fixture_catalog(), &shelf, Path::new("/tmp"), &mut call);
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
            Every call gives you a compiled grounding bundle assembled BEFORE you were dispatched — the command catalog, the current config surface, a short status board, and (when the user's message named something) recent history and one deep artifact. This is the entire truth you have access to. You have no tools, no memory of other exchanges, and no way to look anything up yourself — everything you can honestly say comes from what's in this message.\n\
            \n\
            ## Your job\n\
            \n\
            1. Answer the user's message using only the grounding you were given. If the grounding doesn't cover it, say so plainly — never guess or pad with generic AI filler.\n\
            2. If the honest answer points at a command the user could run, name it with its exact slash syntax (e.g. `/pr-list`) so it's unambiguous — never invent a command id that isn't in the catalog you were given.\n\
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
}

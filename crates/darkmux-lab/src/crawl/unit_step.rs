//! (#2301) `crawl.unit` + `crawl.summary` — the crawl's DISPATCH half as
//! step kinds, so `crawl.json` is the whole crawl and the literal-routed
//! launcher (`src/crawl_launch.rs`, retired here) has no job left.
//!
//! `crawl.unit` wraps exactly one unit dispatch: look the unit up in the
//! plan its `crawl.plan` producer wrote, build the model-facing message,
//! dispatch the `crawler` role against the materialized tree mounted
//! read-only, classify what came back, and hand a small per-unit outcome
//! JSON downstream as the step's `output`.
//!
//! `crawl.summary` is the fan-in: it reads every `crawl.unit` step this
//! mission ran and writes the run's totals — the SAME keys the retired
//! launcher's `mission close` payload carried, so a reader keyed on
//! `units_completed`/`findings`/`stopped_by`/`tokens_per_hour` keeps
//! working. The generic launcher promotes the LAST phase's last step
//! output to the `mission close` payload when it is a JSON object, which
//! is the seam this kind is built for.
//!
//! **Tier 3 by #1352's test**, co-located with the crawl module rather
//! than `darkmux-crew`'s shared `step_kinds/`: a unit dispatch is a whole
//! per-unit procedure (plan lookup → message → dispatch → four-way
//! classification → two independent per-unit bounds → finding readback)
//! that `dispatch.internal`'s one-call-per-step shape does not have and
//! could not gain without changing its observable behavior. Same
//! reasoning `plan_step.rs` records for `crawl.plan`.
//!
//! **Every cross-kind output is a TYPED struct, and the read IS the
//! check.** `crawl.unit`'s `output` is [`UnitOutcome`] serialized — a
//! `schema_version`, required fields plain, optional fields
//! `#[serde(default)]` — never a free-form blob. `crawl.summary`
//! deserializes every unit output THROUGH that struct and fails by field
//! name at the read; the plan is read through [`Plan`], already the same
//! rule. No content names on ports: a port stays a label.
//!
//! **Testing (no model, no container).** [`CrawlUnitStepKind`] holds its
//! dispatch function — production is `darkmux_crew::dispatch::dispatch`
//! ([`CrawlUnitStepKind::production`]); a test constructs the kind with a
//! closure returning a scripted [`DispatchResult`] pointing at a tempdir
//! seeded with `.darkmux-runtime/findings.jsonl`. Same injection
//! discipline the retired launcher used, moved onto the kind because a
//! `StepKind::run` takes no such argument.

use crate::crawl::plan::{Plan, ReadFileEntry, Site, Unit};
use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::dispatch::{CompactionDispatchArgs, DispatchOpts, DispatchResult};
use darkmux_crew::rules::{self, Rule};
use darkmux_crew::step_kinds::{Port, StepKind, StepKindRegistry, StepOutcome, StepRunCtx};
use darkmux_crew::types::{Step, Task};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const CRAWL_UNIT_KIND: &str = "crawl.unit";
pub const CRAWL_SUMMARY_KIND: &str = "crawl.summary";

/// (#2301) CONTENT ids — what a step PRODUCES, checked by whoever reads it.
/// Separate from the step-kind ids above on purpose: a kind is a thing that
/// runs, a content id is a thing that is read.
pub const UNIT_OUTCOME_KIND: &str = "crawl.unit-outcome";
pub const CRAWL_SUMMARY_OUTPUT_KIND: &str = "crawl.summary";

/// The one `create_finding` field this module rewrites — the container
/// path is stripped off `file`; every other field is copied verbatim.
const FINDING_FILE_KEY: &str = "file";

/// The named outcome BOTH per-unit bounds end a unit under — counted
/// separately from `error` in the summary, never folded into
/// `units_errored`: "ran out of room to work in" and "genuinely broke"
/// are different operator questions (#2193).
const UNIT_BUDGET_EXHAUSTED: &str = "unit_budget_exhausted";

/// A rough multiple of turns per site — read the site, maybe grep around
/// it, decide, call `create_finding` (or not). Deliberately generous: a
/// unit needing fewer turns finishes early via `result: "stop"`; this
/// only bounds the worst case.
const TURNS_PER_SITE: u32 = 3;
/// However small a unit's site/file count, it gets at least this many turns.
const MIN_UNIT_MAX_TURNS: u32 = 12;
/// However large a unit's site/file count, its ceiling never exceeds this
/// — the backstop that closes #2193.
const MAX_UNIT_MAX_TURNS: u32 = 40;
/// Default no-progress bound (#2193's "N=8"), overridable per step via
/// `config.no_progress_turns`; `0` disables the check.
const DEFAULT_NO_PROGRESS_TURNS: usize = 8;

// ── the typed per-unit outcome ───────────────────────────────────────────

/// [`UnitOutcome`]'s own schema version. Bumped when the shape below
/// changes; a consumer is strict on REQUIRED fields (a missing one fails
/// the read by name) and lenient on the rest.
pub const UNIT_OUTCOME_SCHEMA_VERSION: &str = "1.1";

/// (#2302) ONE finding a unit's dispatch recorded, named by the key its
/// store answers to — the address a FOLLOW-ON step hands to `brief_refs`.
///
/// The key is `<dispatch session id>/<emit_seq>`, exactly the form
/// [`darkmux_crew::findings::parse_key`] splits and
/// [`darkmux_crew::findings::load_at`] resolves: the unit's dispatch owns
/// the session id, and `emit_seq` is the 1-based ordinal of the acceptance
/// within that dispatch, which is the finding file's own non-empty-line
/// ordinal (the runtime writes `emit_seq = count + 1` after appending).
/// Nothing is re-derived from the model's prose — `file`/`line`/`rule` are
/// copied off the record the crawl already stamps, so this carries no
/// interpretation of its own.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FindingRef {
    /// `<dispatch>/<seq>` — the finding store's key.
    pub key: String,
    /// The same key with `/` replaced by `-`. A grown task's id suffix
    /// becomes part of a task id and a step id, and `/` in either would
    /// read as a path separator — so the config's `grow.id` names THIS
    /// field and `grow.config.brief_refs` names `key`.
    pub id: String,
    /// The file the finding names, source-relative (the container prefix
    /// already stripped), when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// The ONE rule id this finding was recorded under.
    pub rule: String,
    /// The materialized workspace root the unit was dispatched against —
    /// the same directory `crawl.unit` mounts, so a create-mods step can
    /// name it as its `workdir` and see the very tree the finding cites.
    pub tree_root: String,
}

/// (#2301) What ONE `crawl.unit` step produces, and the ONLY thing
/// `crawl.summary` reads. A typed struct, not a JSON blob: the consumer
/// deserializes through it, so a producer that drifts fails at the read
/// naming the field rather than summarizing zeros.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct UnitOutcome {
    pub schema_version: String,
    /// The plan unit this step dispatched.
    pub unit: String,
    /// ONE rule id for a single-rule unit; `null` for a multi-rule read
    /// unit, whose `rules` lists them (the same convention every crawl
    /// payload uses).
    pub rule: Option<String>,
    pub source: String,
    /// `stop` | `unit_budget_exhausted` | `timeout` | `error`.
    pub result: String,
    /// Accepted `create_finding` calls this unit's dispatch made.
    ///
    /// (#2302) A COUNT, and it keeps the name: the retired launcher's
    /// close payload carried `findings` as a number and readers are keyed
    /// on it. The findings themselves are named by [`Self::finding_refs`].
    pub findings: u64,
    /// `create_finding` calls the runtime REJECTED — engagement that did
    /// not become a finding, never folded into `findings`.
    pub findings_rejected: u64,
    pub wall_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub model: Option<String>,
    pub out_dir: String,

    // Optional, lenient on read — descriptive context a summary may use
    // and an older producer may not have written.
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub sha: String,
    #[serde(default)]
    pub rest_ms: u64,
    /// Why this unit did not produce numbers, when it did not: the step
    /// kind's own error text, as the scheduler recorded it. Present only on
    /// a row the summary built from an ERRORED step (#2301 review).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown | null"))]
    pub detections: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "unknown | null"))]
    pub host: Option<Value>,
    /// (#2302) This unit's accepted findings, named by store key — one per
    /// `findings`, in the order the dispatch recorded them. Additive in
    /// schema 1.1; a 1.0 producer simply wrote none.
    #[serde(default)]
    pub finding_refs: Vec<FindingRef>,
}

// ── model-facing message (AI-convention vocabulary; CLAUDE.md's
//    "Model-facing prompt construction") ─────────────────────────────────

/// One rule's prose, verbatim from its title/match/no_match/evidence/
/// why_hint fields, wrapped so a Read unit binding several rules can carry
/// more than one without ambiguity about which sentence belongs to which
/// pattern.
///
/// (#2310 P4c) A `confirm: "search"`/`"question"` rule gets ONE more
/// instruction appended, naming the confirmation step DESIGN.md's "a rule
/// is a procedure" section describes: `detect` (the `match`/`no_match`
/// prose above) is unchanged for either form — only what happens AFTER a
/// candidate is spotted differs. `create_finding`'s wire schema carries no
/// `context`/`form` argument the model can set (see `runtime/src/tools/
/// mod.rs`'s `CreateFinding` schema — deliberately not extended by this
/// packet, see this crate's #2310 P4c doc notes), so the confirmation step
/// is instead told to run BEFORE `create_finding` is called and to fold
/// its result into `why` — the finding's `context` (which mission/rule/
/// unit produced it, stamped by the host, never the model) is what a
/// downstream reader keys on to know a rule ran the search/question form
/// at all; `why`'s text is where the search's own results or the
/// question's own answer actually live.
fn pattern_block(rule: &Rule) -> String {
    let mut out = format!(
        "<pattern name=\"{id}\">\nTitle: {title}\n\nReport a match when: {matches}\n\nDo NOT report when: {no_match}\n\nWhat evidence to cite: {evidence}\n\nHow to explain why: {why_hint}\n",
        id = rule.id,
        title = rule.title.as_deref().unwrap_or(&rule.id),
        matches = rule.matches.as_deref().unwrap_or(""),
        no_match = rule.no_match.as_deref().unwrap_or(""),
        evidence = rule.evidence.as_deref().unwrap_or(""),
        why_hint = rule.why_hint.as_deref().unwrap_or(""),
    );
    match rule.confirm {
        darkmux_crew::rules::ConfirmForm::Mod => {}
        darkmux_crew::rules::ConfirmForm::Search => {
            if let Some(recipe) = &rule.search {
                out.push_str(&format!(
                    "\nBefore you call create_finding for this pattern: this pattern is confirmed by SEARCHING, \
                     not by reasoning alone. {}Then, in `why`, list every instance the search returned \
                     (file:line for each) before explaining the match — a finding for this pattern with no \
                     search results named in `why` is incomplete.\n",
                    search_instruction(recipe),
                ));
            }
        }
        darkmux_crew::rules::ConfirmForm::Question => {
            // (#2310 P4c) DESIGN.md's rule shape is detect -> search ->
            // compare -> deliver as four steps regardless of the final
            // delivery form (`existing-solution`'s "search the tree
            // mechanically, THEN ask the question" is exactly this): a
            // `confirm: "question"` rule may ALSO declare a `search`
            // recipe, run first, whose results feed the question's own
            // answer.
            if let Some(recipe) = &rule.search {
                out.push_str(&format!(
                    "\nBefore you answer the question below: {}List what you find in `why` before your \
                     answer — the question is answered FROM the search, not guessed.\n",
                    search_instruction(recipe),
                ));
            }
            if let Some(compare) = &rule.compare {
                out.push_str(&format!(
                    "\nBefore you call create_finding for this pattern: answer this question in one line, then \
                     put your answer AND its reasoning at the start of `why` — this pattern's finding is a \
                     QUESTION for the author, not a claim, so be honest that it is unconfirmed: {compare}\n",
                ));
            }
        }
    }
    out.push_str("</pattern>\n\n");
    out
}

/// (#2310 P4c) One sentence telling the model to run the `search` tool,
/// over the tree (not just its window). Some rules fix the search terms
/// up front (`shared-symbol-callers`: "search for THIS symbol's name",
/// known from the rule's own detect step); others cannot — the term IS
/// the thing the model just observed in the hunk (`existing-solution`:
/// the new routine's own name), so `patterns` is deliberately empty and
/// `note` alone carries the instruction. Rendering "run the tool for each
/// of these patterns: " with an empty list would read as broken
/// instructions, so the two cases get different sentences rather than one
/// template papering over the difference.
fn search_instruction(recipe: &darkmux_crew::rules::SearchRecipe) -> String {
    let note = recipe.note.as_deref().unwrap_or("");
    if recipe.patterns.is_empty() {
        format!("Run the `search` tool over the tree (not just the window you were given). {note} ")
    } else {
        format!(
            "Run the `search` tool once for each of these patterns over the tree (not just the window you were \
             given): {}. {note} ",
            recipe.patterns.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", "),
        )
    }
}

fn render_sites(source: &str, sites: &[Site]) -> String {
    // Full container paths: the workspace root is the MATERIALIZED tree, so a
    // path relative to the source (`ui/src/x.ts`) does not resolve and the
    // tool boundary rejects it (observed on the first live mission, #1959).
    sites
        .iter()
        .map(|s| format!("- /workspace/{source}/{}:{} (read lines {}-{})", s.file, s.line, s.start, s.end))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_files(source: &str, files: &[ReadFileEntry]) -> String {
    files
        .iter()
        .map(|f| match f {
            ReadFileEntry::Whole(path) => format!("- /workspace/{source}/{path}"),
            ReadFileEntry::Range { file, start, end } => {
                format!("- /workspace/{source}/{file} (lines {start}-{end})")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// This block ends every dispatch message regardless of unit kind —
/// deliberately reusing the wording from the first crawler workload
/// (`templates/builtin/workloads/crawl-error-discard.json`) for the two
/// load-bearing sentences (the tool's exact five keys; the coverage
/// request), so a model already tuned against that workload sees familiar
/// phrasing here.
const REPORT_FINDING_INSTRUCTIONS: &str = "\nFor each match, call `create_finding` with these five keys exactly: `file`, `line`, `pattern`, `evidence`, `why`. `file` must be the full path exactly as listed above, starting with `/workspace/`. `evidence` must be the source line copied verbatim, and `line` must be where it appears.\n\nWhen you are done, say which files or sites you examined, which you did not get to, and whether you covered the whole scope.\n";

/// Build the dispatch message for one unit. Model-facing (AI-convention
/// terms; the words `unit`/`ledger`/`corpus`/`packet` never appear —
/// darkmux-internal vocabulary a clean-context model can't ground).
///
/// (#2310 P4c review round 2, item (e)) `intent` is the diff's stated
/// intent (a PR body or intent file's own text), rendered up front when
/// present — `intent-vs-diff`'s own `match`/`compare` prose already
/// assumes "the intent you were given... provided alongside your window"
/// is real; before this parameter existed, nothing ever supplied it, so
/// that rule was structurally inert regardless of the seat. `None` for
/// every crawl.json task (which never sets `config.intent_file`) and for
/// a review-v2 launch with no `intent_file` param — the block is omitted
/// entirely rather than rendered empty, so a unit's message is unchanged
/// from before this parameter existed whenever there is nothing to say.
pub fn build_message(rules_by_id: &BTreeMap<String, Rule>, unit: &Unit, intent: Option<&str>) -> Result<String> {
    let missing = |rule: &str| {
        anyhow!(
            "crawl.unit: no rule resolved for id `{rule}` — the plan names a rule the current \
             rule set no longer declares; re-plan this rule"
        )
    };
    let mut out = String::new();
    if let Some(text) = intent {
        out.push_str(&format!(
            "The stated intent for this change (a PR body or intent file) is:\n\n{text}\n\n"
        ));
    }
    match unit {
        Unit::Site { rule, sites, source, .. } => {
            let r = rules_by_id.get(rule).ok_or_else(|| missing(rule))?;
            out.push_str(&pattern_block(r));
            out.push_str(&format!(
                "Your scope is these sites in `/workspace/{source}`. For each, read lines noted below and decide whether the cited line matches the pattern. `read` numbers every line it returns as `N: content`, so the cited line is the one beginning `<line>: ` — do not count lines. Cite it as `file:line`. Sites:\n{}\n",
                render_sites(source, sites)
            ));
        }
        Unit::Read { rules: rule_ids, files, source, .. } => {
            for rid in rule_ids {
                let r = rules_by_id.get(rid).ok_or_else(|| missing(rid))?;
                out.push_str(&pattern_block(r));
            }
            out.push_str(&format!(
                "Your scope is these files in `/workspace/{source}`. Read each one in full and apply every pattern above:\n{}\n",
                render_files(source, files)
            ));
        }
        Unit::Edge {
            rule,
            sites,
            source,
            library,
            package,
            pinned,
            library_version,
            library_surface,
            ..
        } => {
            let r = rules_by_id.get(rule).ok_or_else(|| missing(rule))?;
            out.push_str(&pattern_block(r));
            out.push_str(&format!(
                "Your scope is these import sites in `/workspace/{source}`. `read` numbers every line it returns as `N: content`, so the cited line is the one beginning `<line>: `. Cite it as `file:line`.\n{}\n\n",
                render_sites(source, sites)
            ));
            out.push_str(&format!(
                "The library `{package}` at the version being examined is at `/workspace/{library}`; its entry files and changelog are: {}. The consumer pins `{pinned}`; the library version is `{library_version}`.\n",
                if library_surface.is_empty() { "(none)".to_string() } else { library_surface.join(", ") }
            ));
        }
    }
    out.push_str(REPORT_FINDING_INSTRUCTIONS);
    Ok(out)
}

// ── dispatch classification ──────────────────────────────────────────────

/// Whether `stderr` carries the host watchdog's structured inactivity-
/// timeout marker (#363) — the one reliable `DispatchResult`-level signal
/// that a non-clean exit was specifically the watchdog hard-killing the
/// container rather than any other failure shape.
fn watchdog_timeout_fired(stderr: &str) -> bool {
    stderr.contains(darkmux_crew::dispatch_internal::INACTIVITY_TIMEOUT_MARKER)
}

/// `(result, wall_ms, prompt_tokens, completion_tokens, model, detections,
/// rest_ms, host)` — named so clippy's `type_complexity` lint doesn't have
/// to be silenced instead.
type UnitDispatchOutcome = (String, u64, u64, u64, Option<String>, Option<Value>, u64, Option<Value>);

/// Pull the per-unit numbers out of a dispatch's `--json` envelope.
/// `result` is `"stop"` on a clean finish, `unit_budget_exhausted` when
/// the runtime reported `max_turns` (#2193 — a BOUND, not a failure),
/// `"timeout"` when stderr carries the watchdog marker, else `"error"`.
pub fn interpret_dispatch_result(unit_id: &str, res: &DispatchResult) -> UnitDispatchOutcome {
    let envelope: Option<Value> =
        if res.stdout.trim().starts_with('{') { serde_json::from_str(&res.stdout).ok() } else { None };
    if envelope.is_none() && !res.stdout.trim().is_empty() {
        let excerpt: String = res.stdout.chars().take(120).collect();
        eprintln!(
            "{}",
            darkmux_types::style::warn(&format!(
                "crawl.unit: unit `{unit_id}` produced non-JSON stdout (expected a `--json` \
                 envelope) — first 120 chars: {excerpt:?}"
            ))
        );
    }
    let timed_out = watchdog_timeout_fired(&res.stderr);
    let result_label = match envelope.as_ref().and_then(|e| e.get("result")).and_then(Value::as_str) {
        Some("stop") => "stop".to_string(),
        // Checked BEFORE the `timed_out` arm: a watchdog kill AFTER the
        // turn cap was already hit is still, first and foremost, a budget
        // exhaustion.
        Some("max_turns") => UNIT_BUDGET_EXHAUSTED.to_string(),
        Some(_) if timed_out => "timeout".to_string(),
        Some(_) => "error".to_string(),
        None if timed_out => "timeout".to_string(),
        None => {
            if res.exit_code == 0 {
                "stop".to_string()
            } else {
                "error".to_string()
            }
        }
    };
    let num = |e: &Option<Value>, p: &str| e.as_ref().and_then(|e| e.pointer(p)).and_then(Value::as_u64).unwrap_or(0);
    let model =
        envelope.as_ref().and_then(|e| e.pointer("/metrics/model")).and_then(Value::as_str).map(String::from);
    (
        result_label,
        num(&envelope, "/metrics/wall_ms"),
        num(&envelope, "/metrics/prompt_tokens"),
        num(&envelope, "/metrics/completion_tokens"),
        model,
        envelope.as_ref().and_then(|e| e.get("detections")).cloned(),
        num(&envelope, "/metrics/rest_ms"),
        // (#2107) Absent (not zeroed) whenever the sampler never got a
        // reading — `.cloned()` on a missing key preserves that honestly.
        envelope.as_ref().and_then(|e| e.get("host")).cloned(),
    )
}

// ── per-unit bounds (#2193) ──────────────────────────────────────────────

/// This unit's own site/file count — the plan's own estimate of how much
/// ground it covers, and therefore the input to [`default_unit_max_turns`].
fn unit_site_count(unit: &Unit) -> usize {
    match unit {
        Unit::Site { sites, .. } | Unit::Edge { sites, .. } => sites.len(),
        Unit::Read { files, .. } => files.len(),
    }
}

/// The per-unit `max_turns` ceiling this kind sets by DEFAULT. A DEFAULT,
/// not a mandate: `DispatchOpts::max_turns_override` only fills the gap an
/// operator's own `runtime.max_turns` setting left open.
pub fn default_unit_max_turns(unit: &Unit) -> u32 {
    let sites = unit_site_count(unit).max(1) as u32;
    sites.saturating_mul(TURNS_PER_SITE).clamp(MIN_UNIT_MAX_TURNS, MAX_UNIT_MAX_TURNS)
}

/// (#2193) Whether this unit's LAST `n` turns collectively made no
/// progress: no `create_finding` ATTEMPT (accepted or rejected) and no
/// path read that an earlier turn hadn't already read. Best-effort — a
/// missing/unreadable trajectory reports `false`, never escalating a unit
/// this code can't fully inspect. A unit that hasn't run `n` turns yet
/// reports `false`: the bound only fires once there IS a full trailing
/// window to judge.
pub fn unit_hit_no_progress_bound(out_dir: &Path, n: usize) -> bool {
    if n == 0 {
        return false;
    }
    let Ok(body) = std::fs::read_to_string(out_dir.join(".darkmux-runtime").join("trajectory.jsonl")) else {
        return false;
    };

    let mut by_turn: BTreeMap<u64, bool> = BTreeMap::new();
    let mut read_paths_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("tool.completed") {
            continue;
        }
        let Some(seq) = v.get("seq").and_then(Value::as_u64) else { continue };
        let tool_name = v.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let entry = by_turn.entry(seq).or_insert(false);
        match tool_name {
            "create_finding" => *entry = true,
            "read" => {
                let args = v.get("args").and_then(Value::as_str).unwrap_or("");
                if let Some(path) = serde_json::from_str::<Value>(args)
                    .ok()
                    .and_then(|a| a.get("path").and_then(Value::as_str).map(str::to_string))
                {
                    if read_paths_seen.insert(path) {
                        *entry = true;
                    }
                }
            }
            _ => {}
        }
    }

    if by_turn.len() < n {
        return false;
    }
    by_turn.values().rev().take(n).all(|progressed| !progressed)
}

/// (#1959) Count `create_finding` tool calls THIS unit's dispatch made
/// that the runtime rejected (`tool.completed` with `ok == false`). A
/// missing/unreadable trajectory or an unparseable line is silently
/// skipped — this is a best-effort operator-facing count, never a
/// correctness-bearing value.
pub fn count_rejected_create_findings(out_dir: &Path) -> usize {
    let Ok(body) = std::fs::read_to_string(out_dir.join(".darkmux-runtime").join("trajectory.jsonl")) else {
        return 0;
    };
    body.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v.get("type").and_then(Value::as_str) == Some("tool.completed")
                && v.get("tool_name").and_then(Value::as_str) == Some("create_finding")
                && v.get("ok").and_then(Value::as_bool) == Some(false)
        })
        .count()
}

// ── crawler seat (#2188) ─────────────────────────────────────────────────

/// A dispatched role's resolved model + locality + profile. Resolved
/// per-unit here (the binding does not change mid-run, so this is a cheap
/// repeat of the same registry read the retired launcher did once) purely
/// to STAMP provenance onto this dispatch's records: `DispatchOpts::
/// profile_name` stays `None`, i.e. "use the role's own `role_profiles.
/// <role>` binding". An unresolvable registry leaves every field
/// `None`/`"unknown"` rather than failing the unit. Named `CrawlerSeat`
/// from when it only ever resolved the `crawler` role (#1959); kept the
/// name across #2310 P4c's generalization to any role since the type
/// itself is role-agnostic and a rename here would touch every call site
/// for no behavior change.
pub struct CrawlerSeat {
    pub model: Option<String>,
    pub locality: &'static str,
    pub profile_name: Option<String>,
}

/// (#2310 P4c) `role` is the Task's own `role_id` — see `CrawlUnitStepKind::
/// run`'s doc for why the dispatch itself now reads `task.role_id` rather
/// than a hardcoded `"crawler"`; this resolver generalizes the same way so
/// a reviewer-role unit's provenance stamp names `role_profiles.reviewer`,
/// not a crawler binding that was never actually dispatched.
pub fn resolve_crawler_seat(role: &str) -> CrawlerSeat {
    let unresolved = || CrawlerSeat { model: None, locality: "unknown", profile_name: None };
    let Ok(loaded) = darkmux_profiles::profiles::load_registry(None) else {
        return unresolved();
    };
    let Ok(resolved) = darkmux_profiles::profiles::resolve_role_profile(role, &loaded.registry) else {
        return unresolved();
    };
    let Some(model_id) = resolved.profile.default_model_id() else {
        return CrawlerSeat { model: None, locality: "unknown", profile_name: Some(resolved.profile_name) };
    };
    let is_remote = resolved.profile.models.iter().find(|m| m.id == model_id).is_some_and(|m| m.is_remote());
    CrawlerSeat {
        model: Some(model_id.to_string()),
        locality: if is_remote { "endpoint" } else { "local" },
        profile_name: Some(resolved.profile_name),
    }
}

// ── finding readback ─────────────────────────────────────────────────────

/// (#1959) Pick the ONE rule id a finding belongs to. A single-rule unit
/// needs no disambiguation; a multi-rule (read) unit matches the model's
/// reported `pattern` case-insensitively against the unit's rule ids. When
/// it names none of them, the first rule stands in and the pattern is
/// returned so the record can say so.
pub fn finding_rule_for(pattern: Option<&str>, rule_ids: &[String]) -> (String, Option<String>) {
    if let [only] = rule_ids {
        return (only.clone(), None);
    }
    let wanted = pattern.map(str::trim).unwrap_or("");
    if let Some(hit) = rule_ids.iter().find(|r| r.eq_ignore_ascii_case(wanted)) {
        return (hit.clone(), None);
    }
    (
        rule_ids.first().cloned().unwrap_or_default(),
        if wanted.is_empty() { None } else { Some(wanted.to_string()) },
    )
}

/// Strip a container-path prefix off a finding's `file` field — either the
/// absolute form (`/workspace/<source-id>/<rel>`) or the bare relative one
/// (`<source-id>/<rel>`, since a model may have copied the scope listing
/// verbatim). Falls through unchanged when neither matches.
fn strip_source_prefix(source_id: &str, raw: &str) -> String {
    if let Some(rel) = raw.strip_prefix(&format!("/workspace/{source_id}/")) {
        return rel.to_string();
    }
    if let Some(rel) = raw.strip_prefix(&format!("{source_id}/")) {
        return rel.to_string();
    }
    raw.to_string()
}

/// Read this unit's accepted findings, stamp the crawl's provenance onto
/// each, and write them beside the run as `<unit>.findings.jsonl`.
/// Returns how many there were AND one [`FindingRef`] per accepted
/// finding, from the SAME read (#2302). The two are returned separately
/// rather than as one list because they answer to different guards: the
/// count is what the unit observed, and stays true even when the keys are
/// unaddressable.
///
/// The DURABLE record of a finding is the finding store the dispatch
/// tailer writes (#2265) plus the `dispatch.tool` record its hook
/// transform reads; this copy is the run-local artifact an operator
/// inspects, which is why a write failure here is a warning, not a unit
/// failure.
fn readback_findings(
    ctx: &UnitContext,
    out_dir: &Path,
    into: &Path,
    model: Option<&str>,
    session_id: &str,
) -> (usize, Vec<FindingRef>) {
    let findings_path = out_dir.join(".darkmux-runtime").join("findings.jsonl");
    let Ok(body) = std::fs::read_to_string(&findings_path) else {
        return (0, Vec::new());
    };
    // (#2302) A key's dispatch half becomes a path segment under the
    // finding store, so a session id that could escape it produces NO refs
    // at all rather than an unresolvable key a create-mods step would refuse on.
    // The COUNT is unaffected: the unit observed what it observed, whether
    // or not the observations can be addressed. The crawl mints its own
    // session ids, so this is a backstop, not an expected path — and it is
    // loud.
    let addressable = darkmux_crew::findings::is_safe_dispatch_segment(session_id);
    if !addressable {
        eprintln!(
            "{}",
            darkmux_types::style::warn(&format!(
                "crawl.unit: session id `{session_id}` is not a finding-store segment — this unit's findings are counted but not addressable"
            ))
        );
    }
    let mut refs: Vec<FindingRef> = Vec::new();
    let mut found = 0usize;
    let mut buf = String::new();
    // The ordinal is over NON-EMPTY LINES, which is exactly how the runtime
    // derives the `emit_seq` it stamps on the record it materializes
    // (`existing.lines().filter(non-empty).count() + 1`). Counting parsed
    // lines instead would drift the key by one for every line that failed
    // to parse.
    for (idx, line) in body.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let seq = idx as u64 + 1;
        let Ok(mut rec) = serde_json::from_str::<Value>(line) else { continue };
        found += 1;
        if let Some(obj) = rec.as_object_mut() {
            let raw_file = obj.get(FINDING_FILE_KEY).and_then(Value::as_str).unwrap_or("").to_string();
            obj.insert("file_raw".to_string(), json!(raw_file));
            obj.insert(FINDING_FILE_KEY.to_string(), json!(strip_source_prefix(&ctx.source, &raw_file)));
            obj.insert("workspace".to_string(), json!(ctx.workspace));
            obj.insert("unit".to_string(), json!(ctx.unit_id));
            obj.insert("source".to_string(), json!(ctx.source));
            obj.insert("sha".to_string(), json!(ctx.sha));
            // `rule` is ONE id — the pattern the model reported this
            // finding under — never the unit's whole list: the hook
            // receiver keys finding identity on it and refuses an array.
            let pattern = obj.get("pattern").and_then(Value::as_str).map(str::to_string);
            let (rule_id, unmatched) = finding_rule_for(pattern.as_deref(), &ctx.rule_ids);
            obj.insert("rule".to_string(), json!(rule_id));
            obj.insert("rules".to_string(), json!(ctx.rule_ids));
            if let Some(u) = unmatched {
                obj.insert("rule_unmatched_pattern".to_string(), json!(u));
            }
            obj.insert("session_id".to_string(), json!(session_id));
            if let Some(m) = model {
                obj.insert("model".to_string(), json!(m));
            }
            if addressable {
                let key = format!("{session_id}/{seq}");
                refs.push(FindingRef {
                    id: key.replace('/', "-"),
                    key,
                    file: obj.get(FINDING_FILE_KEY).and_then(Value::as_str).map(str::to_string),
                    line: obj.get("line").and_then(Value::as_u64),
                    rule: rule_id.clone(),
                    tree_root: ctx.tree_root.display().to_string(),
                });
            }
        }
        buf.push_str(&serde_json::to_string(&rec).unwrap_or_default());
        buf.push('\n');
    }
    if !buf.is_empty() {
        if let Err(e) = std::fs::write(into, buf) {
            eprintln!(
                "{}",
                darkmux_types::style::warn(&format!("crawl.unit: writing {} — {e:#}", into.display()))
            );
        }
    }
    (found, refs)
}

// ── the `crawl.unit` step kind ───────────────────────────────────────────

/// The dispatch seam. Production is [`darkmux_crew::dispatch::dispatch`];
/// a test injects a closure. `Send + Sync` because the scheduler runs a
/// step on a worker thread.
pub type UnitDispatchFn = Arc<dyn Fn(DispatchOpts) -> Result<DispatchResult> + Send + Sync>;

pub struct CrawlUnitStepKind {
    dispatch: UnitDispatchFn,
}

impl CrawlUnitStepKind {
    /// The registered kind — dispatches for real.
    pub fn production() -> Self {
        Self { dispatch: Arc::new(darkmux_crew::dispatch::dispatch) }
    }
    /// A kind whose dispatch is `f`. Test-only in practice; `pub` so a
    /// consumer outside this module can drive it without a container.
    pub fn with_dispatch(f: UnitDispatchFn) -> Self {
        Self { dispatch: f }
    }
}

/// The parsed step config. Separate from the kind so the resolution is
/// testable without a `Step`.
#[derive(Debug, Clone)]
pub struct UnitStepConfig {
    /// The PLAN producer's own `output` — a `ref`, a path, or inline JSON.
    /// Read through `Output<Plan>`, never opened directly.
    pub plan: String,
    pub unit: String,
    pub rule: Option<String>,
    pub no_progress_turns: usize,
    pub timeout_seconds: Option<u32>,
    /// (#2310 P4c review round 2, item (e); wired P4c-2 item 0) Path to the
    /// diff's stated intent (a PR body or intent file). Optional.
    /// `review-v2.json`'s `unit-<rule>` tasks grow `"intent_file":
    /// "{{intent_file}}"` into every unit's config, and `mission_config::
    /// substitute_step_config` resolves it from the launch's own
    /// `intent_file` input (same generic mechanism `plan.sites`'s
    /// `{{workspace}}` resolves through). `crawl.json`'s tasks never set
    /// this, and a review-v2 launch with no `intent_file` param leaves it
    /// `None` too (an unset optional input's placeholder key is omitted at
    /// mint, not substituted as an empty string). When present, its
    /// content is read at dispatch time and rendered into the unit's
    /// message — see `build_message`'s `intent` parameter.
    pub intent_file: Option<PathBuf>,
    /// (#2310 P4c-2b) How many times to dispatch this unit — the measured
    /// k-draw recall technique DESIGN.md's "Units" section ports from the
    /// retired funnel ("A draws-per-unit knob, off by default"). `1`
    /// (the default) is BYTE-IDENTICAL to every pre-#2310-P4c-2b behavior:
    /// one dispatch, `ctx.session_id` unchanged, `host_out` unchanged — see
    /// `run`'s own doc on why draw 0 alone is never renamed. `>1` draws
    /// each get their OWN session id (`<session>-d<n>`, `n` starting at 2)
    /// and their own `out/`/readback paths — the finding store addresses a
    /// finding by `<dispatch session>/<seq>`, so two draws sharing one
    /// session id would silently collide (`findings::materialize` is
    /// write-once; the second draw's real findings would be reported
    /// `AlreadyPresent` and lost).
    pub draws: usize,
}

impl UnitStepConfig {
    pub fn from_step(step: &Step) -> Result<Self> {
        let str_field = |key: &str| -> Result<String> {
            step.config
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .ok_or_else(|| anyhow!("step `{}`: `{CRAWL_UNIT_KIND}` requires config.{key}", step.id))
        };
        let plan = str_field("plan")?;
        let unit = str_field("unit")?;
        let rule = step.config.get("rule").and_then(|v| v.as_str()).map(String::from);
        let no_progress_turns = match step.config.get("no_progress_turns") {
            Some(v) => usize::try_from(v.as_u64().ok_or_else(|| {
                anyhow!(
                    "step `{}`: `{CRAWL_UNIT_KIND}` config.no_progress_turns must be a \
                     non-negative integer, got {v}",
                    step.id
                )
            })?)
            .context("no_progress_turns does not fit usize")?,
            None => DEFAULT_NO_PROGRESS_TURNS,
        };
        let timeout_seconds = step
            .config
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
        // Empty-string filtered (matches `str_field`'s convention above):
        // an unresolved `{{intent_file}}` template on a launch with no
        // `intent_file` param renders as `""`, which must read as ABSENT,
        // not as a path to read-and-fail-and-warn about on every run.
        let intent_file = step
            .config
            .get("intent_file")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        // (#2310 P4c-2b) String-or-number, leniently — the same parse
        // `crawl::plan::parse_sizing_and_no_fetch` uses, for the same
        // reason: a `--param draws=3` reaches step config as a JSON
        // string, never a number.
        let draws = match step.config.get("draws") {
            None => 1,
            Some(serde_json::Value::Null) => 1,
            Some(v) => {
                let n = v
                    .as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                    .ok_or_else(|| {
                        anyhow!(
                            "step `{}`: `{CRAWL_UNIT_KIND}` config.draws must be a positive integer, got {v}",
                            step.id
                        )
                    })?;
                anyhow::ensure!(n >= 1, "step `{}`: `{CRAWL_UNIT_KIND}` config.draws must be >= 1, got {n}", step.id);
                usize::try_from(n).context("draws does not fit usize")?
            }
        };
        Ok(Self { plan, unit, rule, no_progress_turns, timeout_seconds, intent_file, draws })
    }
}

/// Everything one unit's dispatch and finding readback needs, resolved
/// from the plan before anything runs.
struct UnitContext {
    workspace: String,
    unit_id: String,
    source: String,
    sha: String,
    rule_ids: Vec<String>,
    session_id: String,
    /// The materialized workspace tree ROOT — the parent of this unit's
    /// own source tree, so the container's `/workspace/<source>/…` paths
    /// (which every message renders) resolve.
    tree_root: PathBuf,
}

/// Resolve one unit out of an already-loaded plan: its source's sha and
/// tree root, and the rule ids it names. Every failure names the unit.
fn unit_context(the_plan: &Plan, unit: &Unit, mission_id: &str) -> Result<UnitContext> {
    let source = match unit {
        Unit::Site { source, .. } | Unit::Read { source, .. } | Unit::Edge { source, .. } => source.clone(),
    };
    let ps = the_plan.sources.iter().find(|s| s.id == source).ok_or_else(|| {
        anyhow!(
            "`{CRAWL_UNIT_KIND}`: unit `{}` names source `{source}`, which the plan's `sources` \
             list does not declare — re-plan this rule",
            unit.id()
        )
    })?;
    // An empty sha must never reach a dispatch: every finding is stamped
    // with it and the tracker keys sightings on it, so a dispatch under
    // `sha: ""` would silently produce unversioned findings.
    if ps.sha.trim().is_empty() {
        bail!(
            "`{CRAWL_UNIT_KIND}`: unit `{}` names source `{source}`, whose plan entry records an \
             empty sha — a finding stamped with no sha is unversioned; re-plan this rule",
            unit.id()
        );
    }
    let tree_root = ps.tree.parent().map(Path::to_path_buf).ok_or_else(|| {
        anyhow!(
            "`{CRAWL_UNIT_KIND}`: unit `{}` names source `{source}`, whose recorded tree `{}` has \
             no parent to mount as the workspace root",
            unit.id(),
            ps.tree.display()
        )
    })?;
    Ok(UnitContext {
        workspace: the_plan.workspace.clone(),
        unit_id: unit.id().to_string(),
        source,
        sha: ps.sha.clone(),
        rule_ids: unit_rules(unit),
        session_id: format!("crawl-{mission_id}-{}", unit.id()),
        tree_root,
    })
}

/// The rule ids one unit names, in the plan's own order.
pub fn unit_rules(u: &Unit) -> Vec<String> {
    match u {
        Unit::Site { rule, .. } | Unit::Edge { rule, .. } => vec![rule.clone()],
        Unit::Read { rules, .. } => rules.clone(),
    }
}

/// (#1959) A payload's `rule`: ONE id when the unit has exactly one rule
/// (site and edge units always do) so a receiver can key on it; `null` for
/// a multi-rule read unit, whose `rules` array lists them and whose
/// findings name their own `pattern`.
fn single_rule(rule_ids: &[String]) -> Value {
    match single_rule_id(rule_ids) {
        Some(only) => json!(only),
        None => Value::Null,
    }
}

/// The typed twin of [`single_rule`], for [`UnitOutcome::rule`].
fn single_rule_id(rule_ids: &[String]) -> Option<String> {
    match rule_ids {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// (#2360) The rule-namespaced path component for one unit's on-disk
/// home: `units/<rule_dir>/<unit_id>/` and the mission-root
/// `<rule_dir>.<unit_id>.findings.jsonl`. A per-rule plan numbers its own
/// units from `u-0001`, so two different rules' plans routinely mint the
/// SAME unit id in one mission — the rule component is what keeps their
/// on-disk homes from colliding (live evidence: mission
/// `review-v2-1788566897-9c149e`, 2026-09-05, 4 of 5 units errored with
/// "caller-provided out-dir already exists"). Prefers the step's OWN
/// `config.rule` — set on every grown unit task in both `crawl.json` and
/// `review-v2.json` (see `UnitStepConfig::rule`), and already checked a
/// few lines above `run`'s own call site to agree with the plan — over
/// re-deriving it from the plan unit's rule id(s), so the two never
/// silently disagree. Falls back to the plan unit's own rule ids only
/// when the step config left `rule` unset (not a shape either built-in
/// mission config produces today, but a multi-rule read unit's own
/// `rule_ids` still yields a stable, non-colliding component rather than
/// panicking).
fn unit_rule_dir(declared: Option<&str>, rule_ids: &[String]) -> String {
    declared.map(str::to_string).or_else(|| single_rule_id(rule_ids)).unwrap_or_else(|| rule_ids.join("+"))
}

/// (#2310 P4c) Same "one id or null" shape as [`single_rule`], but the
/// resolved rule's `confirm` form (`"mod"`/`"search"`/`"question"`) rather
/// than its id — stamped into `record_context` (host-known, never
/// model-supplied) so a downstream reader of a finding this unit produced
/// knows which of the three ways it was meant to be confirmed WITHOUT
/// re-resolving the rule registry itself. `null` for a multi-rule read
/// unit (ambiguous which rule a given finding answers to without the
/// finding's own `pattern`, same reasoning `single_rule` already carries)
/// or when the single rule id names nothing in `rules_by_id` (the
/// `missing` rule case `build_message` already refuses loudly before this
/// is ever reached in practice).
fn single_confirm(rules_by_id: &BTreeMap<String, Rule>, rule_ids: &[String]) -> Value {
    match single_rule_id(rule_ids).and_then(|id| rules_by_id.get(&id)) {
        Some(rule) => json!(match rule.confirm {
            darkmux_crew::rules::ConfirmForm::Mod => "mod",
            darkmux_crew::rules::ConfirmForm::Search => "search",
            darkmux_crew::rules::ConfirmForm::Question => "question",
        }),
        None => Value::Null,
    }
}

impl StepKind for CrawlUnitStepKind {
    fn id(&self) -> &'static str {
        CRAWL_UNIT_KIND
    }

    fn display_name(&self) -> &'static str {
        "Crawl unit"
    }

    /// (#2301) Port labels ARE the wrapper kinds — see
    /// `plan_step::CrawlPlanStepKind::provides`.
    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(crate::crawl::plan_step::CRAWL_PLAN_OUTPUT_KIND)];
        &PORTS
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(UNIT_OUTCOME_KIND)];
        &PORTS
    }

    /// (#2321) Declare where the unit will run, in the terms the scheduler's
    /// wave packer reads. A kind that stays silent here is queued as a REMOTE
    /// job under `remote_cap` (1 on the launch path), which is how sibling
    /// units of one plan ran strictly one at a time on an already-resident
    /// model — 3× the wall-clock of the same three units wave-packed. The
    /// dispatch below runs the Task's OWN `role_id` (#2310 P4c — see `run`'s
    /// own doc on the same generalization) with no explicit profile
    /// (`profile_name: None`), which the dispatch resolves as `role_profiles.
    /// <role>` first, `default_profile` second — and `resolve_local_placement`
    /// now resolves the very same way (#2329 review), so the wave leases the
    /// model the dispatch will actually use. A registry that cannot resolve
    /// yields `None` (one stderr warning per unit) and the units fall back to
    /// the remote queue; the dispatch then surfaces the real error itself.
    fn residency(
        &self,
        step: &Step,
        task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> Option<darkmux_crew::step_kinds::Placement> {
        darkmux_crew::step_kinds::resolve_local_placement(
            task.role_id.as_deref().unwrap_or("crawler"),
            None,
            None,
            &format!("step:{}", step.id),
        )
    }
    fn run(&self, step: &Step, task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let cfg = UnitStepConfig::from_step(step)?;
        let mission_id = mission_id_for(task)?;
        let run_dir = darkmux_crew::loader::missions_dir().join(&mission_id);

        // The read IS the check: the content id is verified before the
        // body, and the body is read through `Plan`.
        let the_plan = darkmux_crew::step_output::Output::<Plan>::read(
            &cfg.plan,
            crate::crawl::plan_step::CRAWL_PLAN_OUTPUT_KIND,
        )
        .with_context(|| format!("`{CRAWL_UNIT_KIND}`: step `{}` reading its plan", step.id))?
        .body;
        let unit = the_plan.units.iter().find(|u| u.id() == cfg.unit).ok_or_else(|| {
            anyhow!(
                "`{CRAWL_UNIT_KIND}`: the plan has no unit `{}` (it holds {} unit(s)) — the step's \
                 `unit` must name one the plan it points at actually planned",
                cfg.unit,
                the_plan.units.len()
            )
        })?;
        let ctx = unit_context(&the_plan, unit, &mission_id)?;
        // The step's own `rule` (the grow item's `rule`, when present) must
        // agree with the plan — a silent mismatch would stamp every finding
        // with the wrong rule id, which is the key the tracker dedups on.
        if let Some(declared) = &cfg.rule {
            if !ctx.rule_ids.iter().any(|r| r == declared) {
                bail!(
                    "`{CRAWL_UNIT_KIND}`: step config names rule `{declared}`, but plan unit `{}` \
                     names {:?} — the step and the plan disagree about what this unit is for",
                    ctx.unit_id,
                    ctx.rule_ids
                );
            }
        }

        let (rules_vec, warnings) = rules::resolve_default(&ctx.rule_ids)?;
        for w in warnings.iter().filter(|w| ctx.rule_ids.iter().any(|r| w.contains(r.as_str()))) {
            eprintln!("{}", darkmux_types::style::warn(&format!("crawl.unit: {w}")));
        }
        let rules_by_id: BTreeMap<String, Rule> = rules_vec.into_iter().map(|r| (r.id.clone(), r)).collect();
        // (#2310 P4c review round 2, item (e)) A read failure (missing
        // file, not UTF-8) is a named warning, never a dispatch failure —
        // same leniency posture as every other optional-input read in
        // this function.
        let intent_text = cfg.intent_file.as_ref().and_then(|p| match std::fs::read_to_string(p) {
            Ok(text) => Some(text),
            Err(e) => {
                eprintln!(
                    "{}",
                    darkmux_types::style::warn(&format!("crawl.unit: reading intent_file {}: {e}", p.display()))
                );
                None
            }
        });
        let message = build_message(&rules_by_id, unit, intent_text.as_deref())?;

        // (#2360) Namespace this unit's on-disk home by rule FIRST: a
        // per-rule plan numbers its own units from `u-0001`, so two
        // different rules routinely grow a unit named `u-0001` into the
        // SAME mission — see `unit_rule_dir`'s own doc for the live
        // collision this closes.
        let rule_dir = unit_rule_dir(cfg.rule.as_deref(), &ctx.rule_ids);

        // Mint this unit's own out dir BEFORE dispatch (#2153): the dir is
        // then known and recorded even if the dispatch returns `Err` with
        // no `DispatchResult::out_dir` to read back.
        let unit_dir = run_dir.join("units").join(&rule_dir).join(&ctx.unit_id);
        std::fs::create_dir_all(&unit_dir).with_context(|| format!("creating {}", unit_dir.display()))?;
        let host_out = unit_dir.join("out");

        // (#2310 P4c) The Task's OWN `role_id` — `"crawler"` for every
        // crawl.json task, unchanged from before this generalization; a
        // review-v2.json task instead declares `"role_id": "reviewer"`,
        // which is what makes this ONE step kind ("Units. Already
        // generic: a map step over units with a role that carries the
        // finding tool" — DESIGN.md) actually reusable rather than only
        // described as reusable.
        let role_id = task.role_id.clone().unwrap_or_else(|| "crawler".to_string());
        let seat = resolve_crawler_seat(&role_id);

        // (#2310 P4c-2b) `draws` dispatches this SAME unit `cfg.draws`
        // times — DESIGN.md "Units... A draws-per-unit knob, off by
        // default, ports the measured k-draw recall technique from the
        // funnel." Draw 0 uses `ctx.session_id`/`host_out` UNCHANGED from
        // every pre-#2310-P4c-2b run (so `draws: 1`, the default, is
        // byte-identical); draw `n` (n>=1) gets its own session id
        // (`<session>-d<n+1>`) and its own `out-d<n+1>/` — see
        // `UnitStepConfig::draws`'s own doc for why they cannot share one.
        // A draw that does not converge (result != "stop"/budget-exhausted)
        // aborts the WHOLE step immediately, same as a single-draw run
        // always has — later draws are simply never attempted.
        let mut total_wall_ms = 0u64;
        let mut total_prompt_tokens = 0u64;
        let mut total_completion_tokens = 0u64;
        let mut total_findings = 0u64;
        let mut total_rejected = 0u64;
        let mut all_finding_refs: Vec<FindingRef> = Vec::new();
        let mut last_result = String::new();
        let mut last_model: Option<String> = None;
        let mut last_detections = None;
        let mut last_rest_ms = 0u64;
        let mut last_host = None;

        for draw in 0..cfg.draws {
            let (draw_session_id, draw_host_out) = if draw == 0 {
                (ctx.session_id.clone(), host_out.clone())
            } else {
                (format!("{}-d{}", ctx.session_id, draw + 1), unit_dir.join(format!("out-d{}", draw + 1)))
            };
            let started = std::time::Instant::now();
            let opts = DispatchOpts {
                brief_refs: Vec::new(),
                role_id: role_id.clone(),
                message: message.clone(),
                session_id: Some(draw_session_id.clone()),
                timeout_seconds: cfg.timeout_seconds.unwrap_or(600),
                skip_preflight: false,
                json: true,
                workdir: Some(ctx.tree_root.clone()),
                phase_id: Some(task.phase_id.clone()),
                machine: None,
                wait: true,
                compaction: CompactionDispatchArgs::default(),
                profile_name: None,
                config_path: None,
                force_container: false,
                max_completion_tokens: None,
                image: None,
                model_base_url_override: None,
                step_id: Some(step.id.clone()),
                system_prompt_override: None,
                workspace_read_only: true,
                host_out: Some(draw_host_out.clone()),
                // (#2301) Per-unit resume is the scheduler's step-output reuse
                // now, not this kind's own parameter — see the issue.
                resume_from: None,
                max_turns_override: Some(default_unit_max_turns(unit)),
                // Provenance the runtime cannot know — merged by the host
                // tailer under `payload.context` on every record this unit's
                // dispatch produces.
                record_context: Some(json!({
                    "workspace": ctx.workspace,
                    "source": ctx.source,
                    "sha": ctx.sha,
                    "rule": single_rule(&ctx.rule_ids),
                    "rules": ctx.rule_ids,
                    // (#2310 P4c review round 2, item (g) — stated precisely
                    // for the PR: `crawl.json`'s DISPATCHED MESSAGE
                    // (`build_message`'s own output) is byte-identical to
                    // before this packet for every `mod`-confirm rule — which
                    // is all four of crawl's own built-ins — since
                    // `pattern_block`'s confirm-form appendix renders nothing
                    // for `ConfirmForm::Mod`. The one thing that DOES change
                    // for every crawl unit, `mod`-confirm included, is this
                    // flow record's `context` blob: it gains exactly ONE new
                    // key, `confirm`, host-stamped here (never model-supplied
                    // — see `pattern_block`'s own doc for why the confirmation
                    // form rides in `context.confirm` rather than a
                    // `create_finding` tool argument). No other `context` key
                    // changes shape or value.
                    "confirm": single_confirm(&rules_by_id, &ctx.rule_ids),
                    "unit": ctx.unit_id,
                    "model": seat.model.clone(),
                    "locality": seat.locality,
                    "profile": seat.profile_name.clone(),
                })),
            };

            let outcome = (self.dispatch)(opts);
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (mut result, wall_ms, prompt_tokens, completion_tokens, model, detections, rest_ms, host) =
                match &outcome {
                    Err(_) => ("error".to_string(), elapsed_ms, 0, 0, None, None, 0, None),
                    Ok(res) => interpret_dispatch_result(&ctx.unit_id, res),
                };

            // (#2193) No-progress bound — only over a dispatch that actually
            // ran and reported a clean `"stop"`. Never overrides an already-
            // `error`/`timeout` label: those are more specific failure shapes.
            let out_dir = outcome.as_ref().ok().and_then(|r| r.out_dir.clone());
            if result == "stop" {
                if let Some(d) = &out_dir {
                    if unit_hit_no_progress_bound(d, cfg.no_progress_turns) {
                        result = UNIT_BUDGET_EXHAUSTED.to_string();
                    }
                }
            }

            let exclusions = out_dir.as_deref().map(count_rejected_create_findings).unwrap_or(0);
            // (#2302) ONE read of `findings.jsonl` yields both the count and
            // the keys. (#2360) Rule-namespaced for the SAME reason
            // `unit_dir` above is: `<rule>.<unit>.findings.jsonl`, not
            // `<unit>.findings.jsonl` — otherwise a second rule growing
            // the same unit id would collide on this mission-root file too.
            let readback_into = if draw == 0 {
                run_dir.join(format!("{rule_dir}.{}.findings.jsonl", ctx.unit_id))
            } else {
                run_dir.join(format!("{rule_dir}.{}.findings.jsonl.d{}", ctx.unit_id, draw + 1))
            };
            let (findings, finding_refs) = match &out_dir {
                Some(d) => readback_findings(&ctx, d, &readback_into, model.as_deref(), &draw_session_id),
                None => (0, Vec::new()),
            };

            total_wall_ms += wall_ms;
            total_prompt_tokens += prompt_tokens;
            total_completion_tokens += completion_tokens;
            total_findings += findings as u64;
            total_rejected += exclusions as u64;
            all_finding_refs.extend(finding_refs);
            last_result = result.clone();
            last_model = model.clone();
            last_detections = detections;
            last_rest_ms = rest_ms;
            last_host = host;

            // A unit that did not converge fails its STEP: the run's
            // outcome must not read clean when a unit errored. A
            // budget-exhausted unit is a BOUND, not a failure, and
            // completes (the summary counts it in its own bucket) — the
            // same distinction the retired launcher drew between
            // `units_errored` and `units_budget_exhausted`. A later draw
            // is never attempted once an earlier one fails this way.
            if result != "stop" && result != UNIT_BUDGET_EXHAUSTED {
                let detail = match &outcome {
                    Err(e) => format!("{e:#}"),
                    Ok(_) => format!("dispatch ended `{result}`"),
                };
                let partial = UnitOutcome {
                    schema_version: UNIT_OUTCOME_SCHEMA_VERSION.to_string(),
                    unit: ctx.unit_id.clone(),
                    rule: single_rule_id(&ctx.rule_ids),
                    source: ctx.source.clone(),
                    result: result.clone(),
                    findings: total_findings,
                    findings_rejected: total_rejected,
                    wall_ms: total_wall_ms,
                    prompt_tokens: total_prompt_tokens,
                    completion_tokens: total_completion_tokens,
                    model: last_model.clone(),
                    out_dir: host_out.display().to_string(),
                    rules: ctx.rule_ids.clone(),
                    workspace: ctx.workspace.clone(),
                    sha: ctx.sha.clone(),
                    rest_ms: last_rest_ms,
                    reason: None,
                    detections: last_detections.clone(),
                    host: last_host.clone(),
                    finding_refs: dedup_finding_refs(all_finding_refs),
                };
                return Err(anyhow!(
                    "`{CRAWL_UNIT_KIND}`: unit `{}` ended `{result}` on draw {} of {} — {detail} (outcome: {})",
                    ctx.unit_id,
                    draw + 1,
                    cfg.draws,
                    serde_json::to_string(&partial).unwrap_or_default()
                ));
            }
        }

        // (#2310 P4c-2b) Dedup ACROSS draws before this becomes the
        // step's own output — a create-mods phase grows one coder task
        // PER `finding_refs` entry (`crawl.summary`'s own `finding_refs`
        // union), so an undeduped multi-draw unit would dispatch a coder
        // twice at the "same" finding two draws independently observed.
        let finding_refs = dedup_finding_refs(all_finding_refs);

        let outcome_record = UnitOutcome {
            schema_version: UNIT_OUTCOME_SCHEMA_VERSION.to_string(),
            unit: ctx.unit_id.clone(),
            rule: single_rule_id(&ctx.rule_ids),
            source: ctx.source.clone(),
            result: last_result,
            findings: total_findings,
            findings_rejected: total_rejected,
            wall_ms: total_wall_ms,
            prompt_tokens: total_prompt_tokens,
            completion_tokens: total_completion_tokens,
            model: last_model,
            out_dir: host_out.display().to_string(),
            rules: ctx.rule_ids.clone(),
            workspace: ctx.workspace.clone(),
            sha: ctx.sha.clone(),
            rest_ms: last_rest_ms,
            // A row this kind produced itself always has its numbers; only
            // the summary's errored rows carry a reason.
            reason: None,
            // Present only when the dispatch actually produced them — an
            // omitted key reads as "never sampled", a null as "sampled
            // zero".
            detections: last_detections,
            host: last_host,
            finding_refs,
        };

        Ok(StepOutcome {
            output: darkmux_crew::step_output::Output::wrap(
                UNIT_OUTCOME_KIND,
                outcome_record,
                darkmux_crew::step_output::Producer::of(&mission_id, &task.id, &step.id),
            )
            .to_output_string()?,
            flow_records: Vec::new(),
        })
    }
}

/// (#2310 P4c-2b) Dedup [`FindingRef`]s across a unit's draws — DESIGN.md
/// "A draws-per-unit knob... ports the measured k-draw recall technique
/// from the funnel", and the funnel's own dedup key is what two draws
/// independently observing the SAME underlying issue share: the rule and
/// the exact window (`rule`, `file`, `line`) — NEVER the store `key`
/// (`<dispatch session>/<seq>`), which is unique per draw by construction
/// and so could never collide. First occurrence (draw order) wins; a
/// finding whose `file`/`line` is absent (a rare, non-windowed report)
/// dedups on `(rule, None, None)`, which still merges two IDENTICAL
/// windowless reports of the same rule rather than doubling them.
fn dedup_finding_refs(refs: Vec<FindingRef>) -> Vec<FindingRef> {
    let mut seen: std::collections::BTreeSet<(String, Option<String>, Option<u64>)> = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(refs.len());
    for r in refs {
        let finding_key = (r.rule.clone(), r.file.clone(), r.line);
        if seen.insert(finding_key) {
            out.push(r);
        }
    }
    out
}

// ── the `crawl.summary` step kind ────────────────────────────────────────

pub struct CrawlSummaryStepKind;

impl StepKind for CrawlSummaryStepKind {
    fn id(&self) -> &'static str {
        CRAWL_SUMMARY_KIND
    }

    fn display_name(&self) -> &'static str {
        "Crawl summary"
    }

    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(UNIT_OUTCOME_KIND)];
        &PORTS
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(CRAWL_SUMMARY_OUTPUT_KIND)];
        &PORTS
    }

    /// Fan-in over every `crawl.unit` step this MISSION ran.
    ///
    /// Deliberately NOT `gather_inputs`: a unit task is GROWN from the
    /// plan at the phase boundary (#2300), so its id does not exist when
    /// `crawl.json` is authored and no statically-declared `reads` can
    /// name it. The mission's own step records are the one place the whole
    /// set is knowable, and they are already on disk by the time this
    /// phase runs.
    fn run(&self, step: &Step, task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let mission_id = mission_id_for(task)?;
        let summary = summarize_mission(&mission_id)?;
        Ok(StepOutcome {
            output: darkmux_crew::step_output::Output::wrap(
                CRAWL_SUMMARY_OUTPUT_KIND,
                summary,
                darkmux_crew::step_output::Producer::of(&mission_id, &task.id, &step.id),
            )
            .to_output_string()?,
            flow_records: Vec::new(),
        })
    }
}

/// (#2301) The typed crawl run summary — `crawl.summary`'s body, and the
/// mission's `mission close` payload.
///
/// Every key the retired launcher's close payload carried is here under the
/// same name, so a reader keyed on `units_completed`/`findings`/
/// `stopped_by`/`tokens_per_hour` keeps working.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CrawlSummary {
    pub schema_version: String,
    pub mission_id: String,
    pub workspace: String,
    pub units_in_plan: usize,
    /// Every planned unit is grown into a task, so the selection IS the
    /// plan; `--param rules=` narrows which RULES are planned at all
    /// (pruned before the mint, reported in `graph-report.json`), never
    /// which of a plan's units run.
    pub units_selected: usize,
    pub units_not_run: usize,
    pub units_completed: usize,
    pub units_errored: usize,
    pub units_interrupted: usize,
    pub units_budget_exhausted: usize,
    /// The scheduler runs every grown unit; there is no between-units skip
    /// loop to stop early any more, so this is always 0. Kept, present, for
    /// readers keyed on the retired launcher's shape.
    pub units_skipped: usize,
    pub findings: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub wall_ms: u64,
    pub tokens_per_hour: u64,
    pub stopped_by: String,
    pub est_tokens: u64,
    pub model: Option<String>,
    pub profile: Option<String>,
    #[serde(default)]
    pub sources: Vec<PlanSourceRef>,
    /// One row per unit, in the order the run's step records were read.
    #[serde(default)]
    pub units: Vec<UnitOutcome>,
    /// (#2302) Every finding this run recorded, named by store key — the
    /// union over [`Self::units`] in unit order, and the array a create-mods phase
    /// phase GROWS one task from (`grow: { from: "summary", items:
    /// "finding_refs" }`). `findings` above stays the COUNT under the name
    /// the retired launcher's close payload used; this is the roster.
    #[serde(default)]
    pub finding_refs: Vec<FindingRef>,
    /// (#2310 P4c-2b PR #2357 round-2 review item 5) Rule ids (falling
    /// back to the step id when a rule can't be named) whose `crawl.plan`
    /// step did NOT reach `Complete` this run. `crawl.json` shares
    /// `src/mission_launch.rs::grow_phase` with `review-v2.json`: since
    /// that function stopped `bail!`ing the whole launch on an errored
    /// grow producer (#2310 P4c-2b MUST FIX C) and instead grows zero
    /// units from the affected rule, a crawl whose plan step failed now
    /// reaches `summarize` with a SILENTLY smaller `units_in_plan` and no
    /// way to tell "the rule matched nothing" from "the rule's plan
    /// step crashed". This field is that distinction, named.
    #[serde(default)]
    pub plans_errored: Vec<String>,
}

/// The source identity a summary reports — id + the sha it was cut at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct PlanSourceRef {
    pub id: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
}

/// [`CrawlSummary`]'s own schema version.
pub const CRAWL_SUMMARY_SCHEMA_VERSION: &str = "1.2";

/// Build the crawl's run totals from what this mission's `crawl.unit`
/// steps recorded, plus what its `plan/` directory planned.
///
/// Every unit output is read through [`UnitOutcome`], inside the
/// `Output<UnitOutcome>` envelope its producer wrapped it in — the read IS
/// the validation. A step whose recorded `output` is present but does not
/// deserialize is a REFUSAL naming the field, not a zero silently folded
/// into the totals: a summary that quietly under-counts is worse than one
/// that stops. A step with NO output at all (its kind returned `Err`, so
/// nothing was recorded) is an honest error row instead — absent is a known
/// outcome, malformed is drift.
pub fn summarize_mission(mission_id: &str) -> Result<CrawlSummary> {
    let run_dir = darkmux_crew::loader::missions_dir().join(mission_id);
    let mut rows: Vec<UnitOutcome> = Vec::new();
    // (#2310 P4c-2b PR #2357 round-2 review item 5) Every `crawl.plan`
    // step that did NOT reach `Complete` — see `CrawlSummary::
    // plans_errored`'s own doc for why this is named separately from
    // "units_in_plan" silently shrinking.
    let mut plans_errored: Vec<String> = Vec::new();

    let phases = darkmux_crew::loader::load_phases().context("loading phase records to find the run's units")?;
    for phase in phases.iter().filter(|p| p.mission_id == mission_id) {
        let Ok(steps) = darkmux_crew::lifecycle::load_steps_for_phase(mission_id, &phase.id) else { continue };
        for step in steps.iter().filter(|s| s.kind == crate::crawl::plan_step::CRAWL_PLAN_KIND) {
            if step.status != darkmux_crew::types::NodeStatus::Complete {
                let rule = step
                    .config
                    .get("rule")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| step.id.clone());
                plans_errored.push(rule);
            }
        }
        for step in steps.iter().filter(|s| s.kind == CRAWL_UNIT_KIND) {
            // (#2301 review, MUST FIX) Branch on STATUS, not on whether an
            // output is present. The scheduler writes a failing kind's own
            // ERROR TEXT into `step.output` (`scheduler.rs`'s `Err` arm),
            // so an errored unit has a non-empty output that is not a
            // `UnitOutcome` — reading every output typed made ONE failed
            // unit refuse the whole summary, and with it the mission's
            // close payload. A `Complete` step is the only one that
            // promised numbers, so it is the only one read typed (and a
            // drifted producer is still refused there); anything else is an
            // errored row carrying that text as its `reason`.
            if step.status != darkmux_crew::types::NodeStatus::Complete {
                rows.push(errored_row(step));
                continue;
            }
            let raw = step.output.as_deref().map(str::trim).filter(|o| !o.is_empty());
            match raw {
                Some(raw) => {
                    let parsed = darkmux_crew::step_output::Output::<UnitOutcome>::read(raw, UNIT_OUTCOME_KIND)
                        .with_context(|| {
                            format!(
                                "`{CRAWL_SUMMARY_KIND}`: step `{}` completed but recorded an output \
                                 that is not a `UnitOutcome` — every `{CRAWL_UNIT_KIND}` output is \
                                 read through that struct, so a producer that drifted is refused \
                                 here rather than summarized as zeros",
                                step.id
                            )
                        })?;
                    rows.push(parsed.body);
                }
                // Complete with nothing recorded: not drift, just nothing
                // to add. Counted, with no numbers claimed.
                None => rows.push(errored_row(step)),
            }
        }
    }

    let count = |want: &str| rows.iter().filter(|r| r.result == want).count();
    let prompt_tokens: u64 = rows.iter().map(|r| r.prompt_tokens).sum();
    let completion_tokens: u64 = rows.iter().map(|r| r.completion_tokens).sum();
    let wall_ms: u64 = rows.iter().map(|r| r.wall_ms).sum();
    let findings: u64 = rows.iter().map(|r| r.findings).sum();
    let wall_hours = (wall_ms as f64) / 1000.0 / 3600.0;
    let tokens_per_hour = if wall_hours > 0.0 {
        ((prompt_tokens + completion_tokens) as f64 / wall_hours).round() as u64
    } else {
        0
    };

    let (units_in_plan, est_tokens, sources, workspace) = plan_totals(&run_dir.join("plan"));
    let units_completed = count("stop");
    let units_budget_exhausted = count(UNIT_BUDGET_EXHAUSTED);
    let units_interrupted = count("interrupted");
    let units_errored = rows.len() - units_completed - units_budget_exhausted - units_interrupted;

    Ok(CrawlSummary {
        schema_version: CRAWL_SUMMARY_SCHEMA_VERSION.to_string(),
        mission_id: mission_id.to_string(),
        workspace,
        units_in_plan,
        units_selected: units_in_plan,
        units_not_run: units_in_plan.saturating_sub(rows.len()),
        units_completed,
        units_errored,
        units_interrupted,
        units_budget_exhausted,
        units_skipped: 0,
        findings,
        prompt_tokens,
        completion_tokens,
        wall_ms,
        tokens_per_hour,
        stopped_by: if units_errored > 0 { "error".into() } else { "done".into() },
        est_tokens,
        model: rows.iter().find_map(|r| r.model.clone()),
        // (#2310 P4c) `crawl.summary` is not reused by review-v2 (its own
        // phase declares no summary task) — hardcoded "crawler" here is
        // unchanged behavior, not a gap.
        profile: resolve_crawler_seat("crawler").profile_name,
        sources,
        finding_refs: rows.iter().flat_map(|r| r.finding_refs.iter().cloned()).collect(),
        units: rows,
        plans_errored,
    })
}

/// The honest zero row for a unit step that produced no `UnitOutcome` —
/// its kind returned `Err`, so its numbers never existed.
///
/// The unit id comes from the step's own `config.unit` (what the grow seam
/// stamped there), falling back to the step id: a summary that named the
/// step instead of the unit would be unjoinable against the plan, which is
/// the one thing an operator wants from a failed row. The scheduler's
/// recorded error text becomes `reason`, so the run says WHY without
/// anyone opening a second file.
fn errored_row(step: &Step) -> UnitOutcome {
    let unit = step
        .config
        .get("unit")
        .and_then(Value::as_str)
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| step.id.clone());
    let rule = step.config.get("rule").and_then(Value::as_str).map(str::to_string);
    UnitOutcome {
        schema_version: UNIT_OUTCOME_SCHEMA_VERSION.to_string(),
        unit,
        rules: rule.iter().cloned().collect(),
        rule,
        source: String::new(),
        result: match step.status {
            darkmux_crew::types::NodeStatus::Abandoned => "interrupted".to_string(),
            darkmux_crew::types::NodeStatus::Error => "error".to_string(),
            // Planned/Running at summary time: the step never settled.
            _ => "not_run".to_string(),
        },
        findings: 0,
        findings_rejected: 0,
        wall_ms: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        model: None,
        out_dir: String::new(),
        workspace: String::new(),
        sha: String::new(),
        rest_ms: 0,
        reason: step.output.as_deref().map(str::trim).filter(|o| !o.is_empty()).map(str::to_string),
        detections: None,
        host: None,
        // (#2302) A row this summary BUILT names no findings: the step
        // recorded no outcome, so nothing was observed to address.
        finding_refs: Vec::new(),
    }
}



/// Read every `plan/<rule>.json` this run wrote: how many units were
/// planned across all of them, their estimated token total, and the
/// sources they were planned against. An unreadable/unparseable plan file
/// is skipped — the summary is descriptive, and refusing to summarize a
/// finished run because one plan file went missing helps nobody.
fn plan_totals(plan_dir: &Path) -> (usize, u64, Vec<PlanSourceRef>, String) {
    let mut units = 0usize;
    let mut est_tokens = 0u64;
    let mut sources: BTreeMap<String, PlanSourceRef> = BTreeMap::new();
    let mut workspace = String::new();
    let Ok(entries) = std::fs::read_dir(plan_dir) else {
        return (0, 0, Vec::new(), workspace);
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "json")) {
        let Ok(p) = darkmux_crew::step_output::Output::<Plan>::read_path(
            path,
            crate::crawl::plan_step::CRAWL_PLAN_OUTPUT_KIND,
        )
        .map(|o| o.body) else {
            continue;
        };
        if workspace.is_empty() {
            workspace = p.workspace.clone();
        }
        units += p.units.len();
        est_tokens += p.units.iter().map(|u| u.est_tokens() as u64).sum::<u64>();
        for s in &p.sources {
            sources.entry(s.id.clone()).or_insert_with(|| PlanSourceRef {
                id: s.id.clone(),
                sha: s.sha.clone(),
                git_ref: s.git_ref.clone(),
            });
        }
    }
    (units, est_tokens, sources.into_values().collect(), workspace)
}

/// The mission a step's task belongs to, resolved through its phase
/// record. A phase with no record is a refusal, not a guess — the same
/// rule `plan_step::default_plan_path` applies.
fn mission_id_for(task: &Task) -> Result<String> {
    let phases = darkmux_crew::loader::load_phases().context("loading phase records to locate the run")?;
    phases
        .iter()
        .find(|p| p.id == task.phase_id)
        .map(|p| p.mission_id.clone())
        .ok_or_else(|| {
            anyhow!(
                "crawl step: task `{}` names phase `{}`, which has no record — the run cannot be \
                 located",
                task.id,
                task.phase_id
            )
        })
}

/// Register the crawl's dispatch-side step kinds. Called from
/// [`crate::crawl::plan_step::register_crawl_kinds`].
pub fn register(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(CrawlUnitStepKind::production())).context("registering crawl.unit")?;
    registry.register(Arc::new(CrawlSummaryStepKind)).context("registering crawl.summary")?;
    Ok(())
}

#[cfg(test)]
#[path = "unit_step_tests.rs"]
mod tests;

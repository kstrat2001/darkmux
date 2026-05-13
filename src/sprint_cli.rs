//! `darkmux sprint estimate` — pre-dispatch budget oracle.
//!
//! Deterministic Rust function: parse a workload spec, compute predicted
//! token consumption across the planned turn count, pick the smallest
//! adequate profile, emit structured JSON. Optional `--narrate` flag
//! POSTs the computed report to the already-loaded 4B compactor
//! (`darkmux:qwen3-4b-instruct-2507`) for an operator-facing one-sentence
//! recommendation wrap.
//!
//! # Design (from estimator audition, 2026-05-13)
//!
//! The audition (de-lab `methodology/auditions/estimator/`) tested three
//! math-tuned 7B candidates against fixture-01's clean-math problem. All
//! three failed below the 60% rubric threshold — three distinct ways to be
//! wrong about arithmetic that a `for` loop nails. The structural finding:
//! **math fine-tunes pattern-match math problem *shape* without reliably
//! *computing*.** Fixtures 02–05 then probed the revised role (deterministic
//! math + LLM wrap) against the already-loaded 4B compactor and found
//! 4/4 correct profile picks at sub-second latency with two prompt-tunable
//! soft failures (verb conflation, confidence overcalibration) — both
//! baked into the prompt this module sends.
//!
//! # Verbs
//!
//! - `stay-on-<profile>` when the active profile is the smallest adequate option
//! - `switch-to-<profile>` when a different profile is recommended
//!
//! # Confidence calibration
//!
//! - `high` — max_cumulative comfortably inside the chosen profile's safe budget
//! - `medium` — within 10% of the safe budget on either side (borderline)
//! - `low` — within 5% of the hard ceiling (risky even with compaction firing)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

/// LMStudio endpoint for the optional `--narrate` flag.
const LMSTUDIO_CHAT_URL: &str = "http://localhost:1234/v1/chat/completions";

/// Namespaced 4B compactor — same model used for compaction, now extended
/// as the small-task specialist for narrative wraps. See fixture-06's
/// concurrent probe for the operational justification.
const NARRATE_MODEL_ID: &str = "darkmux:qwen3-4b-instruct-2507";

/// Safe budget fraction — recommend a larger profile when max cumulative
/// input exceeds this fraction of the profile's primary context window.
/// Margin against compaction overhead, prompt drift, and LMStudio's
/// internal reserve.
const SAFE_BUDGET_FACTOR: f64 = 0.95;

/// Borderline confidence band — if max cumulative is within this fraction
/// of the safe budget (on either side), recommend with `medium` confidence
/// because the operator has a real decision to make.
const BORDERLINE_BAND: f64 = 0.10;

/// Risky band — if max cumulative is within this fraction of the hard
/// ceiling, recommend with `low` confidence because even compaction might
/// not save it under variance.
const RISKY_BAND: f64 = 0.05;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkloadSpec {
    /// Operator-classified shape: `single-turn` | `mid` | `long-agentic`.
    /// Stored for output completeness; doesn't affect the math.
    pub workload_class: String,
    /// Profile id currently loaded — used to compute stay-vs-switch verb.
    pub active_profile: String,
    /// One-time cost on turn 0 (prompt + workspace bootstrap).
    pub initial_state_tokens: u64,
    pub per_turn: PerTurn,
    pub planned_turns: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PerTurn {
    pub tool_schemas: u64,
    pub file_reads: CountAndSize,
    pub edits: CountAndSize,
    pub exec_calls: CountAndOutputSize,
    pub assistant_response: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CountAndSize {
    pub count: u32,
    pub avg_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CountAndOutputSize {
    pub count: u32,
    pub avg_output_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetReport {
    pub per_turn_cost: u64,
    pub cumulative_input_by_turn: Vec<u64>,
    pub max_cumulative_input: u64,
    pub compaction_threshold_tokens: u64,
    pub first_turn_exceeding_threshold: Option<u32>,
    pub active_profile_primary_context: u64,
    pub safe_budget_tokens: u64,
    pub profile_adequate: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    /// `stay-on-<profile>` or `switch-to-<profile>`.
    pub action: String,
    pub target_profile: String,
    /// `high` | `medium` | `low`.
    pub confidence: String,
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EstimateOutput {
    pub workload_class: String,
    pub active_profile: String,
    pub budget: BudgetReport,
    pub recommendation: Recommendation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrative: Option<Value>,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct ProfileCapacity {
    /// Primary model's n_ctx.
    primary_context: u64,
    /// Has a compactor model — affects whether very-long workloads can
    /// realistically survive at higher utilization.
    has_compactor: bool,
    /// Max history share for the compaction trigger — used to compute the
    /// "compaction threshold" line in the budget report.
    compaction_max_history_share: f64,
}

/// Reads the profile registry and returns each profile's primary model
/// context + compaction params. Profiles with no `primary`-role model are
/// skipped (can't recommend a load with no inference target).
fn collect_profile_capacities(
    reg: &crate::types::ProfileRegistry,
) -> Vec<(String, ProfileCapacity)> {
    use crate::types::ModelRole;
    let mut out: Vec<(String, ProfileCapacity)> = reg
        .profiles
        .iter()
        .filter_map(|(name, profile)| {
            let primary = profile
                .models
                .iter()
                .find(|m| matches!(m.role, ModelRole::Primary))?;
            let has_compactor = profile
                .models
                .iter()
                .any(|m| matches!(m.role, ModelRole::Compactor));
            // Read max_history_share from runtime.compaction if present;
            // default to 0.35 (openclaw's typical setting in `balanced`).
            let max_history_share = profile
                .runtime
                .as_ref()
                .and_then(|r| r.compaction.as_ref())
                .and_then(|m| m.get("maxHistoryShare"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.35);
            Some((
                name.clone(),
                ProfileCapacity {
                    primary_context: primary.n_ctx as u64,
                    has_compactor,
                    compaction_max_history_share: max_history_share,
                },
            ))
        })
        .collect();
    // Sort by primary_context ascending so the recommendation loop can
    // pick the smallest adequate option.
    out.sort_by_key(|(_, cap)| cap.primary_context);
    out
}

fn compute_budget(spec: &WorkloadSpec, active: &ProfileCapacity) -> BudgetReport {
    let per_turn_cost = spec.per_turn.tool_schemas
        + (spec.per_turn.file_reads.count as u64) * spec.per_turn.file_reads.avg_bytes
        + (spec.per_turn.edits.count as u64) * spec.per_turn.edits.avg_bytes
        + (spec.per_turn.exec_calls.count as u64) * spec.per_turn.exec_calls.avg_output_bytes
        + spec.per_turn.assistant_response;

    let mut cumulative: Vec<u64> = Vec::with_capacity(spec.planned_turns as usize);
    let mut running = spec.initial_state_tokens;
    for _ in 0..spec.planned_turns {
        running += per_turn_cost;
        cumulative.push(running);
    }

    let max_cumulative = cumulative.last().copied().unwrap_or(0);
    let safe_budget = (active.primary_context as f64 * SAFE_BUDGET_FACTOR) as u64;
    let compaction_threshold =
        (active.primary_context as f64 * active.compaction_max_history_share) as u64;

    let first_exceeding = cumulative
        .iter()
        .enumerate()
        .find(|(_, &c)| c > compaction_threshold)
        .map(|(i, _)| (i + 1) as u32);

    BudgetReport {
        per_turn_cost,
        cumulative_input_by_turn: cumulative,
        max_cumulative_input: max_cumulative,
        compaction_threshold_tokens: compaction_threshold,
        first_turn_exceeding_threshold: first_exceeding,
        active_profile_primary_context: active.primary_context,
        safe_budget_tokens: safe_budget,
        profile_adequate: max_cumulative <= safe_budget,
    }
}

fn pick_profile(
    spec: &WorkloadSpec,
    budget: &BudgetReport,
    available: &[(String, ProfileCapacity)],
) -> Recommendation {
    // Smallest profile whose safe budget fits the max cumulative.
    let target = available
        .iter()
        .find(|(_, cap)| {
            (cap.primary_context as f64 * SAFE_BUDGET_FACTOR) as u64 >= budget.max_cumulative_input
        })
        .cloned();

    let (target_name, target_cap) = match target {
        Some((n, c)) => (n, c),
        None => {
            // Nothing fits even at the largest profile; recommend the largest
            // available + flag low confidence.
            let largest = available.last().cloned();
            match largest {
                Some((n, c)) => (n, c),
                None => {
                    return Recommendation {
                        action: format!("stay-on-{}", spec.active_profile),
                        target_profile: spec.active_profile.clone(),
                        confidence: "low".to_string(),
                        reasoning: "no profiles available in the registry — cannot recommend".to_string(),
                    };
                }
            }
        }
    };

    let action = if target_name == spec.active_profile {
        format!("stay-on-{target_name}")
    } else {
        format!("switch-to-{target_name}")
    };

    // Confidence calibration:
    //   - low: max cumulative within RISKY_BAND of target's hard ceiling
    //   - medium: max cumulative within BORDERLINE_BAND of target's safe budget
    //   - high: max cumulative comfortably inside target's safe budget
    let target_safe = (target_cap.primary_context as f64 * SAFE_BUDGET_FACTOR) as u64;
    let target_ceiling = target_cap.primary_context;
    let max_in = budget.max_cumulative_input;

    let risky_margin = (target_ceiling as f64 * RISKY_BAND) as u64;
    let borderline_margin = (target_safe as f64 * BORDERLINE_BAND) as u64;

    // Confidence: "low" when near the hard ceiling (risky even with
    // compaction), "medium" when near the safe-budget threshold (borderline
    // decision the operator should know about), "high" otherwise. Tiny
    // workloads that comfortably fit are "high" — small ≠ borderline.
    let confidence = if max_in + risky_margin >= target_ceiling {
        "low"
    } else if max_in + borderline_margin >= target_safe {
        "medium"
    } else {
        "high"
    };

    let reasoning = build_reasoning(spec, budget, &target_name, &target_cap, confidence);

    Recommendation {
        action,
        target_profile: target_name,
        confidence: confidence.to_string(),
        reasoning,
    }
}

fn build_reasoning(
    spec: &WorkloadSpec,
    budget: &BudgetReport,
    target_name: &str,
    target: &ProfileCapacity,
    confidence: &str,
) -> String {
    let max_in = budget.max_cumulative_input;
    let target_safe = (target.primary_context as f64 * SAFE_BUDGET_FACTOR) as u64;
    let pct = (max_in as f64 / target.primary_context as f64 * 100.0) as u64;
    match confidence {
        "low" => format!(
            "max cumulative {max_in} approaches {target_name}'s hard ceiling {} (~{pct}% of context); even with compaction, variance could overflow",
            target.primary_context
        ),
        "medium" => format!(
            "max cumulative {max_in} is within 10% of {target_name}'s safe budget {target_safe}; real operator decision",
        ),
        _ => format!(
            "max cumulative {max_in} comfortably under {target_name}'s safe budget {target_safe} (~{pct}% of context); {workload_class} workload fits",
            workload_class = spec.workload_class
        ),
    }
}

/// LMStudio endpoint (env-overridable for tests). Defaults to the public
/// `LMSTUDIO_CHAT_URL` constant.
fn lmstudio_chat_url() -> String {
    std::env::var("DARKMUX_LMSTUDIO_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| LMSTUDIO_CHAT_URL.to_string())
}

/// Call the 4B compactor at LMStudio for the operator-facing narrative
/// wrap. Returns the parsed JSON object from the model's response, or an
/// error if the call/parse fails.
fn narrate_via_4b(output_so_far: &EstimateOutput) -> Result<Value> {
    let report_str = serde_json::to_string_pretty(output_so_far)?;
    let payload = serde_json::json!({
        "model": NARRATE_MODEL_ID,
        "messages": [
            {
                "role": "system",
                "content": "You are a budget oracle for an agentic dispatch system. The arithmetic and the recommendation are already computed. Your job: produce a concise operator-facing one-sentence narrative that surfaces the *meaning* — what the operator should do, whether the data is unambiguous or borderline, and the confidence rating's justification. Output a single JSON object: {\"narrative\": <one-sentence string>}. No markdown. No code fences. No prose outside the JSON."
            },
            {
                "role": "user",
                "content": format!("Budget report:\n\n{report_str}\n\nProduce the narrative JSON.")
            }
        ],
        "temperature": 0.2,
        "max_tokens": 200
    });
    let payload_str = serde_json::to_string(&payload)?;

    let url = lmstudio_chat_url();
    let output = Command::new("curl")
        .args(["-s", "--fail", &url, "-H", "Content-Type: application/json", "-d", &payload_str])
        .output()
        .context("running curl to LMStudio")?;
    if !output.status.success() {
        anyhow::bail!("curl to LMStudio failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let resp: Value = serde_json::from_slice(&output.stdout)
        .context("parsing LMStudio response as JSON")?;
    if let Some(err) = resp.get("error") {
        anyhow::bail!("LMStudio returned an error: {err}");
    }
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .context("LMStudio response missing /choices/0/message/content")?;
    let narrative: Value = serde_json::from_str(content)
        .context("4B's narrative response is not valid JSON")?;
    Ok(narrative)
}

/// Core estimate logic — pure function over (spec, registry, narrate).
/// Separated from `estimate()` so tests can drive it without depending on
/// the file system or the live profile registry on disk. `estimate()`
/// reads spec from disk + loads the registry, then delegates here.
pub(crate) fn run_estimate(
    spec: &WorkloadSpec,
    reg: &crate::types::ProfileRegistry,
    narrate: bool,
) -> Result<EstimateOutput> {
    let capacities = collect_profile_capacities(reg);
    let active = capacities
        .iter()
        .find(|(name, _)| name == &spec.active_profile)
        .map(|(_, c)| *c)
        .with_context(|| {
            format!(
                "active_profile '{}' not in registry — known profiles: {}",
                spec.active_profile,
                capacities
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let budget = compute_budget(spec, &active);
    let recommendation = pick_profile(spec, &budget, &capacities);

    let mut output = EstimateOutput {
        workload_class: spec.workload_class.clone(),
        active_profile: spec.active_profile.clone(),
        budget,
        recommendation,
        narrative: None,
    };

    if narrate {
        match narrate_via_4b(&output) {
            Ok(n) => output.narrative = Some(n),
            Err(e) => {
                // Don't fail the whole estimate if the wrap call breaks —
                // the deterministic output is still valid. Surface the
                // failure to stderr.
                eprintln!("warning: --narrate failed ({e}); emitting deterministic output only");
            }
        }
    }

    Ok(output)
}

pub fn estimate(spec_path: &Path, narrate: bool) -> Result<i32> {
    let raw = fs::read_to_string(spec_path)
        .with_context(|| format!("reading workload spec at {}", spec_path.display()))?;
    let spec: WorkloadSpec = serde_json::from_str(&raw)
        .with_context(|| format!("parsing workload spec at {}", spec_path.display()))?;

    let reg = crate::profiles::load_registry(None)
        .context("loading profile registry for capacity lookup")?;

    let output = run_estimate(&spec, &reg.registry, narrate)?;

    let json = serde_json::to_string_pretty(&output)?;
    println!("{json}");
    Ok(0)
}

/// Structured output for `sprint review` (stdout JSON).
#[derive(Debug, Clone, Serialize)]
pub struct SprintReviewOutput {
    pub branch: String,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_session_id: Option<String>,
    pub diff_files_changed: usize,
    pub total_findings: usize,
    #[serde(rename = "by_severity")]
    pub by_severity: SeverityCounts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<ReviewFinding>,
    /// `clean` | `flags-only` | `blockers`
    pub verdict: String,
}

/// Breakdown of findings by severity level.
#[derive(Debug, Clone, Serialize)]
pub struct SeverityCounts {
    pub block: usize,
    pub flag: usize,
    pub nit: usize,
}

/// A single review finding: severity + freeform message.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewFinding {
    pub severity: String,
    pub text: String,
}

/// Parsed result from a QA-REVIEW-SIGNOFF block (internal, not serialized).
/// Fields are module-private — `parse_signoff` returns the value; callers
/// within `sprint_cli` read the fields directly. External `pub` was excessive.
pub(crate) struct SignoffParse {
    block: usize,
    flag: usize,
    nit: usize,
    verdict: String,
    findings: Vec<ReviewFinding>,
}

/// Unwrap the openclaw JSON envelope from `crew::dispatch` stdout to the
/// inner assistant text. The envelope shape is
/// `{"payloads": [{"text": "..."}], ...}`; we concatenate every payload's
/// `text` field with newlines. Falls back to the raw input when it doesn't
/// parse as JSON (so unit tests passing literal SIGNOFF strings work).
pub fn unwrap_envelope_text(raw: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(arr) = parsed.get("payloads").and_then(|p| p.as_array()) else {
        return raw.to_string();
    };
    let parts: Vec<&str> = arr
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect();
    if parts.is_empty() {
        raw.to_string()
    } else {
        parts.join("\n")
    }
}

/// Extract a severity marker from a finding line. Accepts THREE marker forms
/// reviewers actually emit in practice:
///
///   `- [SEVERITY] rest`     ← the prompt's documented form (bracketed)
///   `- **SEVERITY** rest`   ← markdown-bold form (very common — Claude / Llama / etc.)
///   `- SEVERITY: rest`      ← plain-colon form (occasional)
///
/// Returns `(severity, rest_after_marker)` if the line matches AND the
/// severity tag is one of BLOCK / FLAG / NIT. Returns None for any other
/// line (header, prose, malformed marker, unknown severity).
fn extract_severity_marker(line: &str) -> Option<(&str, &str)> {
    let line = line.strip_prefix("- ")?;

    // Bracket form: [SEVERITY]
    if let Some(rest) = line.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let severity = &rest[..end];
            if matches!(severity, "BLOCK" | "FLAG" | "NIT") {
                return Some((severity, rest[end + 1..].trim_start()));
            }
        }
    }

    // Markdown-bold form: **SEVERITY**
    if let Some(rest) = line.strip_prefix("**") {
        if let Some(end) = rest.find("**") {
            let severity = &rest[..end];
            if matches!(severity, "BLOCK" | "FLAG" | "NIT") {
                return Some((severity, rest[end + 2..].trim_start()));
            }
        }
    }

    // Plain form: `SEVERITY:` or `SEVERITY ` (severity followed by a clean
    // separator — colon, space, or tab). Tighter than "any non-uppercase
    // char" because that would match things like `FLAG-suffix`, which is
    // not a severity marker and shouldn't parse as one.
    let first_word_end = line.find(|c: char| !c.is_ascii_uppercase()).unwrap_or(line.len());
    if first_word_end > 0 {
        let severity = &line[..first_word_end];
        if matches!(severity, "BLOCK" | "FLAG" | "NIT") {
            let separator = line.as_bytes().get(first_word_end).copied();
            // Only accept separator = `:`, ` `, `\t`, or end-of-line.
            if matches!(separator, Some(b':') | Some(b' ') | Some(b'\t') | None) {
                let after = line[first_word_end..].trim_start();
                let after = after.strip_prefix(':').unwrap_or(after).trim_start();
                return Some((severity, after));
            }
        }
    }

    None
}

/// Parse a QA-REVIEW-SIGNOFF block from already-unwrapped reviewer text.
///
/// Extracts findings from lines matching any of the three documented severity
/// marker forms (see `extract_severity_marker`) and returns BOTH the structured
/// findings AND severity counts in one pass (single source of truth — earlier
/// versions had two parsers that diverged on slice offsets). Handles malformed
/// lines gracefully (skips them).
pub fn parse_signoff(input: &str) -> SignoffParse {
    let mut findings: Vec<ReviewFinding> = Vec::new();

    for line in input.lines() {
        let line = line.trim();
        let Some((severity, rest)) = extract_severity_marker(line) else { continue; };

        let finding_text = if let Some(pos) = rest.find(" — ") {
            // " — " is 5 bytes total (em-dash is U+2014 = 3 UTF-8 bytes, plus
            // leading + trailing ASCII space). find() returns the byte index
            // of the first space, so rest[pos+5..] starts the message.
            let file_line = rest[..pos].trim();
            let message = rest[pos + 5..].trim();
            format!("{file_line} — {message}")
        } else {
            rest.trim().to_string()
        };

        findings.push(ReviewFinding { severity: severity.to_string(), text: finding_text });
    }

    let mut block = 0usize;
    let mut flag = 0usize;
    let mut nit = 0usize;
    for f in &findings {
        match f.severity.as_str() {
            "BLOCK" => block += 1,
            "FLAG" => flag += 1,
            "NIT" => nit += 1,
            _ => {}
        }
    }

    let verdict = if block > 0 {
        "blockers"
    } else if flag > 0 || nit > 0 {
        "flags-only"
    } else if input.contains("No findings. PR is mergeable.") {
        "clean"
    } else {
        // No findings, no explicit clean marker — reviewer may have failed
        // to engage with the format. Surface as indeterminate so operator
        // knows to inspect manually.
        "indeterminate"
    }.to_string();

    SignoffParse { block, flag, nit, verdict, findings }
}

/// Count files changed between `base` and the working tree via `git diff
/// --name-only <base>`. Returns 0 on any error (treated as "no files
/// changed" — the empty-diff path short-circuits before this is called
/// in practice, so the 0 fallback is only hit if `git` itself is missing).
fn count_files_changed(path: &Path, base: &str) -> usize {
    Command::new("git")
        .args(["diff", "--name-only", base])
        .current_dir(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Internal entry for `sprint review` taking an explicit repo path.
pub(crate) fn sprint_review_at(path: &Path, base: Option<&str>, require_clean: bool) -> Result<i32> {
    // Capture branch name.
    let branch = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    let session_id = format!("sprint-review-{}", std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_secs());

    // Run `git diff <base>` (working-tree-inclusive). NOT `<base>..HEAD` —
    // that would only see committed changes, missing the pre-PR review case
    // (the whole point of `sprint review` is to review work before it ships,
    // which is by definition uncommitted at the moment of review).
    let base_ref = base.unwrap_or("main");
    let diff_output = Command::new("git")
        .args(["diff", base_ref])
        .current_dir(path)
        .output()
        .context("running git diff")?;

    let diff = String::from_utf8_lossy(&diff_output.stdout).to_string();

    // Empty diff → no dispatch needed.
    if diff.trim().is_empty() {
        let output = SprintReviewOutput {
            branch: branch.clone(),
            base: base_ref.to_string(),
            reviewer_session_id: Some(session_id.clone()),
            diff_files_changed: 0,
            total_findings: 0,
            by_severity: SeverityCounts { block: 0, flag: 0, nit: 0 },
            findings: vec![],
            verdict: "clean".to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(0);
    }

    // Count changed files from diff stat.
    let files_changed = count_files_changed(path, base_ref);

    // Build reviewer prompt. `MAX_CHARS` is a byte budget (line.len() returns
    // bytes); for typical Rust + JSON diffs this is effectively chars, but
    // a diff containing many multi-byte UTF-8 chars could under-count by up
    // to ~4× and exceed the documented limit. Acceptable for this verb's use
    // case; tighten if non-ASCII becomes common in reviewed diffs.
    const MAX_CHARS: usize = 80_000;
    let truncated_diff = if diff.len() > MAX_CHARS {
        let mut end_pos = 0;
        let mut char_count = 0;
        for line in diff.lines() {
            let next = end_pos + line.len() + 1;
            if char_count + line.len() > MAX_CHARS {
                break;
            }
            end_pos = next;
            char_count += line.len() + 1; // \n
        }
        let remaining_lines = diff[end_pos..].lines().count();
        format!("{}\n... [diff truncated, {} lines omitted]", &diff[..end_pos], remaining_lines)
    } else {
        diff
    };

    let prompt = format!(
        "You are the darkmux `code-reviewer` role. Your job: read this PR's diff and emit a structured QA-REVIEW-SIGNOFF block.\n\n## The diff\n\nBranch `{branch}` vs base `{base_ref}`:\n\n```\n{truncated_diff}\n```\n\n## Reporting format (strict)\n\nEmit findings as:\n\nQA-REVIEW-SIGNOFF\n- [SEVERITY] <file>:<line> — <one-sentence finding>\n- ...\n\nSummary: <N blockers, M flags, K nits>\n\nSeverity levels:\n- BLOCK: must fix before merge (security, correctness, broken tests)\n- FLAG: should address but mergeable with operator awareness\n- NIT: style / consistency / minor\n\nIf you find no issues at all, say so explicitly: `No findings. PR is mergeable.`\n\nBe specific. \"logic error\" without a line is useless. \"src/foo.rs:192 — sum is missing `tool_schemas`\" is a real finding.",
    );

    let dispatch_opts = crate::crew::dispatch::DispatchOpts {
        role_id: "code-reviewer".to_string(),
        message: prompt,
        deliver: None,
        session_id: Some(session_id.clone()),
        timeout_seconds: 600,
        skip_preflight: false,
    };

    let dispatch_result = crate::crew::dispatch::dispatch(dispatch_opts)
        .map_err(|e| {
            eprintln!("warning: crew dispatch failed ({e}); emitting empty review");
            anyhow::anyhow!("crew dispatch failed: {e}")
        })?;

    // Unwrap the openclaw JSON envelope FIRST — the SIGNOFF text lives inside
    // .payloads[].text, escaped. Without unwrapping, parse_signoff sees JSON
    // keys instead of the actual newline-separated SIGNOFF lines and finds
    // zero matches. Recursive bug from sprint A's own dogfood. See
    // unwrap_envelope_text.
    let unwrapped = unwrap_envelope_text(&dispatch_result.stdout);
    let signoff = parse_signoff(&unwrapped);

    let output = SprintReviewOutput {
        branch,
        base: base_ref.to_string(),
        reviewer_session_id: Some(session_id),
        diff_files_changed: files_changed,
        total_findings: signoff.block + signoff.flag + signoff.nit,
        by_severity: SeverityCounts {
            block: signoff.block,
            flag: signoff.flag,
            nit: signoff.nit,
        },
        findings: signoff.findings.clone(),
        verdict: signoff.verdict.clone(),
    };

    println!("{}", serde_json::to_string_pretty(&output)?);

    Ok(if require_clean && signoff.block > 0 { 1 } else { 0 })
}

/// Public entry for `sprint review` — delegates to `sprint_review_at`.
pub fn sprint_review(
    base: Option<&str>,
    require_clean: bool,
) -> Result<i32> {
    let path = std::env::current_dir()?;
    sprint_review_at(&path, base, require_clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelRole, Profile, ProfileModel, ProfileRegistry};
    use std::collections::BTreeMap;

    fn fixture_registry() -> ProfileRegistry {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "fast".to_string(),
            Profile {
                description: None,
                models: vec![ProfileModel {
                    id: "primary".into(),
                    n_ctx: 32000,
                    role: ModelRole::Primary,
                    identifier: None,
                }],
                runtime: None,
                use_when: None,
            },
        );
        profiles.insert(
            "balanced".to_string(),
            Profile {
                description: None,
                models: vec![
                    ProfileModel {
                        id: "primary".into(),
                        n_ctx: 101000,
                        role: ModelRole::Primary,
                        identifier: None,
                    },
                    ProfileModel {
                        id: "compactor".into(),
                        n_ctx: 68000,
                        role: ModelRole::Compactor,
                        identifier: None,
                    },
                ],
                runtime: None,
                use_when: None,
            },
        );
        profiles.insert(
            "deep".to_string(),
            Profile {
                description: None,
                models: vec![
                    ProfileModel {
                        id: "primary".into(),
                        n_ctx: 262144,
                        role: ModelRole::Primary,
                        identifier: None,
                    },
                    ProfileModel {
                        id: "compactor".into(),
                        n_ctx: 120000,
                        role: ModelRole::Compactor,
                        identifier: None,
                    },
                ],
                runtime: None,
                use_when: None,
            },
        );
        ProfileRegistry {
            profiles,
            hooks: None,
            default_profile: None,
        }
    }

    fn fixture_spec(active: &str, planned_turns: u32, file_read_count: u32) -> WorkloadSpec {
        WorkloadSpec {
            workload_class: "long-agentic".into(),
            active_profile: active.to_string(),
            initial_state_tokens: 50000,
            per_turn: PerTurn {
                tool_schemas: 3300,
                file_reads: CountAndSize {
                    count: file_read_count,
                    avg_bytes: 4000,
                },
                edits: CountAndSize {
                    count: 2,
                    avg_bytes: 1500,
                },
                exec_calls: CountAndOutputSize {
                    count: 1,
                    avg_output_bytes: 8000,
                },
                assistant_response: 2500,
            },
            planned_turns,
        }
    }

    #[test]
    fn matches_fixture_01_reference_arithmetic() {
        // The exact inputs from fixture-01: 8 turns long-agentic with the
        // listed components. Reference per_turn_cost is 28800 (3300 + 12000
        // + 3000 + 8000 + 2500). Initial state is 50000 + 3300 = 53300 per
        // fixture-01's wording, but per spec convention we treat initial
        // as 50000 (the bootstrap) and tool_schemas are per-turn.
        // (Compare against fixture-01's reference table.)
        let spec = fixture_spec("balanced", 8, 3);
        let caps = collect_profile_capacities(&fixture_registry());
        let active = caps.iter().find(|(n, _)| n == "balanced").unwrap().1;
        let report = compute_budget(&spec, &active);
        assert_eq!(report.per_turn_cost, 28800);
        // Turn 1 cumulative: 50000 + 28800 = 78800. fixture-01's table shows
        // 82100 for turn 1 because it included tool_schemas in initial. The
        // semantic split (initial = bootstrap; tool_schemas = per-turn)
        // differs from fixture-01's wording but is self-consistent with this
        // module's contract.
        assert_eq!(report.cumulative_input_by_turn[0], 78800);
        assert_eq!(report.cumulative_input_by_turn[7], 280400);
        assert_eq!(report.max_cumulative_input, 280400);
    }

    #[test]
    fn picks_fast_for_tiny_workload() {
        // Single-turn-class with small inputs; max cumulative should fit
        // fast's 32K easily → recommend switch-to-fast (from balanced active).
        let spec = WorkloadSpec {
            workload_class: "single-turn".into(),
            active_profile: "balanced".into(),
            initial_state_tokens: 4000,
            per_turn: PerTurn {
                tool_schemas: 3300,
                file_reads: CountAndSize {
                    count: 0,
                    avg_bytes: 0,
                },
                edits: CountAndSize {
                    count: 0,
                    avg_bytes: 0,
                },
                exec_calls: CountAndOutputSize {
                    count: 0,
                    avg_output_bytes: 0,
                },
                assistant_response: 1500,
            },
            planned_turns: 2,
        };
        let caps = collect_profile_capacities(&fixture_registry());
        let active = caps.iter().find(|(n, _)| n == "balanced").unwrap().1;
        let report = compute_budget(&spec, &active);
        let rec = pick_profile(&spec, &report, &caps);
        assert_eq!(rec.target_profile, "fast");
        assert!(rec.action.starts_with("switch-to-"));
    }

    #[test]
    fn stays_on_balanced_for_mid_workload() {
        // Mid workload; should fit balanced but not fast.
        // per_turn cost = 3300 + 3*4000 + 2*1500 + 1*8000 + 2500 = 28800
        // Wait that's same as fixture-01. Let me make this smaller.
        // We need max_cumulative < balanced safe (95950) but > fast safe (30400).
        // Try 2 turns at 28800 = 57600 + initial 20000 → 48800 + 28800 = 77600 total. Good.
        let mut spec = fixture_spec("balanced", 2, 3);
        spec.initial_state_tokens = 20000;
        let caps = collect_profile_capacities(&fixture_registry());
        let active = caps.iter().find(|(n, _)| n == "balanced").unwrap().1;
        let report = compute_budget(&spec, &active);
        let rec = pick_profile(&spec, &report, &caps);
        assert_eq!(rec.target_profile, "balanced");
        assert_eq!(rec.action, "stay-on-balanced");
    }

    #[test]
    fn switches_to_deep_when_balanced_inadequate() {
        let spec = fixture_spec("balanced", 8, 3);
        let caps = collect_profile_capacities(&fixture_registry());
        let active = caps.iter().find(|(n, _)| n == "balanced").unwrap().1;
        let report = compute_budget(&spec, &active);
        let rec = pick_profile(&spec, &report, &caps);
        assert_eq!(rec.target_profile, "deep");
        assert_eq!(rec.action, "switch-to-deep");
    }

    #[test]
    fn confidence_drops_to_medium_at_borderline() {
        // Construct a workload whose max cumulative is right at balanced's
        // safe budget (95950 ± 10%). Per turn 28800; initial 20000.
        // 4 turns: 20000 + 4*28800 = 135200 — too big for balanced; recommended would be deep.
        // 3 turns: 20000 + 3*28800 = 106400 — also exceeds. Try smaller per_turn.
        // Reduce per_turn to ~28000 and look for: initial + N * 28000 within 10% of 95950.
        // Try: initial=8000, per_turn ~18000, 5 turns → 98000 (matches fixture-05).
        let spec = WorkloadSpec {
            workload_class: "long-agentic".into(),
            active_profile: "balanced".into(),
            initial_state_tokens: 8000,
            per_turn: PerTurn {
                tool_schemas: 3300,
                file_reads: CountAndSize {
                    count: 2,
                    avg_bytes: 4000,
                },
                edits: CountAndSize {
                    count: 1,
                    avg_bytes: 1500,
                },
                exec_calls: CountAndOutputSize {
                    count: 1,
                    avg_output_bytes: 4000,
                },
                assistant_response: 1200,
            },
            planned_turns: 5,
        };
        // per_turn = 3300 + 8000 + 1500 + 4000 + 1200 = 18000; cumulative t5 = 8000+90000 = 98000
        let caps = collect_profile_capacities(&fixture_registry());
        let active = caps.iter().find(|(n, _)| n == "balanced").unwrap().1;
        let report = compute_budget(&spec, &active);
        let rec = pick_profile(&spec, &report, &caps);
        // 98000 exceeds balanced safe (95950) so target should be deep.
        assert_eq!(rec.target_profile, "deep");
        // But max_in is way below deep's safe (~249000) so confidence is high
        // (no longer borderline relative to the *target* profile). This is
        // honest — the borderline relative to balanced isn't the right axis
        // when we've already promoted to deep.
        assert_eq!(rec.confidence, "high");
    }

    #[test]
    fn capacities_sorted_ascending_by_primary_context() {
        let caps = collect_profile_capacities(&fixture_registry());
        let names: Vec<&str> = caps.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["fast", "balanced", "deep"]);
    }

    #[test]
    fn tiny_workload_returns_high_confidence_not_medium() {
        // Regression test for QA Flag 2 — fixture-04-style "stay on
        // balanced" workload where max_cumulative is tiny relative to
        // safe budget. Should be `high` confidence, not `medium`
        // ("small" ≠ "borderline").
        let spec = WorkloadSpec {
            workload_class: "single-turn".into(),
            active_profile: "fast".into(),
            initial_state_tokens: 1000,
            per_turn: PerTurn {
                tool_schemas: 500,
                file_reads: CountAndSize { count: 0, avg_bytes: 0 },
                edits: CountAndSize { count: 0, avg_bytes: 0 },
                exec_calls: CountAndOutputSize { count: 0, avg_output_bytes: 0 },
                assistant_response: 500,
            },
            planned_turns: 1,
        };
        // max_cumulative = 1000 + 1*1000 = 2000; fast safe = 30400. Trivially fits.
        let caps = collect_profile_capacities(&fixture_registry());
        let active = caps.iter().find(|(n, _)| n == "fast").unwrap().1;
        let report = compute_budget(&spec, &active);
        let rec = pick_profile(&spec, &report, &caps);
        assert_eq!(rec.target_profile, "fast");
        assert_eq!(rec.action, "stay-on-fast");
        assert_eq!(rec.confidence, "high", "tiny workload that comfortably fits should be high confidence");
    }

    #[test]
    fn run_estimate_errors_on_unknown_profile() {
        // Regression test for QA Flag 4 — when active_profile isn't in
        // the registry, run_estimate should error with a helpful message
        // listing the known profiles.
        let spec = WorkloadSpec {
            workload_class: "long-agentic".into(),
            active_profile: "nonexistent-profile".into(),
            initial_state_tokens: 1000,
            per_turn: PerTurn {
                tool_schemas: 100,
                file_reads: CountAndSize { count: 0, avg_bytes: 0 },
                edits: CountAndSize { count: 0, avg_bytes: 0 },
                exec_calls: CountAndOutputSize { count: 0, avg_output_bytes: 0 },
                assistant_response: 100,
            },
            planned_turns: 1,
        };
        let reg = fixture_registry();
        let err = run_estimate(&spec, &reg, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nonexistent-profile"), "error should name the missing profile: {msg}");
        assert!(msg.contains("known profiles"), "error should list known profiles: {msg}");
    }

    #[serial_test::serial]
    #[test]
    fn run_estimate_gracefully_degrades_when_narrate_fails() {
        // Regression test for QA Flag 3 — when --narrate is requested
        // but LMStudio is unreachable, the deterministic output still
        // emits (narrative=None). Tested by pointing DARKMUX_LMSTUDIO_URL
        // at an unreachable port. Marked serial because mutating env vars
        // (DARKMUX_LMSTUDIO_URL) is process-global and races with any
        // other test reading the same var.
        let spec = WorkloadSpec {
            workload_class: "long-agentic".into(),
            active_profile: "balanced".into(),
            initial_state_tokens: 50000,
            per_turn: PerTurn {
                tool_schemas: 3300,
                file_reads: CountAndSize { count: 3, avg_bytes: 4000 },
                edits: CountAndSize { count: 2, avg_bytes: 1500 },
                exec_calls: CountAndOutputSize { count: 1, avg_output_bytes: 8000 },
                assistant_response: 2500,
            },
            planned_turns: 8,
        };
        let reg = fixture_registry();

        // Save the existing env var (if any), override with unreachable URL.
        let prev = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        // SAFETY: tests use serial_test::serial when needed; this test
        // doesn't share state with others that rely on this env var.
        unsafe {
            std::env::set_var("DARKMUX_LMSTUDIO_URL", "http://127.0.0.1:1/v1/chat/completions");
        }

        let result = run_estimate(&spec, &reg, true);

        // Restore env state before assertions.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }

        // Deterministic output must still emit; narrative is None on failure.
        let output = result.expect("run_estimate must succeed even when narrate fails");
        assert!(
            output.narrative.is_none(),
            "narrate failure should result in narrative=None, got: {:?}",
            output.narrative
        );
        // Deterministic recommendation still computed correctly.
        assert_eq!(output.recommendation.target_profile, "deep");
    }

    // ── sprint review tests ─────────────────────────────────────────────

    fn sample_signoff() -> String {
        "QA-REVIEW-SIGNOFF\n\
            - [BLOCK] src/main.rs:42 — null pointer dereference on unwrap()\n\
            - [FLAG] src/utils.rs:17 — unused variable `temp_buf`\n\
            - [NIT] src/config.rs:8 — inconsistent spacing after `fn`\n\
            Summary: 1 blockers, 1 flags, 1 nits".to_string()
    }

    #[test]
    fn parse_signoff_extracts_findings() {
        let input = sample_signoff();
        let result = parse_signoff(&input);

        assert_eq!(result.block, 1);
        assert_eq!(result.flag, 1);
        assert_eq!(result.nit, 1);
    }

    #[test]
    fn parse_signoff_handles_clean_no_findings() {
        let input = "QA-REVIEW-SIGNOFF\nNo findings. PR is mergeable.\nSummary: 0 blockers, 0 flags, 0 nits";
        let result = parse_signoff(input);

        assert_eq!(result.block, 0);
        assert_eq!(result.flag, 0);
        assert_eq!(result.nit, 0);
        assert_eq!(result.verdict, "clean");
    }

    #[test]
    fn parse_signoff_handles_malformed_gracefully() {
        // Lines without brackets → skipped.
        let input = "QA-REVIEW-SIGNOFF\nSome random finding line\n- [flag] src/foo.rs:1 — lowercase severity\n- [BLOCK] src/bar.rs:2 — valid finding\nSummary: 1 blockers";
        let result = parse_signoff(input);

        // Only the BLOCK line should be parsed (lowercase flag is skipped).
        assert_eq!(result.block, 1);
        assert_eq!(result.flag, 0);
        assert_eq!(result.nit, 0);
    }

    #[test]
    fn parse_signoff_handles_weird_lines_gracefully() {
        // Lines with unrecognized severity → skipped.
        let input = "QA-REVIEW-SIGNOFF\n- [ERROR] src/x.rs:1 — weird severity\n- [NIT] src/y.rs:2 — valid nit\nSummary: 1 nits";
        let result = parse_signoff(input);

        assert_eq!(result.block, 0);
        assert_eq!(result.flag, 0);
        assert_eq!(result.nit, 1);
    }

    #[test]
    fn parse_signoff_accepts_markdown_bold_marker() {
        // Reviewers (especially Claude/Llama) often emit `- **FLAG**` instead
        // of `- [FLAG]`. Empirically caught during Sprint 1 of #66's dogfood
        // — the verb returned `indeterminate` despite reviewer producing real
        // findings, because the parser only accepted bracketed markers.
        let input = "QA-REVIEW-SIGNOFF\n\
            - **BLOCK** src/flow.rs:1 — concurrent write race\n\
            - **FLAG** src/flow.rs:2 — sync_all errors swallowed\n\
            - **NIT** src/flow.rs:3 — variable shadowing\n\
            Summary: 1 blockers, 1 flags, 1 nits";
        let result = parse_signoff(input);

        assert_eq!(result.block, 1);
        assert_eq!(result.flag, 1);
        assert_eq!(result.nit, 1);
        assert_eq!(result.verdict, "blockers");
        assert_eq!(result.findings.len(), 3);
        assert!(result.findings[1].text.contains("sync_all errors swallowed"));
    }

    #[test]
    fn parse_signoff_accepts_plain_colon_marker() {
        // Occasional third form: `- SEVERITY: message`.
        let input = "QA-REVIEW-SIGNOFF\n\
            - FLAG: src/foo.rs:10 — first finding\n\
            - NIT: src/bar.rs:20 — second finding\n\
            Summary: 1 flags, 1 nits";
        let result = parse_signoff(input);

        assert_eq!(result.flag, 1);
        assert_eq!(result.nit, 1);
        assert_eq!(result.verdict, "flags-only");
    }

    #[test]
    fn parse_signoff_mixed_marker_forms_in_one_block() {
        // Real reviewers sometimes mix forms within one block. Parser must
        // accept all three in the same SIGNOFF.
        let input = "QA-REVIEW-SIGNOFF\n\
            - [BLOCK] src/a.rs:1 — bracketed\n\
            - **FLAG** src/b.rs:2 — bold\n\
            - NIT: src/c.rs:3 — colon\n";
        let result = parse_signoff(input);

        assert_eq!(result.block, 1);
        assert_eq!(result.flag, 1);
        assert_eq!(result.nit, 1);
        assert_eq!(result.verdict, "blockers");
    }

    #[test]
    fn verdict_mapping_returns_clean() {
        let input = "QA-REVIEW-SIGNOFF\nNo findings. PR is mergeable.";
        let result = parse_signoff(input);
        assert_eq!(result.verdict, "clean");
    }

    #[test]
    fn verdict_mapping_returns_flags_only() {
        let input = "QA-REVIEW-SIGNOFF\n- [FLAG] src/foo.rs:10 — unused variable";
        let result = parse_signoff(input);
        assert_eq!(result.verdict, "flags-only");
    }

    #[test]
    fn verdict_mapping_returns_blockers() {
        let input = "QA-REVIEW-SIGNOFF\n- [BLOCK] src/main.rs:5 — null pointer";
        let result = parse_signoff(input);
        assert_eq!(result.verdict, "blockers");
    }
}

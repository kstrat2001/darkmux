//! Code-review output rendering for the coder-phase QA gate.
//!
//! (#1463) The top-level `phase` verb family retired entirely — its
//! `estimate` (a pre-graph budget oracle duplicating gestalt's planner) and
//! its standalone `review` CLI verb (a second review path beside the funnel,
//! `mission launch review`) both went, along with the manual `start`/
//! `complete`/`abandon` bookkeeping the graph now derives (#1406). What
//! SURVIVES here is the review-dispatch core the coder-phase pipeline's QA
//! step depends on: [`phase_review_output_at`] runs the `code-reviewer` role
//! against a worktree diff and parses its QA-REVIEW-SIGNOFF block into a typed
//! [`PhaseReviewOutput`]. `src/coder_phase.rs`'s `MissionVerifyStepKind` calls
//! it directly at the sign-off gate; there is no longer a CLI verb over it.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// Build a `FlowRecord` for phase review verbs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_review_record(
    level: crate::flow::Level,
    category: crate::flow::Category,
    tier: crate::flow::Tier,
    stage: crate::flow::Stage,
    action: String,
    handle: String,
    session_id: &str,
    phase_id: Option<&str>,
) -> crate::flow::FlowRecord {
    use crate::flow::ts_utc_now;
    crate::flow::FlowRecord {
        ts: ts_utc_now(),
        level,
        category,
        tier,
        stage,
        action,
        handle,
        phase_id: phase_id.map(String::from),
        source: Some("phase_review".to_string()),
        session_id: Some(session_id.to_string()),
        // Phase review records describe the review verb's lifecycle —
        // they don't represent a single model's work. The inner crew
        // dispatch (`code-reviewer`) emits its own dispatch records via
        // crew::dispatch::build_dispatch_record with model stamped.
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    }
}

/// Truncate a string to at most `max` bytes, adding "..." if truncated.
/// Honors UTF-8 char boundaries — cuts at the largest valid boundary ≤ max
/// so multi-byte chars in error messages (emojis, accents) don't panic.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = (0..=max)
            .rev()
            .find(|i| s.is_char_boundary(*i))
            .unwrap_or(0);
        format!("{}...", &s[..end])
    }
}

/// Structured output for `phase review` (stdout JSON).
#[derive(Debug, Clone, Serialize)]
pub struct PhaseReviewOutput {
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
/// within `phase_cli` read the fields directly. External `pub` was excessive.
pub(crate) struct SignoffParse {
    block: usize,
    flag: usize,
    nit: usize,
    verdict: String,
    findings: Vec<ReviewFinding>,
}

/// Unwrap a legacy JSON envelope shape from `crew::dispatch` stdout to the
/// inner assistant text, for reading historical run artifacts predating the
/// internal runtime (#1405). The envelope shape is
/// `{"payloads": [{"text": "..."}], ...}`; we concatenate every payload's
/// `text` field with newlines. Falls back to the raw input when it doesn't
/// parse as JSON — the common case today, since the internal runtime's
/// plain-text dispatch mode (what `phase review` uses) returns unwrapped text.
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
    let first_word_end = line
        .find(|c: char| !c.is_ascii_uppercase())
        .unwrap_or(line.len());
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
        let Some((severity, rest)) = extract_severity_marker(line) else {
            continue;
        };

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

        findings.push(ReviewFinding {
            severity: severity.to_string(),
            text: finding_text,
        });
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
    }
    .to_string();

    SignoffParse {
        block,
        flag,
        nit,
        verdict,
        findings,
    }
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
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .count()
        })
        .unwrap_or(0)
}

/// Builds the record fired by `phase_review_output_at`'s bookend guard on
/// `Drop` when the guarded region exits without reaching one of its own
/// named terminal emits (`verdict: clean`, `dispatch failed`, or
/// `verdict: <verdict>`): a panic, or an early `?`-return from something
/// like the `git diff` call that has no terminal record of its own today.
/// Named separately so a test can drive the abort shape directly (#1413).
fn phase_review_abort_record(
    branch: &str,
    session_id: &str,
    phase_id: Option<&str>,
) -> crate::flow::FlowRecord {
    build_review_record(
        crate::flow::Level::Error,
        crate::flow::Category::Review,
        crate::flow::Tier::Frontier,
        crate::flow::Stage::Review,
        "phase review aborted".to_string(),
        branch.to_string(),
        session_id,
        phase_id,
    )
}

/// The code-review core (#1463): run the `code-reviewer` role against `path`'s
/// diff vs `base` and return the structured [`PhaseReviewOutput`] (verdict +
/// findings). Does NOT print or map an exit code — callers decide how to
/// surface it. The coder-phase pipeline's `MissionVerifyStepKind`
/// (`src/coder_phase.rs`) consumes the struct directly for a colorized
/// at-the-gate summary; there is no CLI verb over it (the `phase review` verb
/// retired — the funnel, `mission launch review`, is the standalone review path).
pub(crate) fn phase_review_output_at(
    path: &Path,
    base: Option<&str>,
    phase_id: Option<&str>,
) -> Result<PhaseReviewOutput> {
    // Capture branch name.
    let branch = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let session_id = darkmux_types::session_id::phase_review(
        std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_secs(),
    );

    // (#1413) `phase review begin` / `verdict: ...` used to be plain paired
    // writes: a panic (or an early `?`-return with no terminal of its
    // own, e.g. the `git diff` call below) orphaned the begin record.
    // Bookend-guard the pair so every exit path fires a matching terminal.
    let mut sink = |r: crate::flow::FlowRecord| {
        let _ = crate::flow::record(r);
    };
    let abort_branch = branch.clone();
    let abort_session_id = session_id.clone();
    let abort_phase_id = phase_id.map(String::from);
    let on_abort = move |_id: &str, _kind: &str| {
        phase_review_abort_record(&abort_branch, &abort_session_id, abort_phase_id.as_deref())
    };
    let mut bookend = crate::flow::BookendGuard::new(&mut sink, on_abort);

    // Emit review-start flow record.
    bookend.open(
        "phase-review",
        "phase-review",
        build_review_record(
            crate::flow::Level::Info,
            crate::flow::Category::Review,
            crate::flow::Tier::Frontier,
            crate::flow::Stage::Review,
            "phase review begin".to_string(),
            branch.clone(),
            &session_id,
            phase_id,
        ),
    );

    // Run `git diff <base>` (working-tree-inclusive). NOT `<base>..HEAD` —
    // that would only see committed changes, missing the pre-PR review case
    // (the whole point of `phase review` is to review work before it ships,
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
        let output = PhaseReviewOutput {
            branch: branch.clone(),
            base: base_ref.to_string(),
            reviewer_session_id: Some(session_id.clone()),
            diff_files_changed: 0,
            total_findings: 0,
            by_severity: SeverityCounts {
                block: 0,
                flag: 0,
                nit: 0,
            },
            findings: vec![],
            verdict: "clean".to_string(),
        };
        // Emit verdict flow record. `close()` disarms the guard so this
        // named terminal is the only record for this exit path.
        bookend.close(
            "phase-review",
            build_review_record(
                crate::flow::Level::Info,
                crate::flow::Category::Review,
                crate::flow::Tier::Frontier,
                crate::flow::Stage::Review,
                "verdict: clean".to_string(),
                "0B / 0F / 0N".to_string(),
                &session_id,
                phase_id,
            ),
        );
        return Ok(output);
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
        format!(
            "{}\n... [diff truncated, {} lines omitted]",
            &diff[..end_pos],
            remaining_lines
        )
    } else {
        diff
    };

    let prompt = format!(
        "You are the darkmux `code-reviewer` role. Your job: read this PR's diff and emit a structured QA-REVIEW-SIGNOFF block.\n\n## The diff\n\nBranch `{branch}` vs base `{base_ref}`:\n\n```\n{truncated_diff}\n```\n\n## Reporting format (strict)\n\nEmit findings as:\n\nQA-REVIEW-SIGNOFF\n- [SEVERITY] <file>:<line> — <one-sentence finding>\n- ...\n\nSummary: <N blockers, M flags, K nits>\n\nSeverity levels:\n- BLOCK: must fix before merge (security, correctness, broken tests)\n- FLAG: should address but mergeable with operator awareness\n- NIT: style / consistency / minor\n\nIf you find no issues at all, say so explicitly: `No findings. PR is mergeable.`\n\nBe specific. \"logic error\" without a line is useless. \"src/foo.rs:192 — sum is missing `tool_schemas`\" is a real finding.",
    );

    let dispatch_opts = crate::crew::dispatch::DispatchOpts {
        role_id: "code-reviewer".to_string(),
        message: prompt,
        session_id: Some(session_id.clone()),
        timeout_seconds: 600,
        skip_preflight: false,
        // Phase review parses the SIGNOFF text from the dispatch's
        // human-readable stdout — no JSON envelope needed.
        json: false,
        // Phase review doesn't ask the reviewer to write files — it
        // parses the SIGNOFF text from the dispatch's stdout. Skip the
        // watched-state echo to keep phase review output focused.
        // Phase review reads operator-supplied diffs; no scope override.
        workdir: None,
        // Phase review is a code-reviewer dispatch on the phase's diff,
        // not on the phase's own work. No cross-phase context needed.
        phase_id: None,
        // Phase review dispatches through the internal Docker-bounded
        // runtime — the only dispatch path (#309, #1405).
        machine: None,
        wait: true,
        // Phase dispatch defaults to runtime's built-in compaction;
        // phase context isn't tied to a profile in this entry path.
        compaction: crate::crew::dispatch::CompactionDispatchArgs::default(),
        // (#549) No `--profile` override; fall back to `default_profile`.
        profile_name: None,
        // (#984) No --profiles-file here; dispatch resolves from env > default.
        config_path: None,
        // (#703) default image.
        // (#1199) Bench-only knobs; defaults preserve existing behavior.
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
    };

    // Emit dispatch flow record. This is a mid-point progress emit, not a
    // terminal, so it goes through `emit_now` (the open unit stays armed).
    bookend.emit_now(build_review_record(
        crate::flow::Level::Info,
        crate::flow::Category::Machinery,
        crate::flow::Tier::Local,
        crate::flow::Stage::Review,
        "dispatch code-reviewer".to_string(),
        session_id.clone(),
        &session_id,
        phase_id,
    ));

    let dispatch_result = match crate::fleet::dispatch_routed(dispatch_opts) {
        Ok(r) => r,
        Err(e) => {
            // `close()` disarms the guard. This named terminal is the
            // only record for the dispatch-failure exit path.
            bookend.close(
                "phase-review",
                build_review_record(
                    crate::flow::Level::Error,
                    crate::flow::Category::Machinery,
                    crate::flow::Tier::Local,
                    crate::flow::Stage::Review,
                    "dispatch failed".to_string(),
                    truncate(&format!("{e}"), 200),
                    &session_id,
                    phase_id,
                ),
            );
            eprintln!("warning: dispatch failed ({e}); emitting empty review");
            return Err(anyhow::anyhow!("dispatch failed: {e}"));
        }
    };

    // Unwrap the legacy envelope shape FIRST (in case this is a historical
    // dispatch reply) — the SIGNOFF text would otherwise live inside
    // .payloads[].text, escaped. Without unwrapping, parse_signoff sees JSON
    // keys instead of the actual newline-separated SIGNOFF lines and finds
    // zero matches. Recursive bug from phase A's own dogfood. See
    // unwrap_envelope_text.
    let unwrapped = unwrap_envelope_text(&dispatch_result.stdout);
    let signoff = parse_signoff(&unwrapped);

    let output = PhaseReviewOutput {
        branch,
        base: base_ref.to_string(),
        reviewer_session_id: Some(session_id.clone()),
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

    // Emit verdict flow record.
    bookend.close(
        "phase-review",
        build_review_record(
            crate::flow::Level::Info,
            crate::flow::Category::Review,
            crate::flow::Tier::Frontier,
            crate::flow::Stage::Review,
            format!("verdict: {}", signoff.verdict.clone()),
            format!("{}B / {}F / {}N", signoff.block, signoff.flag, signoff.nit),
            &session_id,
            phase_id,
        ),
    );

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── phase review tests ─────────────────────────────────────────────

    fn sample_signoff() -> String {
        "QA-REVIEW-SIGNOFF\n\
            - [BLOCK] src/main.rs:42 — null pointer dereference on unwrap()\n\
            - [FLAG] src/utils.rs:17 — unused variable `temp_buf`\n\
            - [NIT] src/config.rs:8 — inconsistent spacing after `fn`\n\
            Summary: 1 blockers, 1 flags, 1 nits"
            .to_string()
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
        // of `- [FLAG]`. Empirically caught during Phase 1 of #66's dogfood
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
        assert!(result.findings[1]
            .text
            .contains("sync_all errors swallowed"));
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

    // ── phase review flow record tests ───────────────────────────────────

    struct FlowsDirGuard {
        prev: Option<String>,
        tmp: tempfile::TempDir,
    }

    impl FlowsDirGuard {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via `#[serial_test::serial]`.
            unsafe {
                std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
            }
            Self { prev, tmp }
        }

        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }
    }

    impl Drop for FlowsDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via the test attribute.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn review_start_record_has_review_category_and_frontier_tier() {
        let guard = FlowsDirGuard::new();

        // Create a minimal git repo with NO diff (empty-diff path).
        let repo = tempfile::TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo.path())
            .output()
            .ok();

        crate::phase_cli::phase_review_output_at(repo.path(), None, Some("66")).unwrap();

        // Read all jsonl files; expect 2 records (start + verdict).
        let files: Vec<_> = std::fs::read_dir(guard.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();

        let records: Vec<String> = files
            .iter()
            .flat_map(|p| {
                let content = std::fs::read_to_string(p).unwrap();
                content.lines().map(String::from).collect::<Vec<_>>()
            })
            .filter(|l| !l.contains("\"_type\":\"schema\""))
            .collect();

        assert_eq!(records.len(), 2, "expected start + verdict records");

        // Parse each and check the first is review-start.
        let start: serde_json::Value = serde_json::from_str(&records[0]).unwrap();
        assert_eq!(start["action"], "phase review begin");
        assert_eq!(start["category"], "review");
        assert_eq!(start["tier"], "frontier");

        // Second is verdict.
        let verdict: serde_json::Value = serde_json::from_str(&records[1]).unwrap();
        let action: &str = verdict["action"].as_str().unwrap();
        assert!(action.starts_with("verdict:"));
    }

    #[serial_test::serial]
    #[test]
    fn verdict_record_handle_has_block_flag_nit_count_format() {
        let guard = FlowsDirGuard::new();

        // Empty diff → verdict handle = "0B / 0F / 0N".
        let repo = tempfile::TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo.path())
            .output()
            .ok();

        crate::phase_cli::phase_review_output_at(repo.path(), None, Some("66")).unwrap();

        let records = collect_records(guard.path());
        // Find verdict record (action starts with "verdict:").
        let verdict = records
            .iter()
            .find(|r| {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(r) {
                    if let Some(s) = v["action"].as_str() {
                        return s.starts_with("verdict: ");
                    }
                }
                false
            })
            .expect("expected verdict record");

        let v: serde_json::Value = serde_json::from_str(verdict).unwrap();
        assert_eq!(v["handle"], "0B / 0F / 0N");
    }

    #[test]
    fn dispatch_failed_record_has_error_level_and_machinery_category() {
        // Test the dispatch-failed record SHAPE directly via build_review_record.
        // Triggering a real dispatch failure inside phase_review_output_at requires a
        // repo with a non-empty diff against `main` AND a guaranteed dispatch
        // path — neither is reliable across CI envs (the temp repo has no
        // `main` branch in CI's git defaults). Testing the helper directly
        // captures the contract without env-coupling.
        let record = crate::phase_cli::build_review_record(
            crate::flow::Level::Error,
            crate::flow::Category::Machinery,
            crate::flow::Tier::Local,
            crate::flow::Stage::Review,
            "dispatch failed".to_string(),
            "openclaw exit 1".to_string(),
            "phase-review-12345",
            Some("66"),
        );

        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["level"], "error");
        assert_eq!(json["category"], "machinery");
        assert_eq!(json["tier"], "local");
        assert_eq!(json["stage"], "review");
        assert_eq!(json["action"], "dispatch failed");
        assert_eq!(json["source"], "phase_review");
        assert_eq!(json["phase_id"], "66");
        assert_eq!(json["session_id"], "phase-review-12345");
    }

    #[serial_test::serial]
    #[test]
    fn flow_record_failure_does_not_crash_review() {
        // Point DARKMUX_FLOWS_DIR at a read-only path so flow::record fails.
        // The review should still complete normally (observability is non-fatal).
        // Empty-diff repo: avoids dispatching to live LMStudio.

        let tmp = tempfile::TempDir::new().unwrap();
        // Make the dir read-only — record() will fail to write.
        let mut perms = std::fs::metadata(tmp.path()).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(tmp.path(), perms).unwrap();

        // Empty-diff git repo: the empty-diff early-return path still emits
        // both start and verdict flow records, exercising the non-fatal
        // contract without going down the dispatch path.
        let repo = tempfile::TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo.path())
            .output()
            .ok();

        let prev = std::env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: serial test.
        unsafe {
            std::env::set_var("DARKMUX_FLOWS_DIR", tmp.path());
        }

        let result = crate::phase_cli::phase_review_output_at(repo.path(), None, Some("66"));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        // Even though flow recording fails, review should succeed (non-fatal).
        assert!(
            result.is_ok(),
            "review verb succeeds despite flow record failure"
        );

        // Verify no records were written (flow recording failed non-fatally).
        let jsonl_count: usize = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .count();
        assert_eq!(
            jsonl_count, 0,
            "expected no flow records written to read-only dir"
        );
    }

    /// Read all non-schema record lines from jsonl files in a directory.
    fn collect_records(dir: &std::path::Path) -> Vec<String> {
        let paths: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        paths
            .iter()
            .flat_map(|p| {
                let content = std::fs::read_to_string(p).unwrap();
                content.lines().map(String::from).collect::<Vec<_>>()
            })
            .filter(|l| !l.contains("\"_type\":\"schema\""))
            .collect()
    }

    // ---- #1413: liveness-guard regression tests ----

    /// `phase_review_output_at`'s begin/verdict pair used to be plain
    /// paired writes (#1413): a panic between them orphaned the begin
    /// record. Drives the same `BookendGuard` + `on_abort` shape the
    /// production code wires up and confirms a panic mid-guarded-region
    /// still fires the abort record on Drop (mirrors
    /// `darkmux-flow::bookend`'s own `panic_while_armed_still_fires_the_abort_record`).
    #[test]
    fn phase_review_bookend_fires_abort_record_on_panic() {
        let mut actions: Vec<String> = Vec::new();
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut sink = |r: crate::flow::FlowRecord| actions.push(r.action);
            let on_abort =
                move |_id: &str, _kind: &str| phase_review_abort_record("main", "sess-1", Some("66"));
            let mut guard = crate::flow::BookendGuard::new(&mut sink, on_abort);
            guard.open(
                "phase-review",
                "phase-review",
                build_review_record(
                    crate::flow::Level::Info,
                    crate::flow::Category::Review,
                    crate::flow::Tier::Frontier,
                    crate::flow::Stage::Review,
                    "phase review begin".to_string(),
                    "main".to_string(),
                    "sess-1",
                    Some("66"),
                ),
            );
            panic!("simulated mid-review panic");
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err());
        assert_eq!(actions, vec!["phase review begin", "phase review aborted"]);
    }

    /// `dispatch_compiler`'s `mission.compile.start`/`.error` pair used to
    /// be plain paired writes (#1413): a panic between them orphaned the
    /// start record. Drives the same guard shape mission_propose.rs wires
    /// up and confirms a panic mid-guarded-region still fires the abort
    /// record on Drop.
    #[test]
    fn mission_compile_bookend_fires_abort_record_on_panic() {
        let mut actions: Vec<String> = Vec::new();
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut sink = |r: crate::flow::FlowRecord| actions.push(r.action);
            let on_abort =
                move |_id: &str, _kind: &str| crate::mission_propose::mission_compile_abort_record("mission-compile-1");
            let mut guard = crate::flow::BookendGuard::new(&mut sink, on_abort);
            guard.open(
                "mission.compile",
                "mission.compile",
                crate::crew::dispatch::build_dispatch_record_with_payload(
                    crate::flow::Level::Info,
                    "mission.compile.start",
                    "mission-compiler",
                    "mission-compile-1",
                    None,
                    None,
                    None,
                    None,
                ),
            );
            panic!("simulated mid-compile panic");
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err());
        assert_eq!(
            actions,
            vec!["mission.compile.start", "mission.compile.error"]
        );
    }

    /// Happy-path action-name pin: the empty-diff `phase review` path
    /// still emits exactly `"phase review begin"` then `"verdict: clean"`.
    /// The bookend-guard refactor (#1413) must not rename the vocabulary
    /// downstream liveness/viewer consumers key on.
    #[serial_test::serial]
    #[test]
    fn phase_review_happy_path_action_names_unchanged() {
        let guard = FlowsDirGuard::new();

        let repo = tempfile::TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo.path())
            .output()
            .ok();

        crate::phase_cli::phase_review_output_at(repo.path(), None, Some("66")).unwrap();

        let records = collect_records(guard.path());
        assert_eq!(records.len(), 2, "expected begin + verdict records only");

        let start: serde_json::Value = serde_json::from_str(&records[0]).unwrap();
        assert_eq!(start["action"], "phase review begin");

        let verdict: serde_json::Value = serde_json::from_str(&records[1]).unwrap();
        assert_eq!(verdict["action"], "verdict: clean");
    }
}

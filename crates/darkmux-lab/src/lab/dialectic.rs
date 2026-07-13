//! (#1222) Dialectic (adversarial) review orchestration — review-bench's
//! dialectic mode. Three chained crew dispatches per case:
//!
//! ```text
//! prosecutor(diff + repo) → defender(diff + repo + charges) → judge(record)
//! ```
//!
//! The dispatches stay atomic (one role, one model, one flow stream each);
//! the debate exists as a composite ARTIFACT — the per-case [`DebateEnvelope`]
//! written beside `scores.json` — not as a new dispatch or flow shape. Every
//! sustained finding carries its evidence chain: the charge, the defense's
//! answer, and the judge's decisive evidence.
//!
//! Design notes (booked on #1222):
//! - Sequential rebuttal, not parallel-blind: a defender with no specific
//!   charges rubber-stamps; answering a handed claim is the shape local
//!   models demonstrably handle. Sequential also serializes local RAM use.
//! - Citation validation is HARNESS-side: charge anchors are verified
//!   against the diff before the defense sees them (fabricated anchors are
//!   struck), and defense counter-evidence quotes are verified against the
//!   diff + repo tree (a fabricated quote voids nothing silently — the judge
//!   is told to weigh the answer as uncited).
//! - The judge rules on the record: tool-less, single-turn, over charged-hunk
//!   excerpts — advocates gather evidence; the judge weighs what is presented.
//! - Charges carry EXPLICIT `CHARGE <n>` numbers (frontier QA on the P0 PR:
//!   positional numbering would let one misparsed line desync
//!   rebuttals ↔ verdicts), and the harness canonically renumbers anyway.
//!
//! Parsers here are pure and unit-tested; dispatching goes through a
//! caller-provided closure so the chain is testable without containers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use super::review_bench::{Case, Finding, Review};

/// Which seat a dispatch belongs to — the closure maps this to the seat's
/// profile (debug phase: one local profile fills all three seats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seat {
    Prosecutor,
    Defender,
    Judge,
}

impl Seat {
    pub fn role_id(self) -> &'static str {
        match self {
            Seat::Prosecutor => "dialectic-prosecutor",
            Seat::Defender => "dialectic-defender",
            Seat::Judge => "dialectic-judge",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Seat::Prosecutor => "prosecutor",
            Seat::Defender => "defender",
            Seat::Judge => "judge",
        }
    }
}

// ─── charge sheet (prosecutor output) ────────────────────────────────────

/// One parsed charge. `number` is the harness-canonical number (1-based file
/// order — what the defender and judge see); `model_number` is what the
/// model wrote in the marker, kept for diagnostics.
#[derive(Debug, Clone, Serialize)]
pub struct Charge {
    pub number: u32,
    pub model_number: u32,
    pub path: String,
    /// Verbatim quoted line from the diff; `None` = a general charge.
    pub anchor: Option<String>,
    pub body: String,
    /// Anchor not found in the diff (fabricated / mistyped): struck before
    /// the defense and judge see the charge, per the role contract.
    pub struck: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Closing {
    Rested,
    NoCharges,
    Missing,
}

#[derive(Debug)]
pub struct ChargeSheet {
    pub charges: Vec<Charge>,
    pub closing: Closing,
    /// False only for a genuinely empty reply (degenerate dispatch).
    pub parsed: bool,
}

/// Parse a prosecutor reply: `CHARGE <n> [path]` (+ optional backtick anchor)
/// marker blocks, closed by `CASE: rested` / `CASE: no-charges`. Marker
/// detection requires the FULL shape (word + digits + `[path]`), so a body
/// line that merely begins with "Charge" can never spawn a phantom charge.
/// Charges are canonically renumbered 1..n in file order.
pub fn parse_charges(text: &str) -> ChargeSheet {
    let mut charges: Vec<Charge> = Vec::new();
    let mut closing = Closing::Missing;
    // (model_number, path, anchor, body)
    let mut current: Option<(u32, String, Option<String>, String)> = None;
    let flush = |cur: &mut Option<(u32, String, Option<String>, String)>,
                 out: &mut Vec<Charge>| {
        if let Some((model_number, path, anchor, body)) = cur.take() {
            out.push(Charge {
                number: out.len() as u32 + 1,
                model_number,
                path,
                anchor,
                body: body.trim().to_string(),
                struck: false,
            });
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if let Some((n, path, anchor)) = charge_marker(line) {
            flush(&mut current, &mut charges);
            current = Some((n, path, anchor, String::new()));
        } else if let Some(c) = case_closing(line) {
            flush(&mut current, &mut charges);
            closing = c;
        } else if let Some((_, _, _, body)) = current.as_mut() {
            if !line.is_empty() {
                if !body.is_empty() {
                    body.push(' ');
                }
                body.push_str(line);
            }
        }
    }
    flush(&mut current, &mut charges);
    ChargeSheet {
        charges,
        closing,
        parsed: !text.trim().is_empty(),
    }
}

/// Strip leading list/emphasis decoration the way the freeform parser does.
fn strip_deco(line: &str) -> &str {
    line.trim_start_matches(|c: char| matches!(c, '-' | '*' | '•' | '_') || c.is_whitespace())
}

/// `CHARGE <digits> [path]` (+ optional `` `anchor` ``) — full shape or no
/// marker. Uses `str::get` so arbitrary UTF-8 can't panic on a boundary.
fn charge_marker(line: &str) -> Option<(u32, String, Option<String>)> {
    let s = strip_deco(line);
    let word = s.get(.."CHARGE".len())?;
    if !word.eq_ignore_ascii_case("CHARGE") {
        return None;
    }
    let s = s["CHARGE".len()..].trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    let s = s[digits.len()..].trim_start().trim_start_matches(':').trim_start();
    let rest = s.strip_prefix('[')?;
    let close = rest.find(']')?;
    let path = rest[..close].trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some((n, path, extract_backtick(&rest[close + 1..])))
}

/// `CASE: rested` / `CASE: no-charges` (tolerating "no charges").
fn case_closing(line: &str) -> Option<Closing> {
    let s = strip_deco(line);
    let word = s.get(.."CASE".len())?;
    if !word.eq_ignore_ascii_case("CASE") {
        return None;
    }
    let rest = s["CASE".len()..]
        .trim_start()
        .trim_start_matches(':')
        .trim();
    let rest = rest.to_ascii_lowercase();
    if rest.starts_with("rested") {
        Some(Closing::Rested)
    } else if rest.starts_with("no-charges") || rest.starts_with("no charges") {
        Some(Closing::NoCharges)
    } else {
        None
    }
}

/// First non-empty backtick-quoted span in `s`.
fn extract_backtick(s: &str) -> Option<String> {
    let open = s.find('`')?;
    let close_rel = s[open + 1..].find('`')?;
    let inner = s[open + 1..open + 1 + close_rel].trim();
    (!inner.is_empty()).then(|| inner.to_string())
}

// ─── answer sheet (defender output) ──────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    Refute,
    Mitigate,
    Concede,
    /// A well-shaped marker with an off-contract stance word — kept (the
    /// judge reads the raw word) but tallied separately in review-pipeline metrics.
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rebuttal {
    pub charge: u32,
    pub stance: Stance,
    pub raw_stance: String,
    pub body: String,
    /// A qualifying quoted span in this answer was not found in the diff or
    /// the repo tree — the judge is told to weigh the answer as uncited.
    pub voided: bool,
}

#[derive(Debug)]
pub struct AnswerSheet {
    pub rebuttals: Vec<Rebuttal>,
    pub rests: bool,
    pub parsed: bool,
}

/// Parse a defender reply: `REBUTTAL <n>: <stance>` blocks closed by
/// `DEFENSE: rests`. Inline text after the stance word folds into the body.
pub fn parse_rebuttals(text: &str) -> AnswerSheet {
    let mut rebuttals: Vec<Rebuttal> = Vec::new();
    let mut rests = false;
    let mut current: Option<Rebuttal> = None;
    let flush = |cur: &mut Option<Rebuttal>, out: &mut Vec<Rebuttal>| {
        if let Some(mut r) = cur.take() {
            r.body = r.body.trim().to_string();
            out.push(r);
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(r) = rebuttal_marker(line) {
            flush(&mut current, &mut rebuttals);
            current = Some(r);
        } else if defense_rests(line) {
            flush(&mut current, &mut rebuttals);
            rests = true;
        } else if let Some(r) = current.as_mut() {
            if !line.is_empty() {
                if !r.body.is_empty() {
                    r.body.push(' ');
                }
                r.body.push_str(line);
            }
        }
    }
    flush(&mut current, &mut rebuttals);
    AnswerSheet {
        rebuttals,
        rests,
        parsed: !text.trim().is_empty(),
    }
}

fn rebuttal_marker(line: &str) -> Option<Rebuttal> {
    let s = strip_deco(line);
    let word = s.get(.."REBUTTAL".len())?;
    if !word.eq_ignore_ascii_case("REBUTTAL") {
        return None;
    }
    let s = s["REBUTTAL".len()..].trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let charge: u32 = digits.parse().ok()?;
    let s = s[digits.len()..]
        .trim_start()
        .trim_start_matches(':')
        .trim_start();
    let raw_stance: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if raw_stance.is_empty() {
        return None;
    }
    let lower = raw_stance.to_ascii_lowercase();
    let stance = if lower.starts_with("refut") {
        Stance::Refute
    } else if lower.starts_with("mitigat") {
        Stance::Mitigate
    } else if lower.starts_with("conced") {
        Stance::Concede
    } else {
        Stance::Other
    };
    let inline = s[raw_stance.len()..]
        .trim_start_matches(|c: char| matches!(c, '-' | '—' | ':' | '–') || c.is_whitespace())
        .trim()
        .to_string();
    Some(Rebuttal {
        charge,
        stance,
        raw_stance,
        body: inline,
        voided: false,
    })
}

fn defense_rests(line: &str) -> bool {
    let s = strip_deco(line);
    let Some(word) = s.get(.."DEFENSE".len()) else {
        return false;
    };
    if !word.eq_ignore_ascii_case("DEFENSE") {
        return false;
    }
    s["DEFENSE".len()..]
        .trim_start()
        .trim_start_matches(':')
        .trim()
        .to_ascii_lowercase()
        .starts_with("rest")
}

// ─── rulings (judge output) ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawJudgeOutput {
    #[serde(default)]
    summary: String,
    verdicts: Vec<RawVerdict>,
}

#[derive(Debug, Deserialize)]
struct RawVerdict {
    charge: u32,
    ruling: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    decisive_evidence: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Ruling {
    Sustained,
    Dismissed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub charge: u32,
    pub ruling: Ruling,
    pub severity: Option<String>,
    pub decisive_evidence: String,
    pub reasoning: String,
}

#[derive(Debug)]
pub struct Rulings {
    pub summary: String,
    pub verdicts: Vec<Verdict>,
    /// Verdicts dropped for an off-contract `ruling` value.
    pub dropped: usize,
}

/// Parse the judge reply: reasoning prose then one fenced JSON block. The
/// prose may itself contain code fences (quoted evidence), so candidates are
/// tried LAST-fence-first, `json`-tagged fences ahead of untagged ones
/// (frontier QA note 5 on the P0 PR), then the whole-text / brace-span
/// fallbacks the strict reviewer parser uses.
pub fn parse_rulings(text: &str) -> Option<Rulings> {
    for cand in judge_json_candidates(text) {
        if let Ok(raw) = serde_json::from_str::<RawJudgeOutput>(&cand) {
            let mut verdicts = Vec::new();
            let mut dropped = 0usize;
            for v in raw.verdicts {
                let ruling = if v.ruling.eq_ignore_ascii_case("sustained") {
                    Ruling::Sustained
                } else if v.ruling.eq_ignore_ascii_case("dismissed") {
                    Ruling::Dismissed
                } else {
                    dropped += 1;
                    continue;
                };
                verdicts.push(Verdict {
                    charge: v.charge,
                    ruling,
                    severity: v.severity.and_then(normalize_severity),
                    decisive_evidence: v.decisive_evidence.unwrap_or_default(),
                    reasoning: v.reasoning.unwrap_or_default(),
                });
            }
            return Some(Rulings {
                summary: raw.summary,
                verdicts,
                dropped,
            });
        }
    }
    None
}

fn normalize_severity(s: String) -> Option<String> {
    let l = s.trim().to_ascii_lowercase();
    matches!(l.as_str(), "high" | "medium" | "low").then_some(l)
}

fn judge_json_candidates(text: &str) -> Vec<String> {
    let mut tagged: Vec<String> = Vec::new();
    let mut untagged: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(close) = after.find("```") else { break };
        let block = &after[..close];
        let (is_json, inner) = match block.strip_prefix("json") {
            Some(inner) => (true, inner),
            None => (false, block),
        };
        let inner = inner.trim();
        if !inner.is_empty() {
            if is_json {
                tagged.push(inner.to_string());
            } else {
                untagged.push(inner.to_string());
            }
        }
        rest = &after[close + 3..];
    }
    let mut out: Vec<String> = Vec::new();
    out.extend(tagged.into_iter().rev());
    out.extend(untagged.into_iter().rev());
    let s = text.trim();
    out.push(s.to_string());
    if let (Some(a), Some(b)) = (s.find('{'), s.rfind('}')) {
        if b > a {
            out.push(s[a..=b].to_string());
        }
    }
    out
}

// ─── citation validation (harness-side, mechanical) ─────────────────────

/// The content of a unified-diff line: one leading `+`/`-`/space stripped.
///
/// `pub(crate)`: reused by `super::review`'s anchor extraction (#1222 Phase
/// B packet 4) so both charge-anchor matchers share ONE normalization
/// discipline rather than re-deriving the same wrapped-line / marker-strip
/// fixes twice.
pub(crate) fn diff_line_content(line: &str) -> &str {
    line.strip_prefix(['+', '-', ' ']).unwrap_or(line)
}

/// Normalize a model-quoted anchor for matching. Models sometimes keep the
/// diff's leading `+`/`-` marker despite the "line content only" contract —
/// strip one, exactly like `diff_line_content` does on the diff side, so a
/// verbatim-with-marker quote never false-strikes a real charge (frontier QA
/// MUST-FIX on the P1 PR: an asymmetric match struck the charge, and a fully
/// struck bug case scored as a clean pass). Used by every anchor/quote
/// matcher so the strike decision, the excerpt window, and the quote check
/// stay consistent.
pub(crate) fn normalize_anchor(a: &str) -> &str {
    diff_line_content(a.trim()).trim()
}

/// Whitespace runs collapsed to single spaces — the form both sides of a
/// wrapped-line match are reduced to.
pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The diff's CONTENT (markers stripped) joined into one whitespace-collapsed
/// string. The fallback haystack for quotes of a LOGICAL line the diff wraps
/// across physical lines. Shakedown-1 (#1222): the prosecutor quoted
/// `const gapStart = lastRecord ? lastRecord.billingEndAt.plus({` — a direct
/// hit on the case's labeled bug — but the diff breaks that expression after
/// `lastRecord`, so per-line containment struck the charge and the case
/// scored as a clean pass. Wrapped quotes are legitimate verbatim evidence;
/// only per-PHYSICAL-line matching says otherwise.
pub(crate) fn collapsed_diff_content(diff: &str) -> String {
    collapse_ws(
        &diff
            .lines()
            .map(diff_line_content)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Strike every charge whose anchor is not a line in the diff — fabricated
/// (or mistyped) evidence never reaches the defense or the judge. Returns
/// the struck count.
pub fn validate_charge_anchors(charges: &mut [Charge], diff: &str) -> usize {
    let collapsed = collapsed_diff_content(diff);
    let mut struck = 0usize;
    for c in charges.iter_mut() {
        let Some(anchor) = c.anchor.as_deref() else {
            continue; // general charge — nothing to verify mechanically
        };
        let a = normalize_anchor(anchor);
        // Per-physical-line first; the collapsed haystack catches verbatim
        // quotes of a logical line the diff wraps (shakedown-1 false strike).
        let found = diff.lines().any(|l| diff_line_content(l).contains(a))
            || collapsed.contains(&collapse_ws(a));
        if !found {
            c.struck = true;
            struck += 1;
        }
    }
    struck
}

/// Minimum length for a quoted span to be treated as verifiable evidence —
/// shorter spans (`x`, `?？`, `}`) are inline code styling, not citations.
/// `pub(crate)`: `super::review`'s anchor extraction applies the same floor
/// so a trivial span (`0`, `}`) can never become an anchor / dedup key.
pub(crate) const MIN_EVIDENCE_SPAN: usize = 8;

/// Verify each rebuttal's quoted counter-evidence against the diff and the
/// repo tree (paths named in the rebuttal body + the charged file). A span
/// found nowhere marks the rebuttal `voided` — rendered to the judge as
/// "weigh it as uncited", never silently dropped. With no workdir available
/// nothing is voided (insufficient evidence to strike). Returns the voided
/// count.
pub fn validate_rebuttal_quotes(
    rebuttals: &mut [Rebuttal],
    charges: &[Charge],
    diff: &str,
    workdir: Option<&Path>,
) -> usize {
    let Some(root) = workdir else {
        return 0;
    };
    let mut file_cache: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut read = |root: &Path, rel: &str| -> Option<String> {
        file_cache
            .entry(rel.to_string())
            .or_insert_with(|| std::fs::read_to_string(root.join(rel)).ok())
            .clone()
    };
    let collapsed = collapsed_diff_content(diff);
    let mut voided = 0usize;
    for r in rebuttals.iter_mut() {
        let spans: Vec<String> = backtick_spans(&r.body)
            .into_iter()
            .filter(|s| s.trim().len() >= MIN_EVIDENCE_SPAN)
            // A backticked `[path]` is a FILE REFERENCE, not an evidence
            // quote — defenders habitually write `[app/x.ts]` with the
            // brackets inside the backticks. Shakedown-1+3 (#1222): that
            // single span failed lookup in 9 of 9 voided rebuttals and
            // discredited every honest defense. (The path itself is still
            // harvested by bracket_paths below — it scans the whole body.)
            .filter(|s| !is_bracketed_path(s))
            .collect();
        if spans.is_empty() {
            continue;
        }
        // Candidate sources: the diff, files named [in brackets] in the
        // body, and the charged file itself.
        let mut paths: Vec<String> = bracket_paths(&r.body);
        if let Some(c) = charges.iter().find(|c| c.number == r.charge) {
            paths.push(c.path.clone());
        }
        let all_found = spans.iter().all(|span| {
            let s = normalize_anchor(span);
            diff.lines().any(|l| diff_line_content(l).contains(s))
                || collapsed.contains(&collapse_ws(s))
                || paths.iter().any(|p| {
                    read(root, p).is_some_and(|body| {
                        body.contains(s) || collapse_ws(&body).contains(&collapse_ws(s))
                    })
                })
        });
        if !all_found {
            r.voided = true;
            voided += 1;
        }
    }
    voided
}

/// Every SINGLE-backtick inline span in `s`, skipping fenced regions.
/// Rebuttal bodies carry ```-fenced code blocks (the marker parser folds
/// them inline), and a naive pairwise scanner treats fence backticks as
/// span delimiters — producing garbage "spans" that can never validate.
/// Shakedown-1 (#1222): that voided 8 of 9 honest rebuttals, telling the
/// judge to distrust defenses that were quoting the diff verbatim. Runs of
/// 2+ backticks toggle fence state and cancel any open inline span; only
/// single-backtick pairs outside a fence produce spans. An unclosed fence
/// swallows the remainder (fewer spans → skip-validation, never a void).
pub(crate) fn backtick_spans(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut span_start: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && chars[j] == '`' {
            j += 1;
        }
        if j - i >= 2 {
            in_fence = !in_fence;
            span_start = None;
        } else if !in_fence {
            match span_start.take() {
                None => span_start = Some(j),
                Some(start) => {
                    let inner: String = chars[start..i].iter().collect();
                    let inner = inner.trim();
                    if !inner.is_empty() {
                        out.push(inner.to_string());
                    }
                }
            }
        }
        i = j;
    }
    out
}

/// True when the span is exactly one `[bracketed]` path-looking reference —
/// the shape defenders emit as `` `[app/x.ts]` `` (a citation of WHERE, not
/// WHAT; never validated as an evidence quote).
fn is_bracketed_path(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('[') && t.ends_with(']') && bracket_paths(t).len() == 1
}

/// `[bracketed]` spans that look like file paths (contain `/` or `.`).
fn bracket_paths(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        let inner = after[..close].trim();
        if !inner.is_empty() && (inner.contains('/') || inner.contains('.')) && !inner.contains(' ')
        {
            out.push(inner.to_string());
        }
        rest = &after[close + 1..];
    }
    out
}

// ─── the judge's record: charged-hunk excerpts ───────────────────────────

/// Context lines around a charged anchor in the diff excerpt.
const EXCERPT_CONTEXT_LINES: usize = 25;
/// Full-diff fallback cap when no charge is anchored — truncation is loud.
const EXCERPT_FALLBACK_CAP: usize = 24_000;

/// Judge context scope v1 (design §4, cheapest rung): procedurally excerpted
/// windows around each active anchored charge, merged when they overlap.
/// Falls back to the (capped, loudly-truncated) full diff when nothing is
/// anchored; uses the full diff outright when the excerpts wouldn't save
/// anything.
pub fn charged_excerpts(diff: &str, charges: &[Charge]) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    // (start, end, charge numbers)
    let mut ranges: Vec<(usize, usize, Vec<u32>)> = Vec::new();
    for c in charges.iter().filter(|c| !c.struck) {
        let Some(anchor) = c.anchor.as_deref() else {
            continue;
        };
        let a = normalize_anchor(anchor);
        // Per-line locate first; wrapped logical-line quotes fall back to the
        // first physical line whose content opens the quote (same class as
        // the validate_charge_anchors fallback — the window just needs to
        // land in the right neighborhood, ±context covers the rest).
        let idx = lines
            .iter()
            .position(|l| diff_line_content(l).contains(a))
            .or_else(|| {
                let ca = collapse_ws(a);
                lines.iter().position(|l| {
                    let cl = collapse_ws(diff_line_content(l));
                    cl.len() >= 4 && ca.starts_with(&cl)
                })
            });
        if let Some(idx) = idx {
            let start = idx.saturating_sub(EXCERPT_CONTEXT_LINES);
            let end = (idx + EXCERPT_CONTEXT_LINES).min(lines.len().saturating_sub(1));
            ranges.push((start, end, vec![c.number]));
        }
    }
    if ranges.is_empty() {
        if diff.len() <= EXCERPT_FALLBACK_CAP {
            return diff.to_string();
        }
        let mut cut = EXCERPT_FALLBACK_CAP;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        return format!(
            "{}\n… [diff truncated at {EXCERPT_FALLBACK_CAP} characters — no charge \
             carried a locatable anchor, so no excerpt window could be built]",
            &diff[..cut]
        );
    }
    ranges.sort_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize, Vec<u32>)> = Vec::new();
    for (start, end, nums) in ranges {
        match merged.last_mut() {
            Some((_, last_end, last_nums)) if start <= *last_end + 1 => {
                *last_end = (*last_end).max(end);
                last_nums.extend(nums);
            }
            _ => merged.push((start, end, nums)),
        }
    }
    let mut out = String::new();
    for (start, end, nums) in &merged {
        let labels: Vec<String> = nums.iter().map(|n| n.to_string()).collect();
        out.push_str(&format!(
            "── diff excerpt (charge {}) — lines {}-{} of the diff ──\n",
            labels.join(", "),
            start + 1,
            end + 1
        ));
        out.push_str(&lines[*start..=*end].join("\n"));
        out.push('\n');
    }
    // Excerpting that saves nothing is just a worse full diff.
    if out.len() >= diff.len() {
        return diff.to_string();
    }
    out
}

// ─── prompt builders ─────────────────────────────────────────────────────

fn intent_block(case: &Case) -> String {
    let body = if case.label.intent_body.trim().is_empty() {
        "(no description provided)"
    } else {
        case.label.intent_body.trim()
    };
    format!(
        "PR review request: {}\n\nDescription (the author's stated intent for this change):\n{}\n",
        case.label.intent_title, body
    )
}

pub fn prosecutor_prompt(case: &Case) -> String {
    format!(
        "{}\nFile your charges per your role contract. Assess the diff against the \
         stated intent above. The full diff is below, and the repository at the \
         reviewed commit is checked out in your working directory — investigate the \
         surrounding code to gather evidence before charging.\n\n```diff\n{}\n```\n",
        intent_block(case),
        case.diff
    )
}

/// Render active (non-struck) charges the way the defender and judge see
/// them — harness-canonical numbers, `Charge <n> [path]` headers.
fn render_charges(charges: &[Charge]) -> String {
    let mut out = String::new();
    for c in charges.iter().filter(|c| !c.struck) {
        match &c.anchor {
            Some(a) => out.push_str(&format!("Charge {} [{}] `{}`\n", c.number, c.path, a)),
            None => out.push_str(&format!("Charge {} [{}]\n", c.number, c.path)),
        }
        out.push_str(&c.body);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

pub fn defender_prompt(case: &Case, charges: &[Charge]) -> String {
    format!(
        "{}\nThe prosecution has filed the numbered charges below against this \
         change. Answer each charge per your role contract. The full diff follows \
         the charges, and the repository at the reviewed commit is checked out in \
         your working directory — verify each charge against the actual code before \
         answering.\n\n## Charges\n\n{}\n\n```diff\n{}\n```\n",
        intent_block(case),
        render_charges(charges),
        case.diff
    )
}

fn render_defense(charges: &[Charge], answers: &AnswerSheet, defense_degenerate: bool) -> String {
    if defense_degenerate {
        return "The defense did not submit an answer sheet; every charge stands \
                unanswered."
            .to_string();
    }
    let mut out = String::new();
    for c in charges.iter().filter(|c| !c.struck) {
        let answered: Vec<&Rebuttal> = answers
            .rebuttals
            .iter()
            .filter(|r| r.charge == c.number)
            .collect();
        if answered.is_empty() {
            out.push_str(&format!(
                "Charge {} received no answer from the defense.\n\n",
                c.number
            ));
            continue;
        }
        for r in answered {
            out.push_str(&format!(
                "Answer to charge {} — {}:\n{}\n",
                r.charge, r.raw_stance, r.body
            ));
            if r.voided {
                out.push_str(
                    "(note: a quoted line in this answer could not be verified \
                     against the repository — weigh it as uncited)\n",
                );
            }
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

pub fn judge_prompt(
    case: &Case,
    charges: &[Charge],
    answers: &AnswerSheet,
    defense_degenerate: bool,
) -> String {
    format!(
        "{}\nRule on the numbered charges per your role contract. The record \
         follows: the relevant diff excerpts, the prosecution's charges, and the \
         defense's answers.\n\n## Diff excerpts\n\n```diff\n{}\n```\n\n\
         ## Charges\n\n{}\n\n## Defense\n\n{}\n",
        intent_block(case),
        charged_excerpts(&case.diff, charges),
        render_charges(charges),
        render_defense(charges, answers, defense_degenerate)
    )
}

// ─── the debate envelope (the composite artifact) ────────────────────────

#[derive(Debug, Default, Serialize)]
pub struct SeatRecord {
    pub role_id: String,
    pub model: Option<String>,
    pub total_tokens: Option<u64>,
}

/// The per-case composite artifact — the debate as a unit, written beside
/// `scores.json`. Every sustained finding's evidence chain is recoverable
/// from it: the charge, the answer, the ruling and its decisive evidence.
#[derive(Debug, Default, Serialize)]
pub struct DebateEnvelope {
    pub case_id: String,
    pub prosecutor: SeatRecord,
    pub defender: Option<SeatRecord>,
    pub judge: Option<SeatRecord>,
    pub charges: Vec<Charge>,
    pub prosecutor_closing: Option<Closing>,
    pub struck_charges: usize,
    pub rebuttals: Vec<Rebuttal>,
    pub defense_rests: bool,
    pub defense_degenerate: bool,
    pub voided_rebuttals: usize,
    pub judge_summary: String,
    pub verdicts: Vec<Verdict>,
    pub dropped_verdicts: usize,
    pub unruled_charges: Vec<u32>,
    pub sustained: usize,
}

/// Synthesize the final `Review` from the rulings: sustained charges become
/// findings (judge-graded severity; the charge's anchor + body feed the
/// scorer's anchor/title haystacks); verdict flags iff anything sustained.
/// Entirely upstream of `score()` — every arm stays comparable.
pub fn synthesize_review(charges: &[Charge], rulings: &Rulings) -> Review {
    let mut findings = Vec::new();
    for v in &rulings.verdicts {
        if v.ruling != Ruling::Sustained {
            continue;
        }
        let Some(c) = charges.iter().find(|c| c.number == v.charge && !c.struck) else {
            continue; // ruling on a nonexistent/struck charge number
        };
        findings.push(Finding {
            severity: v.severity.clone().unwrap_or_else(|| "medium".to_string()),
            anchor: c.anchor.clone().unwrap_or_else(|| c.body.clone()),
            title: c.body.clone(),
        });
    }
    Review {
        verdict: if findings.is_empty() { "pass" } else { "flag" }.to_string(),
        findings,
        parsed: true,
    }
}

// ─── the chain ───────────────────────────────────────────────────────────

/// Run one case through prosecutor → defender → judge. `dispatch` performs
/// one seat's crew dispatch and returns the raw `--json` envelope stdout
/// (the closure owns profile/workdir mapping — debug phase: one local 35B
/// profile fills every seat). Dispatch-level errors propagate; contract
/// failures degrade honestly (a degenerate prosecutor or an unparseable
/// judge yields `Review { parsed: false }`, never a silent pass).
pub fn run_debate(
    case: &Case,
    workdir: Option<&Path>,
    mut dispatch: impl FnMut(Seat, &str) -> Result<String>,
) -> Result<(Review, DebateEnvelope)> {
    use super::review_bench::envelope_meta;
    use crate::providers::prompt::extract_reply_text;

    let mut env = DebateEnvelope {
        case_id: case.id.clone(),
        ..Default::default()
    };
    let seat_record = |seat: Seat, stdout: &str| -> SeatRecord {
        let m = envelope_meta(stdout);
        SeatRecord {
            role_id: seat.role_id().to_string(),
            model: m.model,
            total_tokens: m.total_tokens,
        }
    };

    // 1. Prosecution.
    let p_out = dispatch(Seat::Prosecutor, &prosecutor_prompt(case))
        .with_context(|| format!("dispatching {} for case {}", Seat::Prosecutor.label(), case.id))?;
    env.prosecutor = seat_record(Seat::Prosecutor, &p_out);
    let mut sheet = parse_charges(&extract_reply_text(&p_out));
    if !sheet.parsed {
        // Empty reply — the #1113 degenerate class; never a silent pass.
        env.prosecutor_closing = Some(Closing::Missing);
        return Ok((
            Review {
                verdict: String::new(),
                findings: Vec::new(),
                parsed: false,
            },
            env,
        ));
    }
    env.prosecutor_closing = Some(sheet.closing);
    env.struck_charges = validate_charge_anchors(&mut sheet.charges, &case.diff);
    let active = sheet.charges.iter().filter(|c| !c.struck).count();
    if active == 0 {
        // A rested no-case (or every charge struck) is a real, scoreable pass.
        env.charges = sheet.charges;
        return Ok((
            Review {
                verdict: "pass".to_string(),
                findings: Vec::new(),
                parsed: true,
            },
            env,
        ));
    }

    // 2. Defense.
    let d_out = dispatch(Seat::Defender, &defender_prompt(case, &sheet.charges))
        .with_context(|| format!("dispatching {} for case {}", Seat::Defender.label(), case.id))?;
    env.defender = Some(seat_record(Seat::Defender, &d_out));
    let mut answers = parse_rebuttals(&extract_reply_text(&d_out));
    // A defense that produced nothing usable doesn't abort the case: the
    // judge is told every charge stands unanswered and weighs them alone.
    let defense_degenerate = !answers.parsed || answers.rebuttals.is_empty();
    env.defense_degenerate = defense_degenerate;
    env.defense_rests = answers.rests;
    env.voided_rebuttals =
        validate_rebuttal_quotes(&mut answers.rebuttals, &sheet.charges, &case.diff, workdir);

    // 3. Judgment.
    let j_out = dispatch(
        Seat::Judge,
        &judge_prompt(case, &sheet.charges, &answers, defense_degenerate),
    )
    .with_context(|| format!("dispatching {} for case {}", Seat::Judge.label(), case.id))?;
    env.judge = Some(seat_record(Seat::Judge, &j_out));
    let rulings = parse_rulings(&extract_reply_text(&j_out));

    env.rebuttals = answers.rebuttals;
    let review = match rulings {
        Some(r) => {
            let ruled: Vec<u32> = r.verdicts.iter().map(|v| v.charge).collect();
            env.unruled_charges = sheet
                .charges
                .iter()
                .filter(|c| !c.struck && !ruled.contains(&c.number))
                .map(|c| c.number)
                .collect();
            env.judge_summary = r.summary.clone();
            env.dropped_verdicts = r.dropped;
            env.sustained = r
                .verdicts
                .iter()
                .filter(|v| v.ruling == Ruling::Sustained)
                .count();
            let review = synthesize_review(&sheet.charges, &r);
            env.verdicts = r.verdicts;
            review
        }
        None => {
            // The judge broke the contract — degenerate, loud, rerun; the
            // envelope keeps the full record for diagnosis.
            Review {
                verdict: String::new(),
                findings: Vec::new(),
                parsed: false,
            }
        }
    };
    env.charges = sheet.charges;
    Ok((review, env))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lab::review_bench::Label;

    fn tiny_case(diff: &str) -> Case {
        Case {
            id: "t-1".to_string(),
            label: Label {
                kind: "bug".to_string(),
                intent_title: "Add billing window".to_string(),
                intent_body: "Computes the period end.".to_string(),
                expect_verdict: String::new(),
                bug_class: None,
                anchor_contains: None,
                expected: Vec::new(),
                notes: None,
            },
            diff: diff.to_string(),
        }
    }

    const DIFF: &str = "--- a/billing.ts\n+++ b/billing.ts\n@@ -1,3 +1,4 @@\n context line\n+const end = start.plus(30)\n+const total = base * rate\n more context\n";

    // ── charge parsing ────────────────────────────────────────────────

    #[test]
    fn charges_parse_anchored_and_general() {
        let text = "The change is weakest at the boundary.\n\n\
                    CHARGE 1 [billing.ts] `const end = start.plus(30)`\n\
                    Off-by-one: the window includes the boundary day twice.\n\n\
                    CHARGE 2 [billing.ts]\n\
                    The reporting path still applies rounding this path dropped.\n\n\
                    CASE: rested\n";
        let sheet = parse_charges(text);
        assert!(sheet.parsed);
        assert_eq!(sheet.closing, Closing::Rested);
        assert_eq!(sheet.charges.len(), 2);
        assert_eq!(sheet.charges[0].number, 1);
        assert_eq!(
            sheet.charges[0].anchor.as_deref(),
            Some("const end = start.plus(30)")
        );
        assert!(sheet.charges[0].body.contains("boundary day twice"));
        assert_eq!(sheet.charges[1].anchor, None);
        assert!(sheet.charges[1].body.contains("rounding"));
    }

    #[test]
    fn charge_body_line_starting_with_charge_word_is_not_a_marker() {
        // "Charge the user twice" lacks the digits+[path] shape — folds into
        // the body instead of spawning a phantom charge (the P0 QA finding
        // explicit numbering exists to prevent).
        let text = "CHARGE 1 [billing.ts] `const end = start.plus(30)`\n\
                    Charge the user twice and the ledger drifts.\n\
                    CASE: rested\n";
        let sheet = parse_charges(text);
        assert_eq!(sheet.charges.len(), 1);
        assert!(sheet.charges[0].body.contains("Charge the user twice"));
    }

    #[test]
    fn charges_renumber_canonically_when_model_numbers_are_broken() {
        let text = "CHARGE 1 [a.ts] `x`\nfirst.\n\
                    CHARGE 1 [b.ts] `y`\nduplicate model number.\n\
                    CASE: rested\n";
        let sheet = parse_charges(text);
        assert_eq!(sheet.charges.len(), 2);
        assert_eq!(sheet.charges[0].number, 1);
        assert_eq!(sheet.charges[1].number, 2);
        assert_eq!(sheet.charges[1].model_number, 1);
    }

    #[test]
    fn charges_no_case_and_empty_are_distinct() {
        let none = parse_charges("Investigated thoroughly; nothing chargeable.\nCASE: no-charges\n");
        assert!(none.parsed);
        assert_eq!(none.closing, Closing::NoCharges);
        assert!(none.charges.is_empty());
        let empty = parse_charges("   \n");
        assert!(!empty.parsed);
        assert_eq!(empty.closing, Closing::Missing);
    }

    // ── rebuttal parsing ──────────────────────────────────────────────

    #[test]
    fn rebuttals_parse_all_stances_and_rest() {
        let text = "The prosecution over-reaches on two of three counts.\n\n\
                    REBUTTAL 1: refute\n\
                    The caller clamps first — [intake.ts] `const days = Math.min(raw, 30)`.\n\n\
                    REBUTTAL 2: concede\nThe charge holds.\n\n\
                    REBUTTAL 3: mitigate — only leap years trigger it.\n\n\
                    DEFENSE: rests\n";
        let a = parse_rebuttals(text);
        assert!(a.parsed);
        assert!(a.rests);
        assert_eq!(a.rebuttals.len(), 3);
        assert_eq!(a.rebuttals[0].stance, Stance::Refute);
        assert_eq!(a.rebuttals[1].stance, Stance::Concede);
        assert_eq!(a.rebuttals[2].stance, Stance::Mitigate);
        // Inline body after the stance word folds in.
        assert!(a.rebuttals[2].body.contains("leap years"));
    }

    #[test]
    fn rebuttal_off_contract_stance_is_kept_as_other() {
        let a = parse_rebuttals("REBUTTAL 1: reject\nNot how the code works.\nDEFENSE: rests\n");
        assert_eq!(a.rebuttals.len(), 1);
        assert_eq!(a.rebuttals[0].stance, Stance::Other);
        assert_eq!(a.rebuttals[0].raw_stance, "reject");
    }

    // ── judge parsing ─────────────────────────────────────────────────

    #[test]
    fn rulings_take_last_json_fence_not_reasoning_fences() {
        let text = "Weighing charge 1: the defense quotes\n```\nconst days = Math.min(raw, 30)\n```\nwhich is decisive.\n\n```json\n{\"summary\":\"one sustained\",\"verdicts\":[{\"charge\":1,\"ruling\":\"dismissed\",\"severity\":null,\"decisive_evidence\":\"the clamp\",\"reasoning\":\"input never reaches the line\"},{\"charge\":2,\"ruling\":\"sustained\",\"severity\":\"high\",\"decisive_evidence\":\"conceded\",\"reasoning\":\"defense concedes\"}]}\n```\n";
        let r = parse_rulings(text).expect("parses");
        assert_eq!(r.summary, "one sustained");
        assert_eq!(r.verdicts.len(), 2);
        assert_eq!(r.verdicts[0].ruling, Ruling::Dismissed);
        assert_eq!(r.verdicts[1].ruling, Ruling::Sustained);
        assert_eq!(r.verdicts[1].severity.as_deref(), Some("high"));
    }

    #[test]
    fn rulings_off_contract_ruling_is_dropped_and_counted() {
        let text = "```json\n{\"verdicts\":[{\"charge\":1,\"ruling\":\"partially sustained\"},{\"charge\":2,\"ruling\":\"SUSTAINED\",\"severity\":\"HIGH\"}]}\n```";
        let r = parse_rulings(text).expect("parses");
        assert_eq!(r.dropped, 1);
        assert_eq!(r.verdicts.len(), 1);
        assert_eq!(r.verdicts[0].charge, 2);
        // case-insensitive ruling + severity normalization
        assert_eq!(r.verdicts[0].severity.as_deref(), Some("high"));
    }

    #[test]
    fn rulings_unfenced_json_still_parses() {
        let r = parse_rulings("{\"verdicts\":[{\"charge\":1,\"ruling\":\"dismissed\"}]}")
            .expect("parses");
        assert_eq!(r.verdicts.len(), 1);
        assert!(parse_rulings("no json here at all").is_none());
    }

    // ── citation validation ───────────────────────────────────────────

    #[test]
    fn fabricated_anchor_is_struck_real_anchor_survives() {
        let mut charges = vec![
            Charge {
                number: 1,
                model_number: 1,
                path: "billing.ts".into(),
                anchor: Some("const end = start.plus(30)".into()),
                body: "real".into(),
                struck: false,
            },
            Charge {
                number: 2,
                model_number: 2,
                path: "billing.ts".into(),
                anchor: Some("const end = start.minus(30)".into()),
                body: "fabricated".into(),
                struck: false,
            },
            Charge {
                number: 3,
                model_number: 3,
                path: "billing.ts".into(),
                anchor: None,
                body: "general — nothing to verify".into(),
                struck: false,
            },
        ];
        let struck = validate_charge_anchors(&mut charges, DIFF);
        assert_eq!(struck, 1);
        assert!(!charges[0].struck);
        assert!(charges[1].struck);
        assert!(!charges[2].struck);
    }

    /// Frontier QA MUST-FIX on the P1 PR: a model quoting the diff line
    /// VERBATIM WITH its leading `+`/`-` marker (the most common LLM
    /// diff-quoting slip) must not be false-struck — a fully struck bug case
    /// would silently score as a clean pass. The same normalization must
    /// keep the excerpt window locatable.
    #[test]
    fn anchor_keeping_diff_marker_is_not_struck_and_still_excerpts() {
        let mut charges = vec![Charge {
            number: 1,
            model_number: 1,
            path: "billing.ts".into(),
            anchor: Some("+const end = start.plus(30)".into()),
            body: "kept the + marker".into(),
            struck: false,
        }];
        assert_eq!(validate_charge_anchors(&mut charges, DIFF), 0);
        assert!(!charges[0].struck);
        let mut long = String::from("--- a/f.ts\n+++ b/f.ts\n");
        for i in 0..120 {
            long.push_str(&format!(" context {i}\n"));
        }
        long.push_str("+const end = start.plus(30)\n");
        for i in 0..120 {
            long.push_str(&format!(" tail {i}\n"));
        }
        let ex = charged_excerpts(&long, &charges);
        assert!(ex.contains("diff excerpt (charge 1)"));
        assert!(ex.contains("const end = start.plus(30)"));
    }

    #[test]
    fn rebuttal_quote_verified_against_workdir_file_or_voided() {
        let root = std::env::temp_dir().join(format!("dmx-dialectic-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/intake.ts"),
            "export const days = Math.min(raw, 30)\n",
        )
        .unwrap();
        let charges = vec![Charge {
            number: 1,
            model_number: 1,
            path: "billing.ts".into(),
            anchor: None,
            body: "b".into(),
            struck: false,
        }];
        let mut rebuttals = vec![
            Rebuttal {
                charge: 1,
                stance: Stance::Refute,
                raw_stance: "refute".into(),
                body: "The clamp in [src/intake.ts] `const days = Math.min(raw, 30)` stops it."
                    .into(),
                voided: false,
            },
            Rebuttal {
                charge: 1,
                stance: Stance::Refute,
                raw_stance: "refute".into(),
                body: "Guarded by [src/intake.ts] `if (never_written_anywhere_xyz)` upstream."
                    .into(),
                voided: false,
            },
        ];
        let voided = validate_rebuttal_quotes(&mut rebuttals, &charges, DIFF, Some(&root));
        assert_eq!(voided, 1);
        assert!(!rebuttals[0].voided);
        assert!(rebuttals[1].voided);
        // No workdir → insufficient evidence to strike anything.
        let mut again = rebuttals.clone();
        again.iter_mut().for_each(|r| r.voided = false);
        assert_eq!(validate_rebuttal_quotes(&mut again, &charges, DIFF, None), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    // ── excerpts ──────────────────────────────────────────────────────

    #[test]
    fn excerpts_window_around_anchor_and_fall_back_to_full_diff() {
        // A long diff so the excerpt is genuinely smaller than the whole.
        let mut long = String::from("--- a/f.ts\n+++ b/f.ts\n");
        for i in 0..200 {
            long.push_str(&format!(" context {i}\n"));
        }
        long.push_str("+const end = start.plus(30)\n");
        for i in 0..200 {
            long.push_str(&format!(" tail {i}\n"));
        }
        let charges = vec![Charge {
            number: 1,
            model_number: 1,
            path: "f.ts".into(),
            anchor: Some("const end = start.plus(30)".into()),
            body: "b".into(),
            struck: false,
        }];
        let ex = charged_excerpts(&long, &charges);
        assert!(ex.contains("diff excerpt (charge 1)"));
        assert!(ex.contains("const end = start.plus(30)"));
        assert!(ex.len() < long.len());
        // No anchored charge → the (small) full diff, verbatim.
        let general = vec![Charge {
            number: 1,
            model_number: 1,
            path: "f.ts".into(),
            anchor: None,
            body: "b".into(),
            struck: false,
        }];
        assert_eq!(charged_excerpts(DIFF, &general), DIFF);
    }

    /// Shakedown-1 regression (#1222): the prosecutor quoted the LOGICAL line
    /// `const gapStart = lastRecord ? lastRecord.billingEndAt.plus({` — a
    /// direct hit on the case's labeled bug — but the diff wraps that
    /// expression across two physical lines, so per-line matching struck the
    /// charge and the bug case scored as a clean pass. Wrapped verbatim
    /// quotes must survive validation AND still locate an excerpt window.
    #[test]
    fn wrapped_logical_line_anchor_survives_and_excerpts() {
        let mut wrapped = String::from("--- a/charges.ts\n+++ b/charges.ts\n");
        for i in 0..80 {
            wrapped.push_str(&format!(" ctx {i}\n"));
        }
        wrapped.push_str("+      const gapStart = lastRecord\n");
        wrapped.push_str("+        ? lastRecord.billingEndAt.plus({\n");
        wrapped.push_str("+            days: 1,\n");
        for i in 0..80 {
            wrapped.push_str(&format!(" tail {i}\n"));
        }
        let mut charges = vec![Charge {
            number: 1,
            model_number: 1,
            path: "charges.ts".into(),
            anchor: Some(
                "const gapStart = lastRecord ? lastRecord.billingEndAt.plus({".into(),
            ),
            body: "boundary double-count".into(),
            struck: false,
        }];
        assert_eq!(
            validate_charge_anchors(&mut charges, &wrapped),
            0,
            "a verbatim quote of a wrapped logical line must not be struck"
        );
        assert!(!charges[0].struck);
        let ex = charged_excerpts(&wrapped, &charges);
        assert!(ex.contains("diff excerpt (charge 1)"));
        assert!(ex.contains("const gapStart = lastRecord"));
        // A genuinely fabricated wrapped-style quote still strikes.
        let mut fake = vec![Charge {
            number: 1,
            model_number: 1,
            path: "charges.ts".into(),
            anchor: Some("const gapStart = firstRecord ? something.else.entirely({".into()),
            body: "fabricated".into(),
            struck: false,
        }];
        assert_eq!(validate_charge_anchors(&mut fake, &wrapped), 1);
    }

    /// Shakedown-1 regression (#1222): rebuttal bodies carry ```-fenced code
    /// blocks (folded inline by the marker parser); the naive backtick
    /// scanner turned fence backticks into span delimiters, producing
    /// garbage spans that voided 8 of 9 honest rebuttals. Fenced content
    /// must be ignored; real inline citations still validate; fabricated
    /// inline citations still void.
    #[test]
    fn fenced_code_block_does_not_void_honest_rebuttal() {
        let charges = vec![Charge {
            number: 1,
            model_number: 1,
            path: "billing.ts".into(),
            anchor: None,
            body: "b".into(),
            struck: false,
        }];
        let root = std::env::temp_dir().join(format!("dmx-fence-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Honest: one real inline span (verbatim from the diff) + a folded
        // fenced block whose content is NOT a verbatim line anywhere.
        let mut rebuttals = vec![
            Rebuttal {
                charge: 1,
                stance: Stance::Concede,
                raw_stance: "concede".into(),
                body: "The charge holds — `const total = base * rate` mixes units. \
                       ```typescript const paraphrased = whatIRemember(of, it) ``` \
                       shows the shape."
                    .into(),
                voided: false,
            },
            Rebuttal {
                charge: 1,
                stance: Stance::Refute,
                raw_stance: "refute".into(),
                body: "Guarded by `a_line_that_exists_nowhere_at_all()` upstream.".into(),
                voided: false,
            },
        ];
        let voided = validate_rebuttal_quotes(&mut rebuttals, &charges, DIFF, Some(&root));
        assert_eq!(voided, 1, "only the fabricated inline citation voids");
        assert!(
            !rebuttals[0].voided,
            "fence content must not manufacture unverifiable spans"
        );
        assert!(rebuttals[1].voided);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Shakedown-1+3 regression (#1222): defenders write the file reference
    /// with the brackets INSIDE the backticks — `` `[app/x.ts]` `` — and
    /// that span (a WHERE, not a WHAT) failed evidence lookup in 9 of 9
    /// voided rebuttals, discrediting every honest defense. Path-shaped
    /// spans are file references, never validated as quotes.
    #[test]
    fn backticked_path_reference_is_not_an_evidence_span() {
        let charges = vec![Charge {
            number: 1,
            model_number: 1,
            path: "billing.ts".into(),
            anchor: None,
            body: "b".into(),
            struck: false,
        }];
        let root = std::env::temp_dir().join(format!("dmx-path-span-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut rebuttals = vec![Rebuttal {
            charge: 1,
            stance: Stance::Refute,
            raw_stance: "refute".into(),
            body: "The wrapper in `[inertia/pages/admin/Show.tsx]` uses \
                   `const total = base * rate` which the diff shows."
                .into(),
            voided: false,
        }];
        let voided = validate_rebuttal_quotes(&mut rebuttals, &charges, DIFF, Some(&root));
        assert_eq!(
            voided, 0,
            "a backticked [path] reference must not void a rebuttal whose \
             real citation validates"
        );
        assert!(!rebuttals[0].voided);
        std::fs::remove_dir_all(&root).ok();
    }

    // ── synthesis + the full chain ────────────────────────────────────

    #[test]
    fn synthesize_sustained_flags_all_dismissed_passes() {
        let charges = vec![Charge {
            number: 1,
            model_number: 1,
            path: "billing.ts".into(),
            anchor: Some("const end = start.plus(30)".into()),
            body: "off-by-one at the boundary".into(),
            struck: false,
        }];
        let sustained = Rulings {
            summary: String::new(),
            verdicts: vec![Verdict {
                charge: 1,
                ruling: Ruling::Sustained,
                severity: Some("high".into()),
                decisive_evidence: "unrebutted".into(),
                reasoning: String::new(),
            }],
            dropped: 0,
        };
        let r = synthesize_review(&charges, &sustained);
        assert_eq!(r.verdict, "flag");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, "high");
        assert_eq!(r.findings[0].anchor, "const end = start.plus(30)");
        let dismissed = Rulings {
            summary: String::new(),
            verdicts: vec![Verdict {
                charge: 1,
                ruling: Ruling::Dismissed,
                severity: None,
                decisive_evidence: "clamped".into(),
                reasoning: String::new(),
            }],
            dropped: 0,
        };
        let r = synthesize_review(&charges, &dismissed);
        assert_eq!(r.verdict, "pass");
        assert!(r.findings.is_empty());
        assert!(r.parsed);
    }

    /// Fake dispatch envelopes: the closure returns `--json` envelope stdout
    /// whose last line is the compact envelope (mirroring the real shape).
    fn fake_envelope(reply: &str) -> String {
        serde_json::json!({
            "result": "stop",
            "final_assistant": reply,
            "metrics": { "model": "test-35b", "total_tokens": 42 }
        })
        .to_string()
    }

    #[test]
    fn run_debate_happy_path_end_to_end() {
        let case = tiny_case(DIFF);
        let (review, env) = run_debate(&case, None, |seat, prompt| {
            Ok(match seat {
                Seat::Prosecutor => {
                    assert!(prompt.contains("File your charges"));
                    fake_envelope(
                        "CHARGE 1 [billing.ts] `const end = start.plus(30)`\n\
                         Boundary day counted twice.\n\
                         CHARGE 2 [billing.ts] `const total = base * rate`\n\
                         Rate is per-day but base is per-month.\nCASE: rested\n",
                    )
                }
                Seat::Defender => {
                    assert!(prompt.contains("Charge 1 [billing.ts]"));
                    fake_envelope(
                        "REBUTTAL 1: refute\nThe window is half-open downstream.\n\
                         REBUTTAL 2: concede\nThe units disagree.\nDEFENSE: rests\n",
                    )
                }
                Seat::Judge => {
                    assert!(prompt.contains("## Defense"));
                    assert!(prompt.contains("concede"));
                    fake_envelope(
                        "Charge 1 falls, charge 2 stands.\n```json\n{\"summary\":\"units bug\",\"verdicts\":[{\"charge\":1,\"ruling\":\"dismissed\",\"severity\":null,\"decisive_evidence\":\"half-open interval\",\"reasoning\":\"no double count\"},{\"charge\":2,\"ruling\":\"sustained\",\"severity\":\"high\",\"decisive_evidence\":\"conceded\",\"reasoning\":\"defense concedes\"}]}\n```",
                    )
                }
            })
        })
        .expect("debate runs");
        assert!(review.parsed);
        assert_eq!(review.verdict, "flag");
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.findings[0].anchor, "const total = base * rate");
        assert_eq!(env.sustained, 1);
        assert_eq!(env.charges.len(), 2);
        assert_eq!(env.rebuttals.len(), 2);
        assert_eq!(env.verdicts.len(), 2);
        assert!(env.defense_rests);
        assert_eq!(env.prosecutor.model.as_deref(), Some("test-35b"));
        assert_eq!(env.prosecutor_closing, Some(Closing::Rested));
        assert!(env.unruled_charges.is_empty());
    }

    #[test]
    fn run_debate_no_charges_short_circuits_to_pass() {
        let case = tiny_case(DIFF);
        let mut dispatched: Vec<&'static str> = Vec::new();
        let (review, env) = run_debate(&case, None, |seat, _| {
            dispatched.push(seat.role_id());
            Ok(fake_envelope("Nothing chargeable.\nCASE: no-charges\n"))
        })
        .expect("debate runs");
        assert!(review.parsed);
        assert_eq!(review.verdict, "pass");
        assert!(review.findings.is_empty());
        assert_eq!(dispatched, vec!["dialectic-prosecutor"]);
        assert_eq!(env.prosecutor_closing, Some(Closing::NoCharges));
        assert!(env.defender.is_none());
        assert!(env.judge.is_none());
    }

    #[test]
    fn run_debate_all_struck_short_circuits_and_counts() {
        let case = tiny_case(DIFF);
        let (review, env) = run_debate(&case, None, |seat, _| {
            assert_eq!(seat, Seat::Prosecutor, "struck case must not reach the defense");
            Ok(fake_envelope(
                "CHARGE 1 [billing.ts] `line that is not in the diff`\nfabricated.\nCASE: rested\n",
            ))
        })
        .expect("debate runs");
        assert_eq!(review.verdict, "pass");
        assert_eq!(env.struck_charges, 1);
        assert!(env.charges[0].struck);
    }

    #[test]
    fn run_debate_degenerate_prosecutor_and_broken_judge_are_never_silent_passes() {
        let case = tiny_case(DIFF);
        // Empty prosecutor reply → degenerate.
        let (review, _) = run_debate(&case, None, |_, _| Ok(fake_envelope(""))).expect("runs");
        assert!(!review.parsed);
        // Healthy advocates, judge emits no JSON → degenerate, envelope keeps
        // the record.
        let (review, env) = run_debate(&case, None, |seat, _| {
            Ok(match seat {
                Seat::Prosecutor => fake_envelope(
                    "CHARGE 1 [billing.ts] `const end = start.plus(30)`\nbad.\nCASE: rested\n",
                ),
                Seat::Defender => fake_envelope("REBUTTAL 1: concede\nyes.\nDEFENSE: rests\n"),
                Seat::Judge => fake_envelope("I find this all very interesting."),
            })
        })
        .expect("runs");
        assert!(!review.parsed);
        assert_eq!(env.charges.len(), 1);
        assert_eq!(env.rebuttals.len(), 1);
    }

    #[test]
    fn run_debate_missing_defense_is_noted_not_fatal() {
        let case = tiny_case(DIFF);
        let (review, env) = run_debate(&case, None, |seat, prompt| {
            Ok(match seat {
                Seat::Prosecutor => fake_envelope(
                    "CHARGE 1 [billing.ts] `const end = start.plus(30)`\nbad.\nCASE: rested\n",
                ),
                Seat::Defender => fake_envelope(""),
                Seat::Judge => {
                    assert!(prompt.contains("did not submit an answer sheet"));
                    fake_envelope(
                        "```json\n{\"verdicts\":[{\"charge\":1,\"ruling\":\"sustained\",\"severity\":\"medium\",\"decisive_evidence\":\"unanswered, concrete\",\"reasoning\":\"stands\"}]}\n```",
                    )
                }
            })
        })
        .expect("runs");
        assert!(env.defense_degenerate);
        assert_eq!(review.verdict, "flag");
        assert_eq!(review.findings.len(), 1);
    }
}

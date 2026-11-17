//! `darkmux finding` (#2265) — read the finding store, and replay the flow
//! stream into it.
//!
//! A finding is what was OBSERVED: an event from a role execution, keyed
//! `<execution>/<seq>`, written once and never rewritten. The flow stream stays
//! the audit trail; this directory is the queryable copy, so JSON on disk is
//! the truth the same way it is for roles.
//!
//! **darkmux never interprets the emission.** `list` shows a truncated, compact
//! preview of the raw JSON the model produced and nothing else — no parsed
//! fields, no severity, no verdict. A finding's location is domain-specific (a
//! line for text, a page for a PDF, a rect for an image), so there is nothing
//! for darkmux to read inside it. The filters below key on the record's own
//! `context` — provenance the LAUNCHER supplied — never on the emission.

use anyhow::Result;
use darkmux_crew::findings::{self, FindingRecord};
use darkmux_types::config_access;
use darkmux_types::execution_id::ExecutionId;
use darkmux_types::user_files::{escape_text as shown, escape_text_keeping_lines};
use serde::Serialize;

use crate::cli_json;

/// `finding list --json`: the records that matched, ts-ascending.
#[derive(Serialize, schemars::JsonSchema)]
pub struct FindingList<'a> {
    pub findings: Vec<&'a FindingRecord>,
}

/// How many characters of the raw emission `list` previews. Enough to
/// recognize a finding, short enough to keep one finding on one line.
const PREVIEW_CHARS: usize = 100;

/// `finding list` — every record in the store, ts-ascending.
pub fn list(
    mission: Option<&str>,
    execution: Option<&ExecutionId>,
    rule: Option<&str>,
    json: bool,
) -> Result<i32> {
    let root = config_access::findings_dir();
    let all = findings::load_all_at(&root)?;
    let rows: Vec<&FindingRecord> = all
        .iter()
        .filter(|r| execution.is_none_or(|e| r.execution == e.as_str()))
        // The mission is the RECORD's own field, not something inside the
        // launcher's `context` blob (which carries workspace / source / sha /
        // rule / unit and no mission at all). Reading it from `context` was
        // #2288's live-proof gap: every filter matched except this one.
        .filter(|r| mission.is_none_or(|m| r.mission_id.as_deref() == Some(m)))
        .filter(|r| rule.is_none_or(|x| context_rule(r).as_deref() == Some(x)))
        .collect();

    if json {
        cli_json::emit(&FindingList { findings: rows })?;
        return Ok(0);
    }

    if rows.is_empty() {
        // An empty RESULT and an empty STORE need different remedies, so they
        // must not print the same line: one means "widen your filter", the
        // other means "nothing has been recorded here yet".
        let filtered = mission.is_some() || execution.is_some() || rule.is_some();
        if filtered && !all.is_empty() {
            println!("(no findings match — {} in the store)", all.len());
            return Ok(0);
        }
        println!(
            "(no findings){}",
            if root.exists() {
                String::new()
            } else {
                format!(" — {} does not exist yet", root.display())
            }
        );
        println!("  `darkmux finding sync` replays the flow stream into the store.");
        return Ok(0);
    }

    for r in &rows {
        println!("{}", list_row(r));
    }
    println!("\n{} finding(s) in {}", rows.len(), root.display());
    Ok(0)
}

/// `finding show <execution>/<seq>` — one record, whole.
pub fn show(key: &str, json: bool) -> Result<i32> {
    let root = config_access::findings_dir();
    let Some((execution, seq)) = findings::parse_key(key) else {
        eprintln!("not a finding key: {key} (expected <execution>/<seq>, e.g. exec-18f3a2c-1b2-0/1)");
        return Ok(1);
    };
    let Some(rec) = findings::load_at(&root, &execution, seq)? else {
        eprintln!(
            "no finding {key} under {}\n  `darkmux finding sync` replays the flow stream into the store.",
            root.display()
        );
        return Ok(1);
    };

    if json {
        cli_json::emit(&rec)?;
        return Ok(0);
    }

    print!("{}", show_text(&rec)?);
    // (#2265) The mods that name this finding — DERIVED by scanning the mod
    // store, never stored on the finding. A finding is an event and is never
    // rewritten, so the only stored link lives on the thing created later;
    // this read turns it around. It prints LAST, so everything above stays
    // byte-identical to what `finding show` printed before mods existed.
    let mod_root = config_access::mods_dir();
    let all_mods = darkmux_crew::mods::load_all_at(&mod_root)?;
    let naming = darkmux_crew::mods::mods_for(&all_mods, &rec.key);
    if !naming.is_empty() {
        println!("\nmods");
        for m in naming {
            println!("  {}  {}  {}", shown(&m.key), shown(&m.ts), shown(&m.by));
        }
    }
    Ok(0)
}

/// One `finding list` row: key, time, proposer, the launcher's context, and
/// a preview of the emission. Every field a model or a peer can have written
/// goes through `escape_text`, so an escape sequence is shown, not acted on.
fn list_row(r: &FindingRecord) -> String {
    let mut context_bits: Vec<String> = Vec::new();
    if let Some(m) = r.mission_id.as_deref() {
        context_bits.push(format!("mission={}", shown(m)));
    }
    if let Some(v) = r.context.as_ref().and_then(|c| c.unit.as_deref()) {
        context_bits.push(format!("unit={}", shown(v)));
    }
    if let Some(v) = context_rule(r) {
        context_bits.push(format!("rule={}", shown(&v)));
    }
    let context = if context_bits.is_empty() { String::new() } else { format!("  [{}]", context_bits.join(" ")) };
    format!(
        "{}  {}  {} ({}){}\n    {}",
        shown(&r.key),
        shown(&r.ts),
        shown(&r.proposer.handle),
        shown(&r.proposer.model),
        context,
        preview(&r.emitted),
    )
}

/// `finding show`'s text for `rec`, up to its mods. The emission is the
/// model's own argument object: pretty-printed when it is JSON so it is
/// readable, raw otherwise, never interpreted. JSON escapes the C0 controls
/// but not C1 or the bidi controls, so the printed form goes through
/// `escape_text_keeping_lines` like every other field here.
fn show_text(rec: &FindingRecord) -> Result<String> {
    let mut lines = vec![
        format!("finding   {}", shown(&rec.key)),
        format!("execution {}", shown(&rec.execution)),
        format!("seq       {}", rec.seq),
        format!("recorded  {}", shown(&rec.ts)),
        format!("tool      {}", shown(&rec.tool_name)),
    ];
    // The dispatch's scope, printed only when it HAD one: a plain `darkmux
    // dispatch` belongs to no mission, and a row of "(none)" would be noise.
    for (label, value) in [("mission", &rec.mission_id), ("phase", &rec.phase_id), ("step", &rec.step_id)] {
        lines.extend(value.as_deref().map(|v| format!("{label:<10}{}", shown(v))));
    }
    let machine = rec.proposer.machine_id.as_deref().map(|m| format!(" on {}", shown(m))).unwrap_or_default();
    lines.push(format!("proposer  {} ({}){machine}", shown(&rec.proposer.handle), shown(&rec.proposer.model)));
    lines.push(match &rec.context {
        None => "context   (none)".to_string(),
        Some(context) => format!("context   {}", shown(&serde_json::to_string(context)?)),
    });
    lines.push("\nemitted".to_string());
    let emitted = if rec.emitted.is_object() || rec.emitted.is_array() {
        serde_json::to_string_pretty(&rec.emitted)?
    } else {
        rec.emitted.to_string()
    };
    lines.extend(escape_text_keeping_lines(&emitted).lines().map(|line| format!("  {line}")));
    Ok(lines.join("\n") + "\n")
}

/// `finding sync` — the second producer. Replays the flow stream into the
/// store for anything the live tailer missed (an older binary, a killed
/// process). Idempotent: the materializer is write-once, so a second pass
/// reports every record as already present.
pub fn sync(since: Option<&str>, json: bool) -> Result<i32> {
    let flows = config_access::flows_dir();
    let root = config_access::findings_dir();
    let report = findings::sync_at(&flows, &root, since)?;

    if json {
        cli_json::emit(&report)?;
        return Ok(0);
    }

    println!("scanned {} accepted finding call(s) in {}", report.scanned, flows.display());
    println!("  created  {}", report.created);
    println!("  present  {}", report.present);
    if report.skipped_no_emission > 0 {
        // Named, never dropped silently: these calls exist in the stream and
        // CANNOT become records — the emission was not carried before FLOW
        // 1.33.0, so there is nothing to store.
        println!(
            "  skipped  {} (no emission — recorded before FLOW 1.33.0 carried one; \
they exist in the stream but cannot become records)",
            report.skipped_no_emission
        );
    }
    println!("\nstore: {}", root.display());
    Ok(0)
}

/// A record's `context.rule`, when the launcher named exactly one. Provenance
/// only — the `emitted` blob is never read, and the mission scope is NOT here
/// (it is the record's own field; see the `--mission` filter).
fn context_rule(rec: &FindingRecord) -> Option<String> {
    match rec.context.as_ref()?.rule.as_ref()? {
        darkmux_flow::payload::RuleRef::One(id) => Some(id.clone()),
        darkmux_flow::payload::RuleRef::Many(_) => None,
    }
}

/// A one-line, TRUNCATED preview of the raw emission as compact JSON. Not an
/// interpretation: whatever the model produced, clipped to fit a line.
fn preview(emitted: &serde_json::Value) -> String {
    let compact = shown(&emitted.to_string());
    if compact.chars().count() <= PREVIEW_CHARS {
        return compact;
    }
    format!("{}…", compact.chars().take(PREVIEW_CHARS).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An OSC 0 title, a screen clear and an OSC 52 clipboard write, as a
    /// model can put them in a `create_finding` call, plus a C1 CSI and a
    /// bidi override, which JSON does not escape.
    const HOSTILE: &str = "\x1b]0;owned\x07\x1b[2J\x1b]52;c;aGk=\x07\u{9b}2J\u{202e}";

    fn record(text: &str) -> FindingRecord {
        serde_json::from_value(serde_json::json!({
            "schema_version": "1", "key": "exec-1/1", "execution": "exec-1", "seq": 1, "ts": "t", "tool_name": "create_finding",
            "proposer": {"handle": "reviewer", "model": text, "machine_id": text},
            "mission_id": text,
            "emitted": {"note": text, "body": "line one\n\tline two"},
        }))
        .unwrap()
    }

    fn inert(text: &str) -> bool {
        !text.chars().any(|c| (c.is_control() && c != '\n' && c != '\t') || c == '\u{202e}')
    }

    #[test]
    fn finding_show_and_list_print_a_hostile_emission_inert() {
        let rec = record(HOSTILE);
        let shown_text = show_text(&rec).unwrap();
        assert!(inert(&shown_text), "{shown_text:?}");
        assert!(inert(&list_row(&rec)), "{:?}", list_row(&rec));
    }

    #[test]
    fn finding_show_leaves_plain_text_and_unicode_unchanged() {
        let text = show_text(&record("café 日本語 🇺🇸")).unwrap();
        assert!(text.contains("proposer  reviewer (café 日本語 🇺🇸) on café 日本語 🇺🇸\n"), "{text}");
        assert!(text.contains("\n    \"note\": \"café 日本語 🇺🇸\""), "{text}");
        assert!(text.contains("\"body\": \"line one\\n\\tline two\""), "{text}");
    }
}

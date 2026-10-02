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
        let mut context_bits: Vec<String> = Vec::new();
        if let Some(m) = r.mission_id.as_deref() {
            context_bits.push(format!("mission={m}"));
        }
        if let Some(v) = r.context.as_ref().and_then(|c| c.unit.as_deref()) {
            context_bits.push(format!("unit={v}"));
        }
        if let Some(v) = context_rule(r) {
            context_bits.push(format!("rule={v}"));
        }
        let context = if context_bits.is_empty() {
            String::new()
        } else {
            format!("  [{}]", context_bits.join(" "))
        };
        println!(
            "{}  {}  {} ({}){}\n    {}",
            r.key,
            r.ts,
            r.proposer.handle,
            r.proposer.model,
            context,
            preview(&r.emitted),
        );
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

    println!("finding   {}", rec.key);
    println!("execution {}", rec.execution);
    println!("seq       {}", rec.seq);
    println!("recorded  {}", rec.ts);
    println!("tool      {}", rec.tool_name);
    // The dispatch's scope, printed only when it HAD one — a plain `darkmux
    // dispatch` belongs to no mission, and a row of "(none)" would be noise.
    for (label, value) in [
        ("mission", rec.mission_id.as_deref()),
        ("phase", rec.phase_id.as_deref()),
        ("step", rec.step_id.as_deref()),
    ] {
        if let Some(v) = value {
            println!("{label:<10}{v}");
        }
    }
    println!(
        "proposer  {} ({}){}",
        rec.proposer.handle,
        rec.proposer.model,
        rec.proposer
            .machine_id
            .as_deref()
            .map(|m| format!(" on {m}"))
            .unwrap_or_default()
    );
    match &rec.context {
        None => println!("context   (none)"),
        Some(context) => println!("context   {}", serde_json::to_string(context)?),
    }
    // The emission is the model's own argument object. Pretty-printed when it
    // is JSON so it is readable; raw otherwise. Never interpreted.
    println!("\nemitted");
    if rec.emitted.is_object() || rec.emitted.is_array() {
        for line in serde_json::to_string_pretty(&rec.emitted)?.lines() {
            println!("  {line}");
        }
    } else {
        println!("  {}", rec.emitted);
    }

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
            println!("  {}  {}  {}", m.key, m.ts, m.by);
        }
    }
    Ok(0)
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
    let compact = emitted.to_string();
    if compact.chars().count() <= PREVIEW_CHARS {
        return compact;
    }
    format!("{}…", compact.chars().take(PREVIEW_CHARS).collect::<String>())
}

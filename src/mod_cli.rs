//! `darkmux mod` (#2265) — write and read the mod store.
//!
//! A mod is how something COULD change: a kit of instructions plus data, in
//! whatever form the proposer chose. **darkmux never types a kit and never
//! opens it.** `list` previews the raw kit truncated and nothing else — no
//! parsed fields, no diff rendering, no verdict — the same discipline
//! `finding list` applies to an emission.
//!
//! This module is the CLI producer: a change made OUTSIDE darkmux, recorded by
//! whoever made it. The runtime `create_mod` tool is the second producer, for
//! a change made inside a dispatch; both write the same record through
//! `darkmux_crew::mods`.

use anyhow::Result;
use darkmux_crew::mods::{self, ModRecord};
use darkmux_types::config_access;
use std::path::PathBuf;

/// How many characters of the raw kit `list` previews. Enough to recognize a
/// mod, short enough to keep one mod on one line.
const PREVIEW_CHARS: usize = 100;

/// `mod create` — mint one mod and write it.
///
/// Every call mints a NEW key. Idempotence is deliberately not a goal: two
/// agents proposing for one finding at different times are two mods, and a
/// finding-derived key would have made the second overwrite the first.
pub fn create(
    by: &str,
    for_keys: &[String],
    kit_source: Option<&str>,
    attachments: &[PathBuf],
    json: bool,
) -> Result<i32> {
    let root = config_access::mods_dir();
    // `-` is stdin, so a kit can be piped from whatever composed it without a
    // temp file. Anything else is a path.
    let kit = match kit_source {
        None => None,
        Some("-") => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Some(buf)
        }
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading kit from {path}: {e}"))?,
        ),
    };

    let rec = mods::create(
        &root,
        &config_access::findings_dir(),
        by,
        for_keys,
        kit.as_deref(),
        attachments,
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rec)?);
        return Ok(0);
    }

    println!("{}", rec.key);
    // A `for` key with no stored finding is allowed — the change was still
    // proposed — but it is named, because the usual cause is a typo and a
    // silent acceptance would look identical to a copied context.
    let missing: Vec<&str> =
        rec.context.findings.iter().filter(|f| f.missing).map(|f| f.key.as_str()).collect();
    if !missing.is_empty() {
        println!(
            "  note: no stored finding for {} — recorded as missing",
            missing.join(", ")
        );
    }
    println!("  {}", mods::record_path_at(&root, &rec.key).display());
    Ok(0)
}

/// `mod list` — every mod in the store, ts-ascending.
pub fn list(for_key: Option<&str>, mission: Option<&str>, json: bool) -> Result<i32> {
    let root = config_access::mods_dir();
    let all = mods::load_all_at(&root)?;
    let rows: Vec<&ModRecord> = all
        .iter()
        .filter(|m| for_key.is_none_or(|k| m.r#for.iter().any(|f| f == k)))
        // The mission is matched through the `for` finding's OWN mission,
        // copied onto the mod at create time — so the filter answers from the
        // mod alone, and a mod naming no finding belongs to no mission.
        .filter(|m| mission.is_none_or(|x| mods::names_mission(m, x)))
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "mods": rows }))?);
        return Ok(0);
    }

    if rows.is_empty() {
        // An empty RESULT and an empty STORE need different remedies, so they
        // must not print the same line.
        let filtered = for_key.is_some() || mission.is_some();
        if filtered && !all.is_empty() {
            println!("(no mods match — {} in the store)", all.len());
            return Ok(0);
        }
        println!(
            "(no mods){}",
            if root.exists() {
                String::new()
            } else {
                format!(" — {} does not exist yet", root.display())
            }
        );
        println!("  `darkmux mod create --by <actor> --kit <file>` records one.");
        return Ok(0);
    }

    for m in &rows {
        let for_bit = if m.r#for.is_empty() {
            "for (none)".to_string()
        } else {
            format!("for {}", m.r#for.join(", "))
        };
        let attach = match m.attachments.len() {
            0 => String::new(),
            n => format!("  {n} attachment(s)"),
        };
        println!("{}  {}  {}  [{for_bit}]{attach}\n    {}", m.key, m.ts, m.by, preview(&m.kit));
    }
    println!("\n{} mod(s) in {}", rows.len(), root.display());
    Ok(0)
}

/// `mod show <key>` — one record, whole, with the kit printed RAW.
pub fn show(key: &str, json: bool) -> Result<i32> {
    let root = config_access::mods_dir();
    let Some(rec) = mods::load_at(&root, key)? else {
        eprintln!(
            "no mod {key} under {}\n  `darkmux mod list` shows what is recorded.",
            root.display()
        );
        return Ok(1);
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&rec)?);
        return Ok(0);
    }

    println!("mod       {}", rec.key);
    println!("recorded  {}", rec.ts);
    println!("by        {}", rec.by);
    if rec.r#for.is_empty() {
        println!("for       (none)");
    } else {
        for f in &rec.context.findings {
            let detail = if f.missing {
                " (no stored finding)".to_string()
            } else {
                f.mission_id.as_deref().map(|m| format!(" [mission={m}]")).unwrap_or_default()
            };
            println!("for       {}{detail}", f.key);
        }
    }
    if !rec.attachments.is_empty() {
        println!("\nattachments");
        for name in &rec.attachments {
            let path = mods::attachments_dir_at(&root, &rec.key).join(name);
            let size = std::fs::metadata(&path).map(|m| m.len());
            match size {
                Ok(n) => println!("  {name}  ({n} bytes)"),
                // A recorded attachment whose file is gone is a fact worth
                // showing, not a reason to fail the read.
                Err(_) => println!("  {name}  (missing from {})", path.display()),
            }
        }
    }
    // The kit is the proposer's own. Pretty-printed when it is JSON so it is
    // readable; raw otherwise. Never interpreted.
    println!("\nkit");
    if rec.kit.is_object() || rec.kit.is_array() {
        for line in serde_json::to_string_pretty(&rec.kit)?.lines() {
            println!("  {line}");
        }
    } else if let Some(s) = rec.kit.as_str() {
        for line in s.lines() {
            println!("  {line}");
        }
    } else {
        println!("  {}", rec.kit);
    }
    Ok(0)
}

/// A one-line, TRUNCATED preview of the raw kit as compact JSON. Not an
/// interpretation: whatever the proposer wrote, clipped to fit a line.
fn preview(kit: &serde_json::Value) -> String {
    let compact = kit.to_string();
    if compact.chars().count() <= PREVIEW_CHARS {
        return compact;
    }
    format!("{}…", compact.chars().take(PREVIEW_CHARS).collect::<String>())
}

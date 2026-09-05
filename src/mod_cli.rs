//! `darkmux mod` (#2265) — write and read the mod store.
//!
//! A mod is how something COULD change: a kit of instructions plus data, in
//! whatever form the proposer chose. **darkmux never types a kit and never
//! opens it.** `list` previews the raw kit truncated and nothing else — no
//! parsed fields, no diff rendering, no verdict — the same discipline
//! `finding list` applies to an emission.
//!
//! `--mission` answers from the mod's own create-time SNAPSHOT of its
//! findings, not from the finding store as it stands now: a mod created
//! before its finding was synced carries no mission and will not match. The
//! snapshot is what makes a mod self-describing, and this is its limit.
//!
//! This module is the CLI producer: a change made OUTSIDE darkmux, recorded by
//! whoever made it. The runtime `create_mod` tool is the second producer, for
//! a change made inside a dispatch; both write the same record through
//! `darkmux_crew::mods`.

use anyhow::{Context, Result};
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
#[allow(clippy::too_many_arguments)]
pub fn create(
    by: &str,
    for_keys: &[String],
    kit_source: Option<&str>,
    attachments: &[PathBuf],
    // (#2310 P4b review, M-B) `--kit-kind` — an optional, proposer-declared
    // hint at the kit's shape (`"unified-diff"` is the one a consumer
    // recognizes today). Threaded straight through to `ModRecord::
    // kit_kind`, unvalidated — see that field's own doc.
    kit_kind: Option<&str>,
    // (#2386) `--allow-missing-finding` — record a `for` key whose finding
    // is not in the store. Off by default: the usual cause of such a key is
    // a typo or a copied example, and a link nothing can follow is what the
    // store exists to prevent. On, for the deliberate case (a finding not
    // synced yet, or one recorded on another machine).
    allow_missing_finding: bool,
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
        kit_kind,
        allow_missing_finding,
    )?;

    let path = mods::record_path_at(&root, &rec.key);

    if json {
        // `path` sits BESIDE the record, never inside it: `record` has to stay
        // byte-equal to what is on disk, or a consumer that diffs the two sees
        // a field darkmux invented.
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "record": &rec,
                "path": path.to_string_lossy(),
            }))?
        );
        return Ok(0);
    }

    // **stdout is the KEY, alone.** The caller of this verb is usually an
    // orchestrator that pipes the output straight into `mod show <key>`, a
    // `--for`, or a tracker — so a second stdout line would be captured by
    // `$(darkmux mod create …)` and turn a key into a key-plus-a-path.
    // Everything else this command has to say goes to stderr, where it is
    // still visible to a person and invisible to a substitution.
    println!("{}", rec.key);
    // (#2386) Only reachable under `--allow-missing-finding` now — without
    // it, a `for` key with no stored finding is refused before anything is
    // written. Still named here, because the operator who asked for the
    // link anyway should see which one is dangling.
    let missing: Vec<&str> =
        rec.context.findings.iter().filter(|f| f.missing).map(|f| f.key.as_str()).collect();
    if !missing.is_empty() {
        eprintln!("note: no stored finding for {} — recorded as missing", missing.join(", "));
    }
    eprintln!("recorded {}", path.display());
    Ok(0)
}

/// `mod list` — every mod in the store, ts-ascending.
pub fn list(for_key: Option<&str>, mission: Option<&str>, json: bool) -> Result<i32> {
    let root = config_access::mods_dir();
    // The query is canonicalized to the one form every stored key is in, and
    // a key that can address no finding is refused rather than returning an
    // empty list — "no mods for that finding" and "that names no finding" are
    // different answers.
    let for_key = for_key
        .map(|k| {
            mods::canonical_finding_key(k).with_context(|| {
                format!("not a finding key: {k:?} (expected <dispatch>/<seq>, e.g. sess-abc/1)")
            })
        })
        .transpose()?;
    let all = mods::load_all_at(&root)?;
    let rows: Vec<&ModRecord> = all
        .iter()
        .filter(|m| for_key.as_deref().is_none_or(|k| m.r#for.iter().any(|f| f == k)))
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
        // (#2310 P4c-2b neighbor check) A compact gate indicator, same
        // discipline `mod show`'s own `gate` line follows.
        let gate = match (&m.gate, &m.gate_skipped_reason) {
            (Some(g), _) if g.passed => "  [gate: pass]".to_string(),
            (Some(_), _) => "  [gate: fail]".to_string(),
            (None, Some(_)) => "  [gate: skipped]".to_string(),
            (None, None) => String::new(),
        };
        println!("{}  {}  {}  [{for_bit}]{attach}{gate}\n    {}", m.key, m.ts, m.by, preview(m.kit.as_deref()));
    }
    println!("\n{} mod(s) in {}", rows.len(), root.display());
    Ok(0)
}

/// `mod show <key>` — one record, whole, with the kit printed RAW.
pub fn show(key: &str, json: bool) -> Result<i32> {
    let root = config_access::mods_dir();
    // An invalid key and a missing one need different remedies, so they must
    // not print the same line — the same split `finding show` makes.
    if !mods::is_safe_key(key) {
        eprintln!("not a mod key: {key} (expected a minted key, e.g. mod-1788423454-3b4ef1)");
        return Ok(1);
    }
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
    println!("kind      {}", rec.kit_kind.as_deref().unwrap_or("(untyped)"));
    // (#2310 P4c-2b neighbor check) `mods.gate` is the one write path
    // besides `mod create`/the runtime `create_mod` tool that mutates a
    // stored mod — `mod show --json` already surfaces it for free (the
    // whole record serializes), but the plain-text rendering did not name
    // it at all, which would have made a gated mod look identical to a
    // never-gated one here.
    println!(
        "gate      {}",
        match (&rec.gate, &rec.gate_skipped_reason) {
            (Some(g), _) if g.passed => format!("passed ({})", g.command),
            (Some(g), _) => format!("failed ({})", g.command),
            (None, Some(reason)) => format!("skipped — {reason}"),
            (None, None) => "(not yet gated)".to_string(),
        }
    );
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
            // Checked BEFORE the path is joined or stat'd: a recorded name is
            // data off disk, and only a plain basename addresses a file
            // inside this mod's own directory.
            if !mods::is_safe_basename(name) {
                println!("  {name}  (not a usable file name — skipped)");
                continue;
            }
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
    // A mod the host wrote PARTIALLY says so here, above the kit: the
    // `warnings` field exists so the record is honest about what was dropped,
    // and a rendering that hid it would make the record look whole again.
    if !rec.warnings.is_empty() {
        println!("\nwarnings");
        for w in &rec.warnings {
            println!("  {w}");
        }
    }
    // The kit is the proposer's own bytes, printed unindented, unparsed, and
    // with NOTHING appended — a trailing newline this did not receive is a
    // byte it must not add. This rendering is for reading; the byte-exact
    // channel a script should use is `mod show --json | jq -j .kit`, named in
    // the verb's own `--help` rather than only in a comment here.
    println!("\nkit");
    if let Some(kit) = rec.kit.as_deref() {
        print!("{kit}");
    }
    Ok(0)
}

/// A one-line, TRUNCATED clip of the kit's own text — whitespace runs
/// collapsed so it fits a line, then cut. Plain text, not JSON: the kit is
/// never parsed, and this is a glance at it, not an interpretation of it.
fn preview(kit: Option<&str>) -> String {
    let compact: String = kit.unwrap_or("(attachments only)").split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= PREVIEW_CHARS {
        return compact;
    }
    format!("{}…", compact.chars().take(PREVIEW_CHARS).collect::<String>())
}

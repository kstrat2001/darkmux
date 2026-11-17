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
use darkmux_types::user_files::{escape_text as shown, escape_text_keeping_lines};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::cli_json;

/// `mod create --json`. `path` sits BESIDE the record, never inside it:
/// `record` has to stay byte-equal to what is on disk, or a consumer that
/// diffs the two sees a field darkmux invented.
#[derive(Serialize, schemars::JsonSchema)]
pub struct ModCreated<'a> {
    pub record: &'a ModRecord,
    pub path: String,
}

/// `mod list --json`: the mods that matched, ts-ascending.
#[derive(Serialize, schemars::JsonSchema)]
pub struct ModList<'a> {
    pub mods: Vec<&'a ModRecord>,
}

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
        cli_json::emit(&ModCreated { record: &rec, path: path.to_string_lossy().into_owned() })?;
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

/// One `mod list` row: key, time, author, the findings it answers, and a kit preview.
fn list_row(m: &ModRecord) -> String {
    let for_bit = if m.r#for.is_empty() {
        "for (none)".to_string()
    } else {
        format!("for {}", shown(&m.r#for.join(", ")))
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
    format!("{}  {}  {}  [{for_bit}]{attach}{gate}\n    {}", shown(&m.key), shown(&m.ts), shown(&m.by), preview(m.kit.as_deref()))
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
                format!("not a finding key: {k:?} (expected <execution>/<seq>, e.g. exec-18f3a2c-1b2-0/1)")
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
        cli_json::emit(&ModList { mods: rows })?;
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
        println!("{}", list_row(m));
    }
    println!("\n{} mod(s) in {}", rows.len(), root.display());
    Ok(0)
}

/// The `gate` line of `mod show`: passed, failed, skipped (with why), or not yet gated.
fn gate_summary(rec: &ModRecord) -> String {
    match (&rec.gate, &rec.gate_skipped_reason) {
        (Some(g), _) if g.passed => format!("passed ({})", shown(&g.command)),
        (Some(g), _) => format!("failed ({})", shown(&g.command)),
        (None, Some(reason)) => format!("skipped: {}", shown(reason)),
        (None, None) => "(not yet gated)".to_string(),
    }
}

/// (#2265, 5.0) The `mod show` lines for the fields a darkmux-written mod
/// adds: the convergence handle, the proposing role, the planned site. Each
/// line appears only when its field is present, so a schema-2 record or a
/// `mod create` mod renders as before.
fn provenance_lines(rec: &ModRecord) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(k) = &rec.change_key {
        lines.push(format!("change    {}", shown(k)));
    }
    if let Some(p) = &rec.proposer {
        let profile = p.profile.as_deref().map(|x| format!(", profile {}", shown(x))).unwrap_or_default();
        lines.push(format!("proposer  {} ({}{profile})", shown(&p.role), shown(&p.model)));
    }
    if let Some(s) = &rec.site {
        let (source, sha, file) = (shown(&s.source), shown(&s.sha), shown(&s.file));
        lines.push(format!("site      {source}@{sha} {file}:{}-{}", s.start_line, s.end_line));
    }
    lines
}

/// The `for` lines of `mod show`: each finding the mod answers, with its
/// mission, or `(none)`.
fn for_lines(rec: &ModRecord) -> Vec<String> {
    if rec.r#for.is_empty() {
        return vec!["for       (none)".to_string()];
    }
    rec.context
        .findings
        .iter()
        .map(|f| {
            let detail = match (f.missing, f.mission_id.as_deref()) {
                (true, _) => " (no stored finding)".to_string(),
                (false, Some(m)) => format!(" [mission={}]", shown(m)),
                (false, None) => String::new(),
            };
            format!("for       {}{detail}", shown(&f.key))
        })
        .collect()
}

/// The `attachments` section of `mod show`, empty when the mod has none.
fn attachment_lines(rec: &ModRecord, root: &Path) -> Vec<String> {
    if rec.attachments.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["\nattachments".to_string()];
    lines.extend(rec.attachments.iter().map(|name| attachment_line(rec, root, name)));
    lines
}

fn attachment_line(rec: &ModRecord, root: &Path, name: &str) -> String {
    // Checked BEFORE the path is joined or stat'd: a recorded name is data
    // off disk, and only a plain basename addresses a file inside this mod's
    // own directory.
    if !mods::is_safe_basename(name) {
        return format!("  {}  (not a usable file name — skipped)", shown(name));
    }
    let path = mods::attachments_dir_at(root, &rec.key).join(name);
    match std::fs::metadata(&path).map(|m| m.len()) {
        Ok(n) => format!("  {}  ({n} bytes)", shown(name)),
        // A recorded attachment whose file is gone is a fact worth showing,
        // not a reason to fail the read.
        Err(_) => format!("  {}  (missing from {})", shown(name), shown(&path.display().to_string())),
    }
}

/// `mod show`'s whole text for `rec`. Every field a model or a peer can
/// have written goes through ONE escape step (`escape_text`, one line per
/// field; the kit through `escape_text_keeping_lines`), so an escape
/// sequence in it is shown as text and never acted on by the terminal.
///
/// A mod the host wrote PARTIALLY says so above the kit: the `warnings`
/// field exists so the record is honest about what was dropped, and a
/// rendering that hid it would make the record look whole again.
///
/// The kit is the proposer's own text, printed unindented, unparsed, and
/// with NOTHING appended: a trailing newline this did not receive is one it
/// must not add. This rendering is for reading; the byte-exact channel a
/// script should use is `mod show --json | jq -j .kit`, named in the verb's
/// own `--help`.
fn show_text(rec: &ModRecord, root: &Path) -> String {
    let mut lines = vec![
        format!("mod       {}", shown(&rec.key)),
        format!("recorded  {}", shown(&rec.ts)),
        format!("by        {}", shown(&rec.by)),
        format!("kind      {}", shown(rec.kit_kind.as_deref().unwrap_or("(untyped)"))),
    ];
    lines.extend(provenance_lines(rec));
    // (#2310 P4c-2b neighbor check) `mods.gate` is the one write path
    // besides `mod create`/the runtime `create_mod` tool that mutates a
    // stored mod, so a gated mod must not look identical to a never-gated one.
    lines.push(format!("gate      {}", gate_summary(rec)));
    lines.extend(for_lines(rec));
    lines.extend(attachment_lines(rec, root));
    if !rec.warnings.is_empty() {
        lines.push("\nwarnings".to_string());
        lines.extend(rec.warnings.iter().map(|w| format!("  {}", shown(w))));
    }
    lines.push("\nkit".to_string());
    let mut text = lines.join("\n");
    text.push('\n');
    text.push_str(&escape_text_keeping_lines(rec.kit.as_deref().unwrap_or("")));
    text
}

/// `mod show <key>` — one record, whole, the kit as its proposer wrote it
/// with every escape sequence shown as text ([`show_text`]).
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
        cli_json::emit(&rec)?;
        return Ok(0);
    }

    print!("{}", show_text(&rec, &root));
    Ok(0)
}

/// A one-line, TRUNCATED clip of the kit's own text — whitespace runs
/// collapsed so it fits a line, then cut. Plain text, not JSON: the kit is
/// never parsed, and this is a glance at it, not an interpretation of it.
fn preview(kit: Option<&str>) -> String {
    let compact = shown(&kit.unwrap_or("(attachments only)").split_whitespace().collect::<Vec<_>>().join(" "));
    if compact.chars().count() <= PREVIEW_CHARS {
        return compact;
    }
    format!("{}…", compact.chars().take(PREVIEW_CHARS).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::mods::{ModProposer, ModSite};

    fn bare() -> ModRecord {
        serde_json::from_str(
            r#"{"key":"mod-1","ts":"t","by":"kain","for":[],"kit":"k","attachments":[],"schema_version":"2"}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_schema_2_record_renders_no_provenance_lines() {
        assert!(provenance_lines(&bare()).is_empty());
    }

    #[test]
    fn show_names_the_change_key_proposer_and_site_when_present() {
        let mut r = bare();
        r.change_key = Some("chg-abc".into());
        r.proposer = Some(ModProposer { role: "coder".into(), profile: Some("p1".into()), model: "m1".into() });
        r.site = Some(ModSite { source: "app".into(), sha: "abc".into(), file: "src/a.ts".into(), start_line: 3, end_line: 9 });
        assert_eq!(
            provenance_lines(&r),
            vec![
                "change    chg-abc".to_string(),
                "proposer  coder (m1, profile p1)".to_string(),
                "site      app@abc src/a.ts:3-9".to_string(),
            ]
        );
    }

    /// An OSC 0 title, a screen clear and an OSC 52 clipboard write, the
    /// kind of bytes a local seat can put in a kit through `create_mod`.
    const HOSTILE: &str = "\x1b]0;owned\x07\x1b[2J\x1b]52;c;aGk=\x07\u{9b}2J\u{202e}";

    fn hostile() -> ModRecord {
        let mut r = bare();
        r.kit = Some(format!("--- a/x\n+{HOSTILE}\n\tline\n"));
        r.by = format!("coder ({HOSTILE})");
        r.attachments = vec![format!("a{HOSTILE}")];
        r.warnings = vec![format!("dropped {HOSTILE}")];
        r.proposer = Some(ModProposer { role: "coder".into(), profile: None, model: HOSTILE.into() });
        r
    }

    fn inert(text: &str) -> bool {
        !text.chars().any(|c| (c.is_control() && c != '\n' && c != '\t') || c == '\u{202e}')
    }

    #[test]
    fn mod_show_prints_a_hostile_kit_and_fields_inert_keeping_its_lines() {
        let root = tempfile::tempdir().unwrap();
        let text = show_text(&hostile(), root.path());
        assert!(inert(&text), "{text:?}");
        assert!(text.ends_with("\nkit\n--- a/x\n+\\u{1b}]0;owned\\u{7}\\u{1b}[2J\\u{1b}]52;c;aGk=\\u{7}\\u{9b}2J\\u{202e}\n\tline\n"), "{text:?}");
    }

    #[test]
    fn mod_list_prints_a_hostile_kit_preview_inert() {
        let row = list_row(&hostile());
        assert!(inert(&row), "{row:?}");
    }

    #[test]
    fn mod_show_leaves_plain_text_newlines_and_unicode_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let mut r = bare();
        r.kit = Some("café 日本語 🇺🇸\n\tindented\n".into());
        let text = show_text(&r, root.path());
        assert_eq!(
            text,
            "mod       mod-1\nrecorded  t\nby        kain\nkind      (untyped)\ngate      (not yet gated)\nfor       (none)\n\nkit\ncafé 日本語 🇺🇸\n\tindented\n"
        );
        assert_eq!(list_row(&r), "mod-1  t  kain  [for (none)]\n    café 日本語 🇺🇸 indented");
    }
}

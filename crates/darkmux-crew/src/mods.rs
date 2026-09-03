//! (#2265) The MOD record: how something could change.
//!
//! A mod is a **kit**: instructions plus data, in whatever form the proposer
//! chose — a diff, a sentence, pixel data, a config value — enough for an AI
//! to make the change correctly later, given the mod's own context. **darkmux
//! never types a kit and never opens it.** The kit is stored verbatim: JSON if
//! the input parsed as JSON, otherwise the text as a string. Nothing reads
//! inside it.
//!
//! **The key is MINTED per mod, never derived from a finding.** Two agents
//! review the same finding at different times; one proposes the code change,
//! the other recommends a comment. Both are valid, and they may overlap,
//! conflict or compose. A finding-derived key would have made the second
//! overwrite the first, so the record keeps both and judges neither.
//!
//! `for` is the only stored link between the two records: zero or more
//! finding keys, living on the thing created later. The view from a finding to
//! its mods is DERIVED by scanning mods — nothing is written back onto the
//! finding, which is an event and is never rewritten.
//!
//! For each `for` key that exists in the finding store, the mod copies that
//! finding's `mission_id`, `context` and `emitted` into its own `context`, so
//! a reader of the mod never has to go find the finding. A `for` key with no
//! stored finding is allowed and recorded as `{key, missing: true}` — the mod
//! is still the change someone proposed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The record's own schema version. Bumped only when the shape below changes;
/// readers stay lenient (unknown fields ride in `extras`).
pub const MOD_SCHEMA_VERSION: &str = "1";

/// Whether a mod key is safe to use as a PATH SEGMENT under the store.
///
/// Minted keys always are, but a key also arrives from the operator (`mod
/// show <key>`), so it is untrusted input that ends up joined onto a
/// filesystem path. Anything that could resolve outside the store is refused
/// rather than sanitized — the same rule, for the same reason, as a finding's
/// dispatch segment.
pub fn is_safe_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('.')
        && !key.contains('/')
        && !key.contains('\\')
        && !key.contains('\0')
}

/// Mint a key for one mod: `mod-<unix-secs>-<6 hex>`.
///
/// Same shape and same token scheme as `mission_launch::mint_run_id` — a
/// blake3 digest over (nanosecond time, pid, an in-process atomic counter), so
/// two mods minted within the same wall-clock second are still distinct. The
/// seconds prefix keeps the store browsable in rough chronological order;
/// ordering itself comes off the record's `ts`, never the key.
pub fn mint_key() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let digest = blake3::hash(format!("{nanos}-{pid}-{n}").as_bytes());
    let hex = digest.to_hex();
    format!("mod-{}-{}", nanos / 1_000_000_000, &hex.as_str()[..6])
}

/// One `for` finding, as copied onto the mod at create time. Either the
/// finding's own provenance (it was in the store) or a `missing` marker (it
/// was not) — never nothing, so a reader can always tell the two apart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForFinding {
    /// The finding key exactly as the proposer named it.
    pub key: String,
    /// The finding's own `mission_id`, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// The finding's `context` verbatim (the launcher's blob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    /// The finding's `emitted` verbatim (the model's own arguments). Copied,
    /// never read: darkmux does not interpret an emission here either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitted: Option<serde_json::Value>,
    /// `true` when no finding with this key was in the store at create time.
    /// A mod for an unstored finding is allowed — the change was still
    /// proposed — so this is a recorded fact, not a refusal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

/// What the mod carries about the findings it names, so a reader of the mod
/// never has to go find them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModContext {
    #[serde(default)]
    pub findings: Vec<ForFinding>,
}

/// One mod, as stored at `<mods dir>/<key>/mod.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModRecord {
    /// The minted key — the address every other surface uses.
    pub key: String,
    /// When the RECORD was written.
    pub ts: String,
    /// Who proposed it. A free actor string: a role handle plus model for a
    /// darkmux seat, or a plain name (`sonnet`, `kain`) for an external actor.
    /// Deliberately not a typed enum — the proposer may be anything, and a
    /// closed set would refuse the next one.
    pub by: String,
    /// The findings this mod addresses — zero or more keys. The only stored
    /// link between the two records, and a LIST because one change can
    /// address three observations.
    #[serde(rename = "for", default)]
    pub r#for: Vec<String>,
    /// The kit, verbatim: JSON when the input parsed as JSON, else the text as
    /// a string. **Never interpreted.**
    pub kit: serde_json::Value,
    /// Basenames of the files under this mod's `attachments/`.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Each `for` finding's own provenance, copied at create time.
    #[serde(default)]
    pub context: ModContext,
    pub schema_version: String,
    /// Lenient-on-read overflow, so a newer writer's fields survive a round
    /// trip through an older reader.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// What [`materialize`] did. Write-once, like a finding: a mod records a
/// moment someone proposed a change, and that moment is not edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Materialized {
    Created,
    AlreadyPresent,
}

/// The store root: `env(DARKMUX_MODS_DIR) > config.dirs.mods > <darkmux
/// root>/mods`.
pub fn mods_dir() -> PathBuf {
    darkmux_types::config_access::mods_dir()
}

/// The directory one mod lives in.
pub fn record_dir_at(root: &Path, key: &str) -> PathBuf {
    root.join(key)
}

/// The file one mod lives at.
pub fn record_path_at(root: &Path, key: &str) -> PathBuf {
    record_dir_at(root, key).join("mod.json")
}

/// Where one mod's attachments are copied.
pub fn attachments_dir_at(root: &Path, key: &str) -> PathBuf {
    record_dir_at(root, key).join("attachments")
}

/// The kit, verbatim. JSON if the whole input parses as JSON; otherwise the
/// text as a string. **This is the only decision darkmux makes about a kit,
/// and it is a storage decision, not an interpretation** — a JSON kit stays
/// queryable, a prose kit stays exactly the prose that was written.
pub fn parse_kit(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))
}

/// Copy each named finding's provenance onto the mod. A key with no stored
/// finding is recorded as missing rather than refused.
pub fn finding_context(findings_root: &Path, for_keys: &[String]) -> Result<ModContext> {
    let mut findings = Vec::new();
    for key in for_keys {
        let stored = match crate::findings::parse_key(key) {
            // An unparseable or unsafe key can address nothing in the store,
            // so it is missing for the same reason an absent one is.
            None => None,
            Some((dispatch, seq)) => crate::findings::load_at(findings_root, &dispatch, seq)?,
        };
        findings.push(match stored {
            Some(f) => ForFinding {
                key: key.clone(),
                mission_id: f.mission_id.clone(),
                context: Some(f.context.clone()),
                emitted: Some(f.emitted.clone()),
                missing: false,
            },
            None => ForFinding {
                key: key.clone(),
                mission_id: None,
                context: None,
                emitted: None,
                missing: true,
            },
        });
    }
    Ok(ModContext { findings })
}

/// Write a mod **once**. A minted key never collides, so an existing file
/// means something else is already at that address — it is left exactly as it
/// is, the same contract a finding gets.
pub fn materialize(root: &Path, record: &ModRecord) -> Result<Materialized> {
    anyhow::ensure!(
        is_safe_key(&record.key),
        "refusing to write a mod under an unsafe key {:?}",
        record.key
    );
    let path = record_path_at(root, &record.key);
    if path.exists() {
        return Ok(Materialized::AlreadyPresent);
    }
    let dir = path.parent().expect("record path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating mod dir {}", dir.display()))?;
    let body = serde_json::to_string_pretty(record)? + "\n";
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes())
                .with_context(|| format!("writing mod {}", path.display()))?;
            Ok(Materialized::Created)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Materialized::AlreadyPresent),
        Err(e) => Err(e).with_context(|| format!("creating mod {}", path.display())),
    }
}

/// Mint a key, copy the attachments, and write one mod.
///
/// **Idempotence is not a goal.** Every call mints a new key: two agents
/// proposing for one finding at different times are two mods, and that is the
/// point.
pub fn create(
    root: &Path,
    findings_root: &Path,
    by: &str,
    for_keys: &[String],
    kit: Option<&str>,
    attachments: &[PathBuf],
) -> Result<ModRecord> {
    anyhow::ensure!(!by.trim().is_empty(), "a mod needs a proposer: pass --by <actor>");
    // A mod with neither instructions nor data is not a kit. Refused here
    // rather than in the CLI so both producers get the same floor.
    anyhow::ensure!(
        kit.is_some() || !attachments.is_empty(),
        "a mod needs a kit: pass --kit <file>|- and/or --attach <path>"
    );

    // Everything that can fail is checked BEFORE the record dir exists, so a
    // refusal leaves no half-written mod behind.
    let mut names: Vec<String> = Vec::new();
    for path in attachments {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| is_safe_key(n))
            .with_context(|| format!("attachment has no usable file name: {}", path.display()))?
            .to_string();
        anyhow::ensure!(
            !names.contains(&name),
            "two attachments share the basename {name:?} — one would overwrite the other"
        );
        anyhow::ensure!(path.is_file(), "attachment is not a readable file: {}", path.display());
        names.push(name);
    }

    let key = mint_key();
    let record = ModRecord {
        key: key.clone(),
        ts: darkmux_flow::ts_utc_now(),
        by: by.to_string(),
        r#for: for_keys.to_vec(),
        kit: kit.map(parse_kit).unwrap_or(serde_json::Value::Null),
        attachments: names.clone(),
        context: finding_context(findings_root, for_keys)?,
        schema_version: MOD_SCHEMA_VERSION.to_string(),
        extras: serde_json::Map::new(),
    };

    materialize(root, &record)?;
    if !attachments.is_empty() {
        let dest = attachments_dir_at(root, &key);
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("creating attachments dir {}", dest.display()))?;
        for (path, name) in attachments.iter().zip(&names) {
            std::fs::copy(path, dest.join(name))
                .with_context(|| format!("copying attachment {}", path.display()))?;
        }
    }
    Ok(record)
}

/// Read one mod by its key.
pub fn load_at(root: &Path, key: &str) -> Result<Option<ModRecord>> {
    // Validated HERE rather than at the join: `mod show '../x'` must not read
    // outside the store.
    if !is_safe_key(key) {
        return Ok(None);
    }
    let path = record_path_at(root, key);
    if !path.exists() {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading mod {}", path.display()))?;
    let rec =
        serde_json::from_str(&body).with_context(|| format!("parsing mod {}", path.display()))?;
    Ok(Some(rec))
}

/// Every mod in the store, ts-ascending. Unreadable or unparseable files are
/// skipped rather than failing the read — the same casual-reader contract the
/// finding store gets.
pub fn load_all_at(root: &Path) -> Result<Vec<ModRecord>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let Ok(body) = std::fs::read_to_string(entry.path().join("mod.json")) else {
            continue;
        };
        if let Ok(rec) = serde_json::from_str::<ModRecord>(&body) {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.key.cmp(&b.key)));
    Ok(out)
}

/// The mods that name one finding — DERIVED by scanning mods, because nothing
/// about a mod is ever written back onto the finding it addresses.
pub fn mods_for<'a>(all: &'a [ModRecord], finding_key: &str) -> Vec<&'a ModRecord> {
    all.iter().filter(|m| m.r#for.iter().any(|k| k == finding_key)).collect()
}

/// Whether a mod names any finding recorded under this mission. The mission
/// comes off the mod's OWN copied context, so the filter answers from the mod
/// alone — the finding store need not still hold the finding.
pub fn names_mission(record: &ModRecord, mission: &str) -> bool {
    record.context.findings.iter().any(|f| f.mission_id.as_deref() == Some(mission))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings;
    use tempfile::TempDir;

    /// A finding in the store, so a mod's `for` has something real to copy.
    fn store_finding(root: &Path, dispatch: &str, seq: u64, mission: Option<&str>) {
        let rec = findings::build_record(
            dispatch,
            seq,
            "2026-09-03T01:00:00Z".to_string(),
            "create_finding",
            findings::Proposer {
                handle: "crawler".into(),
                model: "darkmux:qwen3.6".into(),
                machine_id: Some("studio".into()),
            },
            findings::Scope {
                mission_id: mission.map(String::from),
                phase_id: None,
                step_id: None,
            },
            Some(serde_json::json!({"rule": "unnamed-predicate", "unit": "u1"})),
            serde_json::json!({"file": "a.ts", "line": 4}),
        );
        findings::materialize(root, &rec).unwrap();
    }

    #[test]
    fn minted_keys_are_unique_and_path_safe() {
        let a = mint_key();
        let b = mint_key();
        assert_ne!(a, b, "a key is minted per mod, never derived — two calls, two keys");
        for k in [&a, &b] {
            assert!(k.starts_with("mod-"), "got {k}");
            assert!(is_safe_key(k), "a minted key must be a safe path segment: {k}");
        }
    }

    #[test]
    fn is_safe_key_refuses_anything_that_could_escape_the_store() {
        assert!(is_safe_key("mod-1-abcdef"));
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b", ".hidden"] {
            assert!(!is_safe_key(bad), "must be refused: {bad:?}");
        }
    }

    #[test]
    fn a_kit_is_kept_verbatim_as_json_when_it_is_json_and_as_a_string_otherwise() {
        // JSON in, JSON out — queryable, not reshaped.
        assert_eq!(parse_kit(r#"{"diff": "x"}"#), serde_json::json!({"diff": "x"}));
        assert_eq!(parse_kit("[1,2]"), serde_json::json!([1, 2]));
        // Prose in, exactly that prose out. Never parsed into fields, never
        // trimmed: darkmux does not open a kit.
        let prose = "rename the predicate, then add a test\n";
        assert_eq!(parse_kit(prose), serde_json::Value::String(prose.to_string()));
        assert_eq!(parse_kit("{not json"), serde_json::Value::String("{not json".into()));
    }

    #[test]
    fn create_mints_a_new_key_every_call_so_two_mods_for_one_finding_both_survive() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));
        let for_keys = vec!["sess-a/1".to_string()];

        let one = create(&mods, &finds, "sonnet", &for_keys, Some("change the code"), &[]).unwrap();
        let two = create(&mods, &finds, "kain", &for_keys, Some("add a comment"), &[]).unwrap();

        assert_ne!(one.key, two.key, "the second must NOT overwrite the first");
        assert!(record_path_at(&mods, &one.key).exists());
        assert!(record_path_at(&mods, &two.key).exists());
        assert_eq!(load_all_at(&mods).unwrap().len(), 2, "both records survive");
    }

    #[test]
    fn create_copies_each_stored_findings_provenance_and_marks_a_missing_one() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));

        let rec = create(
            &mods,
            &finds,
            "sonnet",
            &["sess-a/1".to_string(), "sess-z/9".to_string()],
            Some("kit"),
            &[],
        )
        .unwrap();

        assert_eq!(rec.r#for, vec!["sess-a/1", "sess-z/9"]);
        let stored = &rec.context.findings[0];
        assert_eq!(stored.key, "sess-a/1");
        assert!(!stored.missing);
        assert_eq!(stored.mission_id.as_deref(), Some("crawl-1"));
        // A reader of the MOD never has to go find the finding.
        assert_eq!(stored.emitted, Some(serde_json::json!({"file": "a.ts", "line": 4})));
        assert_eq!(
            stored.context,
            Some(serde_json::json!({"rule": "unnamed-predicate", "unit": "u1"}))
        );

        // A `for` key with no stored finding is allowed — the change was still
        // proposed — and is recorded as missing rather than as absent context.
        let absent = &rec.context.findings[1];
        assert_eq!(absent.key, "sess-z/9");
        assert!(absent.missing, "an unstored finding is marked, not silently empty");
        assert!(absent.emitted.is_none());

        // It round-trips through disk with the same shape.
        let back = load_at(&mods, &rec.key).unwrap().expect("round trips");
        assert_eq!(back.context, rec.context);
        assert_eq!(back.by, "sonnet");
        assert_eq!(back.schema_version, MOD_SCHEMA_VERSION);
    }

    #[test]
    fn create_copies_attachments_byte_for_byte_and_refuses_a_colliding_basename() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::create_dir_all(src.join("b")).unwrap();
        std::fs::write(src.join("a/patch.diff"), b"--- a\n+++ b\n").unwrap();
        std::fs::write(src.join("b/shot.png"), [0x89u8, 0x50, 0x4e, 0x47]).unwrap();

        let rec = create(
            &mods,
            &finds,
            "kain",
            &[],
            None,
            &[src.join("a/patch.diff"), src.join("b/shot.png")],
        )
        .unwrap();
        assert_eq!(rec.attachments, vec!["patch.diff", "shot.png"]);
        assert!(rec.kit.is_null(), "attachments alone are a kit; no kit text was given");
        let dest = attachments_dir_at(&mods, &rec.key);
        assert_eq!(std::fs::read(dest.join("patch.diff")).unwrap(), b"--- a\n+++ b\n");
        assert_eq!(std::fs::read(dest.join("shot.png")).unwrap(), [0x89u8, 0x50, 0x4e, 0x47]);

        // Two attachments with one basename: one would overwrite the other, so
        // the whole create is refused rather than quietly losing a file.
        std::fs::write(src.join("b/patch.diff"), b"other").unwrap();
        let err = create(
            &mods,
            &finds,
            "kain",
            &[],
            None,
            &[src.join("a/patch.diff"), src.join("b/patch.diff")],
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("patch.diff"), "the error names it: {err:#}");
    }

    #[test]
    fn create_refuses_a_mod_with_neither_instructions_nor_data() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        assert!(create(&mods, &finds, "kain", &[], None, &[]).is_err());
        assert!(create(&mods, &finds, "  ", &[], Some("kit"), &[]).is_err(), "a mod names its proposer");
        assert!(!mods.exists(), "a refusal writes nothing at all");
    }

    #[test]
    fn materialize_writes_once_and_refuses_a_key_that_could_escape_the_store() {
        let tmp = TempDir::new().unwrap();
        let mut rec = ModRecord {
            key: mint_key(),
            ts: "2026-09-03T01:00:00Z".into(),
            by: "kain".into(),
            r#for: vec![],
            kit: serde_json::Value::String("k".into()),
            attachments: vec![],
            context: ModContext::default(),
            schema_version: MOD_SCHEMA_VERSION.into(),
            extras: serde_json::Map::new(),
        };
        assert_eq!(materialize(tmp.path(), &rec).unwrap(), Materialized::Created);
        let path = record_path_at(tmp.path(), &rec.key);
        std::fs::write(&path, "{\"key\":\"sentinel\"}").unwrap();
        assert_eq!(materialize(tmp.path(), &rec).unwrap(), Materialized::AlreadyPresent);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"key\":\"sentinel\"}");

        for bad in ["../escape", "a/b", ".hidden", ""] {
            rec.key = bad.to_string();
            assert!(materialize(tmp.path(), &rec).is_err(), "must be refused: {bad:?}");
        }
    }

    #[test]
    fn load_all_at_sorts_by_ts_and_skips_what_it_cannot_read() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mods");
        assert!(load_all_at(&root).unwrap().is_empty(), "an absent store is empty, not an error");

        let mk = |key: &str, ts: &str| ModRecord {
            key: key.into(),
            ts: ts.into(),
            by: "kain".into(),
            r#for: vec![],
            kit: serde_json::Value::String("k".into()),
            attachments: vec![],
            context: ModContext::default(),
            schema_version: MOD_SCHEMA_VERSION.into(),
            extras: serde_json::Map::new(),
        };
        materialize(&root, &mk("mod-c", "2026-09-03T03:00:00Z")).unwrap();
        materialize(&root, &mk("mod-a", "2026-09-03T01:00:00Z")).unwrap();
        materialize(&root, &mk("mod-b", "2026-09-03T02:00:00Z")).unwrap();
        std::fs::create_dir_all(root.join("mod-x")).unwrap();
        std::fs::write(root.join("mod-x/mod.json"), "{ not json").unwrap();

        let keys: Vec<String> = load_all_at(&root).unwrap().into_iter().map(|m| m.key).collect();
        assert_eq!(keys, vec!["mod-a", "mod-b", "mod-c"], "ts-ascending");

        // A key that could escape the store never resolves to a read.
        assert!(load_at(&root, "../mod-a").unwrap().is_none());
        assert!(load_at(&root, "mod-nope").unwrap().is_none());
    }

    #[test]
    fn the_view_from_a_finding_to_its_mods_is_derived_by_scanning_mods() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));
        store_finding(&finds, "sess-b", 2, Some("crawl-2"));

        let a1 = create(&mods, &finds, "sonnet", &["sess-a/1".into()], Some("x"), &[]).unwrap();
        let a2 = create(&mods, &finds, "kain", &["sess-a/1".into()], Some("y"), &[]).unwrap();
        let b = create(&mods, &finds, "kain", &["sess-b/2".into()], Some("z"), &[]).unwrap();
        let none = create(&mods, &finds, "kain", &[], Some("standalone"), &[]).unwrap();

        let all = load_all_at(&mods).unwrap();
        let keys: Vec<&str> = mods_for(&all, "sess-a/1").iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys.len(), 2, "one observation can attract competing changes");
        assert!(keys.contains(&a1.key.as_str()) && keys.contains(&a2.key.as_str()));
        assert!(mods_for(&all, "sess-c/9").is_empty());

        // `--mission` matches through the `for` finding's OWN mission, copied
        // onto the mod at create time.
        let by_mission = |m: &str| -> Vec<String> {
            all.iter().filter(|r| names_mission(r, m)).map(|r| r.key.clone()).collect()
        };
        assert_eq!(by_mission("crawl-2"), vec![b.key.clone()]);
        assert_eq!(by_mission("crawl-1").len(), 2);
        assert!(by_mission("no-such-mission").is_empty());
        assert!(
            !by_mission("crawl-1").contains(&none.key),
            "a mod naming no finding belongs to no mission"
        );
    }
}

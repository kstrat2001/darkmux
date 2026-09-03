//! (#2265) The MOD record: how something could change.
//!
//! A mod is a **kit**: instructions plus data, in whatever form the proposer
//! chose — a diff, a sentence, pixel data, a config value — enough for an AI
//! to make the change correctly later, given the mod's own context. **darkmux
//! never types a kit and never opens it.**
//!
//! *Verbatim* means BYTE-EXACT, which is why the kit is always a string and is
//! never parsed on write. An earlier version parsed a JSON-looking kit and
//! re-serialized it; that silently collapsed duplicate keys and rounded large
//! integers through `f64`, so the stored kit was not the kit that was written.
//! A kit is not darkmux's data to normalize. `kit_looks_json` is a reader
//! HINT computed at write time — it says a parse succeeded once, and promises
//! nothing about what a reader will get back.
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
//! `for` keys are CANONICALIZED on create (`<dispatch>/<seq>`, the seq
//! renumbered), so `sess-a/01` and `sess-a/1` are one address. One finding has
//! to have one address, or a mod is attached to a finding by one reader and
//! invisible to another. A key that can address no finding at all is refused
//! loudly rather than stored as a link nothing can follow.
//!
//! For each `for` key that exists in the finding store, the mod copies that
//! finding's `mission_id`, `context` and `emitted` into its own `context`, so
//! a reader of the mod never has to go find the finding. A `for` key with no
//! stored finding is allowed and recorded as `{key, missing: true}` — the mod
//! is still the change someone proposed.
//!
//! **That copy is a SNAPSHOT taken at create time.** It is what makes the mod
//! self-describing, and it is also the limit: a mod created before its finding
//! was synced records that finding as missing and carries no mission, so
//! `mod list --mission` will not see it. The snapshot is never refreshed —
//! rewriting it would make the mod a mutable view of a record that is itself
//! an event.

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

/// Whether an attachment file name is safe to write inside a mod's own
/// `attachments/`.
///
/// Deliberately WEAKER than [`is_safe_key`]: a key is a directory name darkmux
/// mints and controls, so it refuses a leading dot; a file name is the
/// proposer's, and `.env.example` is an ordinary attachment. Only what could
/// escape the directory is refused — a separator, `.`, `..`, an empty name.
pub fn is_safe_basename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
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
    /// The kit, BYTE-EXACT, always a string, never parsed. `None` when no
    /// kit text was given and the attachments are the whole kit — which is a
    /// different fact from a kit whose text is empty, so the two are not
    /// collapsed. A kit whose text is `null` is the four characters `null`.
    pub kit: Option<String>,
    /// A reader HINT: the kit text parsed as JSON at write time. It is not a
    /// promise and nothing in darkmux acts on it — the kit is handed on as
    /// bytes either way.
    #[serde(default)]
    pub kit_looks_json: bool,
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

/// Where a mod is assembled before it becomes visible. A dot name so it is
/// never a valid mod key ([`is_safe_key`] refuses a leading dot), which is
/// what keeps a half-staged mod from ever being read as a record.
pub const STAGING_DIR: &str = ".staging";

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

/// Whether a kit's text parses as JSON. A HINT for readers, computed once at
/// write time and stored — darkmux does not act on the answer, and the kit is
/// handed on as bytes whichever way it goes.
pub fn kit_looks_json(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok()
}

/// One finding, one address. `sess-a/01` and `sess-a/1` name the same finding,
/// so the seq is renumbered and the pair rejoined — the form every reader
/// compares against. A key that resolves to no `<dispatch>/<seq>` at all can
/// address no finding and is refused by the caller.
pub fn canonical_finding_key(key: &str) -> Option<String> {
    let (dispatch, seq) = crate::findings::parse_key(key)?;
    Some(format!("{dispatch}/{seq}"))
}

/// Canonicalize every `for` key, refusing one that can address no finding.
/// Loud, because such a key would otherwise be stored as a link nothing could
/// ever follow.
pub fn canonical_for_keys(for_keys: &[String]) -> Result<Vec<String>> {
    for_keys
        .iter()
        .map(|k| {
            canonical_finding_key(k).with_context(|| {
                format!("not a finding key: {k:?} (expected <dispatch>/<seq>, e.g. sess-abc/1)")
            })
        })
        .collect()
}

/// Copy each named finding's provenance onto the mod. Keys must already be
/// canonical (see [`canonical_for_keys`]).
pub fn finding_context(findings_root: &Path, for_keys: &[String]) -> Result<ModContext> {
    let mut findings = Vec::new();
    for key in for_keys {
        let stored = match crate::findings::parse_key(key) {
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

    // Everything that can be checked without touching the store is checked
    // first; everything that CANNOT is staged (below), so no failure can leave
    // a half-written mod.
    let for_keys = canonical_for_keys(for_keys)?;
    let mut names: Vec<String> = Vec::new();
    for path in attachments {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| is_safe_basename(n))
            .with_context(|| {
                format!(
                    "attachment file name is not usable as a name inside the mod \
(a separator, `.`, `..` or empty): {}",
                    path.display()
                )
            })?
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
        r#for: for_keys.clone(),
        // The bytes that came in, unchanged. No parse, no re-serialize.
        kit: kit.map(str::to_string),
        kit_looks_json: kit.is_some_and(kit_looks_json),
        attachments: names.clone(),
        context: finding_context(findings_root, &for_keys)?,
        schema_version: MOD_SCHEMA_VERSION.to_string(),
        extras: serde_json::Map::new(),
    };

    // Staged, then renamed into place. The record and its attachments become
    // visible TOGETHER or not at all: a copy that fails halfway would
    // otherwise persist a write-once record listing a file that is not on
    // disk — impossible to complete, and a retry would mint a second key and
    // leave the broken one behind forever.
    let staging_root = root.join(STAGING_DIR);
    let staging = staging_root.join(&key);
    let staged = (|| -> Result<()> {
        // Attachments FIRST, then the record that names them, so the last
        // thing written inside the staging dir is the thing that makes it a
        // record at all.
        if !attachments.is_empty() {
            let dest = staging.join("attachments");
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("creating attachments dir {}", dest.display()))?;
            for (path, name) in attachments.iter().zip(&names) {
                std::fs::copy(path, dest.join(name))
                    .with_context(|| format!("copying attachment {}", path.display()))?;
            }
        }
        // A minted key never collides, so anything but `Created` means
        // something else already owns that address — an error, not a shrug,
        // because the alternative is attaching these files to another mod.
        anyhow::ensure!(
            materialize(&staging_root, &record)? == Materialized::Created,
            "a mod already exists at the minted key {key} — refusing to write over it"
        );
        let final_dir = record_dir_at(root, &key);
        anyhow::ensure!(
            !final_dir.exists(),
            "a mod already exists at {} — refusing to write over it",
            final_dir.display()
        );
        // Atomic within one filesystem, and staging lives under the store, so
        // it always is one.
        std::fs::rename(&staging, &final_dir).with_context(|| {
            format!("moving the staged mod into {}", final_dir.display())
        })?;
        Ok(())
    })();
    if staged.is_err() {
        // Nothing partial survives a failure — not the record, not the files.
        let _ = std::fs::remove_dir_all(&staging);
    }
    // Best-effort tidy; only succeeds when no other create is staging.
    let _ = std::fs::remove_dir(&staging_root);
    staged?;
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
    let rec: ModRecord =
        serde_json::from_str(&body).with_context(|| format!("parsing mod {}", path.display()))?;
    // A record's key IS its address. If they disagree, the record cannot be
    // reached by what it claims to be, so it is not served under a name that
    // does not resolve.
    if !is_addressable(&rec, key) {
        return Ok(None);
    }
    Ok(Some(rec))
}

/// Whether a record read out of `<root>/<dir_name>` may be served: its `key`
/// is that directory, and every attachment name is a plain basename. Both are
/// checked BEFORE anything stats or opens an attachment path.
fn is_addressable(rec: &ModRecord, dir_name: &str) -> bool {
    rec.key == dir_name
        && is_safe_key(&rec.key)
        && rec.attachments.iter().all(|n| is_safe_basename(n))
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
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skips the staging dir for free, along with anything else that could
        // not be a mod's address.
        if !is_safe_key(&name) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(entry.path().join("mod.json")) else {
            continue;
        };
        if let Ok(rec) = serde_json::from_str::<ModRecord>(&body) {
            if is_addressable(&rec, &name) {
                out.push(rec);
            }
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.key.cmp(&b.key)));
    Ok(out)
}

/// The mods that name one finding — DERIVED by scanning mods, because nothing
/// about a mod is ever written back onto the finding it addresses.
///
/// The QUERY is canonicalized here, not only at the caller, so every reader
/// gets the same answer for `sess-a/01` and `sess-a/1`. Stored `for` keys are
/// canonical by construction, so canonicalizing only on write would leave one
/// finding with two addresses from the reader's side. A query that can address
/// no finding matches nothing — there is no stored key it could equal.
pub fn mods_for<'a>(all: &'a [ModRecord], finding_key: &str) -> Vec<&'a ModRecord> {
    let Some(key) = canonical_finding_key(finding_key) else {
        return Vec::new();
    };
    all.iter().filter(|m| m.r#for.contains(&key)).collect()
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

    /// VERBATIM means byte-exact. The earlier version parsed a JSON kit and
    /// re-serialized it, which silently collapsed duplicate keys and rounded
    /// large integers through f64 — a kit is not darkmux's data to normalize.
    #[test]
    fn a_kit_is_stored_byte_exact_and_is_never_parsed() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");

        // Two shapes a JSON round trip destroys, plus the exact whitespace.
        let hostile = "{\n  \"a\": 1,\n  \"a\": 2,\n  \"big\": 12345678901234567890123\n}\n";
        let rec = create(&mods, &finds, "kain", &[], Some(hostile), &[]).unwrap();
        assert_eq!(rec.kit.as_deref(), Some(hostile), "the kit is the bytes that came in");
        assert!(rec.kit_looks_json, "a reader HINT — not a parse, and not a promise");
        let back = load_at(&mods, &rec.key).unwrap().unwrap();
        assert_eq!(back.kit.as_deref(), Some(hostile), "byte-exact through disk too");

        // Prose stays prose, with its own whitespace.
        let prose = "rename the predicate, then add a test\n";
        let rec = create(&mods, &finds, "kain", &[], Some(prose), &[]).unwrap();
        assert_eq!(rec.kit.as_deref(), Some(prose));
        assert!(!rec.kit_looks_json);

        // A kit that is literally the text `null` is that text — not a null.
        let rec = create(&mods, &finds, "kain", &[], Some("null"), &[]).unwrap();
        assert_eq!(rec.kit.as_deref(), Some("null"));
        let raw = std::fs::read_to_string(record_path_at(&mods, &rec.key)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["kit"], "null", "stored as a STRING, so `null` is text: {raw}");
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
        assert!(rec.kit.is_none(), "attachments alone are a kit; no kit text was given");
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

    /// A copy that fails HALFWAY must leave no record at all. The record is
    /// write-once, so a persisted mod listing an attachment that is not on
    /// disk could never be completed — and a retry would mint a second key,
    /// leaving the broken one behind forever.
    #[test]
    fn a_failed_attachment_copy_leaves_no_record_behind() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("ok.diff"), b"--- a\n").unwrap();
        let unreadable = src.join("unreadable.bin");
        std::fs::write(&unreadable, b"secret").unwrap();
        // Readable enough to pass the is-a-file check, unreadable at copy time
        // — the failure lands BETWEEN the two attachments.
        std::fs::set_permissions(&unreadable, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();

        let err = create(
            &mods,
            &finds,
            "kain",
            &[],
            Some("kit"),
            &[src.join("ok.diff"), unreadable.clone()],
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unreadable.bin"), "the error names it: {err:#}");

        assert!(load_all_at(&mods).unwrap().is_empty(), "no half-written mod is visible");
        // Nothing at all is left in the store — not a record dir, not staging.
        let leftovers: Vec<String> = std::fs::read_dir(&mods)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "the store is untouched, got: {leftovers:?}");

        std::fs::set_permissions(&unreadable, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
    }

    /// One finding must have ONE address. `sess-a/01` and `sess-a/1` name the
    /// same finding, and storing the raw string made a mod attached to the
    /// finding (context copied, `--mission` matching) yet invisible to
    /// `list --for sess-a/1` and to that finding's own derived section.
    #[test]
    fn create_canonicalizes_for_keys_and_refuses_one_that_cannot_address_a_finding() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));

        let rec = create(&mods, &finds, "kain", &["sess-a/01".into()], Some("k"), &[]).unwrap();
        assert_eq!(rec.r#for, vec!["sess-a/1"], "stored in canonical form");
        assert_eq!(rec.context.findings[0].key, "sess-a/1");
        assert!(!rec.context.findings[0].missing, "it resolved to the real finding");

        // The one address is the one every reader uses.
        // One address, on the READ side too. Stored keys are canonical, so a
        // reader that compares the caller's raw string finds nothing — the
        // same mod is attached by one query and invisible to another.
        let all = load_all_at(&mods).unwrap();
        assert_eq!(mods_for(&all, "sess-a/1").len(), 1, "the derived view finds it");
        assert_eq!(
            mods_for(&all, "sess-a/01").len(),
            1,
            "a non-canonical QUERY finds the mod stored under the canonical key"
        );
        assert_eq!(mods_for(&all, "sess-a/2").len(), 0, "a different finding is still different");
        assert_eq!(
            mods_for(&all, "no-slash").len(),
            0,
            "a query that can address no finding matches nothing"
        );

        // A key that can address no finding is refused LOUDLY at create time,
        // rather than stored as a link that nothing can ever follow.
        for bad in ["no-slash", "sess-a/notanumber", "../x/1", "/1"] {
            let err = create(&mods, &finds, "kain", &[bad.to_string()], Some("k"), &[]).unwrap_err();
            assert!(
                format!("{err:#}").contains("finding key"),
                "the error names the shape for {bad:?}: {err:#}"
            );
        }
    }

    /// A KEY may not start with a dot (it is a directory name darkmux mints).
    /// A FILE may — `.env.example` is an ordinary attachment. The two rules
    /// were the same function, so a legitimate dotfile was refused.
    #[test]
    fn an_attachment_may_be_a_dotfile_but_never_a_traversal_name() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".env.example"), b"KEY=value\n").unwrap();

        let rec = create(&mods, &finds, "kain", &[], None, &[src.join(".env.example")]).unwrap();
        assert_eq!(rec.attachments, vec![".env.example"]);
        assert_eq!(
            std::fs::read(attachments_dir_at(&mods, &rec.key).join(".env.example")).unwrap(),
            b"KEY=value\n"
        );

        assert!(is_safe_basename(".env.example"));
        for bad in [".", "..", "a/b", "a\\b", ""] {
            assert!(!is_safe_basename(bad), "must be refused: {bad:?}");
        }
    }

    /// A record's key is also its address on disk. If the two disagree, the
    /// record cannot be addressed by what it claims to be, so it is skipped
    /// rather than served under a name that does not resolve.
    #[test]
    fn a_record_whose_key_disagrees_with_its_directory_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let good = create(&mods, &finds, "kain", &[], Some("k"), &[]).unwrap();

        std::fs::create_dir_all(mods.join("mod-liar")).unwrap();
        std::fs::write(
            mods.join("mod-liar/mod.json"),
            serde_json::to_string(&serde_json::json!({
                "key": good.key, "ts": "2026-09-03T09:00:00Z", "by": "x", "for": [],
                "kit": "k", "kit_looks_json": false, "attachments": [],
                "context": {"findings": []}, "schema_version": "1",
            }))
            .unwrap(),
        )
        .unwrap();

        let keys: Vec<String> = load_all_at(&mods).unwrap().into_iter().map(|m| m.key).collect();
        assert_eq!(keys, vec![good.key.clone()], "the impostor is skipped, not served twice");
        assert!(
            load_at(&mods, "mod-liar").unwrap().is_none(),
            "a record that does not own its directory does not resolve there"
        );
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
            kit: Some("k".into()),
            kit_looks_json: false,
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
            kit: Some("k".into()),
            kit_looks_json: false,
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

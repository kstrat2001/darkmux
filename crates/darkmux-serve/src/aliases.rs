//! Identity aliases — view-time mapping of `machine_id` values to a
//! canonical name (#629, refs #624).
//!
//! Operators routinely write flow records under multiple `machine_id` values
//! over a machine's lifetime: hostname default (`MacBook-Pro.local`) when no
//! `DARKMUX_MACHINE_ID` is set, operator-named (`laptop`) when the env var
//! is exported, and possibly older experimental values. The viewer treats
//! every distinct `machine_id` as a separate machine, which double- or
//! triple-counts the same physical hardware.
//!
//! This module loads an operator-managed alias table from
//! `~/.darkmux/identity-aliases.json` (or wherever the daemon is told to
//! look) and rewrites `machine_id` fields at HTTP response time, leaving the
//! on-disk JSONL files and the Redis stream completely untouched. The audit
//! chain stays cryptographically intact; aliases are a presentation layer.
//!
//! ## File format
//!
//! ```json
//! {
//!   "aliases": [
//!     {
//!       "canonical": "laptop",
//!       "aliases": ["MacBook-Pro.local", "kain-mbp"],
//!       "note": "Same physical M5 Max MacBook Pro"
//!     }
//!   ]
//! }
//! ```
//!
//! `canonical` is the name the operator wants to see; `aliases` is the list
//! of `machine_id` values that should be remapped to it. `note` is optional
//! and ignored at runtime (purely for operator documentation).
//!
//! ## Behavior on errors
//!
//! - Missing file → load as empty table (single-machine and pre-aliases
//!   fleets work unchanged).
//! - Malformed JSON → log a warning to stderr, load as empty table. The
//!   daemon stays serving rather than refusing to start over a config typo.
//! - Self-referential or duplicate alias entries → last write wins per
//!   alias key. The map is built one entry at a time; later canonicals
//!   that claim the same alias quietly override earlier ones.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// Compiled alias table — `alias_id → canonical_name`. Includes self-maps
/// for canonicals so a single lookup answers both "is this id a known
/// canonical?" and "what should this alias resolve to?"
#[derive(Debug, Clone, Default)]
pub struct IdentityAliases {
    map: HashMap<String, String>,
}

impl IdentityAliases {
    /// Empty alias table — the default when no file is present. All
    /// `rewrite_*` calls become no-ops.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load + compile the alias file. Returns an error if the file is
    /// unreadable or the JSON is malformed. Callers that want
    /// graceful-degrade-to-empty behavior should use `load_or_default`.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading identity-aliases file: {}", path.display()))?;
        let raw: AliasesFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing identity-aliases file: {}", path.display()))?;
        let mut map = HashMap::new();
        for entry in raw.aliases {
            // Self-map the canonical so resolve("laptop") returns "laptop"
            // without a special-case branch in the lookup.
            map.insert(entry.canonical.clone(), entry.canonical.clone());
            for alias in entry.aliases {
                map.insert(alias, entry.canonical.clone());
            }
        }
        Ok(Self { map })
    }

    /// Load the alias file at `path`. Missing file → empty table. Malformed
    /// JSON → warn to stderr + empty table. Designed to be called from
    /// daemon startup where refusing-to-serve over a config typo would be
    /// worse than running with no aliases.
    pub fn load_or_default(path: &Path) -> Self {
        if !path.exists() {
            return Self::empty();
        }
        match Self::from_file(path) {
            Ok(aliases) => aliases,
            Err(e) => {
                eprintln!(
                    "darkmux serve: identity-aliases file at {} is unreadable or malformed; \
                     continuing with no aliases ({e})",
                    path.display()
                );
                Self::empty()
            }
        }
    }

    /// Resolve a `machine_id` to its canonical name. Passes unknown ids
    /// through unchanged so non-aliased machines keep their original
    /// stamping.
    pub fn resolve<'a>(&'a self, machine_id: &'a str) -> &'a str {
        self.map
            .get(machine_id)
            .map(|s| s.as_str())
            .unwrap_or(machine_id)
    }

    /// `true` if no aliases are configured — callers can skip the rewrite
    /// pass entirely when this returns true.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// In-place rewrite of the `machine_id` field on a batch of flow
    /// records. No-op when `is_empty()`; otherwise walks each record's
    /// top-level object, looks up the current `machine_id`, and replaces
    /// it with the canonical only when there's a change to make.
    ///
    /// Records that don't have a `machine_id` field, or whose `machine_id`
    /// isn't in the alias map, are left unchanged.
    pub fn rewrite_records(&self, records: &mut [Value]) {
        if self.is_empty() {
            return;
        }
        for record in records.iter_mut() {
            let Some(obj) = record.as_object_mut() else {
                continue;
            };
            let current = match obj.get("machine_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if let Some(canonical) = self.map.get(&current) {
                if canonical != &current {
                    obj.insert("machine_id".to_string(), Value::String(canonical.clone()));
                }
            }
        }
    }

    /// Re-export the compiled table as JSON for the `/fleet/aliases`
    /// endpoint. Groups by canonical so the shape matches the on-disk
    /// file format (less the optional `note` field, which the runtime
    /// drops since it's documentation-only).
    pub fn as_json(&self) -> Value {
        let mut by_canon: HashMap<String, Vec<String>> = HashMap::new();
        for (alias, canonical) in &self.map {
            if alias != canonical {
                by_canon
                    .entry(canonical.clone())
                    .or_default()
                    .push(alias.clone());
            } else {
                // Ensure canonicals with no aliases still appear in the
                // export, so an operator inspecting the endpoint sees the
                // full set of declared identities.
                by_canon.entry(canonical.clone()).or_default();
            }
        }
        // Sort each alias list for a stable JSON shape (helpful for
        // golden-output tests and for operators diffing the endpoint
        // across runs).
        let mut entries: Vec<_> = by_canon
            .into_iter()
            .map(|(canonical, mut aliases)| {
                aliases.sort();
                serde_json::json!({
                    "canonical": canonical,
                    "aliases": aliases,
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            a.get("canonical")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(b.get("canonical").and_then(|v| v.as_str()).unwrap_or(""))
        });
        serde_json::json!({ "aliases": entries })
    }
}

#[derive(Deserialize)]
struct AliasesFile {
    aliases: Vec<AliasEntry>,
}

#[derive(Deserialize)]
struct AliasEntry {
    canonical: String,
    aliases: Vec<String>,
    // `note` is operator documentation; deserialize so its presence in the
    // file doesn't error, but the runtime doesn't need to keep it.
    #[allow(dead_code)]
    #[serde(default)]
    note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_aliases(json_str: &str) -> NamedTempFile {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(json_str.as_bytes()).unwrap();
        tmp.flush().unwrap();
        tmp
    }

    #[test]
    fn empty_table_passes_everything_through() {
        let aliases = IdentityAliases::empty();
        assert!(aliases.is_empty());
        assert_eq!(aliases.resolve("anything"), "anything");
        let mut records = vec![json!({"machine_id": "MacBook-Pro.local", "action": "note"})];
        aliases.rewrite_records(&mut records);
        assert_eq!(records[0]["machine_id"], "MacBook-Pro.local");
    }

    #[test]
    fn load_or_default_handles_missing_file_silently() {
        let path = std::path::Path::new("/tmp/darkmux-nonexistent-aliases-test-xyz.json");
        // Make sure it doesn't exist
        let _ = std::fs::remove_file(path);
        let aliases = IdentityAliases::load_or_default(path);
        assert!(aliases.is_empty());
    }

    #[test]
    fn load_or_default_handles_malformed_json_silently() {
        let tmp = write_aliases("{ this isn't json");
        let aliases = IdentityAliases::load_or_default(tmp.path());
        assert!(aliases.is_empty());
    }

    #[test]
    fn resolve_returns_canonical_for_aliased_id() {
        let tmp = write_aliases(
            r#"{
                "aliases": [
                    {
                        "canonical": "laptop",
                        "aliases": ["MacBook-Pro.local", "kain-mbp"]
                    }
                ]
            }"#,
        );
        let aliases = IdentityAliases::from_file(tmp.path()).unwrap();
        assert_eq!(aliases.resolve("MacBook-Pro.local"), "laptop");
        assert_eq!(aliases.resolve("kain-mbp"), "laptop");
        // Canonical itself resolves to itself
        assert_eq!(aliases.resolve("laptop"), "laptop");
        // Unknown id passes through
        assert_eq!(aliases.resolve("other-machine"), "other-machine");
    }

    #[test]
    fn rewrite_records_remaps_aliased_machine_ids_only() {
        let tmp = write_aliases(
            r#"{
                "aliases": [
                    { "canonical": "laptop", "aliases": ["MacBook-Pro.local"] },
                    { "canonical": "studio", "aliases": ["m1-max-32gb-studio"] }
                ]
            }"#,
        );
        let aliases = IdentityAliases::from_file(tmp.path()).unwrap();
        let mut records = vec![
            json!({"machine_id": "MacBook-Pro.local", "action": "test-1"}),
            json!({"machine_id": "m1-max-32gb-studio", "action": "test-2"}),
            json!({"machine_id": "laptop", "action": "test-3"}), // canonical — unchanged
            json!({"machine_id": "untouched", "action": "test-4"}), // unknown — unchanged
            json!({"action": "no-machine-id"}),                  // missing field — skipped
        ];
        aliases.rewrite_records(&mut records);
        assert_eq!(records[0]["machine_id"], "laptop");
        assert_eq!(records[1]["machine_id"], "studio");
        assert_eq!(records[2]["machine_id"], "laptop");
        assert_eq!(records[3]["machine_id"], "untouched");
        assert!(records[4].get("machine_id").is_none());
    }

    #[test]
    fn rewrite_is_noop_when_table_empty() {
        let aliases = IdentityAliases::empty();
        let mut records = vec![
            json!({"machine_id": "MacBook-Pro.local", "action": "test-1"}),
            json!({"machine_id": "laptop", "action": "test-2"}),
        ];
        let snapshot = records.clone();
        aliases.rewrite_records(&mut records);
        assert_eq!(records, snapshot);
    }

    #[test]
    fn as_json_groups_by_canonical_and_sorts() {
        let tmp = write_aliases(
            r#"{
                "aliases": [
                    { "canonical": "laptop", "aliases": ["MacBook-Pro.local", "kain-mbp"] },
                    { "canonical": "studio", "aliases": ["m1-max-32gb-studio"] }
                ]
            }"#,
        );
        let aliases = IdentityAliases::from_file(tmp.path()).unwrap();
        let json = aliases.as_json();
        let arr = json["aliases"].as_array().unwrap();
        // Sorted by canonical
        assert_eq!(arr[0]["canonical"], "laptop");
        assert_eq!(arr[1]["canonical"], "studio");
        // Aliases per canonical are sorted
        let laptop_aliases = arr[0]["aliases"].as_array().unwrap();
        assert_eq!(laptop_aliases[0], "MacBook-Pro.local");
        assert_eq!(laptop_aliases[1], "kain-mbp");
    }

    #[test]
    fn note_field_in_file_is_ignored_at_runtime() {
        // Sanity: the file format documents `note` but the runtime doesn't
        // crash on its presence. Just confirms the loader accepts files
        // that include the operator-documentation field.
        let tmp = write_aliases(
            r#"{
                "aliases": [
                    {
                        "canonical": "laptop",
                        "aliases": ["MacBook-Pro.local"],
                        "note": "Same physical M5 Max MacBook Pro — env was inconsistent before satellite onboarding"
                    }
                ]
            }"#,
        );
        let aliases = IdentityAliases::from_file(tmp.path()).unwrap();
        assert_eq!(aliases.resolve("MacBook-Pro.local"), "laptop");
    }
}

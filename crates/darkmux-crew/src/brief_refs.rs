//! (#2295) BRIEF REFS: darkmux records addressed by key, rendered as blocks a
//! model can ground, appended to a dispatch's brief verbatim.
//!
//! `dispatch --finding <key>` (#2293) was one instance of a general operation:
//! resolve a stored record by key, render it for the model, append it after
//! the user's own message, and stamp what was appended on the `dispatch start`
//! record. The second instance is a MOD — "here is a proposed change, apply
//! its kit to this workspace" — and there is no reason for the two to have
//! separate plumbing.
//!
//! This module lives beside `findings` and `mods` rather than inside either,
//! because it spans both and neither one should have to know the other exists:
//! `findings.rs` owns the finding record and its block, `mods.rs` owns the mod
//! record and its block, and the resolution + append order lives HERE. It is
//! not in `dispatch.rs` for the same reason — that module is about dispatch
//! mechanics (the ack gate, the opts), not about what a record renders as.
//!
//! **Record kinds darkmux owns only.** A ref names a finding or a mod, never
//! an arbitrary file — the workspace mount is the file channel, and a brief
//! ref is provenance-bearing by construction (the record it names is stored,
//! addressable, and reproducible after the fact).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which record store a [`BriefRef`]'s key addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefRefKind {
    /// A `findings` record — something an earlier dispatch observed.
    Finding,
    /// A `mods` record — a change someone proposed.
    Mod,
}

impl BriefRefKind {
    /// The wire word, used on the step config and the flow record alike.
    pub fn as_str(self) -> &'static str {
        match self {
            BriefRefKind::Finding => "finding",
            BriefRefKind::Mod => "mod",
        }
    }

    /// Parse the wire word. `None` for anything else — readers of a step
    /// config or a flow record stay lenient, the way every other darkmux
    /// data shape does.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "finding" => Some(BriefRefKind::Finding),
            "mod" => Some(BriefRefKind::Mod),
            _ => None,
        }
    }
}

/// One record the brief carries: a kind plus the key its store answers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefRef {
    pub kind: BriefRefKind,
    pub key: String,
}

impl BriefRef {
    pub fn finding(key: impl Into<String>) -> Self {
        BriefRef { kind: BriefRefKind::Finding, key: key.into() }
    }

    pub fn mod_(key: impl Into<String>) -> Self {
        BriefRef { kind: BriefRefKind::Mod, key: key.into() }
    }
}

/// The two store roots a resolution reads. Grouped so the resolver's callers
/// cannot pass them in the wrong order (they are both `PathBuf`s).
#[derive(Debug, Clone)]
pub struct StoreDirs {
    pub findings: PathBuf,
    pub mods: PathBuf,
}

impl StoreDirs {
    /// The operator's resolved stores (`env > config > default` for each).
    pub fn resolved() -> Self {
        StoreDirs { findings: crate::findings::findings_dir(), mods: crate::mods::mods_dir() }
    }
}

/// Append each named record to a dispatch brief, and return the brief plus the
/// CANONICAL refs that were appended (the key as the stored record spells it,
/// not as the caller typed it).
///
/// A key that addresses no stored record is an ERROR, not a skip, and the
/// error names both the kind and the key: dispatching with a silently missing
/// block would send the role to work on a record it never saw. Every
/// resolution happens HERE, before any container work — before the ack gate,
/// before routing — so a typo costs nothing but the message.
///
/// Blocks are appended in the order the refs are given, each after a blank
/// line, following the user's own message.
pub fn append_to_brief(
    message: &str,
    refs: &[BriefRef],
    dirs: &StoreDirs,
) -> Result<(String, Vec<BriefRef>)> {
    let mut brief = message.to_string();
    let mut appended = Vec::new();
    for r in refs {
        let (block, canonical) = resolve(r, dirs)?;
        brief.push_str("\n\n");
        brief.push_str(&block);
        appended.push(canonical);
    }
    Ok((brief, appended))
}

/// Render one ref, or refuse naming the kind and the key.
fn resolve(r: &BriefRef, dirs: &StoreDirs) -> Result<(String, BriefRef)> {
    match r.kind {
        BriefRefKind::Finding => {
            let key = &r.key;
            let (dispatch, seq) = crate::findings::parse_key(key).with_context(|| {
                format!(
                    "--finding {key:?} is not a finding key. A key is `<dispatch>/<seq>`, \
                     e.g. `sess-abc/1` — `darkmux finding list` shows what is stored."
                )
            })?;
            let record =
                crate::findings::load_at(&dirs.findings, &dispatch, seq)?.with_context(|| {
                    format!(
                        "no finding {key} under {}\n  `darkmux finding sync` replays the flow \
                         stream into the store.",
                        dirs.findings.display()
                    )
                })?;
            let canonical = BriefRef::finding(record.key.clone());
            Ok((crate::findings::brief_block(&record), canonical))
        }
        BriefRefKind::Mod => {
            let key = &r.key;
            if !crate::mods::is_safe_key(key) {
                anyhow::bail!(
                    "--mod {key:?} is not a mod key. A key is one path segment, e.g. \
                     `mod-1757000000-a1b2c3` — `darkmux mod list` shows what is stored."
                );
            }
            let record = crate::mods::load_at(&dirs.mods, key)?.with_context(|| {
                format!(
                    "no mod {key} under {}\n  `darkmux mod list` shows what is stored.",
                    dirs.mods.display()
                )
            })?;
            let mount = crate::mods::attachments_container_dir(&record.key);
            let canonical = BriefRef::mod_(record.key.clone());
            Ok((crate::mods::brief_block(&record, &mount), canonical))
        }
    }
}

/// The refs as the step config and the flow record carry them:
/// `[{"kind": "finding", "key": "sess-a/1"}, …]`.
pub fn to_json(refs: &[BriefRef]) -> serde_json::Value {
    serde_json::Value::Array(
        refs.iter()
            .map(|r| serde_json::json!({ "kind": r.kind.as_str(), "key": r.key }))
            .collect(),
    )
}

/// Read the refs back off a step config's `brief_refs` value. Lenient: an
/// entry whose `kind` is unknown or whose `key` is missing is dropped rather
/// than failing the step, the same contract every other config read has.
pub fn from_json(v: Option<&serde_json::Value>) -> Vec<BriefRef> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let kind = BriefRefKind::parse(e.get("kind")?.as_str()?)?;
            let key = e.get("key")?.as_str()?.to_string();
            Some(BriefRef { kind, key })
        })
        .collect()
}

/// The host attachment directories the named mods want bind-mounted into the
/// container, paired with the container path each is mounted at.
///
/// A mod with no attachments contributes NOTHING — mounting an absent host
/// directory would make docker create an empty one on the host, under the
/// operator's mod store, for a mod that deliberately has no files.
pub fn mod_attachment_mounts(refs: &[BriefRef], mods_root: &Path) -> Vec<(PathBuf, String)> {
    refs.iter()
        .filter(|r| r.kind == BriefRefKind::Mod)
        .filter_map(|r| {
            let host = crate::mods::attachments_dir_at(mods_root, &r.key);
            host.is_dir().then(|| (host, crate::mods::attachments_container_dir(&r.key)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn dirs(findings: &TempDir, mods: &TempDir) -> StoreDirs {
        StoreDirs { findings: findings.path().to_path_buf(), mods: mods.path().to_path_buf() }
    }

    fn write_finding(root: &Path, dispatch: &str, seq: u64) {
        let dir = root.join(dispatch).join(seq.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("finding.json"),
            serde_json::json!({
                "key": format!("{dispatch}/{seq}"), "dispatch": dispatch, "seq": seq,
                "ts": "2026-09-04T00:00:00Z", "tool_name": "create_finding",
                "proposer": {"handle": "crawler", "model": "m"},
                "context": {"unit": "u7"},
                "emitted": {"why": "MARKER-observed"},
                "schema_version": "1"
            })
            .to_string(),
        )
        .unwrap();
    }

    fn write_mod(root: &Path, key: &str, kit: Option<&str>, attachments: &[&str]) {
        let dir = root.join(key);
        fs::create_dir_all(&dir).unwrap();
        if !attachments.is_empty() {
            let att = dir.join("attachments");
            fs::create_dir_all(&att).unwrap();
            for name in attachments {
                fs::write(att.join(name), b"body").unwrap();
            }
        }
        fs::write(
            dir.join("mod.json"),
            serde_json::json!({
                "key": key, "ts": "2026-09-04T00:00:00Z", "by": "sonnet",
                "for": ["sess-x/1", "sess-y/2"],
                "kit": kit,
                "kit_looks_json": false,
                "attachments": attachments,
                "context": {"findings": []},
                "schema_version": "1"
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn both_kinds_append_in_the_order_given() {
        let f = TempDir::new().unwrap();
        let m = TempDir::new().unwrap();
        write_finding(f.path(), "sess-a", 1);
        write_mod(m.path(), "mod-1-aaa", Some("KIT-BODY"), &["patch.diff"]);

        let refs = vec![BriefRef::finding("sess-a/1"), BriefRef::mod_("mod-1-aaa")];
        let (brief, appended) = append_to_brief("do it", &refs, &dirs(&f, &m)).unwrap();

        assert!(brief.starts_with("do it\n\n"), "the user's message comes first: {brief}");
        let finding_at = brief.find("MARKER-observed").expect("the finding block");
        let mod_at = brief.find("KIT-BODY").expect("the mod block");
        assert!(finding_at < mod_at, "blocks follow the order the refs were given: {brief}");
        assert!(
            brief.contains("/darkmux-mods/mod-1-aaa/attachments/patch.diff"),
            "the mod block names the attachment by its in-container path: {brief}"
        );
        assert_eq!(appended, refs);
    }

    /// (#2295) The mod block, whole. The kit is the product: it goes in
    /// BYTE-EXACT, never reflowed and never parsed, and the attachments are
    /// named by the container path they are actually mounted at.
    #[test]
    fn mod_brief_block_carries_the_kit_verbatim_the_for_keys_and_the_mounted_paths() {
        let m = TempDir::new().unwrap();
        let kit = "--- a/x.rs\n+++ b/x.rs\n@@\n-  let x = 1;\n+  let x = 2;\n";
        write_mod(m.path(), "mod-9-zzz", Some(kit), &["one.patch", "two.txt"]);
        let record = crate::mods::load_at(m.path(), "mod-9-zzz").unwrap().unwrap();
        let mount = crate::mods::attachments_container_dir(&record.key);
        let block = crate::mods::brief_block(&record, &mount);

        assert!(block.starts_with("<mod key=\"mod-9-zzz\">"), "{block}");
        assert!(block.contains("<darkmux-term name=\"mod\">"), "{block}");
        assert!(block.contains("<darkmux-term name=\"kit\">"), "{block}");
        assert!(block.contains("proposed by: sonnet"), "{block}");
        assert!(block.contains("addresses findings: sess-x/1, sess-y/2"), "{block}");
        assert!(
            block.contains(&format!("<kit>\n{kit}\n</kit>")),
            "the kit's own bytes, unreflowed and unparsed: {block}"
        );
        assert!(block.contains("- /darkmux-mods/mod-9-zzz/attachments/one.patch"), "{block}");
        assert!(block.contains("- /darkmux-mods/mod-9-zzz/attachments/two.txt"), "{block}");
        assert!(block.contains("</mod>"), "{block}");
    }

    #[test]
    fn mod_brief_block_says_so_when_there_is_no_kit_text_and_no_attachment() {
        let m = TempDir::new().unwrap();
        write_mod(m.path(), "mod-8-yyy", None, &[]);
        let record = crate::mods::load_at(m.path(), "mod-8-yyy").unwrap().unwrap();
        let block = crate::mods::brief_block(&record, "/darkmux-mods/mod-8-yyy/attachments");
        assert!(block.contains("(no kit text"), "{block}");
        assert!(block.contains("attachments: (none)"), "{block}");
    }

    #[test]
    fn a_missing_mod_refuses_and_names_the_kind_and_the_key() {
        let f = TempDir::new().unwrap();
        let m = TempDir::new().unwrap();
        let err = append_to_brief("hi", &[BriefRef::mod_("mod-nope-1")], &dirs(&f, &m))
            .expect_err("a key that addresses no stored mod must refuse");
        let text = format!("{err:#}");
        assert!(text.contains("no mod mod-nope-1"), "{text}");
    }

    #[test]
    fn a_missing_finding_refuses() {
        let f = TempDir::new().unwrap();
        let m = TempDir::new().unwrap();
        let err = append_to_brief("hi", &[BriefRef::finding("sess-a/9")], &dirs(&f, &m))
            .expect_err("a key that addresses no stored finding must refuse");
        assert!(format!("{err:#}").contains("no finding sess-a/9"));
    }

    #[test]
    fn a_traversing_mod_key_is_refused_before_any_read() {
        let f = TempDir::new().unwrap();
        let m = TempDir::new().unwrap();
        let err = append_to_brief("hi", &[BriefRef::mod_("../etc")], &dirs(&f, &m))
            .expect_err("a key that escapes the store must refuse");
        assert!(format!("{err:#}").contains("is not a mod key"));
    }

    #[test]
    fn json_round_trips_through_the_step_config_shape() {
        let refs = vec![BriefRef::finding("sess-a/1"), BriefRef::mod_("mod-1-aaa")];
        let v = to_json(&refs);
        assert_eq!(
            v,
            serde_json::json!([
                {"kind": "finding", "key": "sess-a/1"},
                {"kind": "mod", "key": "mod-1-aaa"},
            ])
        );
        assert_eq!(from_json(Some(&v)), refs);
        assert!(from_json(None).is_empty());
        // Lenient on read: an unknown kind is dropped, not fatal.
        let mixed = serde_json::json!([{"kind": "sandwich", "key": "k"}, {"kind": "mod", "key": "m1"}]);
        assert_eq!(from_json(Some(&mixed)), vec![BriefRef::mod_("m1")]);
    }

    #[test]
    fn only_mods_with_an_attachments_dir_are_mounted() {
        let m = TempDir::new().unwrap();
        write_mod(m.path(), "mod-with", Some("k"), &["a.txt"]);
        write_mod(m.path(), "mod-without", Some("k"), &[]);
        let refs = vec![
            BriefRef::finding("sess-a/1"),
            BriefRef::mod_("mod-with"),
            BriefRef::mod_("mod-without"),
        ];
        let mounts = mod_attachment_mounts(&refs, m.path());
        assert_eq!(
            mounts,
            vec![(
                m.path().join("mod-with").join("attachments"),
                "/darkmux-mods/mod-with/attachments".to_string()
            )],
            "a finding contributes no mount, and a mod with no attachments dir contributes none"
        );
    }
}

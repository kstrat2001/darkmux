//! The frozen `--bundler` JSON contract types — vendored from
//! `darkmux-lab`'s `lab::bundle::{Bundle, BundleRef, BundleSet}` shape
//! EXACTLY (same field names, same serde attributes, same semantics).
//! Not reused via a path dependency, deliberately: matching the public
//! contract by re-declaring it here (the way a real third-party plugin
//! author — who has no access to darkmux's internal crates at all —
//! would have to) is the honest test of whether the contract, as
//! documented, is actually sufficient on its own.
//!
//! This JSON shape is FROZEN. `manifest`/`truncated` are additive
//! fields — safe for an older consumer to ignore.

use serde::{Deserialize, Serialize};

/// A single (path, line-span) pointer into a source file. 1-indexed,
/// inclusive on both ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRef {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

/// One bundle: a changed function's code + one fact family's mechanical
/// findings about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// `"<fn>@<path>"` — shared across a function's family-variant
    /// bundles so a probe-runner can group them.
    pub id: String,
    pub code: Vec<BundleRef>,
    pub facts: Vec<String>,
    pub fact_family: String,
    /// External symbols referenced in `code` but not defined within it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifest: Vec<String>,
    /// True when any region in `code` shows less than the full extent
    /// of the function it starts at.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleSet {
    pub bundles: Vec<Bundle>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_false_and_empty_manifest_are_omitted_not_serialized_as_defaults() {
        let b = Bundle {
            id: "foo@src/lib.rs".to_string(),
            code: vec![BundleRef { path: "src/lib.rs".to_string(), start: 1, end: 3 }],
            facts: vec![],
            fact_family: "differential".to_string(),
            manifest: vec![],
            truncated: false,
        };
        let json = serde_json::to_string(&b).unwrap();
        assert!(!json.contains("manifest"), "empty manifest must be omitted: {json}");
        assert!(!json.contains("truncated"), "false truncated must be omitted: {json}");
    }

    #[test]
    fn truncated_true_and_nonempty_manifest_serialize() {
        let b = Bundle {
            id: "foo@src/lib.rs".to_string(),
            code: vec![BundleRef { path: "src/lib.rs".to_string(), start: 1, end: 3 }],
            facts: vec![],
            fact_family: "differential".to_string(),
            manifest: vec!["note".to_string()],
            truncated: true,
        };
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains("\"manifest\":[\"note\"]"));
        assert!(json.contains("\"truncated\":true"));
    }

    #[test]
    fn bundle_set_round_trips() {
        let set = BundleSet {
            bundles: vec![Bundle {
                id: "foo@src/lib.rs".to_string(),
                code: vec![BundleRef { path: "src/lib.rs".to_string(), start: 1, end: 3 }],
                facts: vec!["a fact".to_string()],
                fact_family: "differential".to_string(),
                manifest: vec![],
                truncated: false,
            }],
        };
        let json = serde_json::to_string(&set).unwrap();
        let back: BundleSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back.bundles.len(), 1);
        assert_eq!(back.bundles[0].id, "foo@src/lib.rs");
    }
}

//! [`ArchFactsReader`] — the #1286 architecture-facts source.
//!
//! The memory ledger's "potential" number (weights + KV-cache commitment at
//! the loaded ctx) needs per-model architecture facts, and they are readable
//! today with no LMStudio API: every sideloaded model carries its own
//! `config.json` under the LMStudio models root.
//!
//! **Path resolution (live-verified against `lms ls --json`):** the catalog
//! `modelKey` is NOT the on-disk directory for most real models — e.g.
//! modelKey `qwen3.6-35b-a3b-turboquant-mlx` lives at
//! `<root>/majentik/Qwen3.6-35B-A3B-TurboQuant-MLX-MXFP4`. The authoritative
//! location comes from the model's own ls entry: its `path` field (falling
//! back to `indexedModelIdentifier`) is the directory relative to the models
//! root. The reader therefore takes the parsed ls entries at construction
//! and resolves `<root>/<entry-path>/config.json` first, then falls back to
//! trying the modelKey as a directory, else `None`.
//!
//! **Named limitation:** catalog-alias models (e.g. `qwen/qwen3.6-27b`
//! downloaded through LMStudio's own model catalog) carry an ls `path` that
//! matches no directory under the models root — their weights live in a
//! separate store with no readable `config.json`. They resolve to `None`
//! (the estimator's unknowable path); `darkmux doctor` can surface the gap
//! later.
//!
//! From the located `config.json` the reader extracts the KV-arithmetic
//! inputs:
//!
//! - `num_hidden_layers`, `num_key_value_heads`, `head_dim` — top-level OR
//!   under `text_config` (both shapes exist in the wild: plain top-level on
//!   e.g. Qwen3-4B; nested `text_config` on hybrid/multimodal wrappers like
//!   Qwen3.6-35B).
//! - `layer_types` — counted for `"full_attention"` entries. This is the
//!   CRITICAL #1286 discovery: Qwen 3.5/3.6-generation models are HYBRID
//!   linear-attention (only every 4th layer is full attention), so their KV
//!   cost is a fraction of a dense model's. Absent ⇒ every layer is full
//!   attention (the dense default).
//! - quantization bits (`quantization.bits`, MLX convention, or
//!   `quantization_config.bits`, HF convention).
//!
//! **Lenient by contract:** any missing required field ⇒ `None` for the
//! whole model — the estimator's unknowable path — never an error. A model
//! the reader can't price degrades to the documented
//! `LoadEstimateUnknown`-style warnings downstream; it never fails a plan.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Architecture facts for one catalog model, as read from its `config.json`.
///
/// TODO(#1286): re-point at `darkmux_gestalt::ArchFacts` once packet 2a
/// lands the pure `ArchEstimator` and its fact types — this local struct
/// carries the same fields so the seam is one `into()` away at merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchFactsRaw {
    pub num_hidden_layers: u64,
    pub num_key_value_heads: u64,
    pub head_dim: u64,
    /// Layers that keep a full KV cache. `layer_types` counted when present;
    /// absent ⇒ equals `num_hidden_layers` (dense default).
    pub full_attention_layers: u64,
    pub quantization_bits: u64,
}

/// Reads per-model architecture facts from the LMStudio models root, using
/// the models' `lms ls --json` entries to locate each on-disk directory
/// (see the module docs — modelKey is NOT the directory for most models).
#[derive(Debug, Clone)]
pub struct ArchFactsReader {
    models_root: PathBuf,
    /// modelKey → on-disk directory relative to `models_root`, built from
    /// the ls entries' `path` (falling back to `indexedModelIdentifier`).
    key_paths: BTreeMap<String, String>,
}

impl ArchFactsReader {
    /// The production constructor: the real LMStudio models root
    /// (`~/.lmstudio/models`) plus the parsed `lms ls --json` entries whose
    /// `path`/`indexedModelIdentifier` fields locate each model on disk.
    pub fn from_ls_entries(entries: &[serde_json::Value]) -> Self {
        let root = dirs::home_dir()
            .map(|h| h.join(".lmstudio").join("models"))
            .unwrap_or_else(|| PathBuf::from(".lmstudio/models"));
        Self::with_root_and_entries(root, entries)
    }

    /// Explicit root + ls entries — the test seam for the entry-path
    /// resolution (tests point at fixture trees and NEVER read the
    /// operator's real `~/.lmstudio`), and the override for a relocated
    /// LMStudio home.
    pub fn with_root_and_entries(root: impl Into<PathBuf>, entries: &[serde_json::Value]) -> Self {
        Self { models_root: root.into(), key_paths: key_paths_from_entries(entries) }
    }

    /// Explicit root with NO entry map: resolution has only the
    /// modelKey-as-dir fallback, so most real models miss (see the module
    /// docs). Prefer [`ArchFactsReader::from_ls_entries`] whenever ls output
    /// is in hand.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { models_root: root.into(), key_paths: BTreeMap::new() }
    }

    /// Facts for `model_key` (the `lms ls` catalog key). Resolution order:
    ///
    /// 1. The model's ls-entry path under the models root — the
    ///    authoritative on-disk location.
    /// 2. The modelKey itself as a directory under the root (the layouts
    ///    where key and directory coincide, e.g. `darkmux-distill/...`).
    /// 3. `None` — absent directory (the named catalog-alias limitation),
    ///    unreadable file, malformed JSON, or a missing required field.
    pub fn read(&self, model_key: &str) -> Option<ArchFactsRaw> {
        if let Some(rel) = self.key_paths.get(model_key) {
            if let Some(facts) =
                read_config_file(&self.models_root.join(rel).join("config.json"))
            {
                return Some(facts);
            }
        }
        read_config_file(&self.models_root.join(model_key).join("config.json"))
    }
}

/// modelKey → relative on-disk path, from the parsed `lms ls --json` rows.
/// `path` is the field the live CLI populates with the directory relative to
/// the models root; `indexedModelIdentifier` mirrors it and serves as the
/// fallback. Rows missing both (or missing `modelKey`) are skipped — those
/// models simply stay on the modelKey-as-dir fallback.
fn key_paths_from_entries(entries: &[serde_json::Value]) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter_map(|e| {
            let key = e.get("modelKey").and_then(|v| v.as_str())?;
            let path = e
                .get("path")
                .or_else(|| e.get("indexedModelIdentifier"))
                .and_then(|v| v.as_str())?;
            Some((key.to_string(), path.to_string()))
        })
        .collect()
}

fn read_config_file(path: &Path) -> Option<ArchFactsRaw> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    arch_from_config_json(&v)
}

/// Pure extraction from a parsed `config.json`. Per-field resolution:
/// `text_config.<field>` wins over top-level `<field>` when both exist —
/// in a multimodal wrapper the text model is the thing that loads for LLM
/// inference. Quantization is top-level in both known conventions.
fn arch_from_config_json(v: &serde_json::Value) -> Option<ArchFactsRaw> {
    let field = |name: &str| -> Option<&serde_json::Value> {
        v.get("text_config")
            .and_then(|t| t.get(name))
            .or_else(|| v.get(name))
    };
    let num_hidden_layers = field("num_hidden_layers")?.as_u64()?;
    let num_key_value_heads = field("num_key_value_heads")?.as_u64()?;
    let head_dim = field("head_dim")?.as_u64()?;
    let full_attention_layers = match field("layer_types").and_then(|l| l.as_array()) {
        Some(types) => types.iter().filter(|t| t.as_str() == Some("full_attention")).count() as u64,
        None => num_hidden_layers,
    };
    let quantization_bits = v
        .get("quantization")
        .and_then(|q| q.get("bits"))
        .and_then(|b| b.as_u64())
        .or_else(|| {
            v.get("quantization_config")
                .and_then(|q| q.get("bits"))
                .and_then(|b| b.as_u64())
        })?;
    Some(ArchFactsRaw {
        num_hidden_layers,
        num_key_value_heads,
        head_dim,
        full_attention_layers,
        quantization_bits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The fixture models root under this crate's tests dir — real-shape
    /// `config.json` files written for these tests. The operator's real
    /// `~/.lmstudio` is never read.
    const FIXTURE_ROOT: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lmstudio-models");

    /// Map-less reader: exercises the modelKey-as-dir fallback (the fixture
    /// keys ARE their directories).
    fn fixture_reader() -> ArchFactsReader {
        ArchFactsReader::with_root(FIXTURE_ROOT)
    }

    #[test]
    fn read_resolves_dir_from_ls_entry_path_not_model_key() {
        // The live-verified shape (#1286): modelKey is NOT the on-disk
        // directory — the ls entry's `path` is. A key with no directory of
        // its own resolves through the entry map.
        let entries = [json!({
            "modelKey": "hybrid-35b-turboquant-mlx",
            "path": "test-pub/hybrid-35b",
            "indexedModelIdentifier": "test-pub/hybrid-35b",
        })];
        let reader = ArchFactsReader::with_root_and_entries(FIXTURE_ROOT, &entries);
        let facts = reader.read("hybrid-35b-turboquant-mlx").expect("resolved via entry path");
        assert_eq!(facts.num_hidden_layers, 40);
        // And the same key without the map is unresolvable — the old
        // modelKey-join behavior this fix replaces.
        assert_eq!(fixture_reader().read("hybrid-35b-turboquant-mlx"), None);
    }

    #[test]
    fn read_uses_indexed_model_identifier_when_path_absent() {
        let entries = [json!({
            "modelKey": "plain-4b-alias",
            "indexedModelIdentifier": "test-pub/plain-4b",
        })];
        let reader = ArchFactsReader::with_root_and_entries(FIXTURE_ROOT, &entries);
        assert_eq!(reader.read("plain-4b-alias").expect("resolved").num_hidden_layers, 36);
    }

    #[test]
    fn read_falls_back_to_model_key_as_dir_when_entry_path_misses() {
        // An entry whose path matches nothing on disk (or a key with no
        // entry at all) still gets the modelKey-as-dir try before None.
        let entries = [json!({
            "modelKey": "test-pub/plain-4b",
            "path": "elsewhere/not-on-disk",
        })];
        let reader = ArchFactsReader::with_root_and_entries(FIXTURE_ROOT, &entries);
        assert_eq!(reader.read("test-pub/plain-4b").expect("fallback").num_hidden_layers, 36);
    }

    #[test]
    fn catalog_alias_with_no_matching_dir_is_none() {
        // The NAMED LIMITATION (module docs): catalog-alias models carry an
        // ls path that matches no directory under the models root — their
        // weights live in a separate store. Unknowable, never an error;
        // doctor can surface the gap later.
        let entries = [json!({
            "modelKey": "qwen/qwen3.6-27b",
            "path": "qwen/qwen3.6-27b",
            "indexedModelIdentifier": "qwen/qwen3.6-27b",
        })];
        let reader = ArchFactsReader::with_root_and_entries(FIXTURE_ROOT, &entries);
        assert_eq!(reader.read("qwen/qwen3.6-27b"), None);
    }

    #[test]
    fn hybrid_text_config_shape_counts_full_attention_layers() {
        // The Qwen3.6-35B-class shape: fields under text_config, hybrid
        // linear attention with 10 of 40 layers full — the KV-is-nearly-free
        // discovery the ledger math depends on (#1286).
        let facts = fixture_reader().read("test-pub/hybrid-35b").expect("hybrid fixture parses");
        assert_eq!(
            facts,
            ArchFactsRaw {
                num_hidden_layers: 40,
                num_key_value_heads: 2,
                head_dim: 256,
                full_attention_layers: 10,
                quantization_bits: 4,
            }
        );
    }

    #[test]
    fn plain_top_level_shape_defaults_all_layers_full_attention() {
        // The Qwen3-4B-class shape: top-level fields, no layer_types ⇒
        // dense default (every layer keeps a full KV cache).
        let facts = fixture_reader().read("test-pub/plain-4b").expect("plain fixture parses");
        assert_eq!(
            facts,
            ArchFactsRaw {
                num_hidden_layers: 36,
                num_key_value_heads: 8,
                head_dim: 128,
                full_attention_layers: 36,
                quantization_bits: 4,
            }
        );
    }

    #[test]
    fn missing_required_field_is_none_for_the_whole_model() {
        // head_dim absent → the whole model is unknowable (lenient contract).
        assert_eq!(fixture_reader().read("test-pub/missing-fields"), None);
    }

    #[test]
    fn malformed_json_is_none_never_an_error() {
        assert_eq!(fixture_reader().read("test-pub/broken"), None);
    }

    #[test]
    fn absent_model_dir_is_none() {
        assert_eq!(fixture_reader().read("nobody/no-such-model"), None);
    }

    // ── pure-extraction edges over inline values ──

    #[test]
    fn text_config_field_wins_over_top_level() {
        let v = json!({
            "num_hidden_layers": 1,
            "num_key_value_heads": 1,
            "head_dim": 1,
            "text_config": {
                "num_hidden_layers": 40,
                "num_key_value_heads": 2,
                "head_dim": 256
            },
            "quantization": { "bits": 4 }
        });
        let facts = arch_from_config_json(&v).expect("parses");
        assert_eq!(facts.num_hidden_layers, 40, "text_config wins");
        assert_eq!(facts.head_dim, 256);
    }

    #[test]
    fn hf_quantization_config_convention_is_accepted() {
        let v = json!({
            "num_hidden_layers": 32,
            "num_key_value_heads": 8,
            "head_dim": 128,
            "quantization_config": { "bits": 8 }
        });
        assert_eq!(arch_from_config_json(&v).unwrap().quantization_bits, 8);
    }

    #[test]
    fn missing_quantization_is_none_for_the_whole_model() {
        // e.g. a GGUF-style config.json with no quantization block — the
        // reader can't price it; unknowable, not guessed.
        let v = json!({
            "num_hidden_layers": 32,
            "num_key_value_heads": 8,
            "head_dim": 128
        });
        assert_eq!(arch_from_config_json(&v), None);
    }

    #[test]
    fn layer_types_counts_only_full_attention_entries() {
        let v = json!({
            "num_hidden_layers": 4,
            "num_key_value_heads": 2,
            "head_dim": 64,
            "layer_types": ["linear_attention", "full_attention", "linear_attention", "full_attention"],
            "quantization": { "bits": 4 }
        });
        assert_eq!(arch_from_config_json(&v).unwrap().full_attention_layers, 2);
    }
}

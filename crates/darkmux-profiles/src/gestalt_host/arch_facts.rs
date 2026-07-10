//! [`ArchFactsReader`] — the #1286 architecture-facts source.
//!
//! The memory ledger's "potential" number (weights + KV-cache commitment at
//! the loaded ctx) needs per-model architecture facts, and they are readable
//! today with no LMStudio API: every downloaded model carries its own
//! `config.json` under the LMStudio models root. This reader locates
//! `<root>/<publisher>/<model-dir>/config.json` for a catalog model key and
//! extracts the KV-arithmetic inputs:
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

/// Reads per-model architecture facts from the LMStudio models root.
#[derive(Debug, Clone)]
pub struct ArchFactsReader {
    models_root: PathBuf,
}

impl Default for ArchFactsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl ArchFactsReader {
    /// The real LMStudio models root (`~/.lmstudio/models`).
    pub fn new() -> Self {
        let root = dirs::home_dir()
            .map(|h| h.join(".lmstudio").join("models"))
            .unwrap_or_else(|| PathBuf::from(".lmstudio/models"));
        Self { models_root: root }
    }

    /// An explicit root — the test seam (tests point at fixture trees and
    /// NEVER read the operator's real `~/.lmstudio`), and the override for
    /// a relocated LMStudio home.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { models_root: root.into() }
    }

    /// Facts for `model_key` (the `lms ls` catalog key, conventionally
    /// `<publisher>/<model-dir>`), joined verbatim under the models root.
    /// `None` on any miss — absent directory, unreadable file, malformed
    /// JSON, or a missing required field (see module docs).
    pub fn read(&self, model_key: &str) -> Option<ArchFactsRaw> {
        let path = self.models_root.join(model_key).join("config.json");
        read_config_file(&path)
    }
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
    fn fixture_reader() -> ArchFactsReader {
        ArchFactsReader::with_root(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/lmstudio-models"
        ))
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

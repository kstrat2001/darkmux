//! [`GgufFactsReader`] — the #1820 GGUF-header architecture-facts source.
//!
//! #1819 gave every GGUF resident a labeled, conservative ESTIMATE
//! (`ArchWithSizeFallback` in `crate::model_ledger`) because a GGUF download
//! ships no sidecar `config.json` — [`super::ArchFactsReader`] simply has
//! nothing to read. But the architecture facts are not actually missing:
//! they live INSIDE the `.gguf` binary itself, in a documented,
//! stable-since-v2 typed key-value metadata block that precedes the tensor
//! data. This reader parses exactly that block — never the tensor-info
//! array, never a byte of the weights — and hands back the SAME
//! [`ArchFactsRaw`] shape [`super::ArchFactsReader`] does, so
//! `gather_with_bin` can feed it into the identical `arch` map and a GGUF
//! resident prices via `ArchEstimator`
//! ([`crate::model_ledger::PotentialSource::Arch`]) — a MEASUREMENT, not an
//! estimate.
//!
//! **Path resolution mirrors [`super::ArchFactsReader`] exactly** (same ls-
//! entry-path fast path, same modelKey-as-dir fallback, same bounded
//! content-scan-by-name-token fallback for when LMStudio's own path
//! metadata lies) — the only difference is what a candidate directory is
//! searched FOR: a `*.gguf` file instead of a `config.json`. The shared
//! primitives (`tokenize`, `model_name_tokens`, `key_paths_from_entries`)
//! are reused verbatim from `arch_facts` (`pub(super)`) rather than
//! reimplemented, so "what counts as a name-token match" can never drift
//! between the two readers. The directory-walk loop itself is NOT shared
//! (it looks for a different filename), and is small enough that
//! duplicating it stays cheaper than a generic abstraction over "what am I
//! looking for" — this repo's own convention (a 10-line inline duplicate
//! beats a premature generalization).
//!
//! **Multi-file (sharded) GGUF downloads.** A large model split across
//! multiple `.gguf` files (llama.cpp's `--split` convention,
//! `<name>-00001-of-000NN.gguf`) carries its FULL metadata KV block only in
//! the first shard; later shards carry a minimal header. When a resolved
//! directory holds more than one `*.gguf` file, this reader picks the one
//! whose name contains `-00001-of-` (case-insensitive); with no such shard
//! marker and more than one candidate, it declines rather than guess which
//! file actually holds the metadata (the same ambiguity-guard philosophy
//! `arch_facts`'s content-scan uses for directory names).
//!
//! **Bounded by construction (#1286 observer-must-not-perturb).** The
//! reader never reads past the metadata KV block: `tensor_count` is read
//! and then ignored (proves the header parsed; the tensor-info array and
//! all weight data that follow are never touched). Every count taken from
//! the file (`kv_count`, a key's length, a string value's length, an
//! array's element count) is checked against a named ceiling before any
//! read or seek proportional to it is attempted; exceeding a ceiling
//! degrades to `None`, never a panic and never an unbounded loop. Measured
//! against the real `microsoft/phi-4` GGUF that motivated #1819 (9.05 GB on
//! disk): the metadata KV block — including skipping its two ~100k-entry
//! tokenizer arrays — ends 3,541,088 bytes into the file; see the `#[ignore]`
//! ground-truth test below for the up-to-date measured cost.
//!
//! **Named limitation: no per-layer attention-pattern field.** An HF
//! `config.json`'s `layer_types` array is how [`arch_facts`] discovers a
//! hybrid linear-attention model's REAL full-attention layer count (the
//! #1286 finding). GGUF's own KV metadata carries no equivalent field —
//! `<arch>.block_count` is the only per-layer-count signal available — so
//! this reader assumes every layer holds a KV cache (`full_attention_layers
//! == block_count`), the SAME dense default `arch_from_config_json` already
//! uses when `layer_types` is absent. A hybrid-attention GGUF (rare today —
//! the Qwen 3.5/3.6 generation this project has actually probed ships MLX
//! builds with `config.json`, which resolve through `arch_facts` first and
//! never reach this reader) would be OVERPRICED, not underpriced — the same
//! safe-direction bias #1819's own fallback already documents.
//!
//! **Named limitation: GGUF v1 unsupported.** GGUF v1 used 32-bit
//! length-prefixed strings and 32-bit counts; v2 (which introduced the
//! current 64-bit-everywhere wire format) and v3 (the current spec revision,
//! additive over v2) share the byte layout this reader implements. A v1
//! file — obsolete; no LMStudio download observed in the wild uses it —
//! degrades to `None` rather than risk silently misreading a different wire
//! format as the current one.

use super::arch_facts::{key_paths_from_entries, model_name_tokens, tokenize, ArchFactsRaw};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Same walk-budget the #1309 content-scan fallback in `arch_facts` uses —
/// the resident set is small (only LOADED models are ever priced), so this
/// is the observer-must-not-perturb guard against an unbounded walk of a
/// pathological models root, not a real-world limit.
const MAX_SCAN_DIRS: usize = 500;

/// GGUF metadata KV pair count ceiling. Real files carry a few dozen (the
/// live phi-4 GGUF has 33); a claimed count in the hundred-thousands is a
/// malformed-or-hostile file, not a large model — refuse rather than loop.
const MAX_KV_COUNT: u64 = 100_000;

/// A metadata key's byte length ceiling. Real GGUF keys are short
/// dotted-path identifiers (`phi3.attention.head_count_kv` is 28 bytes); a
/// claimed length past this is malformed.
const MAX_KEY_LEN: u64 = 65_536;

/// `general.architecture`'s value-string length ceiling — architecture
/// names are short identifiers (`phi3`, `llama`, `qwen2`); this is the only
/// string value this reader ever materializes in memory (every other string
/// value, however long, is skipped via `Seek` without allocating).
const MAX_ARCH_STRING_LEN: u64 = 4_096;

/// Element-count ceiling for any array this reader skips. The real phi-4
/// GGUF's largest arrays are its ~100k-entry tokenizer vocab and merges
/// lists; this ceiling leaves headroom above any real tokenizer while still
/// bounding a hostile `array_len` claim to a few hundred thousand cheap
/// (length-read + seek) iterations rather than an unbounded loop.
const MAX_ARRAY_LEN: u64 = 2_000_000;

/// Sanity ceiling on `block_count` / `head_count` / `head_count_kv`. No
/// real model published today has anywhere near this many transformer
/// layers or attention heads; a value past this ceiling means the file's
/// byte layout was misread (or the file is hostile), not that a very large
/// model was found — refuse rather than hand the estimator a number that
/// would overflow `ArchFacts::kv_per_token`'s multiplication downstream.
const MAX_PLAUSIBLE_COUNT: u64 = 4_096;

/// Sanity ceiling on the derived/declared `head_dim`. Real models sit in the
/// 64–256 range; this leaves generous headroom while still refusing an
/// implausible value for the same overflow-avoidance reason as
/// [`MAX_PLAUSIBLE_COUNT`].
const MAX_PLAUSIBLE_HEAD_DIM: u64 = 65_536;

// ── GGUF value-type codes (spec-fixed, never renumbered across v2/v3) ──────
const T_UINT8: u32 = 0;
const T_INT8: u32 = 1;
const T_UINT16: u32 = 2;
const T_INT16: u32 = 3;
const T_UINT32: u32 = 4;
const T_INT32: u32 = 5;
const T_FLOAT32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_UINT64: u32 = 10;
const T_INT64: u32 = 11;
const T_FLOAT64: u32 = 12;

/// Reads per-model architecture facts by parsing the `.gguf` weights file's
/// own metadata header — the #1820 measurement source, tried AFTER
/// [`super::ArchFactsReader`] (a `config.json`, when one exists, is read
/// directly rather than reconstructed from the binary) and BEFORE the
/// #1819 size-tiered estimate (`crate::model_ledger::ArchWithSizeFallback`).
#[derive(Debug, Clone)]
pub struct GgufFactsReader {
    models_root: PathBuf,
    key_paths: BTreeMap<String, String>,
}

impl GgufFactsReader {
    /// The production constructor — mirrors
    /// [`super::ArchFactsReader::from_ls_entries`] exactly (same root, same
    /// `lms ls --json` entries).
    pub fn from_ls_entries(entries: &[serde_json::Value]) -> Self {
        let root = dirs::home_dir()
            .map(|h| h.join(".lmstudio").join("models"))
            .unwrap_or_else(|| PathBuf::from(".lmstudio/models"));
        Self::with_root_and_entries(root, entries)
    }

    /// Explicit root + ls entries — the test seam (tests point at fixture
    /// trees built in a temp dir and NEVER read the operator's real
    /// `~/.lmstudio`).
    pub fn with_root_and_entries(root: impl Into<PathBuf>, entries: &[serde_json::Value]) -> Self {
        Self { models_root: root.into(), key_paths: key_paths_from_entries(entries) }
    }

    /// Explicit root with no entry map — only the modelKey-as-dir and
    /// content-scan fallbacks are reachable.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { models_root: root.into(), key_paths: BTreeMap::new() }
    }

    /// Facts for `model_key`, read from its `.gguf` file's own header.
    /// Resolution order mirrors [`super::ArchFactsReader::read`]:
    ///
    /// 1. The model's ls-entry path under the models root.
    /// 2. The modelKey itself as a directory under the root.
    /// 3. The bounded content-scan fallback (same ambiguity guard: exactly
    ///    one matching directory, or `None` — never guess).
    ///
    /// Each step also carries its OWN ambiguity guard for shard selection
    /// (see the module docs): a directory with more than one `*.gguf` file
    /// and no unambiguous `-00001-of-` shard marker returns `None` from
    /// that step rather than picking one, so resolution falls through to
    /// the next step (or to the caller's estimate fallback) instead of
    /// risking a wrong file.
    pub fn read(&self, model_key: &str) -> Option<ArchFactsRaw> {
        if let Some(rel) = self.key_paths.get(model_key) {
            if let Some(facts) = read_gguf_dir(&self.models_root.join(rel)) {
                return Some(facts);
            }
        }
        if let Some(facts) = read_gguf_dir(&self.models_root.join(model_key)) {
            return Some(facts);
        }
        content_scan_gguf(&self.models_root, model_key, MAX_SCAN_DIRS)
    }
}

/// Resolve the one `.gguf` file in `dir` (per [`pick_gguf_file`]) and parse
/// its header. `None` on a missing/unreadable directory, an ambiguous
/// multi-file directory, or a header this reader can't parse.
fn read_gguf_dir(dir: &Path) -> Option<ArchFactsRaw> {
    let gguf_path = pick_gguf_file(dir)?;
    parse_gguf_header_file(&gguf_path)
}

/// Picks the single `.gguf` file a directory's header should be read from.
/// See the module docs' "Multi-file (sharded) GGUF downloads" section.
fn pick_gguf_file(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf")) {
            candidates.push(path);
        }
    }
    match candidates.len() {
        0 => None,
        1 => candidates.into_iter().next(),
        _ => {
            let firsts: Vec<PathBuf> = candidates
                .into_iter()
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.to_ascii_lowercase().contains("-00001-of-"))
                        .unwrap_or(false)
                })
                .collect();
            match firsts.len() {
                1 => firsts.into_iter().next(),
                _ => None,
            }
        }
    }
}

/// The content-scan fallback (mirrors `arch_facts::content_scan_match`):
/// used only when LMStudio's reported path itself is wrong. Walks `root`
/// for the ONE directory (bounded to `max_scan_dirs`) that both holds a
/// `.gguf` file AND whose name's tokens superset `model_key`'s. Zero or
/// multiple matches return `None` — never guess.
fn content_scan_gguf(root: &Path, model_key: &str, max_scan_dirs: usize) -> Option<ArchFactsRaw> {
    let wanted = model_name_tokens(model_key);
    if wanted.is_empty() {
        return None;
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    let mut scanned: usize = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        scanned += 1;
        if scanned > max_scan_dirs {
            eprintln!(
                "darkmux: gguf content-scan exceeded {max_scan_dirs} dirs; \
                 leaving '{model_key}' unresolved for the GGUF header reader"
            );
            return None;
        }
        let mut has_gguf = false;
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf")) {
                has_gguf = true;
            }
        }
        if has_gguf {
            if let Some(base) = dir.file_name().and_then(|n| n.to_str()) {
                if wanted.is_subset(&tokenize(base)) {
                    matches.push(dir.clone());
                }
            }
        }
    }
    match matches.as_slice() {
        [only] => read_gguf_dir(only),
        _ => None,
    }
}

// ── the header parser ───────────────────────────────────────────────────

/// Opens `path` and parses its GGUF metadata header. All the actual parsing
/// logic lives in [`parse_gguf_header_from_reader`] over a generic
/// `Read + Seek`, so tests exercise it against small in-memory buffers —
/// never a real multi-gigabyte download.
fn parse_gguf_header_file(path: &Path) -> Option<ArchFactsRaw> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    parse_gguf_header_from_reader(&mut reader)
}

/// Fields this reader extracts from the metadata KV block, collected by
/// KEY SUFFIX regardless of the `<arch>.` prefix (`phi3.block_count`,
/// `llama.block_count`, `qwen3.block_count`, … all match `.block_count`) —
/// so the parser needs no special-casing per architecture family, and no
/// dependency on `general.architecture` appearing before the numeric
/// fields in file order (GGUF does not guarantee KV ordering).
#[derive(Default)]
struct RawFields {
    /// Presence-only: proves this file's metadata is a real model header,
    /// not just four bytes of magic that happened to match.
    has_architecture: bool,
    block_count: Option<u64>,
    head_count: Option<u64>,
    head_count_kv: Option<u64>,
    embedding_length: Option<u64>,
    /// `<arch>.attention.key_length` — when present, the head dimension
    /// DIRECTLY (this is what it means in every architecture that emits
    /// it); preferred over the `embedding_length / head_count` derivation.
    key_length: Option<u64>,
}

/// The pure parsing core: magic → version → counts → metadata KV loop →
/// `ArchFactsRaw`. Never reads past the metadata KV block (the tensor-info
/// array and all tensor/weight data that follow are untouched). Any
/// short read, any count past its named ceiling, or any missing/implausible
/// required field degrades to `None` — never a panic, never an unbounded
/// loop (see the module docs' named ceilings).
fn parse_gguf_header_from_reader<R: Read + Seek>(r: &mut R) -> Option<ArchFactsRaw> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).ok()?;
    if &magic != b"GGUF" {
        return None;
    }
    let version = read_u32(r)?;
    if version != 2 && version != 3 {
        // Named limitation (module docs): v1's differing wire format is not
        // implemented; a version this reader doesn't recognize at all is
        // treated the same way — refuse rather than guess a layout.
        return None;
    }
    let _tensor_count = read_u64(r)?; // read to advance the cursor correctly; never used
    let kv_count = read_u64(r)?;
    if kv_count > MAX_KV_COUNT {
        return None;
    }

    let mut fields = RawFields::default();
    for _ in 0..kv_count {
        let key = read_gguf_string_bounded(r, MAX_KEY_LEN)?;
        let value_type = read_u32(r)?;
        match value_type {
            T_STRING => {
                if key == "general.architecture" {
                    let arch = read_gguf_string_bounded(r, MAX_ARCH_STRING_LEN)?;
                    fields.has_architecture = !arch.is_empty();
                } else {
                    skip_gguf_string(r)?;
                }
            }
            T_ARRAY => {
                skip_gguf_array(r)?;
            }
            _ => {
                let value = read_gguf_scalar(r, value_type)?;
                if let Some(value) = value {
                    if key.ends_with(".attention.head_count_kv") {
                        fields.head_count_kv = Some(value);
                    } else if key.ends_with(".attention.head_count") {
                        fields.head_count = Some(value);
                    } else if key.ends_with(".attention.key_length") {
                        fields.key_length = Some(value);
                    } else if key.ends_with(".block_count") {
                        fields.block_count = Some(value);
                    } else if key.ends_with(".embedding_length") {
                        fields.embedding_length = Some(value);
                    }
                }
            }
        }
    }

    if !fields.has_architecture {
        return None;
    }
    let block_count = fields.block_count?;
    let head_count_kv = fields.head_count_kv?;
    let head_dim = match fields.key_length {
        Some(k) => k,
        None => {
            let embedding_length = fields.embedding_length?;
            let head_count = fields.head_count?;
            if head_count == 0 {
                return None;
            }
            embedding_length / head_count
        }
    };

    if block_count == 0
        || block_count > MAX_PLAUSIBLE_COUNT
        || head_count_kv == 0
        || head_count_kv > MAX_PLAUSIBLE_COUNT
        || head_dim == 0
        || head_dim > MAX_PLAUSIBLE_HEAD_DIM
    {
        // Refuse an implausible reading rather than trust it blindly — see
        // MAX_PLAUSIBLE_COUNT's doc for why (also closes off a downstream
        // multiplication-overflow risk in `ArchFacts::kv_per_token`).
        return None;
    }

    Some(ArchFactsRaw {
        num_hidden_layers: block_count,
        num_key_value_heads: head_count_kv,
        head_dim,
        // GGUF's KV metadata carries no per-layer attention-pattern field
        // the way an HF `config.json`'s `layer_types` does — dense is the
        // documented default (module docs' "Named limitation" section).
        full_attention_layers: block_count,
        // Unused downstream: `model_ledger::arch_facts_v1` never reads this
        // field (the KV-cache dtype width is the fixed v1 fp16 default, not
        // derived from weight quantization). GGUF's own quantization signal
        // (`general.file_type`) is a llama.cpp quant-SCHEME enum, not a bit
        // width, and mapping one to the other would add a lookup table with
        // no consumer — 0 here is an honest "not derived", not a guess.
        quantization_bits: 0,
    })
}

fn read_u32<R: Read>(r: &mut R) -> Option<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).ok()?;
    Some(u32::from_le_bytes(buf))
}

fn read_u64<R: Read>(r: &mut R) -> Option<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).ok()?;
    Some(u64::from_le_bytes(buf))
}

/// Reads a `gguf_string_t` (`u64` length + UTF-8 bytes, NOT
/// null-terminated) and materializes it — used ONLY for
/// `general.architecture`, the one string value this reader ever needs.
/// `max_len` bounds the allocation; a length claim past it degrades to
/// `None` rather than allocate an attacker-chosen amount.
fn read_gguf_string_bounded<R: Read>(r: &mut R, max_len: u64) -> Option<String> {
    let len = read_u64(r)?;
    if len > max_len {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Reads a `gguf_string_t`'s length prefix and seeks PAST its bytes without
/// materializing them — the memory-bounded path for every string value this
/// reader doesn't need (chat templates, tokenizer strings, …). A seek past
/// EOF is harmless on a real file (the next `read_exact` simply fails), so
/// this needs no upper bound on `len` beyond the caller's own ceilings on
/// how many times it gets called (`MAX_KV_COUNT` / `MAX_ARRAY_LEN`).
fn skip_gguf_string<R: Read + Seek>(r: &mut R) -> Option<()> {
    let len = read_u64(r)?;
    r.seek(SeekFrom::Current(i64::try_from(len).ok()?)).ok()?;
    Some(())
}

/// Reads an array's `(element_type, element_count)` header and skips its
/// contents entirely — this reader never needs an array VALUE, only the
/// scalar fields that sit alongside them in the KV block. Fixed-width
/// element types skip in one seek; string-element arrays (the tokenizer
/// vocab/merges lists) skip element-by-element, bounded by
/// [`MAX_ARRAY_LEN`]. Nested arrays are a named non-goal (see the doc on
/// [`T_ARRAY`]'s handling below) — no real GGUF writer emits them.
fn skip_gguf_array<R: Read + Seek>(r: &mut R) -> Option<()> {
    let elem_type = read_u32(r)?;
    let len = read_u64(r)?;
    if len > MAX_ARRAY_LEN {
        return None;
    }
    if elem_type == T_STRING {
        for _ in 0..len {
            skip_gguf_string(r)?;
        }
        return Some(());
    }
    if elem_type == T_ARRAY {
        // Not implemented (module docs) — no known real-world GGUF emits a
        // nested array, and guessing a byte layout for one is worse than
        // declining.
        return None;
    }
    let elem_size = scalar_byte_width(elem_type)?;
    let total = elem_size.checked_mul(len)?;
    r.seek(SeekFrom::Current(i64::try_from(total).ok()?)).ok()?;
    Some(())
}

fn scalar_byte_width(value_type: u32) -> Option<u64> {
    match value_type {
        T_UINT8 | T_INT8 | T_BOOL => Some(1),
        T_UINT16 | T_INT16 => Some(2),
        T_UINT32 | T_INT32 | T_FLOAT32 => Some(4),
        T_UINT64 | T_INT64 | T_FLOAT64 => Some(8),
        _ => None,
    }
}

/// Reads one scalar value of `value_type`, ALWAYS consuming the correct
/// number of bytes (so the cursor stays correctly positioned for the next
/// KV pair even for a type this reader doesn't care about) and widening
/// integer types to `u64` — `Some(Some(value))` for an integer type,
/// `Some(None)` for a float/bool value (consumed, not needed), `None` for
/// an unrecognized type code (no declared width to read — malformed).
fn read_gguf_scalar<R: Read>(r: &mut R, value_type: u32) -> Option<Option<u64>> {
    match value_type {
        T_UINT8 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).ok()?;
            Some(Some(b[0] as u64))
        }
        T_INT8 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).ok()?;
            Some(Some(b[0] as i8 as i64 as u64))
        }
        T_UINT16 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).ok()?;
            Some(Some(u16::from_le_bytes(b) as u64))
        }
        T_INT16 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).ok()?;
            Some(Some(i16::from_le_bytes(b) as i64 as u64))
        }
        T_UINT32 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b).ok()?;
            Some(Some(u32::from_le_bytes(b) as u64))
        }
        T_INT32 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b).ok()?;
            Some(Some(i32::from_le_bytes(b) as i64 as u64))
        }
        T_FLOAT32 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b).ok()?;
            Some(None)
        }
        T_BOOL => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b).ok()?;
            Some(None)
        }
        T_UINT64 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).ok()?;
            Some(Some(u64::from_le_bytes(b)))
        }
        T_INT64 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).ok()?;
            Some(Some(i64::from_le_bytes(b) as u64))
        }
        T_FLOAT64 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).ok()?;
            Some(None)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    // ── synthetic GGUF byte builder (never a real/committed .gguf file) ──

    enum V {
        Str(&'static str),
        U32(u32),
        F32(f32),
        StrArray(Vec<&'static str>),
        U32Array(Vec<u32>),
    }

    fn write_string(buf: &mut Vec<u8>, s: &str) {
        buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }

    fn write_kv(buf: &mut Vec<u8>, key: &str, val: &V) {
        write_string(buf, key);
        match val {
            V::Str(s) => {
                buf.extend_from_slice(&T_STRING.to_le_bytes());
                write_string(buf, s);
            }
            V::U32(v) => {
                buf.extend_from_slice(&T_UINT32.to_le_bytes());
                buf.extend_from_slice(&v.to_le_bytes());
            }
            V::F32(v) => {
                buf.extend_from_slice(&T_FLOAT32.to_le_bytes());
                buf.extend_from_slice(&v.to_le_bytes());
            }
            V::StrArray(items) => {
                buf.extend_from_slice(&T_ARRAY.to_le_bytes());
                buf.extend_from_slice(&T_STRING.to_le_bytes());
                buf.extend_from_slice(&(items.len() as u64).to_le_bytes());
                for it in items {
                    write_string(buf, it);
                }
            }
            V::U32Array(items) => {
                buf.extend_from_slice(&T_ARRAY.to_le_bytes());
                buf.extend_from_slice(&T_UINT32.to_le_bytes());
                buf.extend_from_slice(&(items.len() as u64).to_le_bytes());
                for it in items {
                    buf.extend_from_slice(&it.to_le_bytes());
                }
            }
        }
    }

    fn build_gguf(version: u32, kvs: &[(&str, V)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GGUF");
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count — never read
        buf.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
        for (k, v) in kvs {
            write_kv(&mut buf, k, v);
        }
        buf
    }

    /// The exact live phi-4 shape (#1819's own motivating trace, re-verified
    /// against the real download in the `#[ignore]` test below): dense
    /// GQA, 40 layers, 10 kv_heads, head_dim 128 via embedding_length(5120)
    /// / head_count(40). Includes the two large tokenizer arrays the real
    /// file carries, so tests that use this fixture also exercise the
    /// array-skip path, not just the scalar path.
    fn phi4_like_kvs() -> Vec<(&'static str, V)> {
        vec![
            ("general.architecture", V::Str("phi3")),
            ("general.name", V::Str("Phi 4")),
            ("phi3.context_length", V::U32(16384)),
            ("phi3.embedding_length", V::U32(5120)),
            ("phi3.feed_forward_length", V::U32(17920)),
            ("phi3.block_count", V::U32(40)),
            ("phi3.attention.head_count", V::U32(40)),
            ("phi3.attention.head_count_kv", V::U32(10)),
            ("phi3.attention.layer_norm_rms_epsilon", V::F32(0.000_01)),
            ("phi3.rope.dimension_count", V::U32(128)),
            ("tokenizer.ggml.tokens", V::StrArray(vec!["<bos>", "<eos>", "a", "b", "c"])),
            ("tokenizer.ggml.token_type", V::U32Array(vec![1, 1, 1, 1, 1])),
        ]
    }

    // ── pure header-parsing tests (never touch a real file) ─────────────

    #[test]
    fn parses_phi4_like_synthetic_header() {
        // Ground truth (#1819 issue body, re-derived from Microsoft's own
        // published config.json): 40 layers, 10 kv_heads, head_dim 128 →
        // kv_per_token = 2*40*10*128*2 = 204_800, matching
        // V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN exactly for this one model.
        let bytes = build_gguf(3, &phi4_like_kvs());
        let mut cursor = Cursor::new(bytes);
        let facts = parse_gguf_header_from_reader(&mut cursor).expect("parses");
        assert_eq!(facts.num_hidden_layers, 40);
        assert_eq!(facts.num_key_value_heads, 10);
        assert_eq!(facts.head_dim, 128, "derived: embedding_length 5120 / head_count 40");
        assert_eq!(facts.full_attention_layers, 40, "dense default: no layer_types in GGUF");
    }

    #[test]
    fn key_length_field_wins_over_derived_head_dim() {
        let mut kvs = phi4_like_kvs();
        kvs.push(("phi3.attention.key_length", V::U32(256)));
        let bytes = build_gguf(3, &kvs);
        let mut cursor = Cursor::new(bytes);
        let facts = parse_gguf_header_from_reader(&mut cursor).expect("parses");
        assert_eq!(facts.head_dim, 256, "key_length is head_dim directly, preferred over embedding/head_count");
    }

    #[test]
    fn version_2_is_accepted() {
        let bytes = build_gguf(2, &phi4_like_kvs());
        let mut cursor = Cursor::new(bytes);
        assert!(parse_gguf_header_from_reader(&mut cursor).is_some());
    }

    #[test]
    fn version_1_is_none_named_limitation() {
        let bytes = build_gguf(1, &phi4_like_kvs());
        let mut cursor = Cursor::new(bytes);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn wrong_magic_is_none_not_a_panic() {
        let mut bytes = build_gguf(3, &phi4_like_kvs());
        bytes[0] = b'X';
        let mut cursor = Cursor::new(bytes);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn empty_file_is_none_not_a_panic() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn truncated_mid_key_string_is_none_not_a_panic() {
        // Precisely targets the KEY-string read (`read_gguf_string_bounded`,
        // called once per KV entry for its key) — a coarser "cut the buffer
        // in half" truncation can land inside a SCALAR value's fixed-width
        // read instead and never exercise this path at all, which a mutant
        // that panics only on the key-read (and nowhere else) would sail
        // through undetected (caught red-handed during #1820 mutation
        // testing: `truncated_mid_metadata_is_none_not_a_panic` below did
        // NOT catch a `.unwrap()` mutation in `read_gguf_string_bounded`
        // because its cut point happened to land in a scalar field
        // instead). Cuts 3 bytes into "general.architecture"'s 21-byte
        // UTF-8 payload, after its 8-byte length prefix has been fully
        // written (so the reader believes 21 bytes are coming and gets
        // only 3).
        let full = build_gguf(3, &phi4_like_kvs());
        const HEADER_LEN: usize = 4 + 4 + 8 + 8; // magic + version + tensor_count + kv_count
        const KEY_LEN_PREFIX: usize = 8; // the first key's own u64 length prefix
        let cut = HEADER_LEN + KEY_LEN_PREFIX + 3;
        let truncated = full[..cut].to_vec();
        let mut cursor = Cursor::new(truncated);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn truncated_mid_metadata_is_none_not_a_panic() {
        let bytes = build_gguf(3, &phi4_like_kvs());
        // Cut off partway through the KV block (well past the header, well
        // before the end) — a realistic "download got interrupted" shape.
        let truncated = bytes[..bytes.len() / 2].to_vec();
        let mut cursor = Cursor::new(truncated);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn truncated_mid_array_skip_is_none_not_a_panic() {
        // Required fields deliberately placed AFTER a large array in file
        // order (GGUF guarantees no KV ordering) — a truncation landing
        // INSIDE the array's skip must prevent the parser from ever
        // reaching them, proving the array-skip path degrades cleanly on a
        // "download got interrupted mid-array" shape rather than silently
        // under-reading (a seek past a truncated file's real EOF does NOT
        // itself error) and continuing as if nothing were missing.
        let kvs = vec![
            ("general.architecture", V::Str("phi3")),
            ("tokenizer.ggml.tokens", V::StrArray(vec!["a"; 50])),
            ("phi3.block_count", V::U32(40)),
            ("phi3.attention.head_count", V::U32(40)),
            ("phi3.attention.head_count_kv", V::U32(10)),
            ("phi3.embedding_length", V::U32(5120)),
        ];
        let full = build_gguf(3, &kvs);
        let architecture_only = build_gguf(3, &kvs[..1]);
        let architecture_and_array = build_gguf(3, &kvs[..2]);
        // Roughly halfway through the array entry: well past its
        // (type, elem_type, len) header, solidly inside the array's string
        // payload — `full` shares this exact byte prefix with
        // `architecture_and_array` since GGUF entries are written
        // sequentially and both start with the identical first two.
        let cut = architecture_only.len()
            + (architecture_and_array.len() - architecture_only.len()) / 2;
        let truncated = full[..cut].to_vec();
        let mut cursor = Cursor::new(truncated);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn missing_architecture_key_is_none() {
        let kvs = vec![
            ("phi3.block_count", V::U32(40)),
            ("phi3.attention.head_count", V::U32(40)),
            ("phi3.attention.head_count_kv", V::U32(10)),
            ("phi3.embedding_length", V::U32(5120)),
        ];
        let bytes = build_gguf(3, &kvs);
        let mut cursor = Cursor::new(bytes);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn missing_head_count_kv_is_none() {
        let kvs = vec![
            ("general.architecture", V::Str("phi3")),
            ("phi3.block_count", V::U32(40)),
            ("phi3.attention.head_count", V::U32(40)),
            ("phi3.embedding_length", V::U32(5120)),
        ];
        let bytes = build_gguf(3, &kvs);
        let mut cursor = Cursor::new(bytes);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn missing_embedding_length_and_key_length_is_none() {
        // Neither key_length nor (embedding_length + head_count) available
        // to derive head_dim from.
        let kvs = vec![
            ("general.architecture", V::Str("phi3")),
            ("phi3.block_count", V::U32(40)),
            ("phi3.attention.head_count_kv", V::U32(10)),
        ];
        let bytes = build_gguf(3, &kvs);
        let mut cursor = Cursor::new(bytes);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn implausible_block_count_is_none_never_trusted_blindly() {
        let mut kvs = phi4_like_kvs();
        for kv in kvs.iter_mut() {
            if kv.0 == "phi3.block_count" {
                kv.1 = V::U32(50_000_000);
            }
        }
        let bytes = build_gguf(3, &kvs);
        let mut cursor = Cursor::new(bytes);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn oversized_kv_count_is_none_without_looping() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GGUF");
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&(MAX_KV_COUNT + 1).to_le_bytes());
        // No further bytes — if the reader tried to loop MAX_KV_COUNT+1
        // times it would immediately fail on the first `read_exact`, but
        // the ceiling check must reject it BEFORE the loop even starts.
        let mut cursor = Cursor::new(buf);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    #[test]
    fn empty_architecture_string_is_none() {
        let kvs = vec![
            ("general.architecture", V::Str("")),
            ("phi3.block_count", V::U32(40)),
            ("phi3.attention.head_count", V::U32(40)),
            ("phi3.attention.head_count_kv", V::U32(10)),
            ("phi3.embedding_length", V::U32(5120)),
        ];
        let bytes = build_gguf(3, &kvs);
        let mut cursor = Cursor::new(bytes);
        assert_eq!(parse_gguf_header_from_reader(&mut cursor), None);
    }

    // ── path resolution (temp-dir fixtures — never real fixtures in git) ─

    static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh, uniquely-named temp directory per test — self-contained
    /// fixture trees built at test time, never committed (this project's
    /// own "do not commit large/binary fixtures" rule; these are a few
    /// hundred bytes each and exist only for the duration of the test
    /// process).
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let n = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("darkmux-gguf-facts-test-{}-{label}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create temp fixture root");
            TempRoot(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_gguf(&self, rel_dir: &str, filename: &str, bytes: &[u8]) {
            let dir = self.0.join(rel_dir);
            std::fs::create_dir_all(&dir).expect("create fixture subdir");
            std::fs::write(dir.join(filename), bytes).expect("write fixture gguf");
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn valid_bytes() -> Vec<u8> {
        build_gguf(3, &phi4_like_kvs())
    }

    #[test]
    fn read_resolves_via_ls_entry_path() {
        let root = TempRoot::new("ls-entry");
        root.write_gguf("test-pub/phi-4-gguf", "phi-4-Q4_K_M.gguf", &valid_bytes());
        let entries = [json!({
            "modelKey": "lmstudio-community/phi-4-gguf",
            "path": "test-pub/phi-4-gguf",
        })];
        let reader = GgufFactsReader::with_root_and_entries(root.path(), &entries);
        let facts = reader.read("lmstudio-community/phi-4-gguf").expect("resolved via entry path");
        assert_eq!(facts.num_hidden_layers, 40);
    }

    #[test]
    fn read_falls_back_to_model_key_as_dir() {
        let root = TempRoot::new("key-as-dir");
        root.write_gguf("some-model", "weights.gguf", &valid_bytes());
        let reader = GgufFactsReader::with_root(root.path());
        let facts = reader.read("some-model").expect("resolved via modelKey-as-dir");
        assert_eq!(facts.num_key_value_heads, 10);
    }

    #[test]
    fn read_picks_the_first_shard_of_a_split_download() {
        let root = TempRoot::new("split");
        let dir = "split-model";
        root.write_gguf(dir, "split-model-00001-of-00002.gguf", &valid_bytes());
        // The second shard: NOT a parseable header on its own (a minimal
        // llama.cpp continuation shard) — proves the reader picked shard 1,
        // not shard 2, not "whichever read_dir happens to return first".
        root.write_gguf(dir, "split-model-00002-of-00002.gguf", b"not a gguf header at all");
        let reader = GgufFactsReader::with_root(root.path());
        let facts = reader.read(dir).expect("resolved via the -00001- shard");
        assert_eq!(facts.num_hidden_layers, 40);
    }

    #[test]
    fn read_declines_ambiguous_multi_file_directory() {
        let root = TempRoot::new("ambiguous");
        let dir = "two-variants";
        root.write_gguf(dir, "variant-a.gguf", &valid_bytes());
        root.write_gguf(dir, "variant-b.gguf", &valid_bytes());
        let reader = GgufFactsReader::with_root(root.path());
        assert_eq!(reader.read(dir), None, "no shard marker to disambiguate — must not guess");
    }

    #[test]
    fn read_absent_model_dir_is_none() {
        let root = TempRoot::new("absent");
        let reader = GgufFactsReader::with_root(root.path());
        assert_eq!(reader.read("nobody/no-such-model"), None);
    }

    #[test]
    fn content_scan_resolves_when_reported_path_is_wrong() {
        // Mirrors arch_facts's devstral case: the ls path points nowhere,
        // but exactly one directory under the root holds a .gguf file whose
        // name tokens superset the model_key's.
        let root = TempRoot::new("content-scan");
        root.write_gguf(
            "mlx-community/Fake-Devstral-Small-2-2512-4bit",
            "devstral.gguf",
            &valid_bytes(),
        );
        let entries = [json!({
            "modelKey": "mistralai/devstral-small-2-2512",
            "path": "mistralai/devstral-small-2-2512",
        })];
        let reader = GgufFactsReader::with_root_and_entries(root.path(), &entries);
        let reported = root.path().join("mistralai/devstral-small-2-2512");
        assert!(pick_gguf_file(&reported).is_none(), "reported path must not resolve directly");
        let facts = reader
            .read("mistralai/devstral-small-2-2512")
            .expect("content-scan fallback resolves by token superset");
        assert_eq!(facts.num_hidden_layers, 40);
    }

    #[test]
    fn content_scan_ambiguous_directory_match_is_none() {
        let root = TempRoot::new("content-scan-ambiguous");
        root.write_gguf("pub/phi-4", "phi-4.gguf", &valid_bytes());
        root.write_gguf("pub/phi-4-mini", "phi-4-mini.gguf", &valid_bytes());
        let reader = GgufFactsReader::with_root(root.path());
        assert_eq!(reader.read("phi-4"), None, "phi-4 tokens subset BOTH phi-4 and phi-4-mini");
    }

    #[test]
    fn non_gguf_directory_is_none() {
        // A directory that exists but holds no .gguf file at all (e.g. an
        // MLX resident, which `read()` should never mistake for GGUF).
        let root = TempRoot::new("no-gguf");
        std::fs::create_dir_all(root.path().join("mlx-dir")).unwrap();
        std::fs::write(root.path().join("mlx-dir/config.json"), b"{}").unwrap();
        let reader = GgufFactsReader::with_root(root.path());
        assert_eq!(reader.read("mlx-dir"), None);
    }

    // ── ground truth: the real file (#1819's own motivating trace) ──────

    /// Not run by default — the real GGUF is a 9 GB local LMStudio download,
    /// never committed to this repo. Run explicitly with:
    /// `cargo test -p darkmux-profiles --lib gguf_facts::tests::real_phi4 -- --ignored --nocapture`
    /// This is the test the #1820 issue explicitly asks for: pinning the
    /// reader's output against the REAL file, not just synthetic fixtures,
    /// and reporting the measured header-read cost.
    #[test]
    #[ignore = "requires a real ~/.lmstudio/models/lmstudio-community/phi-4-GGUF/phi-4-Q4_K_M.gguf download"]
    fn real_phi4_gguf_matches_published_architecture() {
        let path = dirs::home_dir()
            .expect("home dir")
            .join(".lmstudio/models/lmstudio-community/phi-4-GGUF/phi-4-Q4_K_M.gguf");
        assert!(path.exists(), "expected the real phi-4 GGUF at {}", path.display());

        let started = std::time::Instant::now();
        let facts = parse_gguf_header_file(&path).expect("parses the real file");
        let elapsed = started.elapsed();

        // Ground truth (#1819 issue body / #1820 issue body, Microsoft's
        // own published config.json): num_hidden_layers 40,
        // num_attention_heads 40, num_key_value_heads 10, hidden_size 5120
        // → head_dim 128.
        assert_eq!(facts.num_hidden_layers, 40);
        assert_eq!(facts.num_key_value_heads, 10);
        assert_eq!(facts.head_dim, 128);
        assert_eq!(facts.full_attention_layers, 40);

        let kv_per_token =
            2 * facts.full_attention_layers * facts.num_key_value_heads * facts.head_dim * 2;
        assert_eq!(kv_per_token, 204_800, "must match V1_FALLBACK_KV_BYTES_PER_CTX_TOKEN exactly");

        let file_len = std::fs::metadata(&path).expect("stat").len();
        eprintln!(
            "real phi-4 GGUF ({file_len} bytes on disk): header parsed in {elapsed:?}, \
             producing {facts:?}"
        );
    }
}

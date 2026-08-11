//! Audit hash-chain + integrity check (#163).
//!
//! The BLAKE3 chain-of-custody helpers used by `AuditFileSink` (in the
//! crate's sink core) plus the `darkmux flow integrity-check` verb that
//! walks an audit file and reports the first chain divergence. Split out
//! of the crate's sink/record core (#508).
//!
//! # Byte-hash format (#1769)
//!
//! Each record line is `<hash-hex><SP><record-json>`, where `record-json`
//! is the record's own serialization (`prev_hash` included, `hash` never
//! embedded — the chain's hash lives in the line's prefix, not inside the
//! JSON body) and `<hash-hex>` is the BLAKE3 hash of those bytes, VERBATIM.
//!
//! Verification splits each line on the FIRST space and hashes the
//! remainder as raw bytes — there is no `serde_json::from_str` anywhere on
//! the hash-computation path, and there must never be one added. That is
//! the entire fix: the old format hashed a RE-SERIALIZATION of a PARSED
//! `FlowRecord` (`audit_hash_of`'s `record.clone() -> hash = None ->
//! serde_json::to_vec`), so a schema change or an enum spelling this binary
//! didn't recognize changed the recomputed bytes for a record nobody
//! touched (#1768's false positive). To paper over that lossiness, the
//! verifier SKIPPED content verification for any record carrying an
//! unrecognized enum value and trusted its stored hash to bind the chain
//! — which let an attacker flip one enum to garbage and rewrite every
//! other field for free (#1769's false negative, confirmed by execution).
//! Hashing the literal on-disk bytes removes the round trip, which removes
//! the reason the bypass existed, which is why it is deleted rather than
//! papered over again.
//!
//! Pre-2.6.0 audit files were written in the OLD format — a bare JSON
//! object per line, no hash prefix, `hash` embedded as a JSON field. Such
//! a file's header line lacks the `hash_format` marker `AUDIT_HASH_FORMAT`
//! below carries; a reader treats that absence as "legacy, not
//! re-verifiable" (see `integrity_check_file`) rather than attempting to
//! recompute anything from it — recomputing would mean repeating the exact
//! lossy round trip that made #1768/#1769 possible.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::schema::{FlowRecord, FLOW_SCHEMA_VERSION};

/// Marker written into a byte-hashed audit file's schema-header line
/// (#1769). Its presence is how a reader tells a byte-hashed audit file
/// (this module's format — see the module doc) apart from a pre-2.6.0
/// legacy audit file (bare JSON per line, `hash` embedded, hashed via a
/// parse-then-re-serialize round trip). Bump this string if the on-disk
/// shape of the prefix or `record-json` ever changes in a way that isn't
/// itself re-verifiable against the prior value — a reader that doesn't
/// recognize the marker treats the file the same way it treats a legacy
/// one: readable, not re-verifiable, reported honestly.
pub(crate) const AUDIT_HASH_FORMAT: &str = "prefix-blake3-v1";

/// `true` when `s` is a 64-character lowercase-or-uppercase hex string —
/// the shape of a BLAKE3 hex digest. Used to sanity-check a line's prefix
/// before trusting it as a hash, both when recovering the chain's tail on
/// write and when verifying on read: a prefix that doesn't look like a
/// hash means the line is corrupted or foreign content, not a hash to
/// compare against.
fn is_blake3_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Compute the BLAKE3 hash of `record_json`'s bytes, VERBATIM — no parse,
/// no re-serialize. This is the entire content-verification primitive for
/// the byte-hash format: whatever bytes a writer put on disk (or a reader
/// is about to write) are what gets hashed, so the chain's integrity
/// depends only on those literal bytes, never on any binary's struct
/// layout at read time. (#1769)
pub(crate) fn audit_hash_of_bytes(record_json: &[u8]) -> String {
    blake3::hash(record_json).to_hex().to_string()
}

/// Compute the BLAKE3 hash of a record's canonical form for the WRITE
/// path, where "canonical form" is simply "the bytes this call is about
/// to serialize" — there is no disk round trip involved here, so this is
/// safe in a way re-deriving it during verification is not (see the
/// module doc). The `hash` field is excluded (cloning the record and
/// setting `hash = None` before serializing) so the chain doesn't
/// self-reference; `prev_hash` IS included — that's what binds the chain.
pub(crate) fn audit_hash_of(record: &FlowRecord) -> Result<String> {
    let mut to_hash = record.clone();
    to_hash.hash = None;
    let bytes = serde_json::to_vec(&to_hash).context("serializing record for hash")?;
    Ok(audit_hash_of_bytes(&bytes))
}

/// Hash of the schema-header line — the chain's deterministic seed. Used
/// as `prev_hash` for the first record in a fresh audit file so the
/// chain starts with a well-defined value rather than `None`. Byte-based
/// already (hashes the header line's own bytes), so this needed no change
/// for #1769 — the header was never round-tripped through a struct.
pub(crate) fn audit_seed_hash(header_line: &str) -> String {
    blake3::hash(header_line.as_bytes()).to_hex().to_string()
}

/// Append `record` to the audit file at `path`, populating `prev_hash`
/// and writing the byte-hash prefix from the existing chain. Cross-process
/// safe via `flock(2)` so concurrent CLI sessions writing the same file
/// serialize correctly. POSIX-only.
///
/// Atomicity model:
///
///   1. Acquire exclusive flock on the file (creating it if absent).
///   2. Read the last record (or the schema header for an empty file)
///      to recover the chain's current tail hash.
///   3. Populate `prev_hash` on a clone of the input; serialize; hash the
///      exact serialized bytes; prefix the line with that hash.
///   4. Append the line.
///   5. Drop the file → flock released.
///
/// First-write-into-new-file emits the schema header AND the first
/// record under the same lock so an interrupt can't leave a header-only
/// file with no chain seed visible.
#[cfg(unix)]
pub(crate) fn audit_record_at(record: &FlowRecord, path: &Path) -> Result<()> {
    darkmux_types::flock::with_locked_file(path, |file| {
        audit_record_at_locked(record, path, file)
    })
}

/// The locked transaction body of [`audit_record_at`] — reads the chain's
/// current tail hash from `file` (the SAME file `flock`'s held on, opened
/// by the shared `darkmux_types::flock::with_locked_file` helper) and
/// appends the new record. Split out so the lock acquisition (now shared
/// with `darkmux-lab`'s registry lock and `darkmux-fleet`'s roster lock)
/// stays separate from this crate's own read/parse/append logic.
#[cfg(unix)]
fn audit_record_at_locked(record: &FlowRecord, path: &Path, file: &mut std::fs::File) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write as _};
    let mut contents = String::new();
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek to start of {}", path.display()))?;
    file.read_to_string(&mut contents)
        .with_context(|| format!("reading audit log {}", path.display()))?;

    let (prev_hash, write_header) = if contents.is_empty() {
        // Fresh file — the seed hash binds the chain to the schema header
        // we're about to write.
        let header = audit_schema_header_line()?;
        let seed = audit_seed_hash(&header);
        (seed, Some(header))
    } else {
        let non_empty: Vec<&str> =
            contents.lines().filter(|l| !l.trim().is_empty()).collect();
        if non_empty.is_empty() {
            // File exists but trims to nothing (whitespace-only) — treat as fresh.
            let header = audit_schema_header_line()?;
            (audit_seed_hash(&header), Some(header))
        } else if non_empty.len() == 1 {
            // Recover ONLY when the sole surviving line is genuinely a
            // byte-hash-format schema header (process/OS crash between
            // header write and the first record).
            //
            // (#899) This recovery MUST require `_type == "schema"`.
            // Otherwise truncating a multi-record log down to one
            // fabricated non-header line would re-seed a fresh, clean-
            // validating chain on the next write — silently laundering
            // tampering.
            //
            // (#1769) The format check folds into the same guard: a
            // legacy (pre-2.6.0) header also cannot seed a byte-hash
            // chain, and this binary has no way to tell "a genuine
            // legacy file, needs rotation" apart from "a real file
            // truncated to one fabricated line" — so both refuse, with
            // the same fail-closed posture #899 already established.
            let last_line = non_empty[0];
            let is_schema_header = serde_json::from_str::<serde_json::Value>(last_line)
                .ok()
                .and_then(|v| v.get("_type").and_then(|t| t.as_str()).map(str::to_string))
                .as_deref()
                == Some("schema");
            if is_schema_header && header_is_byte_hash_format(last_line) {
                (audit_seed_hash(last_line), None)
            } else {
                return Err(anyhow::anyhow!(
                    "audit log {} cannot be safely extended: its sole surviving line is not a \
                     byte-hash-format schema header (#1769) — refusing to re-seed a chain from \
                     unverified content. If this is a genuine pre-2.6.0 legacy file, rotate it \
                     (move/rename so a fresh chain can start); if the file was truncated, this \
                     is the intended fail-closed response.",
                    path.display()
                ));
            }
        } else {
            // Multiple lines present — line 1 must be a byte-hash-format
            // header, or this file predates byte-hash verification and
            // must not be silently extended in the new format (that would
            // produce a file whose header lies about its own shape).
            let header_line = non_empty[0];
            if !header_is_byte_hash_format(header_line) {
                return Err(anyhow::anyhow!(
                    "audit log {} was written in the legacy struct-hash format (pre-2.6.0) and \
                     cannot be safely extended under byte-hash verification (#1769) — rotate \
                     this file (move/rename it) so a fresh chain can start under the new format",
                    path.display()
                ));
            }

            let last_line = *non_empty.last().expect("non_empty has >1 element per this branch");
            // The prefix IS the tail hash — no JSON parse needed to
            // recover it, which is the point of the format.
            match last_line.split_once(' ') {
                Some((hash, _json)) if is_blake3_hex(hash) => (hash.to_string(), None),
                _ => {
                    return Err(anyhow::anyhow!(
                        "audit log {} last line lacks a valid `<hash> <json>` prefix — chain \
                         corrupted (refusing to re-seed)",
                        path.display()
                    ));
                }
            }
        }
    };

    // Build the record to write: stamp prev_hash, never embed hash in the
    // JSON body — the byte-hash format's hash lives in the line prefix, not
    // inside the record. `audit_hash_of` hashes exactly these bytes (it
    // clones, sets `hash = None` — already the case here — and serializes),
    // so calling it here is safe in a way calling it during VERIFY is not:
    // there is no disk round trip between "the record we just built" and
    // "the bytes we're about to hash", so nothing is lost.
    let mut to_write = record.clone();
    to_write.prev_hash = Some(prev_hash);
    to_write.hash = None;
    let record_json = serde_json::to_string(&to_write).context("serializing audit record")?;
    let hash_hex = audit_hash_of(&to_write).context("computing audit hash")?;
    let line = format!("{hash_hex} {record_json}");

    // Append (after seeking to end). flock holds; PIPE_BUF guarantee is
    // belt-and-suspenders for the JSONL line.
    file.seek(SeekFrom::End(0))
        .with_context(|| format!("seek to end of {}", path.display()))?;
    if let Some(header) = write_header {
        file.write_all(header.as_bytes())
            .with_context(|| format!("writing schema header to {}", path.display()))?;
        file.write_all(b"\n")?;
    }
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending record to audit log {}", path.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("syncing audit log {}", path.display()))?;
    Ok(())
}

/// Build the schema header line used by LocalFileSink (via `record_at`).
/// Centralized so casual flow files and audit files carry the same base
/// shape. AuditFileSink uses `audit_schema_header_line` below instead —
/// same base fields, plus the byte-hash format marker LocalFileSink has
/// no reason to carry (it never hashes anything).
pub(crate) fn schema_header_line() -> Result<String> {
    let header = serde_json::json!({
        "_type": "schema",
        "version": FLOW_SCHEMA_VERSION,
        "darkmux_version": env!("CARGO_PKG_VERSION"),
    });
    serde_json::to_string(&header).context("serializing schema header")
}

/// Build the schema header line used by AuditFileSink. Same shape as
/// `schema_header_line()` plus `hash_format` (#1769) — the marker a
/// reader uses to recognize this file as byte-hashed rather than a
/// pre-2.6.0 legacy struct-hashed one. See `header_is_byte_hash_format`.
pub(crate) fn audit_schema_header_line() -> Result<String> {
    let header = serde_json::json!({
        "_type": "schema",
        "version": FLOW_SCHEMA_VERSION,
        "darkmux_version": env!("CARGO_PKG_VERSION"),
        "hash_format": AUDIT_HASH_FORMAT,
    });
    serde_json::to_string(&header).context("serializing audit schema header")
}

/// Extract the `hash_format` field from an audit file's header line, if
/// present. `None` when the header isn't parseable JSON or lacks the
/// field — which is exactly what every pre-2.6.0 (legacy) audit header
/// looks like, since the field didn't exist before #1769.
fn header_hash_format(header_line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(header_line)
        .ok()
        .and_then(|v| v.get("hash_format").and_then(|f| f.as_str()).map(str::to_string))
}

/// `true` when the header names the byte-hash format this binary knows
/// how to verify (`AUDIT_HASH_FORMAT`). `false` for a legacy pre-2.6.0
/// header (field absent) AND for a header naming some OTHER format this
/// binary doesn't recognize (a future format bump) — either way, this
/// binary cannot safely extend or re-verify the chain from here.
fn header_is_byte_hash_format(header_line: &str) -> bool {
    header_hash_format(header_line).as_deref() == Some(AUDIT_HASH_FORMAT)
}

/// Extract the writer's `FLOW_SCHEMA_VERSION` from an audit file's header
/// line, for CONTEXT ONLY — e.g. "written under schema 1.18.0" in a
/// report. Never used to excuse a hash divergence: under byte-hashing a
/// mismatch always means the bytes on disk changed, regardless of which
/// schema version wrote them. `None` when the header isn't parseable or
/// lacks the field.
fn header_schema_version(header_line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(header_line)
        .ok()
        .and_then(|v| v.get("version").and_then(|f| f.as_str()).map(str::to_string))
}

/// Honest, non-accusatory note for a legacy-format file — see
/// `IntegrityReport::legacy_format`.
fn legacy_format_note(hash_format: Option<&str>) -> String {
    match hash_format {
        None => "written in the legacy struct-hash format (pre-2.6.0); not re-verifiable under \
                 byte-hash verification (#1769) — the stored hash was computed over a \
                 re-serialization of the parsed record, which this binary cannot reproduce \
                 byte-for-byte. This is a format boundary, not evidence of editing."
            .to_string(),
        Some(other) => format!(
            "header names hash_format \"{other}\", which this binary does not recognize \
             (expected \"{AUDIT_HASH_FORMAT}\"); not re-verifiable. This is a format boundary, \
             not evidence of editing."
        ),
    }
}

/// Walk a single audit file, recomputing the hash chain and reporting
/// the first divergence (if any). Cheap — sequential read + per-line
/// hash; throughput limited by disk read.
///
/// Content verification NEVER parses JSON: each record line's hash is
/// checked by splitting on the first space and hashing the remainder as
/// raw bytes (see the module doc). JSON parsing is used only to extract
/// `prev_hash` for chain-linkage comparison — a value this binary reads
/// but never feeds back into a hash computation, so it stays lenient
/// (`serde_json::Value`, not the typed `FlowRecord`) without weakening
/// the guarantee.
pub fn integrity_check_file(path: &Path) -> Result<IntegrityReport> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading audit log {}", path.display()))?;
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.is_empty() {
        return Ok(IntegrityReport {
            path: path.display().to_string(),
            records_checked: 0,
            chain_valid: true,
            break_at_line: None,
            break_reason: None,
            legacy_format: false,
            note: None,
            writer_schema_version: None,
        });
    }

    let header_line = lines[0];
    let writer_schema_version = header_schema_version(header_line);

    if !header_is_byte_hash_format(header_line) {
        // (#1769) A legacy (or unrecognized-future) format file. Report
        // honestly — readable, NOT re-verifiable — rather than attempting
        // to recompute anything, which would mean repeating the exact
        // lossy parse -> re-serialize round trip that made #1768/#1769
        // possible in the first place. `chain_valid` stays `true`: this is
        // not evidence of tampering, it's a format boundary, and callers
        // must never fold that into "verified" either — `legacy_format`
        // and `note` carry the honest caveat.
        return Ok(IntegrityReport {
            path: path.display().to_string(),
            records_checked: (lines.len() - 1) as u64,
            chain_valid: true,
            break_at_line: None,
            break_reason: None,
            legacy_format: true,
            note: Some(legacy_format_note(header_hash_format(header_line).as_deref())),
            writer_schema_version,
        });
    }

    // Line 1 is the schema header (no hash); seed the expected prev_hash
    // from its hash so the first record's `prev_hash` should equal it.
    let mut expected_prev = audit_seed_hash(header_line);
    let mut records_checked = 0u64;

    for (idx, line) in lines.iter().enumerate().skip(1) {
        records_checked += 1;

        let Some((stored_hash, record_json)) = line.split_once(' ') else {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64), // 1-indexed
                break_reason: Some(
                    "line has no `<hash> <json>` prefix — not produced by AuditFileSink under \
                     the byte-hash format, or the chain is corrupted"
                        .to_string(),
                ),
                legacy_format: false,
                note: None,
                writer_schema_version,
            });
        };

        if !is_blake3_hex(stored_hash) {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64),
                break_reason: Some(
                    "line's prefix is not a valid BLAKE3 hash — audit log corrupted or contains \
                     foreign content"
                        .to_string(),
                ),
                legacy_format: false,
                note: None,
                writer_schema_version,
            });
        }

        // Chain linkage only — lenient `Value` parse, never fed into a
        // hash computation. See the function doc.
        let parsed: serde_json::Value = match serde_json::from_str(record_json) {
            Ok(v) => v,
            Err(e) => {
                return Ok(IntegrityReport {
                    path: path.display().to_string(),
                    records_checked,
                    chain_valid: false,
                    break_at_line: Some((idx + 1) as u64),
                    break_reason: Some(format!("unparseable JSON: {e}")),
                    legacy_format: false,
                    note: None,
                    writer_schema_version,
                });
            }
        };
        let stored_prev = parsed
            .get("prev_hash")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if stored_prev != expected_prev {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64),
                break_reason: Some(format!(
                    "prev_hash mismatch: stored `{stored_prev}` != expected `{expected_prev}` (audit log has been edited or a write was interleaved)"
                )),
                legacy_format: false,
                note: None,
                writer_schema_version,
            });
        }

        // THE content check. Hash the literal bytes after the first
        // space — no parse, no re-serialize, no struct in between. A
        // mismatch here means the bytes on disk changed, full stop; there
        // is no excused-mismatch path, and there must never be one (#1769).
        let recomputed = audit_hash_of_bytes(record_json.as_bytes());
        if recomputed != stored_hash {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64),
                break_reason: Some(format!(
                    "hash mismatch: stored `{stored_hash}` != recomputed `{recomputed}` (record content has been edited)"
                )),
                legacy_format: false,
                note: None,
                writer_schema_version,
            });
        }

        expected_prev = stored_hash.to_string();
    }

    Ok(IntegrityReport {
        path: path.display().to_string(),
        records_checked,
        chain_valid: true,
        break_at_line: None,
        break_reason: None,
        legacy_format: false,
        note: None,
        writer_schema_version,
    })
}

/// Walk every audit file under `audit_dir()`. Sorted by filename for
/// stable output.
pub fn integrity_check_all() -> Result<Vec<IntegrityReport>> {
    let dir = crate::audit_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(vec![]), // missing dir = nothing to check
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "jsonl")
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    let mut reports = Vec::with_capacity(paths.len());
    for p in paths {
        reports.push(integrity_check_file(&p)?);
    }
    Ok(reports)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityReport {
    pub path: String,
    /// (#906) Number of RECORDS verified — the schema header on line 1 is
    /// NOT counted, so this is a record count, not a file-line count. For
    /// a legacy-format file (`legacy_format == true`) this counts lines
    /// present but NONE of them were actually hash-verified — see `note`.
    pub records_checked: u64,
    pub chain_valid: bool,
    /// (#906) 1-indexed FILE LINE of the break (the header counts as line 1,
    /// so the first record is line 2). Deliberately a file line, not a record
    /// index, so an operator can jump straight to the offending line — but
    /// note it does NOT equal `records_checked` (which excludes the header).
    /// For a break on the first record: `records_checked == 1`, `break_at_line == 2`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub break_at_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub break_reason: Option<String>,
    /// (#1769) True when this file's header does NOT carry the byte-hash
    /// format marker — either a genuine pre-2.6.0 legacy struct-hash file,
    /// or a header naming some other format this binary doesn't recognize.
    /// Such a file is READABLE but its content was NOT hash-verified at
    /// all: recomputing a struct-hash-format hash would repeat the exact
    /// lossy parse -> re-serialize round trip that made #1768 (false
    /// positives) and #1769 (a false-negative bypass) possible, so this
    /// binary does not attempt it.
    ///
    /// `chain_valid` stays `true` for a legacy file — this is a format
    /// boundary, not evidence of tampering — but a caller must not fold
    /// that into "verified" either. `note` carries the honest wording;
    /// `darkmux doctor` reports `Warn`, never `Pass` or `Fail`, for this
    /// case; `darkmux flow integrity-check` keeps exit 0 but prints the
    /// caveat loudly.
    #[serde(default, skip_serializing_if = "is_false")]
    pub legacy_format: bool,
    /// Honest, non-accusatory explanation — set for a legacy-format file
    /// (see `legacy_format`'s doc for the wording). `None` for a normally
    /// verified or normally broken byte-hash-format file; there is no
    /// "excused mismatch" case that sets this alongside `chain_valid ==
    /// false` — under byte-hashing a mismatch always means tampering, full
    /// stop, and is never paired with an excuse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The `FLOW_SCHEMA_VERSION` the file's header names as its writer,
    /// for CONTEXT ONLY (e.g. "written under schema 1.18.0" in a human
    /// report) — never used to excuse a divergence. `None` when the
    /// header is missing or unparseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_schema_version: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

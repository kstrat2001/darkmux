//! Audit hash-chain + integrity check (#163).
//!
//! The BLAKE3 chain-of-custody helpers used by `AuditFileSink` (in the
//! crate's sink core) plus the `darkmux flow integrity-check` verb that
//! walks an audit file and reports the first chain divergence. Split out
//! of the crate's sink/record core (#508).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::schema::{FlowRecord, FLOW_SCHEMA_VERSION};

/// Compute the BLAKE3 hash of a record's canonical form. The `hash` field
/// is intentionally excluded (cloning the record and setting `hash =
/// None` before serializing) so the chain doesn't self-reference. The
/// `prev_hash` field IS included — that's what binds each record to the
/// chain.
pub(crate) fn audit_hash_of(record: &FlowRecord) -> Result<String> {
    let mut to_hash = record.clone();
    to_hash.hash = None;
    let bytes = serde_json::to_vec(&to_hash).context("serializing record for hash")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Hash of the schema-header line — the chain's deterministic seed. Used
/// as `prev_hash` for the first record in a fresh audit file so the
/// chain starts with a well-defined value rather than `None`.
pub(crate) fn audit_seed_hash(header_line: &str) -> String {
    blake3::hash(header_line.as_bytes()).to_hex().to_string()
}

/// Append `record` to the audit file at `path`, populating `prev_hash`
/// and `hash` from the existing chain. Cross-process safe via `flock(2)`
/// so concurrent CLI sessions writing the same file serialize correctly.
/// POSIX-only.
///
/// Atomicity model:
///
///   1. Acquire exclusive flock on the file (creating it if absent).
///   2. Read the last record (or the schema header for an empty file)
///      to recover the chain's current tail hash.
///   3. Populate `prev_hash` + recompute `hash` on a clone of the input.
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
        let header = schema_header_line()?;
        let seed = audit_seed_hash(&header);
        (seed, Some(header))
    } else {
        // Existing file — find the last non-empty line.
        let non_empty: Vec<&str> =
            contents.lines().filter(|l| !l.trim().is_empty()).collect();
        if non_empty.is_empty() {
            // File exists but trims to nothing (whitespace-only) — treat as fresh.
            let header = schema_header_line()?;
            (audit_seed_hash(&header), Some(header))
        } else {
            let last_line = *non_empty.last().expect("non_empty is not empty per check above");
            // Parse the last line. Unparseable = chain corrupted.
            let parsed: serde_json::Value = serde_json::from_str(last_line).map_err(|e| {
                anyhow::anyhow!(
                    "audit log {} last line is unparseable JSON: {e}",
                    path.display()
                )
            })?;
            let last_hash = match parsed.get("hash").and_then(|h| h.as_str()) {
                Some(h) => h.to_string(),
                None => {
                    // No `hash` field on the last line. Recover ONLY when the
                    // sole surviving line is genuinely the schema header
                    // (process/OS crash between header write and the first
                    // record — the within-process atomicity comment above
                    // protects only same-process interrupts).
                    //
                    // (#899) The recovery MUST require `_type == "schema"`.
                    // Otherwise truncating a multi-record log down to one
                    // fabricated non-header line would re-seed a fresh,
                    // clean-validating chain on the next write — silently
                    // laundering tampering. Any other shape (a single
                    // non-header line, or multiple lines whose last lacks
                    // `hash`) means the chain can't continue — bail loudly.
                    let is_schema_header =
                        parsed.get("_type").and_then(|t| t.as_str()) == Some("schema");
                    if non_empty.len() == 1 && is_schema_header {
                        audit_seed_hash(last_line)
                    } else {
                        return Err(anyhow::anyhow!(
                            "audit log {} last line lacks `hash` field and is not the schema \
                             header — chain corrupted (refusing to re-seed)",
                            path.display()
                        ));
                    }
                }
            };
            (last_hash, None)
        }
    };

    // Build the record to write: stamp prev_hash, recompute hash.
    let mut to_write = record.clone();
    to_write.prev_hash = Some(prev_hash);
    to_write.hash = None;
    let hash = audit_hash_of(&to_write).context("computing audit hash")?;
    to_write.hash = Some(hash);

    let line = serde_json::to_string(&to_write).context("serializing audit record")?;

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

/// Build the schema header line used by both LocalFileSink (via
/// `record_at`) and AuditFileSink. Centralized so the two sinks emit
/// byte-identical headers — the audit seed hash is then stable across
/// sink kinds, and any future reader can recognize the line via
/// `_type: "schema"`.
pub(crate) fn schema_header_line() -> Result<String> {
    let header = serde_json::json!({
        "_type": "schema",
        "version": FLOW_SCHEMA_VERSION,
        "darkmux_version": env!("CARGO_PKG_VERSION"),
    });
    serde_json::to_string(&header).context("serializing schema header")
}

/// Walk a single audit file, recomputing the hash chain and reporting
/// the first divergence (if any). Cheap — sequential read + per-line
/// hash; throughput limited by disk read.
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
        });
    }

    // Line 1 is the schema header (no hash); seed the expected prev_hash
    // from its hash so the first record's `prev_hash` should equal it.
    let header_line = lines[0];
    let mut expected_prev = audit_seed_hash(header_line);
    let mut records_checked = 0u64;

    for (idx, line) in lines.iter().enumerate().skip(1) {
        records_checked += 1;
        let rec: FlowRecord = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Ok(IntegrityReport {
                    path: path.display().to_string(),
                    records_checked,
                    chain_valid: false,
                    break_at_line: Some((idx + 1) as u64), // 1-indexed
                    break_reason: Some(format!("unparseable JSON: {e}")),
                });
            }
        };

        let stored_prev = rec.prev_hash.clone().unwrap_or_default();
        if stored_prev != expected_prev {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64),
                break_reason: Some(format!(
                    "prev_hash mismatch: stored `{stored_prev}` != expected `{expected_prev}` (audit log has been edited or a write was interleaved)"
                )),
            });
        }

        let stored_hash = match rec.hash.clone() {
            Some(h) => h,
            None => {
                return Ok(IntegrityReport {
                    path: path.display().to_string(),
                    records_checked,
                    chain_valid: false,
                    break_at_line: Some((idx + 1) as u64),
                    break_reason: Some(
                        "record lacks `hash` field — not produced by AuditFileSink, or chain is corrupted".to_string(),
                    ),
                });
            }
        };

        let recomputed = audit_hash_of(&rec).context("recomputing audit hash")?;
        if recomputed != stored_hash {
            return Ok(IntegrityReport {
                path: path.display().to_string(),
                records_checked,
                chain_valid: false,
                break_at_line: Some((idx + 1) as u64),
                break_reason: Some(format!(
                    "hash mismatch: stored `{stored_hash}` != recomputed `{recomputed}` (record content has been edited)"
                )),
            });
        }

        expected_prev = stored_hash;
    }

    Ok(IntegrityReport {
        path: path.display().to_string(),
        records_checked,
        chain_valid: true,
        break_at_line: None,
        break_reason: None,
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
    /// NOT counted, so this is a record count, not a file-line count.
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
}

//! `darkmux flow status` health subsystem (#170).
//!
//! `darkmux flow status` and the doctor's `flow-sink-health` check both
//! read from `collect_status()`. The single collector ensures the CLI
//! surface and the doctor never drift — same probes, same data shape.
//!
//! Side effects: opens a Redis connection when Redis is configured (so
//! the operator gets accurate reachability + XLEN data). Disk probes are
//! read-only file I/O. No record writes.

use serde::{Deserialize, Serialize};
use std::fs;

use crate::schema::{flows_dir, FLOW_SCHEMA_VERSION};
use crate::{
    default_sink_info, open_redis_connection_bounded, RawRedisUrl, SinkInfo, REDIS_CONNECT_TIMEOUT,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStatus {
    pub schema_version: String,
    pub sinks: SinkSummary,
    /// Present when Redis is configured (via `DARKMUX_REDIS_URL` env
    /// or appearing in the sink graph); `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis: Option<RedisStatus>,
    pub disk: DiskStatus,
    pub schema: SchemaSkew,
    pub overall_state: HealthState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warn_reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fail_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkSummary {
    pub info: SinkInfo,
    /// Flat list of active leaf sink kinds — e.g., `["LocalFile", "Redis"]`.
    pub active_kinds: Vec<String>,
    /// Human-readable composition string — e.g., `Tee([LocalFile, Redis])`.
    pub composition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisStatus {
    pub url: String,
    pub stream: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_len: Option<usize>,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachability_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xlen: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probe_ms: Option<u128>,
    /// True when XLEN is within 5% of MAXLEN — warns the operator the
    /// stream is about to start trimming old records.
    pub near_max_len: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskStatus {
    pub flows_dir: String,
    pub exists: bool,
    pub day_files: u64,
    pub total_bytes: u64,
    /// Distinct schema versions observed in day files (header line of
    /// each `YYYY-MM-DD.jsonl`). Skew detection cross-references this
    /// with `SchemaSkew.observed_versions` (which probes Redis).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_disk_schemas: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSkew {
    pub writer_version: String,
    /// Distinct schema strings observed in the active Redis stream
    /// (best-effort XREVRANGE of the last N entries). Empty when no
    /// Redis is configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_versions: Vec<String>,
    pub skew_detected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    Ok,
    Warn,
    Fail,
}

/// Build a status snapshot. Cheap: ~10ms when Redis is reachable, sub-ms
/// when it isn't. Safe to call from CLI + doctor + daemon endpoint without
/// concern for throughput — the result is meant to be consumed by humans
/// or by a polling UI (every 30s+).
pub fn collect_status() -> FlowStatus {
    let info = default_sink_info();
    let (active_kinds, composition) = summarize_sink(&info);
    let redis_cfg = find_redis_cfg(&info);

    let (redis, redis_observed) = if let Some(cfg) = redis_cfg.clone() {
        let (status, observed) = probe_redis(&cfg);
        (Some(status), observed)
    } else {
        (None, vec![])
    };

    let disk = probe_disk();

    let mut warn_reasons = Vec::new();
    let mut fail_reasons = Vec::new();

    // Skew detection: ONLY Redis-observed schemas count as "live writers".
    // Disk-header schemas from older day files are historical artifacts of
    // earlier writer versions and SHOULD NOT trigger skew warnings on every
    // run — that would mean every operator who's been on darkmux >1 schema
    // bump sees a permanent warn. The Redis stream, by contrast, reflects
    // currently-active writers in the fleet.
    //
    // The disk-schemas data is still surfaced (in DiskStatus.observed_disk_schemas
    // and SchemaSkew.observed_versions) for diagnostic transparency, but
    // doesn't gate the warn_reasons rollup.
    let mut all_versions: Vec<String> = disk
        .observed_disk_schemas
        .iter()
        .chain(redis_observed.iter())
        .cloned()
        .collect();
    all_versions.sort();
    all_versions.dedup();
    let live_foreign: Vec<String> = redis_observed
        .iter()
        .filter(|v| v.as_str() != FLOW_SCHEMA_VERSION)
        .cloned()
        .collect();
    let skew_detected = !live_foreign.is_empty();
    let skew_reason = if skew_detected {
        Some(format!(
            "writer is {} but live Redis stream shows {} — at least one other writer in the fleet is on a different schema",
            FLOW_SCHEMA_VERSION,
            live_foreign.join(", ")
        ))
    } else {
        None
    };
    if skew_detected {
        warn_reasons.push("schema_skew_detected".to_string());
    }

    if let Some(r) = redis.as_ref() {
        if !r.reachable {
            warn_reasons.push("redis_unreachable".to_string());
        }
        if r.near_max_len {
            warn_reasons.push("redis_stream_near_maxlen".to_string());
        }
    }

    if !disk.exists {
        // Disk dir absent isn't fatal — first-write creates it — but the
        // operator should know they have no flows yet.
        warn_reasons.push("flows_dir_absent".to_string());
    }

    // Total sink unreachability: no active sinks (shouldn't happen — at
    // minimum LocalFile is always available — but guard anyway).
    if active_kinds.is_empty() {
        fail_reasons.push("no_active_sinks".to_string());
    }

    let overall_state = if !fail_reasons.is_empty() {
        HealthState::Fail
    } else if !warn_reasons.is_empty() {
        HealthState::Warn
    } else {
        HealthState::Ok
    };

    FlowStatus {
        schema_version: FLOW_SCHEMA_VERSION.to_string(),
        sinks: SinkSummary { info, active_kinds, composition },
        redis,
        disk,
        schema: SchemaSkew {
            writer_version: FLOW_SCHEMA_VERSION.to_string(),
            observed_versions: all_versions,
            skew_detected,
            skew_reason,
        },
        overall_state,
        warn_reasons,
        fail_reasons,
    }
}

/// Flat list of leaf kinds + composition string for a sink tree.
pub(crate) fn summarize_sink(info: &SinkInfo) -> (Vec<String>, String) {
    fn walk_kinds(info: &SinkInfo, out: &mut Vec<String>) {
        if info.children.is_empty() {
            out.push(info.kind.to_string());
        } else {
            for child in &info.children {
                walk_kinds(child, out);
            }
        }
    }
    fn walk_composition(info: &SinkInfo) -> String {
        if info.children.is_empty() {
            info.kind.to_string()
        } else {
            let inner: Vec<String> = info.children.iter().map(walk_composition).collect();
            format!("{}([{}])", info.kind, inner.join(", "))
        }
    }
    let mut kinds = Vec::new();
    walk_kinds(info, &mut kinds);
    (kinds, walk_composition(info))
}

/// Redis config extracted from a SinkInfo tree.
#[derive(Debug, Clone)]
pub(crate) struct RedisCfg {
    pub(crate) url: RawRedisUrl,
    pub(crate) stream: String,
    pub(crate) max_len: Option<usize>,
}

pub(crate) fn find_redis_cfg(info: &SinkInfo) -> Option<RedisCfg> {
    if info.kind == "Redis" {
        // The raw URL — needed for `redis::Client::open` in `probe_redis`
        // — lives on `SinkInfo.raw_url`, NOT `config["url"]`. The latter
        // is the redacted display form. A Redis sink without a populated
        // `raw_url` is unusable for probing, so treat it as absent. (#216)
        let raw_url = info.raw_url.clone()?;
        return Some(RedisCfg {
            url: RawRedisUrl::new(raw_url),
            stream: info.config.get("stream").cloned().unwrap_or_default(),
            max_len: info
                .config
                .get("max_len")
                .and_then(|s| s.parse::<usize>().ok()),
        });
    }
    info.children.iter().find_map(find_redis_cfg)
}

/// Redact `:password@` in a Redis URL for diagnostic display. Operators
/// who put credentials in `DARKMUX_REDIS_URL` shouldn't have those creds
/// echoed back through `darkmux flow status` (which is exposed via the
/// daemon's permissive-CORS endpoint and shown in the browser modal).
/// (#170 QA Q7)
///
/// Conservative: anything between the scheme and the host that contains
/// `@` is treated as `<userinfo>@`; the password portion (after the first
/// `:` in userinfo) is replaced with `***`. URLs without an `@` are
/// returned unchanged.
pub fn redact_url_creds(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return url.to_string();
    };
    let masked_userinfo = if let Some((user, _pass)) = userinfo.split_once(':') {
        format!("{user}:***")
    } else {
        // username only, no password — still keep the username visible.
        userinfo.to_string()
    };
    format!("{scheme}://{masked_userinfo}@{host}")
}

/// Probe Redis: open a connection, run XLEN + XREVRANGE for oldest/newest,
/// time the round-trip. Returns the status + the list of distinct schema
/// strings observed in the last 100 entries (for skew detection).
pub(crate) fn probe_redis(cfg: &RedisCfg) -> (RedisStatus, Vec<String>) {
    let start = std::time::Instant::now();
    let client = match redis::Client::open(cfg.url.expose_for_probe()) {
        Ok(c) => c,
        Err(e) => {
            return (
                RedisStatus {
                    url: cfg.url.to_string(),
                    stream: cfg.stream.clone(),
                    max_len: cfg.max_len,
                    reachable: false,
                    reachability_error: Some(format!("client open: {e}")),
                    xlen: None,
                    oldest_ts: None,
                    newest_ts: None,
                    last_probe_ms: None,
                    near_max_len: false,
                },
                vec![],
            );
        }
    };

    // Bounded by REDIS_CONNECT_TIMEOUT (#278) — a silent-at-TCP-layer
    // OR accept-but-don't-respond peer must not wedge the doctor.
    // Uses the wall-clock-bounded wrapper, not just redis-rs's TCP-
    // connect timeout (which doesn't cover the post-connect handshake
    // hang the Studio-offline scenario can trigger).
    let mut conn = match open_redis_connection_bounded(&client, REDIS_CONNECT_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            return (
                RedisStatus {
                    url: cfg.url.to_string(),
                    stream: cfg.stream.clone(),
                    max_len: cfg.max_len,
                    reachable: false,
                    reachability_error: Some(format!("connect: {e}")),
                    xlen: None,
                    oldest_ts: None,
                    newest_ts: None,
                    last_probe_ms: None,
                    near_max_len: false,
                },
                vec![],
            );
        }
    };

    let xlen_res: redis::RedisResult<u64> = redis::cmd("XLEN").arg(&cfg.stream).query(&mut conn);
    let xlen = xlen_res.ok();

    // XINFO STREAM <key> would give first-entry / last-entry IDs in one
    // shot, but parsing its mixed-array response across redis-rs versions
    // is fragile. XRANGE/XREVRANGE with COUNT 1 is unambiguous.
    let oldest_id: Option<String> = redis::cmd("XRANGE")
        .arg(&cfg.stream)
        .arg("-")
        .arg("+")
        .arg("COUNT")
        .arg(1)
        .query::<Vec<(String, Vec<(String, String)>)>>(&mut conn)
        .ok()
        .and_then(|v| v.into_iter().next().map(|(id, _)| id));
    let (newest_id, schemas) = redis::cmd("XREVRANGE")
        .arg(&cfg.stream)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(100)
        .query::<Vec<(String, Vec<(String, String)>)>>(&mut conn)
        .map(|entries| {
            let newest = entries.first().map(|(id, _)| id.clone());
            let schemas: Vec<String> = entries
                .iter()
                .filter_map(|(_, fields)| {
                    fields
                        .iter()
                        .find(|(k, _)| k == "schema")
                        .map(|(_, v)| v.clone())
                })
                .collect();
            (newest, schemas)
        })
        .unwrap_or((None, vec![]));

    let mut observed = schemas;
    observed.sort();
    observed.dedup();

    let last_probe_ms = start.elapsed().as_millis();

    let near_max_len = match (cfg.max_len, xlen) {
        (Some(cap), Some(len)) if cap > 0 => (len as f64) / (cap as f64) >= 0.95,
        _ => false,
    };

    (
        RedisStatus {
            url: cfg.url.to_string(),
            stream: cfg.stream.clone(),
            max_len: cfg.max_len,
            reachable: true,
            reachability_error: None,
            xlen,
            oldest_ts: oldest_id,
            newest_ts: newest_id,
            last_probe_ms: Some(last_probe_ms),
            near_max_len,
        },
        observed,
    )
}

/// Probe disk: count day files in flows_dir, sum sizes, gather header
/// schema versions for skew detection.
pub(crate) fn probe_disk() -> DiskStatus {
    let dir = flows_dir();
    let dir_str = dir.display().to_string();

    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => {
            return DiskStatus {
                flows_dir: dir_str,
                exists: false,
                day_files: 0,
                total_bytes: 0,
                observed_disk_schemas: vec![],
            };
        }
    };

    let mut day_files = 0u64;
    let mut total_bytes = 0u64;
    let mut schemas: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // YYYY-MM-DD.jsonl naming convention.
        if !name.ends_with(".jsonl") || name.len() < 16 {
            continue;
        }
        day_files += 1;
        if let Ok(meta) = entry.metadata() {
            total_bytes += meta.len();
        }
        // Read just the first line (schema header) without slurping the
        // whole file. Capped at 64 KiB to guard against a corrupted
        // newline-free file forcing an unbounded read — the actual schema
        // header is ~80 bytes (#170 QA S3).
        if let Ok(file) = fs::File::open(&path) {
            use std::io::{BufRead, BufReader, Read};
            let mut reader = BufReader::new(file.take(64 * 1024));
            let mut first = String::new();
            if reader.read_line(&mut first).is_ok() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(first.trim()) {
                    if let Some(v) = val.get("version").and_then(|v| v.as_str()) {
                        schemas.push(v.to_string());
                    }
                }
            }
        }
    }

    schemas.sort();
    schemas.dedup();

    DiskStatus {
        flows_dir: dir_str,
        exists: true,
        day_files,
        total_bytes,
        observed_disk_schemas: schemas,
    }
}

/// Human-readable rendering of a `FlowStatus`. The CLI's default
/// (non-`--json`) output.
pub fn format_status_human(status: &FlowStatus) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let state_marker = match status.overall_state {
        HealthState::Ok => "✓ ok",
        HealthState::Warn => "⚠ warn",
        HealthState::Fail => "✗ fail",
    };
    let _ = writeln!(out, "darkmux flow status — {state_marker}");
    let _ = writeln!(out, "  schema:       {}", status.schema_version);
    let _ = writeln!(out, "  composition:  {}", status.sinks.composition);

    if let Some(r) = status.redis.as_ref() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Redis");
        let _ = writeln!(out, "  url:          {}", r.url);
        let _ = writeln!(out, "  stream:       {}", r.stream);
        let _ = writeln!(
            out,
            "  max_len:      {}",
            r.max_len.map(|n| n.to_string()).unwrap_or_else(|| "unbounded".into())
        );
        let _ = writeln!(out, "  reachable:    {}", r.reachable);
        if let Some(err) = r.reachability_error.as_ref() {
            let _ = writeln!(out, "  error:        {err}");
        }
        if let Some(n) = r.xlen {
            let _ = writeln!(out, "  xlen:         {n}");
        }
        if let Some(id) = r.oldest_ts.as_ref() {
            let _ = writeln!(out, "  oldest_id:    {id}");
        }
        if let Some(id) = r.newest_ts.as_ref() {
            let _ = writeln!(out, "  newest_id:    {id}");
        }
        if let Some(ms) = r.last_probe_ms {
            let _ = writeln!(out, "  probe_ms:     {ms}");
        }
        if r.near_max_len {
            let _ = writeln!(out, "  ⚠ stream is ≥95% of max_len — older records will be trimmed soon");
        }
    } else {
        let _ = writeln!(out);
        let _ = writeln!(out, "Redis: not configured (set DARKMUX_REDIS_URL to enable)");
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "Disk");
    let _ = writeln!(out, "  flows_dir:    {}", status.disk.flows_dir);
    let _ = writeln!(out, "  exists:       {}", status.disk.exists);
    let _ = writeln!(out, "  day_files:    {}", status.disk.day_files);
    let _ = writeln!(out, "  total_bytes:  {}", status.disk.total_bytes);

    let _ = writeln!(out);
    let _ = writeln!(out, "Schema");
    let _ = writeln!(out, "  writer:       {}", status.schema.writer_version);
    if status.schema.observed_versions.is_empty() {
        let _ = writeln!(out, "  observed:     (none)");
    } else {
        let _ = writeln!(out, "  observed:     {}", status.schema.observed_versions.join(", "));
    }
    let _ = writeln!(out, "  skew:         {}", status.schema.skew_detected);
    if let Some(reason) = status.schema.skew_reason.as_ref() {
        let _ = writeln!(out, "  reason:       {reason}");
    }

    if !status.warn_reasons.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Warnings:");
        for r in &status.warn_reasons {
            let _ = writeln!(out, "  - {r}");
        }
    }
    if !status.fail_reasons.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "Failures:");
        for r in &status.fail_reasons {
            let _ = writeln!(out, "  - {r}");
        }
    }

    out
}

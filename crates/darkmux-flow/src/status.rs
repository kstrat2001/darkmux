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
    bound_redis_response, default_sink_info, open_redis_connection_bounded, RawRedisUrl, SinkInfo,
    REDIS_CONNECT_TIMEOUT,
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
    /// (#1959) The flow-record hook sink's per-rule status — folded in
    /// from the retired standalone `darkmux flow` `hooks status` sub-verb.
    pub hooks: HooksStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkSummary {
    pub info: SinkInfo,
    /// Flat list of active leaf sink kinds — e.g., `["LocalFile", "Redis"]`.
    pub active_kinds: Vec<String>,
    /// Human-readable composition string — e.g., `Tee([LocalFile, Redis])`.
    pub composition: String,
}

/// (#1715) The retention ceiling every KNOWN reader of the flow stream
/// enforces independently of `redis.maxlen` — darkmux-serve's
/// `MAX_FLOW_FILE_RECORDS` (the disk-file read cap) and its `XREVRANGE …
/// COUNT` literal (the Redis read cap) are both matched to this value BY
/// DESIGN, not derived from it structurally (they live in a different
/// crate this one can't depend on without inverting the dependency graph
/// — darkmux-serve depends on darkmux-flow, not the reverse). A stream
/// whose `maxlen` sits AT or ABOVE this cap can trim forever without
/// losing a single record any reader could have retrieved — raising
/// retention further only stores records nothing can read back. See
/// `compute_near_max_len`'s doc for how this bounds `near_max_len`.
///
/// **Do not raise this alone.** The near-maxlen warning's silence on an
/// untouched machine depends on `darkmux_types::config::DEFAULT_REDIS_MAXLEN`
/// staying `>=` this value; raising the cap past the shipped default revives
/// the permanent warning #1715 removed, for every operator who never touched
/// retention. The const assertion immediately below refuses to COMPILE instead
/// of letting that happen silently — a live hazard, because
/// `read_flow_records_from_redis`'s own #2409 note already names a follow-up
/// that would want a bigger read.
pub const FLOW_READ_CAP_RECORDS: usize = 10_000;

/// (#1715 review) The near-maxlen warning's precondition, pinned at COMPILE
/// time rather than left as a convention between two crates: an operator on
/// the shipped `redis.maxlen` default must never be able to reach the
/// warning, and that holds only while the default sits at or above this read
/// cap. Raising `FLOW_READ_CAP_RECORDS` alone now fails the build here rather
/// than quietly reviving a permanent warning on every default machine.
/// `shipped_default_maxlen_never_warns` (in `near_max_len_tests`) covers the
/// BEHAVIOR that relationship buys; this covers the relationship itself.
const _: () = assert!(
    darkmux_types::config::DEFAULT_REDIS_MAXLEN >= FLOW_READ_CAP_RECORDS,
    "the shipped redis.maxlen default dropped below FLOW_READ_CAP_RECORDS — an operator on the \
     untouched default would get back the permanent near-maxlen warning #1715 removed. Raise \
     DEFAULT_REDIS_MAXLEN alongside the cap, or re-derive compute_near_max_len's precondition."
);

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
    /// (#1715) True when XLEN is within 5% of MAXLEN **and** MAXLEN is
    /// below `FLOW_READ_CAP_RECORDS` — i.e. raising retention would
    /// genuinely let a reader see more than it does today. A stream at or
    /// above the read cap is the PERMANENT, EXPECTED steady state of any
    /// active fleet (it will always be near/at MAXLEN), so that case no
    /// longer sets this field — it used to, and fired forever, teaching
    /// the operator that doctor's warnings are weather, not signal.
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
        hooks: collect_hooks_status(),
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
/// The userinfo/host boundary is the **last** `@` in the authority (RFC
/// 3986), which is also how the `redis`/`url` crates parse the URL they
/// connect with. This function used to split on the *first* `@` — so a
/// password itself containing `@` (a real shape: cloud Redis providers
/// generate them, and the Tier-1 `DARKMUX_REDIS_URL` path is documented
/// verbatim-no-validation) had everything after its first `@` treated as
/// "host" and echoed in clear:
///
/// `redis://:my@secretpw@real.host:6379/0` → `redis://:***@secretpw@real.host:6379/0`
///
/// A redactor that disagrees with the connection parser about where the
/// password ends leaks exactly the disagreement. The authority is bounded
/// at the first `/`, `?`, or `#` first, so an `@` in the path/query is
/// data, never a boundary. URLs without an `@` in the authority are
/// returned unchanged.
pub fn redact_url_creds(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let masked_userinfo = if let Some((user, _pass)) = userinfo.split_once(':') {
        format!("{user}:***")
    } else {
        // username only, no password — still keep the username visible.
        userinfo.to_string()
    };
    format!("{scheme}://{masked_userinfo}@{host}{tail}")
}

/// (#1715) Whether the near-maxlen warning is genuinely actionable. Pure
/// (no I/O) so the precedence logic is testable without a live Redis —
/// the same split `pick_string`/`pick_parsed` use for their own
/// precedence math.
///
/// Two conditions, both required:
/// 1. `cap` is below `read_cap` — raising `cap` would actually let a
///    reader see MORE records than it does today. At or above `read_cap`,
///    every KNOWN reader is capped at `read_cap` regardless of retention,
///    so raising `cap` further stores records nothing can read back.
/// 2. XLEN is within 5% of `cap` — the stream is genuinely close to
///    trimming records a reader could still have used.
///
/// Before #1715 only condition 2 gated the warning, so the OVERWHELMINGLY
/// common configuration (`cap == read_cap`, the shipped default) warned
/// PERMANENTLY: a stream at its cap is the steady state of any active
/// fleet, and the suggested remedy ("raise DARKMUX_REDIS_MAXLEN") bought
/// nothing in that configuration — the read cap capped it right back down.
pub(crate) fn compute_near_max_len(cap: Option<usize>, xlen: Option<u64>, read_cap: usize) -> bool {
    match (cap, xlen) {
        (Some(cap), Some(len)) if cap > 0 && cap < read_cap => (len as f64) / (cap as f64) >= 0.95,
        _ => false,
    }
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

    // (#2227) The connect above is bounded; the three commands below were not.
    // `probe_redis` backs `darkmux flow status` and `darkmux doctor` — the two
    // verbs an operator reaches for BECAUSE the peer is misbehaving — so "must
    // not wedge the doctor" has to cover the command phase too, not just the
    // connect. Bounds each reply individually; all three are COUNT-capped
    // reads, so none legitimately takes a second.
    bound_redis_response(&conn);

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

    let near_max_len = compute_near_max_len(cfg.max_len, xlen, FLOW_READ_CAP_RECORDS);

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

    let _ = writeln!(out);
    let _ = writeln!(out, "Hooks");
    let _ = writeln!(out, "  enabled:      {}", status.hooks.enabled);
    let _ = writeln!(out, "  outbox_dir:   {}", status.hooks.outbox_dir);
    if status.hooks.rules.is_empty() {
        let _ = writeln!(out, "  (no rules configured)");
    } else {
        for r in &status.hooks.rules {
            let mut flags = Vec::new();
            if r.is_empty_match {
                flags.push("EMPTY MATCH");
            }
            if !r.is_loopback && !r.is_tailnet {
                flags.push("URL REFUSED");
            }
            if r.stalled {
                flags.push("STALLED");
            }
            let flag_str = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join("; ")) };
            let _ = writeln!(out, "  #{}: {} -> {}{flag_str}", r.index, r.match_desc, r.url);
            let _ = writeln!(out, "      undelivered: {}", r.undelivered);
            match &r.last_delivery_ts {
                Some(ts) => {
                    let _ = writeln!(out, "      last delivery: {ts}");
                }
                None => {
                    let _ = writeln!(out, "      last delivery: (never)");
                }
            }
            if let Some(err) = &r.last_error {
                // (#2694) `last_error` is not all darkmux's own voice: the
                // redirect-refusal producer embeds the `Location` header
                // chosen by whatever the hooks target redirects to, and a
                // `.last` sidecar written by an older binary holds that
                // text raw. Rendered inline and unbounded — as this row
                // was — a long value wraps and the remote party controls
                // the first character of the continuation, which lands at
                // column 0 where this renderer's EIGHT FLUSH-LEFT rows
                // live (`Warnings:` and `Failures:` being the two that
                // change how everything below them reads). Same fix as
                // the rejection-reason block below: darkmux owns the
                // wrap, so at the supported width there is no
                // continuation line to forge from.
                //
                // The wrap is the narrower half. `untrusted_display_lines`
                // also SANITIZES, and that is what stops the vector that
                // needs no wrapping and no width assumption at all: a raw
                // NEWLINE inside the value, which the jq-transform
                // producer's excerpt bounds in length but never strips.
                // Unsanitized, one of those puts remote text at column 0
                // at 200 columns as readily as at 60, and turns a 60 KB
                // `.last` sidecar into 1,179 rendered lines. Both are
                // held by tests named in this module.
                //
                // Two shapes, both bounded: a value that fits stays on
                // the familiar one-line row (the common case — every
                // other producer of this field is darkmux's own short
                // prose), and only a value too wide for that gets the
                // label row plus its own indented lines.
                //
                // (#2694 fix-round MUST FIX 1 — the corrected version of
                // a claim the first round got backwards, stated here as
                // measured rather than as assumed.) The terminal is not
                // the only consumer of these exact rows: the viewer's
                // console lens runs `flow status` as a panel. But that
                // panel does NOT inherit the wrap question. Panel STDOUT,
                // where this row lives, renders into
                // `<pre class="panelout">` (`ConsolePanel.tsx`), and
                // `.panelout` is `white-space: pre` with
                // `overflow-x: auto` (`ui/src/styles.css`; the only phone
                // override changes font-size and padding, and the built
                // `docs/demo/index.html` agrees verbatim) — so a long row
                // CLIPS and scrolls sideways there, it does not wrap, and
                // the wrap-based forgery was never reachable through it.
                // `.panelerr` and `.panelwarn` ARE `pre-wrap`, but they
                // carry stderr only, and `flow status` never writes this
                // row to stderr. So the surface owning the wrap protects
                // is the TERMINAL, which is reason enough on its own; the
                // console lens simply is not a second one.
                //
                // What the console lens DOES inherit is the raw-newline
                // vector, which needs no wrap at all: an unsanitized
                // newline in this value puts remote text at column 0
                // under `white-space: pre` exactly as it does in a
                // terminal at any width. That is what
                // `untrusted_display_lines`'s sanitize pass stops, and
                // `flow_status_last_error_cannot_forge_a_flush_left_row_with_a_raw_newline`
                // is what holds it. React escapes the text, so there is
                // no markup injection on either path.
                //
                // Below `MIN_SUPPORTED_TERMINAL_WIDTH` the wrap guarantee
                // lapses for this row exactly as it does for the
                // rejection block — at that width darkmux's own rows wrap
                // too, so a forged row stops being distinguishable from
                // ordinary damage.
                const LAST_ERROR_LABEL: &str = "      last error: ";
                let inline = crate::hooks::untrusted_display_lines(err, LAST_ERROR_LABEL.chars().count());
                match inline.len() {
                    // Sanitized away to nothing (an error string made
                    // only of control characters). The outcome still
                    // happened, so the row is still printed — darkmux
                    // describes what it has, and "no printable text" is
                    // what it has. Deleting this arm would make a rule
                    // whose last delivery FAILED read as one that never
                    // failed, the disclosure-destroying shape #2686
                    // named.
                    //
                    // (#2694 fix round 2, CONSIDER C — the corrected
                    // reason; the first version of this note said an
                    // older binary could have written such a value, and
                    // that is FALSE.) No producer can reach this arm, at
                    // this commit or any earlier one: post-fix an
                    // all-stripped `Location` still yields
                    // `redirect refused: 302 to ""`, and at the base
                    // commit all five pre-fix producers prefix darkmux's
                    // own prose, so none of them can sanitize to empty
                    // either. The arm is reachable only from a `.last`
                    // sidecar darkmux did not write — a hand-edit, a
                    // third-party tool, a restored backup. That is still
                    // reason to keep and test it, because the sidecar is
                    // read LENIENTLY, exactly like every other registry
                    // and config file in this project: what is on disk is
                    // whatever is on disk. Held by
                    // `flow_status_still_discloses_a_last_error_that_sanitizes_to_nothing`.
                    0 => {
                        let _ = writeln!(out, "{LAST_ERROR_LABEL}(no printable text)");
                    }
                    1 => {
                        let _ = writeln!(out, "{LAST_ERROR_LABEL}{}", inline[0]);
                    }
                    _ => {
                        let _ = writeln!(out, "      last error:");
                        let indent = " ".repeat(crate::hooks::UNTRUSTED_TEXT_LINE_INDENT);
                        for line in
                            crate::hooks::untrusted_display_lines(err, crate::hooks::UNTRUSTED_TEXT_LINE_INDENT)
                        {
                            let _ = writeln!(out, "{indent}{line}");
                        }
                    }
                }
            }
            if r.dropped_appends > 0 {
                let _ = writeln!(out, "      dropped: {} (over the outbox cap, or an append failure)", r.dropped_appends);
            }
            if r.quarantined_lines > 0 {
                let _ = writeln!(out, "      quarantined: {} (invalid JSON — never redelivered)", r.quarantined_lines);
            }
            // (#2273 fix-round finding 3) `flow status` is the natural
            // verb for "did my hook deliver?" — a rule whose receiver
            // reported rejecting records must not print clean here.
            // Cumulative and never reset, same as `dropped:` above.
            if r.receiver_rejected_total > 0 {
                let _ = writeln!(
                    out,
                    "      rejected by receiver: {} (request accepted, content rejected — consumed, not retried)",
                    r.receiver_rejected_total
                );
                // (#2196) The receiver's own stated reason(s) for its
                // MOST RECENT rejection — context alongside the
                // cumulative count above, not a substitute for it (the
                // reason is last-value and self-erases on a later clean
                // delivery, same as `last_receiver_rejected`).
                if !r.last_receiver_rejected_reasons.is_empty() {
                    // (#2196 fix-round MUST FIX 1) Quoted + re-sanitized —
                    // see `hooks::format_rejection_reasons_for_display`'s
                    // doc for why this isn't a bare `.join("; ")`.
                    //
                    // (#2196 fix-round 3, MUST FIX G) The reason sits on
                    // its OWN pre-indented continuation line(s) rather
                    // than inline after the label. Inline, the row was
                    // long enough to wrap, and a wrapped continuation
                    // puts receiver-controlled text at column 0 — which
                    // is exactly where this renderer's EIGHT FLUSH-LEFT
                    // rows live (`Hooks`, `Disk`, `Redis`, `Schema`,
                    // `Warnings:`, `Failures:`, the `flow status —
                    // {state}` header, and the `Redis: not configured`
                    // line). The whitespace collapse never protected
                    // those: none of them needs a leading space run to
                    // look genuine. Owning the wrap here means no
                    // continuation line exists at the supported width, so
                    // no receiver text can reach column 0 at all — see
                    // `hooks::format_rejection_reasons_as_indented_lines`.
                    let _ = writeln!(out, "      last rejection reason(s):");
                    for line in
                        crate::hooks::format_rejection_reasons_as_indented_lines(&r.last_receiver_rejected_reasons)
                    {
                        let _ = writeln!(out, "{line}");
                    }
                }
            }
            if r.stalled {
                let _ = writeln!(
                    out,
                    "      STALLED: {} consecutive cursor-write failure(s) — the drainer has stopped attempting \
                     new deliveries for this rule until its cursor file becomes writable again",
                    r.cursor_write_failures
                );
            } else if r.cursor_write_failures > 0 {
                let _ = writeln!(out, "      cursor-write failures: {} (recovered)", r.cursor_write_failures);
            }
            match &r.last_drainer_heartbeat {
                Some(ts) => {
                    let _ = writeln!(out, "      last drainer heartbeat: {ts}");
                }
                None => {
                    let _ = writeln!(out, "      last drainer heartbeat: (none seen — no drainer has cycled here yet)");
                }
            }
        }
    }

    out
}

/// (#1959 flow-hooks-family retirement) One configured hook rule's
/// read-only status, the JSON-facing shape of `hooks::HookRuleSummary` —
/// trimmed of the two `PathBuf` fields (`outbox_path`/`cursor_path`) that
/// don't belong in the operator-facing status surface (they're internal
/// storage detail, not something `flow status`/`--json` consumers act on).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRuleStatus {
    pub index: usize,
    pub match_desc: String,
    pub url: String,
    pub is_loopback: bool,
    /// (#2135 option 2) True for a genuine Tailscale target
    /// (`100.64.0.0/10` or `*.ts.net`) — NOT loopback.
    pub is_tailnet: bool,
    /// (#2135 option 2) True when this rule signs its deliveries
    /// (`signing_secret_keychain_item` configured).
    pub signed: bool,
    pub is_empty_match: bool,
    pub undelivered: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delivery_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub dropped_appends: u64,
    pub cursor_write_failures: u64,
    pub stalled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_drainer_heartbeat: Option<String>,
    pub quarantined_lines: usize,
    /// (#2273 fix-round finding 3) CUMULATIVE count of records this
    /// rule's receiver reported rejecting inside requests it answered
    /// 2xx to — read from the persisted `<key>.rejected` counter
    /// sidecar, so it survives both a later clean delivery and a process
    /// restart. `0` for a rule that has never seen one. Additive to this
    /// shape; `#[serde(default)]` so a consumer holding a document
    /// written by an older binary still deserializes.
    #[serde(default)]
    pub receiver_rejected_total: u64,
    /// (#2196) The receiver's own stated reason(s) for its MOST RECENT
    /// rejection — `results[].error` text from the last delivery the
    /// receiver reported rejecting. Context alongside
    /// `receiver_rejected_total`, not a replacement: this is a LAST-value
    /// field that a later clean delivery erases, same caveat as
    /// `hooks::HookRuleSummary::last_receiver_rejected`. Empty when
    /// there's no current rejection, or the receiver's body carried no
    /// per-record detail. Additive; `#[serde(default)]` so a consumer
    /// holding a document written by an older binary still deserializes.
    #[serde(default)]
    pub last_receiver_rejected_reasons: Vec<String>,
}

/// The flow-record hook sink's status — folded into `FlowStatus` (#1959;
/// previously the standalone `darkmux flow` `hooks status` sub-verb). Always
/// present (unlike `FlowStatus.redis`, which is `None` when Redis isn't
/// configured at all) because `enabled: false` IS the "not configured"
/// state here — an operator running `flow status` on a hooks-disabled
/// install still sees the section, just reporting itself off, the same
/// unconditional shape the retired `hooks status` sub-verb always
/// printed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksStatus {
    pub enabled: bool,
    pub outbox_dir: String,
    pub rules: Vec<HookRuleStatus>,
}

/// Pure builder — takes `rules`/`outbox_dir` as params (rather than
/// reading `config_access` internally) so it stays unit-testable without
/// env-var mutation, mirroring `darkmux_crew::rules::resolve`'s shape.
pub fn build_hooks_status(
    enabled: bool,
    outbox_dir: &std::path::Path,
    rules: &[darkmux_types::config::HookRule],
) -> HooksStatus {
    let summaries = crate::hooks::summarize_configured_rules(rules, outbox_dir);
    HooksStatus {
        enabled,
        outbox_dir: outbox_dir.display().to_string(),
        rules: summaries
            .into_iter()
            .map(|s| HookRuleStatus {
                index: s.index,
                match_desc: s.match_desc,
                url: s.url,
                is_loopback: s.is_loopback,
                is_tailnet: s.is_tailnet,
                signed: s.signed,
                is_empty_match: s.is_empty_match,
                undelivered: s.undelivered,
                last_delivery_ts: s.last_delivery_ts,
                last_error: s.last_error,
                dropped_appends: s.dropped_appends,
                cursor_write_failures: s.cursor_write_failures,
                stalled: s.stalled,
                last_drainer_heartbeat: s.last_drainer_heartbeat,
                quarantined_lines: s.quarantined_lines,
                receiver_rejected_total: s.receiver_rejected_total,
                last_receiver_rejected_reasons: s.last_receiver_rejected_reasons,
            })
            .collect(),
    }
}

/// `build_hooks_status` against the real `config_access` tier.
fn collect_hooks_status() -> HooksStatus {
    let enabled = darkmux_types::config_access::hooks_enabled();
    let rules = darkmux_types::config_access::hooks_rules();
    let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
    build_hooks_status(enabled, &outbox_dir, &rules)
}

#[cfg(test)]
mod near_max_len_tests {
    use super::*;

    use darkmux_types::config::DEFAULT_REDIS_MAXLEN;

    /// (#1715) The exact live scenario reported: `maxlen == read_cap`, XLEN
    /// sitting right at/above it (the permanent steady state of an active
    /// fleet). Before this fix, `compute_near_max_len`'s predecessor warned
    /// here forever; raising `maxlen` buys nothing because every known
    /// reader is independently capped at `read_cap` — the warning must NOT
    /// fire. Written against `FLOW_READ_CAP_RECORDS` rather than a bare
    /// `10_000` so the scenario keeps meaning "at the cap" if the cap moves.
    #[test]
    fn is_quiet_when_maxlen_matches_the_read_cap_even_at_full_saturation() {
        let cap = FLOW_READ_CAP_RECORDS;
        assert!(!compute_near_max_len(Some(cap), Some(cap as u64 + 2), cap));
        assert!(!compute_near_max_len(Some(cap), Some(cap as u64), cap));
    }

    /// (#1715, review) The BEHAVIOR the `DEFAULT_REDIS_MAXLEN >=
    /// FLOW_READ_CAP_RECORDS` const assertion (beside the cap itself) exists
    /// to guarantee: a machine on the shipped `redis.maxlen` default, its
    /// stream fully saturated, stays quiet. The two numbers live in
    /// different crates and were related only by convention — raise
    /// `FLOW_READ_CAP_RECORDS` alone (as #2409's documented follow-up would
    /// want to, to stop a busy stream pushing a `dispatch.start` bookend
    /// below the read cap) and the permanent warning #1715 removed comes
    /// straight back for every operator who never touched retention. The
    /// const assertion catches the relationship at compile time; this
    /// catches a change to `compute_near_max_len` that breaks the same
    /// promise while leaving both numbers alone.
    #[test]
    fn shipped_default_maxlen_never_warns() {
        assert!(
            !compute_near_max_len(
                Some(DEFAULT_REDIS_MAXLEN),
                Some(DEFAULT_REDIS_MAXLEN as u64),
                FLOW_READ_CAP_RECORDS
            ),
            "an operator on the shipped default ({DEFAULT_REDIS_MAXLEN}), at full saturation, \
             must never see the near-maxlen warning"
        );
    }

    /// A `maxlen` set ABOVE the read cap is even less actionable — the
    /// operator already over-provisioned retention beyond what any reader
    /// can use, so nearing that ceiling still isn't a real problem.
    #[test]
    fn is_quiet_when_maxlen_exceeds_the_read_cap() {
        let cap = FLOW_READ_CAP_RECORDS;
        assert!(!compute_near_max_len(Some(cap * 2), Some(cap as u64 * 2 - 500), cap));
    }

    /// The genuine mismatch: `maxlen` BELOW the read cap means a reader
    /// actually wants more than retention keeps — raising `maxlen` here
    /// (up to the read cap) truly buys the operator something, so the
    /// warning is real and must still fire once XLEN nears that lower cap.
    #[test]
    fn warns_when_maxlen_trails_the_read_cap_and_xlen_is_near_it() {
        let cap = FLOW_READ_CAP_RECORDS;
        let under = cap / 10;
        assert!(compute_near_max_len(Some(under), Some(under as u64 * 96 / 100), cap));
        assert!(
            !compute_near_max_len(Some(under), Some(under as u64 / 2), cap),
            "not near yet"
        );
    }

    #[test]
    fn is_quiet_when_maxlen_or_xlen_is_unknown() {
        let cap = FLOW_READ_CAP_RECORDS;
        assert!(!compute_near_max_len(None, Some(cap as u64 - 1), cap));
        assert!(!compute_near_max_len(Some(cap / 10), None, cap));
        assert!(
            !compute_near_max_len(Some(0), Some(1), cap),
            "cap=0 (unbounded) never warns"
        );
    }
}

#[cfg(test)]
mod redis_probe_tests {
    use super::*;

    /// (#2227) Per-site bound: `probe_redis`'s `XLEN` / `XRANGE` / `XREVRANGE`.
    ///
    /// The site's pre-existing comment already claimed "a silent-at-TCP-layer
    /// OR accept-but-don't-respond peer must not wedge the doctor" — but the
    /// bound behind that claim covered the CONNECT phase only, so a peer that
    /// completed the handshake and then went quiet wedged `darkmux flow status`
    /// and `darkmux doctor` anyway. Those are the two verbs an operator runs
    /// BECAUSE the peer is misbehaving.
    #[test]
    fn probe_redis_against_silent_peer_returns_within_bounded_time() {
        let port = crate::spawn_silent_redis_peer(2);
        let cfg = RedisCfg {
            url: RawRedisUrl::new(format!("redis://127.0.0.1:{port}")),
            stream: "darkmux:flow".to_string(),
            max_len: Some(10000),
        };

        let start = std::time::Instant::now();
        let (status, schemas) = probe_redis(&cfg);
        let elapsed = start.elapsed();

        // The connect SUCCEEDED (the peer completes the handshake) — this is
        // the command phase, not #278's connect phase. That is exactly why
        // every field below is empty while `reachable` is true.
        assert!(
            status.reachable,
            "the fake peer completes the handshake, so the probe must have \
             reached the COMMAND phase; a false here means this test degenerated \
             into a connect-phase duplicate"
        );
        assert!(status.xlen.is_none(), "XLEN never answered, so it must be None");
        assert!(schemas.is_empty(), "no entries could be read from a silent peer");
        // The ceiling is deliberately TIGHT (measured ~3.1s: three 1s socket
        // deadlines plus the connect). A looser one is not conservative here,
        // it is vacuous: the fake peer eventually CLOSES the socket, so with
        // the bound removed the first `XLEN` returns at EOF and the other two
        // fail instantly on the broken connection — a total of ~5.0s, which a
        // 6s ceiling accepted. Verified by mutation, both directions.
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "probe_redis took {elapsed:?}; expected bounded by 3 x \
             REDIS_RESPONSE_TIMEOUT + connect (~3.1s). Unbounded before #2227."
        );
    }
}

#[cfg(test)]
mod hooks_status_tests {
    use super::*;
    use darkmux_types::config::{HookMatch, HookRule};

    #[test]
    fn no_rules_configured_reports_enabled_flag_and_empty_rules() {
        let tmp = tempfile::TempDir::new().unwrap();
        let status = build_hooks_status(false, tmp.path(), &[]);
        assert!(!status.enabled);
        assert!(status.rules.is_empty());
        assert_eq!(status.outbox_dir, tmp.path().display().to_string());
    }

    #[test]
    fn one_configured_rule_reports_match_url_and_undelivered_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let status = build_hooks_status(true, tmp.path(), &rules);
        assert!(status.enabled);
        assert_eq!(status.rules.len(), 1);
        let r = &status.rules[0];
        assert!(r.match_desc.contains("crawl.*"), "{}", r.match_desc);
        assert_eq!(r.url, "http://127.0.0.1:8790/events");
        assert!(r.is_loopback);
        assert_eq!(r.undelivered, 0);
    }

    #[test]
    fn human_render_includes_a_hooks_section_with_per_rule_detail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let hooks = build_hooks_status(true, tmp.path(), &rules);
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);
        assert!(rendered.contains("Hooks"), "{rendered}");
        assert!(rendered.contains("crawl.*"), "{rendered}");
        assert!(rendered.contains("http://127.0.0.1:8790/events"), "{rendered}");
        assert!(rendered.contains("undelivered: 0"), "{rendered}");
    }

    /// (#2273 fix-round finding 3) `flow status` is the natural verb for
    /// "did my hook deliver?" — before this fix it mapped
    /// `HookRuleSummary` -> `HookRuleStatus` field by field and dropped
    /// the receiver-rejection count on the floor, printing a rule clean
    /// while its receiver had rejected records. Both halves are pinned:
    /// the serialized field (what `--json` consumers read) and the human
    /// renderer's line.
    #[test]
    fn flow_status_surfaces_the_receiver_rejection_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8790/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "7").unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        assert_eq!(hooks.rules[0].receiver_rejected_total, 7);
        let json = serde_json::to_value(&hooks).unwrap();
        assert_eq!(json["rules"][0]["receiver_rejected_total"], serde_json::json!(7));

        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);
        assert!(rendered.contains("rejected by receiver: 7"), "{rendered}");
    }

    /// (#2196) `flow status` is the natural verb for "did my hook
    /// deliver?" — the receiver's own stated reason for its last
    /// rejection must ride alongside the count, both in `--json` and the
    /// human renderer, so an operator doesn't have to correlate against
    /// the receiver's own log to learn WHY.
    #[test]
    fn flow_status_surfaces_the_receivers_last_rejection_reason() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8790/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":true,"last_receiver_rejected":1,"last_receiver_rejected_reasons":["rule must be a string"]}"#,
        )
        .unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        assert_eq!(hooks.rules[0].last_receiver_rejected_reasons, vec!["rule must be a string".to_string()]);
        let json = serde_json::to_value(&hooks).unwrap();
        assert_eq!(json["rules"][0]["last_receiver_rejected_reasons"], serde_json::json!(["rule must be a string"]));

        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);
        // (#2196 fix-round 3, MUST FIX G) The label and the reason are on
        // SEPARATE lines now — the reason rides its own pre-indented
        // continuation line so the row can never wrap and put
        // receiver-controlled text at column 0. Both halves are asserted,
        // and the reason's line is asserted with its indent, so a
        // regression to the inline form fails here.
        assert!(rendered.contains("      last rejection reason(s):\n"), "{rendered}");
        assert!(rendered.contains("        \"rule must be a string\"\n"), "{rendered}");
    }

    /// (#2196 fix-round MUST FIX 6) The reason ROW's own gate
    /// (`!r.last_receiver_rejected_reasons.is_empty()`) has no direct
    /// test — the existing inverted case above uses
    /// `receiver_rejected_total == 0`, so the OUTER `if
    /// r.receiver_rejected_total > 0` block is skipped entirely and the
    /// inner gate this test targets is never reached; its assertion
    /// proves nothing about the guard beside it. This is the common case
    /// by the PR's own description: rejections on record
    /// (`receiver_rejected_total > 0`) with the reason self-erased by a
    /// later clean delivery (`last_receiver_rejected_reasons` empty) —
    /// `doctor` already has a fixture for exactly this shape
    /// (`hooks_check_still_warns_after_a_later_clean_delivery_erased_the_last_value`);
    /// `flow status` did not.
    #[test]
    fn flow_status_omits_the_reason_row_when_the_count_is_nonzero_but_the_reason_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8790/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "400").unwrap();
        // What one clean delivery leaves behind after 400 rejected ones —
        // `last_receiver_rejected`/`last_receiver_rejected_reasons` both
        // absent, same fixture shape doctor already pins.
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            r#"{"ts":"2026-01-01T00:00:00Z","ok":true}"#,
        )
        .unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        assert_eq!(hooks.rules[0].receiver_rejected_total, 400);
        assert!(hooks.rules[0].last_receiver_rejected_reasons.is_empty());

        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);
        assert!(rendered.contains("rejected by receiver: 400"), "{rendered}");
        assert!(!rendered.contains("last rejection reason"), "{rendered}");
    }

    /// (#2196 fix-round MUST FIX 1, forgery pin) Before this fix, a
    /// receiver could pad a rejection reason so that a real terminal
    /// wrapping the `"      last rejection reason(s): "` row (32 columns)
    /// at 80 columns landed a SIX-SPACE-INDENTED continuation row
    /// character-for-character identical to darkmux's own
    /// `"      cursor-write failures: N (recovered)"` row format — a
    /// receiver-controlled row indistinguishable from real darkmux
    /// output, sitting directly under the genuine count.
    ///
    /// The fixture writes that exact forged reason straight into the
    /// `.last` sidecar (bypassing `extract_rejection_reasons`/
    /// `truncate_reason` entirely — simulating either a sidecar written
    /// before this fix, or a future producer that forgets to sanitize),
    /// so this also proves the CONSIDER item: `format_status_human`
    /// re-sanitizes at render via
    /// `hooks::format_rejection_reasons_for_display`, not just at the
    /// producer.
    ///
    /// The forged reason is exactly 40 display columns — right AT the
    /// display-width cap, not over it — deliberately, so it survives
    /// truncation WHOLE (no ellipsis eats it). A longer padded reason
    /// (the review's own PoC shape, ~90 bytes) would ALSO be defeated,
    /// but only because the width bound truncates it away before the
    /// forged vocabulary is ever reached — that would prove the width
    /// bound works without ever exercising
    /// [`crate::hooks::collapse_whitespace_and_trim`] at all. This exact
    /// size is what makes the test red-provable against a whitespace-
    /// collapse regression specifically.
    ///
    /// Every genuine row in this per-rule block indents with a RUN of
    /// spaces (never one) — `collapse_whitespace_and_trim` guarantees no
    /// receiver text can reproduce that run, so no simulated wrap of the
    /// rendered output, at any column width, can ever produce a
    /// continuation line starting with darkmux's own multi-space
    /// indentation.
    ///
    /// (#2196 fix-round 3, MUST FIX G) Scope correction: that covers the
    /// INDENTED rows and nothing else. `format_status_human` also emits
    /// EIGHT FLUSH-LEFT rows, none of which needs a leading space run to
    /// look genuine, so the whitespace collapse buys them nothing — see
    /// `flow_status_reason_cannot_forge_a_flush_left_row` below for the
    /// separate, render-site defense those required.
    #[test]
    fn flow_status_reason_forgery_cannot_reproduce_darkmuxs_own_row_indentation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8790/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);
        // (#2196 fix-round 2, MUST FIX C) Filler + darkmux's OWN row
        // text, six-space-indented, sized to sit EXACTLY at
        // `REJECTION_REASON_RAW_BUDGET` columns (derived from the real
        // constant, not hardcoded, so this stays exact if the budget is
        // ever retuned) — so this survives `truncate_reason` WHOLE,
        // unlike a longer padded attempt that the width bound alone
        // would already defeat.
        let own_row = "      cursor-write failures: 0";
        let filler_len = crate::hooks::REJECTION_REASON_RAW_BUDGET - own_row.chars().count();
        let filler: String = "1234567890".chars().cycle().take(filler_len).collect();
        let forged_reason = format!("{filler}{own_row}");
        assert_eq!(
            forged_reason.chars().count(),
            crate::hooks::REJECTION_REASON_RAW_BUDGET,
            "fixture must sit exactly at the cap"
        );
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            serde_json::json!({
                "ts": "2026-01-01T00:00:00Z",
                "ok": true,
                "last_receiver_rejected": 1,
                "last_receiver_rejected_reasons": [forged_reason],
            })
            .to_string(),
        )
        .unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);

        // The rendered reason must never contain the forged run of
        // spaces — the property that makes the forgery impossible
        // regardless of where any wrap lands.
        assert!(!rendered.contains(&format!("{filler}      cursor-write")), "{rendered}");
        assert!(!rendered.contains("      cursor-write"), "no 6-space-indented forgery may survive: {rendered}");

        // Simulate wrapping every line at a range of plausible terminal
        // widths and assert no continuation row can ever begin with
        // darkmux's own row vocabulary (two or more leading spaces is
        // darkmux's own indent; a wrapped row starting with it would be
        // the forgery).
        for width in [40usize, 60, 80, 100, 120] {
            for line in rendered.lines() {
                let chars: Vec<char> = line.chars().collect();
                for chunk_start in (width..chars.len()).step_by(width) {
                    let continuation: String = chars[chunk_start..].iter().take(width).collect();
                    assert!(
                        !continuation.starts_with("  "),
                        "wrapped continuation row at width {width} begins with darkmux's own \
                         multi-space indent — forgery survived: {continuation:?} (full line: {line:?})"
                    );
                    assert!(
                        !continuation.starts_with("cursor-write failures"),
                        "wrapped continuation row at width {width} reproduces darkmux's own row \
                         vocabulary: {continuation:?} (full line: {line:?})"
                    );
                }
            }
        }
    }

    /// (#2196 fix-round 2, MUST FIX A + D, re-proof at the widened
    /// widths) Re-runs the verifier's own row-forgery proof end-to-end
    /// through `flow status`'s real renderer, at the widths named in the
    /// fix-round-2 verification brief (60, 72, 80, 100, 120). Covers two
    /// independent primitives: (1) each of the four blank-glyph
    /// characters (U+2800, U+3164, U+115F, U+FFA0), used as "an exact
    /// replica of darkmux's 6-space indent" — none is
    /// `char::is_whitespace()`, so [`crate::hooks::collapse_whitespace_and_trim`]
    /// alone would never have caught them, which is exactly why they
    /// counted as forgeries the first fix-round's whitespace-collapse
    /// defense didn't reach; and (2) a purely-ASCII quote-heavy reason —
    /// the escape-expansion path, which needs no exotic Unicode at all
    /// (this was the padding the verifier's own proof used). The fixture
    /// writes straight into the `.last` sidecar, bypassing the
    /// producer's own sanitization, so this also proves the render-time
    /// re-sanitization catches all of it independently.
    #[test]
    fn flow_status_defeats_the_blank_glyph_and_escape_expansion_forgeries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8791/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);

        let blank_glyphs = ['\u{2800}', '\u{3164}', '\u{115F}', '\u{FFA0}'];
        let mut reasons: Vec<String> = blank_glyphs
            .iter()
            .map(|c| format!("{}{}cursor-write failures: 0 (recovered)", "x".repeat(20), c.to_string().repeat(6)))
            .collect();
        reasons.push("\"".repeat(200)); // the escape-expansion path — no exotic Unicode needed

        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            serde_json::json!({
                "ts": "2026-01-01T00:00:00Z",
                "ok": true,
                "last_receiver_rejected": 1,
                "last_receiver_rejected_reasons": reasons,
            })
            .to_string(),
        )
        .unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);

        for c in blank_glyphs {
            assert!(
                !rendered.contains(c),
                "U+{:06X} must not survive to the rendered row at all: {rendered}",
                c as u32
            );
        }

        for width in [60usize, 72, 80, 100, 120] {
            for line in rendered.lines() {
                let chars: Vec<char> = line.chars().collect();
                for chunk_start in (width..chars.len()).step_by(width) {
                    let continuation: String = chars[chunk_start..].iter().take(width).collect();
                    assert!(
                        !continuation.starts_with("  "),
                        "width {width}: wrapped continuation begins with darkmux's own multi-space \
                         indent — forgery survived: {continuation:?} (full line: {line:?})"
                    );
                    assert!(
                        !continuation.starts_with("cursor-write failures"),
                        "width {width}: wrapped continuation reproduces darkmux's own row \
                         vocabulary: {continuation:?} (full line: {line:?})"
                    );
                }
            }
        }
    }

    /// Every row `format_status_human` renders FLUSH LEFT (column 0).
    /// Derived by enumerating every `writeln!(out, "…")` in that function
    /// whose literal does not begin with a space; there are exactly
    /// eight. These are the rows the whitespace-collapse defense never
    /// protected — none of them needs a leading space run to look
    /// genuine — and the two that matter most are `Warnings:` and
    /// `Failures:`, because they change how an operator reads everything
    /// printed below them.
    const FLUSH_LEFT_ROWS: &[&str] = &[
        // The header's literal is `"darkmux flow status — {state_marker}"`;
        // matched without its trailing space, because the sanitizer trims
        // a reason's ends and a forgery needs no state marker to read as
        // the header.
        "darkmux flow status —",
        "Redis",
        "Redis: not configured (set DARKMUX_REDIS_URL to enable)",
        "Disk",
        "Schema",
        "Warnings:",
        "Failures:",
        "Hooks",
    ];

    /// The label `format_status_human` prints the reason under, in
    /// COLUMNS. The forgery's whole arithmetic keys on it: inline,
    /// receiver text begins at `LABEL_COLUMNS + 1` (the opening quote),
    /// so a filler of `width - LABEL_COLUMNS - 2` characters plus one
    /// space lands the payload on exactly column 0 of the first wrapped
    /// continuation at that width.
    const LABEL_COLUMNS: usize = "      last rejection reason(s): ".len();

    /// Simulate a terminal `width` columns wide hard-wrapping `rendered`,
    /// and return every VISUAL line that is a CONTINUATION — a segment
    /// beginning at column 0 because the logical line overflowed, rather
    /// than because a new logical line started. Those are the only lines
    /// receiver text can reach column 0 through.
    fn wrapped_continuations(rendered: &str, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        for line in rendered.lines() {
            let chars: Vec<char> = line.chars().collect();
            let mut start = width;
            while start < chars.len() {
                out.push(chars[start..].iter().take(width).collect::<String>());
                start += width;
            }
        }
        out
    }

    /// (#2196 fix-round 3, MUST FIX G) The anti-forgery premise the first
    /// two fix rounds rested on — "every darkmux status row indents with
    /// a run of spaces, and the whitespace collapse makes such a run
    /// unconstructible" — is FALSE, and was stated as an absolute. EIGHT
    /// rows render flush left (`FLUSH_LEFT_ROWS`), and not one of them
    /// needs leading whitespace to be plausible.
    ///
    /// The proof needed nothing exotic: filler characters, ONE space,
    /// then the verbatim row text. No whitespace run, no blank glyph, no
    /// escape expansion, well inside the 118-column budget, surviving
    /// truncation whole. Rendered INLINE after its label the row wrapped
    /// and put the forged text at column 0, with the genuine identical
    /// row a few lines above; the only tell was a trailing quote.
    ///
    /// This test is SELF-PROVING rather than mutation-dependent. For
    /// every (width, row) pair it first asserts the forgery really does
    /// land in the INLINE rendering — the exact string that shipped
    /// before this fix, built here from the same
    /// `format_rejection_reasons_for_display` the old render site called
    /// — and only then asserts that the real renderer's output contains
    /// no such continuation at any of the five widths. A future change
    /// that made the payload stop forging would fail the precondition
    /// instead of passing vacuously.
    #[test]
    fn flow_status_reason_cannot_forge_a_flush_left_row() {
        let widths = [60usize, 72, 80, 100, 120];
        let mut exercised = 0usize;

        for payload in FLUSH_LEFT_ROWS {
            let mut exercised_for_payload = 0usize;
            for width in widths {
                // Filler sized so the payload starts on exactly column 0
                // of the first wrapped continuation at this width.
                let filler = width - LABEL_COLUMNS - 2;
                let reason = format!("{} {payload}", "z".repeat(filler));
                // Skip only the combinations the width bound would cut
                // (a long row literal at a wide terminal) — a truncated
                // payload is not a forgery, and asserting on one would be
                // asserting on the ellipsis. The counts below keep this
                // from quietly skipping everything.
                if reason.chars().count() > crate::hooks::MAX_REJECTION_REASON_DISPLAY_WIDTH {
                    continue;
                }

                // PRECONDITION — the inline form that shipped before this
                // fix really does forge the row at this width.
                let inline = format!(
                    "      last rejection reason(s): {}",
                    crate::hooks::format_rejection_reasons_for_display(std::slice::from_ref(&reason))
                );
                assert!(
                    wrapped_continuations(&inline, width).iter().any(|c| c.starts_with(payload)),
                    "the fixture must actually forge {payload:?} at width {width} in the INLINE form, \
                     or this test proves nothing: {inline:?}"
                );
                exercised += 1;
                exercised_for_payload += 1;

                // And the shipped renderer must produce no such
                // continuation, for ANY of the eight rows, at ANY width.
                let rendered = render_with_reasons(&[reason]);
                for w in widths {
                    for continuation in wrapped_continuations(&rendered, w) {
                        for row in FLUSH_LEFT_ROWS {
                            assert!(
                                !continuation.starts_with(row),
                                "width {w}: a wrapped continuation forges the flush-left row {row:?} \
                                 (payload {payload:?} sized for width {width}): {continuation:?}"
                            );
                        }
                    }
                }
            }
            // `Warnings:` and `Failures:` are the two that change how an
            // operator reads everything printed below them, and both are
            // short enough to fit the budget at every width — so require
            // the FULL grid for those two, not merely "at least one".
            let required = if ["Warnings:", "Failures:"].contains(payload) { widths.len() } else { 1 };
            assert!(
                exercised_for_payload >= required,
                "{payload:?} was exercised at only {exercised_for_payload} of the {} widths (required {required})",
                widths.len()
            );
        }

        // Only the longest row literal (the 55-column `Redis: not
        // configured …` line) is cut by the width bound, and only at the
        // two widest terminals — everything else runs the full grid.
        assert!(exercised >= 30, "only {exercised} (width, row) pairs were exercised");
    }

    /// (#2694) The label `format_status_human` prints `last_error` behind,
    /// in COLUMNS, and the darkmux-voice head of the redirect-refusal
    /// reason that follows it. The forgery's arithmetic keys on their
    /// sum: inline, remote-chosen text begins at exactly this column, so
    /// a filler of `width - LAST_ERROR_PREFIX_COLUMNS - 1` characters plus
    /// one space lands the payload on column 0 of the first wrapped
    /// continuation at that width.
    const LAST_ERROR_LABEL: &str = "      last error: ";
    const REDIRECT_REASON_HEAD: &str = "redirect refused: 302 to ";
    const LAST_ERROR_PREFIX_COLUMNS: usize = LAST_ERROR_LABEL.len() + REDIRECT_REASON_HEAD.len();

    /// Render `format_status_human` for one rule whose last terminal
    /// outcome was a failure carrying `err` — the `.last` sidecar is
    /// written DIRECTLY, bypassing `try_post`'s own sanitization, which is
    /// both the pre-#2694 on-disk case (a sidecar written by an older
    /// binary holds the raw `Location` text) and the way to prove the
    /// render site defends independently of the producer.
    fn render_with_last_error(err: &str) -> String {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8794/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            serde_json::json!({ "ts": "2026-01-01T00:00:00Z", "ok": false, "error": err }).to_string(),
        )
        .unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        assert_eq!(hooks.rules[0].last_error.as_deref(), Some(err), "the fixture must reach the renderer verbatim");
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        format_status_human(&status)
    }

    /// (#2694) The same flush-left row forgery #2196 measured on the
    /// rejection-reason row, on the `last error:` row — which renders a
    /// second span of remote-chosen text (the `Location` header of a
    /// refused redirect, passed through verbatim when it does not parse
    /// as a URL) and, before this fix, rendered it INLINE and UNBOUNDED.
    ///
    /// SELF-PROVING rather than mutation-dependent, the same way
    /// `flow_status_reason_cannot_forge_a_flush_left_row` is: for every
    /// (width, row) pair it first asserts the forgery really does land in
    /// the inline rendering that shipped before this fix — the exact
    /// `"      last error: {err}"` string — and only then asserts the real
    /// renderer produces no such continuation at any of the five widths.
    /// A fixture that stopped forging would fail the precondition instead
    /// of passing vacuously.
    ///
    /// No skip arm is needed here (unlike the rejection version): the
    /// pre-fix form bounded nothing at all, so every pair forges.
    #[test]
    fn flow_status_last_error_cannot_forge_a_flush_left_row() {
        let widths = [60usize, 72, 80, 100, 120];
        let mut exercised = 0usize;

        for payload in FLUSH_LEFT_ROWS {
            for width in widths {
                let filler = width - LAST_ERROR_PREFIX_COLUMNS - 1;
                let location = format!("{} {payload}", "z".repeat(filler));
                let err = format!("{REDIRECT_REASON_HEAD}{location}");

                // PRECONDITION — the inline, unbounded form that shipped
                // before this fix really does forge this row at this width.
                let inline = format!("{LAST_ERROR_LABEL}{err}");
                assert!(
                    wrapped_continuations(&inline, width).iter().any(|c| c.starts_with(payload)),
                    "the fixture must actually forge {payload:?} at width {width} in the INLINE form, \
                     or this test proves nothing: {inline:?}"
                );
                exercised += 1;

                // And the shipped renderer must produce no such
                // continuation, for ANY of the eight rows, at ANY width.
                let rendered = render_with_last_error(&err);
                for w in widths {
                    for continuation in wrapped_continuations(&rendered, w) {
                        for row in FLUSH_LEFT_ROWS {
                            assert!(
                                !continuation.starts_with(row),
                                "width {w}: a wrapped continuation forges the flush-left row {row:?} \
                                 (payload {payload:?} sized for width {width}): {continuation:?}"
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(exercised, FLUSH_LEFT_ROWS.len() * widths.len(), "the full (width, row) grid must be exercised");
    }

    /// (#2694, the mechanism — asserted independently of any forged
    /// vocabulary) Every line `format_status_human` emits for the `last
    /// error:` row must stay strictly under the narrowest supported
    /// terminal width, so no terminal at that width or wider has a
    /// continuation line for remote-chosen text to reach column 0
    /// through. That, not the vocabulary check above, is what makes the
    /// forgery unreachable for rows nobody has thought of yet.
    ///
    /// Red-proves by name: restore the inline
    /// `writeln!(out, "      last error: {err}")` and the width assertion
    /// fails on the first fixture; delete the `- 1` from
    /// `untrusted_content_budget` and it fails at exactly 60 columns.
    #[test]
    fn every_rendered_last_error_line_is_bounded_under_the_supported_width() {
        let min_width = crate::hooks::MIN_SUPPORTED_TERMINAL_WIDTH;
        // Four shapes: an unbroken run with no wrap opportunity at all, a
        // run of wide (2-column) characters, a zero-width run that a
        // column-only bound would not catch, and the escape-expansion
        // path (no exotic Unicode needed).
        let fixtures = [
            format!("{REDIRECT_REASON_HEAD}{}", "q".repeat(400)),
            format!("{REDIRECT_REASON_HEAD}{}", "漢".repeat(200)),
            format!("{REDIRECT_REASON_HEAD}{}", "a\u{0301}".repeat(2000)),
            format!("{REDIRECT_REASON_HEAD}{}", "\"".repeat(200)),
        ];
        for err in &fixtures {
            let rendered = render_with_last_error(err);
            let mut block_lines = 0usize;
            let mut in_block = false;
            for line in rendered.lines() {
                // The block is either the one-line `last error: …` row, or
                // a bare `last error:` label followed by indented lines.
                if line.starts_with("      last error:") {
                    in_block = true;
                } else if in_block && !line.starts_with(&" ".repeat(crate::hooks::UNTRUSTED_TEXT_LINE_INDENT)) {
                    in_block = false;
                }
                if !in_block {
                    continue;
                }
                block_lines += 1;
                let width: usize = crate::hooks::display_columns(line);
                assert!(width < min_width, "last-error line is {width} columns, must stay under {min_width}: {line:?}");
            }
            assert!(block_lines >= 2, "the fixture must actually produce a wrapped block, saw {block_lines}: {err:?}");
        }
    }

    /// (#2694, inverted case) A short error — every OTHER producer of
    /// `LastStatus.error` writes darkmux's own short prose — still
    /// renders on the familiar single row, so the test above cannot pass
    /// because the renderer became unconditionally multi-line, and a
    /// rule with no terminal failure prints no `last error` row at all.
    #[test]
    fn flow_status_keeps_a_short_last_error_on_one_row_and_omits_it_when_absent() {
        let rendered = render_with_last_error("invalid outbox line");
        assert!(
            rendered.contains("      last error: invalid outbox line\n"),
            "a short internal error must stay on the one-line row: {rendered}"
        );

        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8795/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let hooks = build_hooks_status(true, tmp.path(), &rules);
        assert!(hooks.rules[0].last_error.is_none());
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        assert!(!format_status_human(&status).contains("last error"), "no terminal failure, no row");
    }

    /// (#2694 fix round 2, CONSIDER B) Every non-empty line `rendered`
    /// emits OUTSIDE the `last error:` block — darkmux's whole structural
    /// voice, not just the column-zero third of it. The first version of
    /// this helper filtered `!l.starts_with(' ')`, which watched the
    /// eight flush-left rows and left the two-space ones (`  schema:`,
    /// `  composition:`, `  url:`, and the `  - {r}` bullets under
    /// `Warnings:` / `Failures:`) outside the comparison entirely: a
    /// fixture carrying `\n  composition:  Compromised` forged a row in
    /// the genuine row's exact indentation with every guard green. That
    /// is unreachable on shipped code — the sanitize pass collapses the
    /// newline, so no extra line of ANY indentation is produced — but the
    /// guard's stated virtue is holding for rows nobody has enumerated,
    /// and comparing every line outside the block costs nothing.
    ///
    /// Everything here must be a function of the STATUS being described,
    /// never of the bytes inside one of its values.
    ///
    /// One row genuinely varies between two renders of the SAME status:
    /// `  outbox_dir:` names a fresh `TempDir` per fixture. Its VALUE is
    /// normalized rather than the row being dropped, so an extra forged
    /// `  outbox_dir:` line still changes the vector's length and fails.
    /// (The widened comparison found this on its first run — the
    /// column-zero-only version never saw the row at all.)
    fn lines_outside_the_last_error_block(rendered: &str) -> Vec<String> {
        const OUTBOX_DIR_LABEL: &str = "  outbox_dir:   ";
        let indent = " ".repeat(crate::hooks::UNTRUSTED_TEXT_LINE_INDENT);
        let mut out = Vec::new();
        let mut in_block = false;
        for line in rendered.lines() {
            if line.starts_with("      last error:") {
                in_block = true;
            } else if in_block && !line.starts_with(&indent) {
                in_block = false;
            }
            if !in_block && !line.is_empty() {
                if line.starts_with(OUTBOX_DIR_LABEL) {
                    out.push(format!("{OUTBOX_DIR_LABEL}<per-fixture tempdir>"));
                } else {
                    out.push(line.to_string());
                }
            }
        }
        out
    }

    /// The lines `format_status_human` emits for the `last error:` block
    /// — the label row plus, in the multi-line arm, its indented
    /// continuation lines.
    fn last_error_block_lines(rendered: &str) -> Vec<String> {
        let indent = " ".repeat(crate::hooks::UNTRUSTED_TEXT_LINE_INDENT);
        let mut out = Vec::new();
        let mut in_block = false;
        for line in rendered.lines() {
            if line.starts_with("      last error:") {
                in_block = true;
            } else if in_block && !line.starts_with(&indent) {
                in_block = false;
            }
            if in_block {
                out.push(line.to_string());
            }
        }
        out
    }

    /// (#2694 fix-round MUST FIX 2) The WRAP is not the only way remote
    /// text reaches column zero, and it is not even the live one: a RAW
    /// NEWLINE does it at ANY width, with no wrapping involved. Producer
    /// #5 (`hooks.rs`'s jq-transform failure, which writes a "transform
    /// ... failed: ..." reason into the `.last` sidecar) bounds its
    /// excerpt to `hook_transform::MAX_ERROR_EXCERPT` characters and
    /// never strips newlines, and jq writes multi-line diagnostics. Left
    /// unsanitized, the renderer's own `writeln!` then emits the
    /// attacker's chosen continuation flush left, under the genuine
    /// label, in darkmux's own voice — reachable at 200 columns, and
    /// reachable inside the viewer's console lens too, whose
    /// `white-space: pre` makes the wrap-based forgery unreachable.
    ///
    /// Asserted as a SET EQUALITY against a benign baseline rather than
    /// as a vocabulary check, so it holds for a row nobody has
    /// enumerated yet: EVERY line outside the `last error:` block must be
    /// a function of the status, not of the bytes inside one of its
    /// values. (#2694 fix round 2, CONSIDER B — the comparison covers
    /// darkmux's two-space rows as well as its flush-left ones; the
    /// payload set exercises both.)
    ///
    /// Red-proves by name: in `hooks::untrusted_display_lines`, drop the
    /// sanitize pass — `wrap_to_display_width(&truncate_reason(text), …)`
    /// → `wrap_to_display_width(text, …)`, leaving the wrap intact — and
    /// this fails while every other test in the crate stays green.
    #[test]
    fn flow_status_last_error_cannot_forge_a_flush_left_row_with_a_raw_newline() {
        // (#2694 fix round 2, CONSIDER B) darkmux's own indented rows,
        // which the first version of this guard left outside the
        // comparison. `  composition:` is the one the review forged.
        const TWO_SPACE_ROWS: &[&str] =
            &["  composition:  Compromised", "  schema:       0.0", "  - no drainer heartbeat for 9999s"];

        let baseline = lines_outside_the_last_error_block(&render_with_last_error("benign"));
        let mut exercised = 0usize;
        for payload in FLUSH_LEFT_ROWS.iter().chain(TWO_SPACE_ROWS.iter()) {
            let err =
                format!("transform `/x/a.jq` failed: jq: error at line 1:\n{payload}\n  - deliveries are healthy");

            // PRECONDITION — the fixture really does put the row text at
            // the start of a raw line, so a renderer that passed it
            // through unsanitized would print it at that line's start.
            assert!(
                err.lines().any(|l| l == *payload),
                "the fixture must actually carry {payload:?} as a whole raw line, or this proves nothing: {err:?}"
            );
            exercised += 1;

            let got = lines_outside_the_last_error_block(&render_with_last_error(&err));
            assert_eq!(
                got, baseline,
                "the structural rows changed when {payload:?} was embedded in the last error — \
                 remote text reached darkmux's own voice"
            );
        }
        assert_eq!(
            exercised,
            FLUSH_LEFT_ROWS.len() + TWO_SPACE_ROWS.len(),
            "every flush-left AND two-space row must be exercised"
        );
    }

    /// (#2694 fix-round MUST FIX 2, the second half of the same guard)
    /// The sanitize pass also carries the LENGTH bound, and a `.last`
    /// sidecar is an attacker-sized file on disk, not a header a
    /// transport capped. Unbounded, one status row becomes a screenful
    /// and scrolls the rest of the report — including the `Hooks` rows
    /// above it — off the terminal, which is a disclosure failure of the
    /// same shape #2686 named even though nothing is forged.
    ///
    /// Red-proves by name: the same `truncate_reason` drop as above takes
    /// this from a 4-line block to 1,178 wrapped lines.
    #[test]
    fn one_last_error_row_cannot_become_a_screenful() {
        // 60 KB, the measured payload size `REJECTION_REASON_CHARS_PER_COLUMN`'s
        // doc records for the reason row, applied to this one.
        let err = format!("{REDIRECT_REASON_HEAD}{}", "z".repeat(60_000));
        let rendered = render_with_last_error(&err);
        let block = last_error_block_lines(&rendered);

        // (#2694 fix round 2, CONSIDER A) PIN THE HELPER FIRST.
        // `!block.is_empty()` is satisfied by anything PLAUSIBLE, so a
        // `last_error_block_lines` that quietly stopped reading the real
        // render — returning a stub, or losing the block to a changed
        // label — would take this bound and the inline sweep vacuous
        // TOGETHER, since both consume it. So: the block must start at
        // the genuine label, and it must carry this fixture's OWN
        // characters, which no stub can supply.
        assert!(
            block.first().is_some_and(|l| l.starts_with("      last error:")),
            "the block must begin at the genuine label row: {block:?}"
        );
        let zs: usize = block.iter().map(|l| l.chars().filter(|c| *c == 'z').count()).sum();
        assert!(zs >= 50, "the block must carry the fixture's own text, not a plausible stub: {block:?}");

        assert!(
            block.len() <= 8,
            "a single `last error:` row rendered {} lines; the raw-width bound holds it to a handful",
            block.len()
        );
    }

    /// (#2694 fix-round CONSIDER) The `(no printable text)` disclosure —
    /// reachable through the `.last` sidecar, which is a file on disk
    /// whose `error` string darkmux did not write this run. Deleting the
    /// row would make a rule whose last delivery FAILED read as one that
    /// never failed, the disclosure-destroying shape #2686 named.
    ///
    /// Red-proves by name: replace the `0 =>` arm's body with `{}` and
    /// this fails.
    #[test]
    fn flow_status_still_discloses_a_last_error_that_sanitizes_to_nothing() {
        let rendered = render_with_last_error("\u{0007}\u{0000}\u{001b}\u{200b}");
        assert!(
            rendered.contains("      last error: (no printable text)\n"),
            "an error made only of control characters must still print the row: {rendered}"
        );
    }

    /// (#2694 fix-round CONSIDER) The INLINE arm's geometry, asserted at
    /// the CALL SITE that declares the prefix. `untrusted_content_budget`
    /// is red-proved one level down, but the budget is only correct if
    /// the caller hands it the true column count of the prefix it then
    /// prints — and the inline arm is the common case, since every
    /// producer of this field other than the redirect refusal writes
    /// darkmux's own short prose.
    ///
    /// Sweeps content lengths across the arm boundary so the single-line
    /// arm is actually exercised (the multi-line bound test ends with
    /// `block_lines >= 2` and therefore never reaches it).
    ///
    /// Red-proves by name: subtract 1 from the prefix argument at the
    /// call site — `LAST_ERROR_LABEL.chars().count()` → `… - 1` — and the
    /// 42-character fixture renders an inline row of exactly
    /// `MIN_SUPPORTED_TERMINAL_WIDTH` columns, the ambiguity the `+ 1`
    /// inside the budget exists to avoid.
    #[test]
    fn the_inline_last_error_row_stays_under_the_supported_width() {
        let min_width = crate::hooks::MIN_SUPPORTED_TERMINAL_WIDTH;
        let mut inline_seen = 0usize;
        for n in 1..=min_width {
            let err = "z".repeat(n);
            let rendered = render_with_last_error(&err);
            let block = last_error_block_lines(&rendered);

            // (#2694 fix round 2, CONSIDER A) Pin the helper against this
            // fixture before measuring geometry with it — see
            // `one_last_error_row_cannot_become_a_screenful`. Every `z`
            // the fixture supplied must be present and accounted for, so
            // a helper returning a plausible stub fails here rather than
            // reporting a comfortable width for a line nobody rendered.
            assert!(
                block.first().is_some_and(|l| l.starts_with("      last error:")),
                "n={n}: the block must begin at the genuine label row: {block:?}"
            );
            let zs: usize = block.iter().map(|l| l.chars().filter(|c| *c == 'z').count()).sum();
            assert_eq!(zs, n, "n={n}: the block must carry every character the fixture supplied: {block:?}");

            if block.len() == 1 {
                inline_seen += 1;
            }
            for line in &block {
                let width = crate::hooks::display_columns(line);
                assert!(width < min_width, "n={n}: a last-error line is {width} columns, must stay under {min_width}: {line:?}");
            }
        }
        assert!(inline_seen > 0, "the sweep must actually exercise the single-row arm");
    }

    /// Render `format_status_human` for one rule carrying `reasons` as
    /// its last receiver rejection.
    fn render_with_reasons(reasons: &[String]) -> String {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8792/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            serde_json::json!({
                "ts": "2026-01-01T00:00:00Z",
                "ok": true,
                "last_receiver_rejected": 1,
                "last_receiver_rejected_reasons": reasons,
            })
            .to_string(),
        )
        .unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        format_status_human(&status)
    }

    /// (#2196 fix-round 3, MUST FIX G — the mechanism, asserted
    /// independently of any forged vocabulary) Every line
    /// `format_status_human` emits that carries receiver-controlled text
    /// must be BOTH indented and strictly narrower than the narrowest
    /// supported terminal width. That — not the vocabulary check above —
    /// is what makes the forgery unreachable for rows nobody has thought
    /// of yet, including ones a future revision adds.
    ///
    /// Red-proves by name: delete the `- 1` from
    /// `format_rejection_reasons_as_indented_lines`'s `content_budget`
    /// and the strict-inequality assertion fails at exactly 60 columns;
    /// restore the inline render and the indent assertion fails.
    #[test]
    fn every_rendered_reason_line_is_indented_and_narrower_than_the_supported_width() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = HookMatch { action: Some("crawl.*".to_string()), ..Default::default() };
        let url = "http://127.0.0.1:8793/events".to_string();
        let rules = vec![HookRule {
            r#match: Some(m.clone()),
            http: Some(url.clone()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let key = crate::hooks::rule_key(&m, &url);

        // Three shapes at once: an unbroken run with no wrap opportunity
        // at all, a run of wide (2-column) characters, and a realistic
        // multi-word reason at the top of the budget.
        let reasons = vec![
            "q".repeat(400),
            "漢".repeat(200),
            "payload field \"file\" must be a non-empty string and the receiver names the offending value here"
                .to_string(),
        ];
        std::fs::write(tmp.path().join(format!("{key}.rejected")), "1").unwrap();
        std::fs::write(
            tmp.path().join(format!("{key}.last")),
            serde_json::json!({
                "ts": "2026-01-01T00:00:00Z",
                "ok": true,
                "last_receiver_rejected": 1,
                "last_receiver_rejected_reasons": reasons,
            })
            .to_string(),
        )
        .unwrap();

        let hooks = build_hooks_status(true, tmp.path(), &rules);
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);

        let min_width = crate::hooks::MIN_SUPPORTED_TERMINAL_WIDTH;
        let indent = " ".repeat(crate::hooks::UNTRUSTED_TEXT_LINE_INDENT);
        let mut reason_lines = 0usize;
        for line in rendered.lines() {
            // The reason lines are exactly the ones carrying a quote —
            // `format_rejection_reasons_for_display` always quotes, and
            // no other row in this renderer emits one.
            if !line.contains('"') {
                continue;
            }
            reason_lines += 1;
            assert!(line.starts_with(&indent), "reason line is not indented: {line:?}");
            let width: usize = line.chars().map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)).sum();
            assert!(width < min_width, "reason line is {width} columns, must stay under {min_width}: {line:?}");
        }
        assert!(reason_lines >= 3, "the fixture must actually produce wrapped reason lines, saw {reason_lines}");
    }

    /// (#2273 fix-round finding 3, inverted case) A rule that has never
    /// seen a rejection prints no rejection line at all — so a renderer
    /// that unconditionally emitted the row could not pass the test above
    /// by accident.
    #[test]
    fn flow_status_prints_no_rejection_line_when_there_are_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let hooks = build_hooks_status(true, tmp.path(), &rules);
        assert_eq!(hooks.rules[0].receiver_rejected_total, 0);
        let status = FlowStatus {
            schema_version: "1.0".to_string(),
            sinks: SinkSummary {
                info: SinkInfo { kind: "LocalFile".into(), config: Default::default(), children: vec![], raw_url: None },
                active_kinds: vec!["LocalFile".to_string()],
                composition: "LocalFile".to_string(),
            },
            redis: None,
            disk: DiskStatus { flows_dir: "x".into(), exists: true, day_files: 0, total_bytes: 0, observed_disk_schemas: vec![] },
            schema: SchemaSkew { writer_version: "1.0".into(), observed_versions: vec![], skew_detected: false, skew_reason: None },
            overall_state: HealthState::Ok,
            warn_reasons: vec![],
            fail_reasons: vec![],
            hooks,
        };
        let rendered = format_status_human(&status);
        assert!(!rendered.contains("rejected by receiver"), "{rendered}");
        assert!(!rendered.contains("last rejection reason"), "{rendered}");
    }
}

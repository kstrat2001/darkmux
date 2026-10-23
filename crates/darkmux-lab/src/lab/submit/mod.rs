//! `darkmux lab submit`: turning lab runs into a darkmux evals bundle, and
//! sending it to the darkmux evals site (evals.darkmux.com).
//!
//! A port of the site's `evals:bundle` reading (darklyenergized
//! `app/lib/darkmux_evals/lab_run.ts`), so anyone running darkmux can share a
//! result: the same refusals, the same as-of-run rule, the same weights hash.
//! The site parses every bundle strictly and is the authority; this side
//! refuses early what the site would, and never sends a path.
//!
//! What cannot be vouched for as of the run is `null` ("not reported"), never
//! guessed, and [`Built::notes`] says why. The pure readings are here and
//! tested; [`gather`] probes the machine (the platform, the engine, git).

pub mod sha256;
pub mod upload;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

pub const BUNDLE_FORMAT: &str = "darkmux-evals-bundle";
/// 3 adds what built darkmux (`darkmuxCommit`, `darkmuxBuild`). 4 adds the
/// structured-output count (`detail.verify.fieldsCorrect`, `fieldsExpected`,
/// `fieldsMissed`) and the reply cap the dispatch sent
/// (`stack.maxCompletionTokens`). A bundle none of whose runs carries either
/// is still written as 3, without those keys, so a site that reads only 3
/// keeps taking every other result.
pub const BUNDLE_FORMAT_VERSION: u64 = 4;
/// The version a bundle with nothing format 4 added is written as.
const BUNDLE_FORMAT_VERSION_BEFORE_FIELDS: u64 = 3;
/// Where results are shared unless `--site` says otherwise.
pub const DEFAULT_SITE: &str = "https://evals.darkmux.com";

/// The site's bounds: a figure outside them is left out, so a run is never refused for it.
const MAX_COUNT: f64 = 1e9;
const MAX_SECONDS: f64 = 7.0 * 24.0 * 3600.0;
const MAX_STATES: usize = 8;

/// A filesystem path, a home directory or a share, also when URL-encoded:
/// the site's `looksLikePath`, so a bundle this side writes is not refused there.
pub fn looks_like_path(value: &str) -> bool {
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            // `(?-u:\b)`: an ASCII word boundary, as in the site's JavaScript.
            r"(?i)/(?:users|home|root|private|var|tmp|opt|volumes|mnt|media|srv|library)(?:/|(?-u:\b))",
            r"(?i)(?-u:\b)file:",
            r"~[A-Za-z0-9._-]*/",
            r"(?:^|[^A-Za-z0-9])[A-Za-z]:[\\/]",
            r"\\\\",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("valid pattern"))
        .collect()
    });
    let decoded = percent_decode(value);
    patterns.iter().any(|re| re.is_match(value) || decoded.as_deref().is_some_and(|d| re.is_match(d)))
}

/// An email, an IP or MAC address, or a UUID: what free text may carry that
/// names a person or a machine. Free text with one is not shared.
pub fn looks_personal(value: &str) -> bool {
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}",
            r"(?-u:\b)(?:\d{1,3}\.){3}\d{1,3}(?-u:\b)",
            r"(?i)(?-u:\b)[0-9a-f]{2}(?::[0-9a-f]{2}){5}(?-u:\b)",
            r"(?i)(?-u:\b)[0-9a-f]{2}(?:-[0-9a-f]{2}){5}(?-u:\b)",
            r"(?i)(?-u:\b)(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}(?-u:\b)",
            r"(?i)(?-u:\b)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}(?-u:\b)",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("valid pattern"))
        .collect()
    });
    patterns.iter().any(|re| re.is_match(value))
}

/// Whether `value` holds one of `names` as a whole word, in any case. A
/// name under three characters is too common to judge by.
pub fn names_any(value: &str, names: &[String]) -> bool {
    let fold = |t: &str| t.to_lowercase().replace('\u{2019}', "'");
    let hay = fold(value);
    names.iter().filter(|n| n.chars().count() >= 3).any(|n| {
        let needle = fold(n);
        hay.match_indices(&needle).any(|(at, m)| {
            let before = hay[..at].chars().next_back();
            let after = hay[at + m.len()..].chars().next();
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
    })
}

/// Every string in a value, or key, that holds one of `names`, by where it is.
pub fn names_in(v: &Value, at: &str, names: &[String], found: &mut Vec<String>) {
    match v {
        Value::String(s) if names_any(s, names) => found.push(at.to_string()),
        Value::Array(items) => items.iter().enumerate().for_each(|(i, x)| names_in(x, &format!("{at}[{i}]"), names, found)),
        Value::Object(map) => {
            for (k, x) in map {
                if names_any(k, names) {
                    found.push(format!("{at}.{k}"));
                }
                names_in(x, &format!("{at}.{k}"), names, found);
            }
        }
        _ => {}
    }
}

/// `decodeURIComponent`, or `None` where it would throw.
fn percent_decode(value: &str) -> Option<String> {
    let b = value.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Every string in a value that looks like a path, by where it is. Empty is
/// the only shareable answer.
pub fn paths_in(v: &Value, at: &str, found: &mut Vec<String>) {
    match v {
        Value::String(s) if looks_like_path(s) => found.push(at.to_string()),
        Value::Array(items) => items.iter().enumerate().for_each(|(i, x)| paths_in(x, &format!("{at}[{i}]"), found)),
        Value::Object(map) => {
            for (k, x) in map {
                if looks_like_path(k) {
                    found.push(format!("{at}.{k}"));
                }
                paths_in(x, &format!("{at}.{k}"), found);
            }
        }
        _ => {}
    }
}

/// darkmux namespaces the models it loads (`darkmux:<id>`, contract 4); the id is the rest.
pub fn model_id_from(dispatched: Option<&str>) -> Option<String> {
    let id = dispatched?.strip_prefix("darkmux:").unwrap_or(dispatched?);
    (!id.is_empty()).then(|| id.to_string())
}

/// `git@github.com:o/r.git` or `https://github.com/o/r(.git)` → `o/r`.
pub fn repo_from_remote(url: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"github\.com[:/]([A-Za-z0-9-]+)/([A-Za-z0-9._-]+?)(?:\.git)?/?$").expect("valid pattern")
    });
    let c = re.captures(url.trim())?;
    Some(format!("{}/{}", &c[1], &c[2]))
}

/// Only `passed: true` in the manifest's verify object is a pass (#2833).
pub fn passed_from(manifest: &Value) -> bool {
    manifest.get("verify").and_then(|v| v.get("passed")) == Some(&Value::Bool(true))
}

/// Why a run cannot be shared, or `None`. Only a run that reached a model,
/// completed cleanly and carries a gated verdict is a result about the model.
pub fn refusal_for(manifest: &Value, stats: &Value) -> Option<&'static str> {
    if stats.get("model").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Some("it never reached a model");
    }
    if manifest.get("ok") != Some(&Value::Bool(true)) {
        return Some("the dispatch did not complete cleanly (a harness failure)");
    }
    if !manifest.get("verify").and_then(|v| v.get("passed")).is_some_and(Value::is_boolean) {
        return Some("it has no verdict to share");
    }
    if stats.get("verify_ungated") == Some(&Value::Bool(true)) {
        return Some("it passed before the work gate existed");
    }
    None
}

/// The commit `HEAD` was on through these runs, from `git reflog show
/// --date=unix --format='%H %gd' HEAD` (newest first). Committer dates say
/// when a commit was made, not when it was checked out.
pub fn commit_at_time(reflog: &str, first_start_ms: u64, last_start_ms: u64) -> std::result::Result<String, String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^([0-9a-f]{40}) \S*@\{(\d+)\}").expect("valid pattern"));
    let entries: Vec<(String, u64)> = reflog
        .lines()
        .filter_map(|l| re.captures(l.trim()))
        .filter_map(|c| Some((c[1].to_string(), c[2].parse::<u64>().ok()? * 1000)))
        .collect();
    let Some(first) = entries.iter().position(|(_, when)| *when <= first_start_ms) else {
        return Err("the reflog does not reach back to these runs; pass --commit <sha>".into());
    };
    // Every entry from the first run's to the last's: a checkout away and
    // back between them leaves the ends agreeing and a middle run elsewhere.
    let last = entries.iter().position(|(_, when)| *when <= last_start_ms).unwrap_or(first);
    let sha = &entries[first].0;
    if entries[last.min(first)..=first].iter().any(|(s, _)| s != sha) {
        return Err("the fixture checkout moved between these runs; submit them separately".into());
    }
    Ok(sha.clone())
}

/// The files the weights hash covers: tensors and the model's own config,
/// tokenizer and chat template. An engine's own files beside them (a lock, a
/// manifest it rewrites) are neither hashed nor dated.
pub fn is_model_file(name: &str) -> bool {
    const TENSORS: [&str; 6] = ["safetensors", "gguf", "bin", "npz", "pt", "pth"];
    const NAMED: [&str; 14] = [
        "model.safetensors.index.json",
        "pytorch_model.bin.index.json",
        "config.json",
        "generation_config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "special_tokens_map.json",
        "added_tokens.json",
        "tokenizer.model",
        "vocab.json",
        "merges.txt",
        "chat_template.jinja",
        "chat_template.json",
        "preprocessor_config.json",
    ];
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    ext.is_some_and(|e| TENSORS.contains(&e.as_str())) || NAMED.contains(&name)
}

/// The weights hash: SHA-256 over each file's relative path and SHA-256,
/// sorted by path (`<path>\0<sha256>\n` each), the site's `weightsHashOf`.
pub fn weights_hash_of(files: &[(String, String)]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let mut sorted: Vec<&(String, String)> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut h = sha256::Sha256::default();
    for (path, sum) in sorted {
        h.update(format!("{path}\0{sum}\n").as_bytes());
    }
    Some(h.hex())
}

/// A fact read now holds for a run only if it last changed before the run
/// began; otherwise, or when either time is unknown, it is not reported.
pub fn as_of_run<T>(value: Option<T>, changed_at_ms: Option<u64>, run_start_ms: Option<u64>) -> Option<T> {
    match (value, changed_at_ms, run_start_ms) {
        (Some(v), Some(changed), Some(start)) if changed <= start => Some(v),
        _ => None,
    }
}

fn count(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_f64) {
        Some(n) if n.fract() == 0.0 && (0.0..=MAX_COUNT).contains(&n) => json!(n as u64),
        _ => Value::Null,
    }
}

fn measure(v: Option<&Value>, max: f64) -> Value {
    match v.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && (0.0..=max).contains(&n) => json!((n * 10.0).round() / 10.0),
        _ => Value::Null,
    }
}

fn seconds(ms: Option<&Value>) -> Value {
    match ms.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n >= 0.0 => {
            let s = (n / 100.0).round() / 10.0;
            if s <= MAX_SECONDS { json!(s) } else { Value::Null }
        }
        _ => Value::Null,
    }
}

fn flag(v: Option<&Value>) -> Value {
    v.filter(|b| b.is_boolean()).cloned().unwrap_or(Value::Null)
}

/// Text to share, or null: never a path, never longer than the site takes.
fn shareable(v: Option<&Value>, max: usize, names: &[String]) -> Value {
    let Some(t) = v.and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
        return Value::Null;
    };
    // Judged whole, before any cut: a name cut in half would no longer read as one.
    if looks_like_path(t) || looks_personal(t) || names_any(t, names) {
        return Value::Null;
    }
    // The site counts UTF-16 units; a char never takes fewer, so this never runs over.
    if t.encode_utf16().count() > max {
        let mut cut = String::new();
        for c in t.chars() {
            if cut.encode_utf16().count() + c.len_utf16() > max - 1 {
                break;
            }
            cut.push(c);
        }
        return json!(format!("{cut}\u{2026}"));
    }
    json!(t)
}

/// Why the verdict is what it is: the verify summary and the work gate's counts.
pub fn verify_detail_from(manifest: &Value, names: &[String]) -> Value {
    let Some(v) = manifest.get("verify").filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let empty = Value::Object(Map::new());
    let g = v.get("work_gate").filter(|g| g.is_object()).unwrap_or(&empty);
    let added = g.get("tests_added").and_then(Value::as_f64).filter(|n| n.fract() == 0.0 && n.abs() <= MAX_COUNT);
    let (fields_correct, fields_expected, fields_missed) = fields_count(v);
    json!({
        "summary": shareable(v.get("details"), 500, names),
        "forcedFailureReason": shareable(g.get("forced_failure_reason"), 500, names),
        "baselineTests": count(g.get("baseline_test_count")),
        "testsTotal": count(g.get("tests_total")),
        "testsPassed": count(g.get("tests_passed")),
        "testsAdded": added.map_or(Value::Null, |n| json!(n as i64)),
        "testsFailed": count(g.get("tests_failed")),
        "sandboxChanged": flag(g.get("sandbox_changed")),
        "coveragePct": measure(g.get("coverage_pct"), 100.0),
        "coverageMinPct": measure(g.get("coverage_min_pct"), 100.0),
        "commandPassed": flag(g.get("command_passed")),
        // A structured-output workload's count (`verify.expect`), the same
        // names for every one; null for any other workload.
        "fieldsCorrect": fields_correct,
        "fieldsExpected": fields_expected,
        "fieldsMissed": fields_missed,
    })
}

/// A structured-output run's count as the site takes it: all three, or all
/// three null when they do not add up (`correct` over `expected`, no field
/// expected, a missed list that is not `expected - correct` distinct names,
/// or a name outside `[A-Za-z0-9_.-]{1,60}`, which also keeps any path or
/// personal text out).
fn fields_count(v: &Value) -> (Value, Value, Value) {
    static NAME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let name = NAME.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9_.-]{1,60}$").expect("valid pattern"));
    let none = (Value::Null, Value::Null, Value::Null);
    let (Some(correct), Some(expected)) = (count(v.get("fields_correct")).as_u64(), count(v.get("fields_expected")).as_u64())
    else {
        return none;
    };
    let Some(missed) = v.get("fields_missed").and_then(Value::as_array) else { return none };
    let names: Vec<&str> = missed.iter().filter_map(Value::as_str).filter(|n| name.is_match(n)).collect();
    let distinct = names.iter().collect::<BTreeSet<_>>().len();
    if expected < 1 || correct > expected || names.len() != missed.len() || distinct != names.len() || names.len() as u64 != expected - correct {
        return none;
    }
    (json!(correct), json!(expected), json!(names))
}

/// The detail beyond the basics. Host figures need busy samples: with none
/// they are null, never zero.
pub fn detail_from(manifest: &Value, stats: &Value, names: &[String]) -> Value {
    let sampled = stats.get("samples_busy").and_then(Value::as_u64).is_some_and(|n| n > 0);
    let host = |v: Value| if sampled { v } else { Value::Null };
    let thermal = stats.get("thermal_states_busy").and_then(Value::as_object).map(|states| {
        let mut kept: Vec<(&String, u64)> = states
            .iter()
            .filter(|(k, _)| {
                let b = k.as_bytes();
                !b.is_empty() && b.len() <= 20 && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || *c == b'_')
            })
            .filter_map(|(k, n)| count(Some(n)).as_u64().map(|n| (k, n)))
            .collect();
        // The states seen most, as many as the site takes.
        kept.sort_by_key(|k| std::cmp::Reverse(k.1));
        kept.truncate(MAX_STATES);
        kept.into_iter().map(|(k, n)| (k.clone(), json!(n))).collect::<Map<String, Value>>()
    });
    let s = |k: &str| stats.get(k);
    json!({
        "wallSeconds": seconds(s("wall_ms")),
        "restSeconds": seconds(s("rest_ms")),
        "turns": count(s("turns")),
        "toolCalls": count(s("tool_calls_total")),
        "toolCallsFailed": count(s("tool_calls_failed")),
        "completionTokens": count(s("completion_tokens")),
        "reasoningTokens": count(s("reasoning_tokens")),
        "packageWatts": host(measure(s("pkg_w_busy"), 10_000.0)),
        "cpuWatts": host(measure(s("cpu_w_busy"), 10_000.0)),
        "gpuWatts": host(measure(s("gpu_w_busy"), 10_000.0)),
        "gpuBusyPct": host(measure(s("gpu_duty_pct"), 100.0)),
        "joulesPer1kTokens": host(measure(s("pkg_j_per_1k_tokens"), 1e7)),
        "memoryPressureMaxPct": host(measure(s("mem_pct_busy_max"), 100.0)),
        "thermalStates": host(thermal.filter(|t| !t.is_empty()).map_or(Value::Null, Value::Object)),
        "cpuSpeedLimitMinPct": host(count(s("cpu_speed_limit_min"))),
        "throttledSamples": host(count(s("throttled_samples"))),
        "verify": verify_detail_from(manifest, names),
    })
}

/// What built the darkmux that ran a run, as far as it is known.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RanBy {
    pub version: Option<String>,
    pub commit: Option<String>,
    /// `release` or `dev`.
    pub build: Option<String>,
}

/// One run's stack: the field names are the site's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stack {
    pub chip: String,
    pub ram_gb: Option<u64>,
    pub accelerator: Option<String>,
    pub os: Option<String>,
    pub engine: Option<String>,
    pub engine_backend: Option<String>,
    pub engine_version: Option<String>,
    pub model: Option<String>,
    pub weights_hash: Option<String>,
    pub quant: Option<String>,
    pub context_length: Option<u64>,
    /// Physical cores per CPU cluster by the OS's name, fastest first; empty when not read.
    pub cpu_cores: Vec<(String, u32)>,
    pub gpu_cores: Option<u32>,
    /// The Mac's model identifier (`Mac16,5`).
    pub model_id: Option<String>,
}

impl Stack {
    fn to_json(&self) -> Value {
        json!({
            "chip": self.chip,
            "ramGb": self.ram_gb,
            "accelerator": self.accelerator,
            "os": self.os,
            "engine": self.engine,
            "engineBackend": self.engine_backend,
            "engineVersion": self.engine_version,
            "model": self.model,
            "weightsHash": self.weights_hash,
            "quant": self.quant,
            "contextLength": self.context_length,
            "kvCacheDtype": null,
            "cpuCores": (!self.cpu_cores.is_empty()).then(|| {
                self.cpu_cores.iter().map(|(cluster, cores)| json!({ "cluster": cluster, "cores": cores })).collect::<Vec<_>>()
            }),
            "gpuCores": self.gpu_cores,
            "modelId": self.model_id,
        })
    }

    /// The fields left null, for the notes.
    pub fn unreported(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (name, missing) in [
            ("ramGb", self.ram_gb.is_none()),
            ("accelerator", self.accelerator.is_none()),
            ("os", self.os.is_none()),
            ("engine", self.engine.is_none()),
            ("engineBackend", self.engine_backend.is_none()),
            ("engineVersion", self.engine_version.is_none()),
            ("weightsHash", self.weights_hash.is_none()),
            ("quant", self.quant.is_none()),
            ("contextLength", self.context_length.is_none()),
            ("cpuCores", self.cpu_cores.is_empty()),
            ("gpuCores", self.gpu_cores.is_none()),
            ("modelId", self.model_id.is_none()),
        ] {
            if missing {
                out.push(name);
            }
        }
        out.push("kvCacheDtype");
        out
    }
}

/// One run as the site takes it.
pub fn bundle_run(run_id: &str, manifest: &Value, stats: &Value, ran_by: &RanBy, stack: &Stack, names: &[String]) -> Value {
    let active = stats.get("active_ms").and_then(Value::as_f64).filter(|n| *n > 0.0);
    let all_billed = stats.get("checks").and_then(|c| c.get("all_streams_billed")) != Some(&Value::Bool(false));
    let started = stats.get("started_at_unix_ms").and_then(Value::as_u64);
    json!({
        "key": sha256::hex(run_id.as_bytes()),
        "darkmuxVersion": ran_by.version,
        "darkmuxCommit": ran_by.commit,
        "darkmuxBuild": ran_by.build,
        "passed": passed_from(manifest),
        "activeSeconds": active.map(|ms| (ms / 100.0).round() / 10.0),
        // A rate over part of the generation is quoted with its share, never alone.
        "tokensPerSecond": stats.get("tok_per_s").and_then(Value::as_f64).filter(|_| all_billed),
        // darkmux reports machine-wide memory pressure, not the run's own peak.
        "peakMemoryGb": null,
        "startedAt": started.map(iso_utc),
        "stack": with_reply_cap(stack.to_json(), manifest),
        "detail": detail_from(manifest, stats, names),
    })
}

/// The stack with the reply cap the run's dispatch sent (`replyRoom`), null
/// when darkmux sent none of its own.
fn with_reply_cap(mut stack: Value, manifest: &Value) -> Value {
    stack["maxCompletionTokens"] = count(manifest.get("max_completion_tokens"));
    stack
}

/// Unix milliseconds as `2026-10-22T08:02:00.000Z` (JavaScript's `toISOString`).
pub fn iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

pub fn assemble(repo: &str, commit_sha: &str, fixture_manifest: &Value, runs: Vec<Value>) -> Result<Value> {
    let name = fixture_manifest
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| anyhow!("the fixture's .fixture.json has no name"))?;
    // darkmux reads a fixture with no version as 1.0; a number reads as JavaScript writes it (2.0 is "2").
    let version = match fixture_manifest.get("version") {
        None | Some(Value::Null) => "1.0".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        Some(other) => other.to_string(),
    };
    let (version_number, runs) = format_for(runs);
    Ok(json!({
        "format": BUNDLE_FORMAT,
        "formatVersion": version_number,
        "fixture": { "repo": repo, "commitSha": commit_sha, "name": name, "version": version, "manifest": fixture_manifest },
        "runs": runs,
    }))
}

/// Format 4 when any run carries what 4 added; otherwise 3, with those keys
/// (null in every run) left out.
fn format_for(mut runs: Vec<Value>) -> (u64, Vec<Value>) {
    let carries = |r: &Value| {
        !r["stack"]["maxCompletionTokens"].is_null()
            || ["fieldsCorrect", "fieldsExpected", "fieldsMissed"].iter().any(|k| !r["detail"]["verify"][*k].is_null())
    };
    if runs.iter().any(carries) {
        return (BUNDLE_FORMAT_VERSION, runs);
    }
    for r in &mut runs {
        if let Some(stack) = r.get_mut("stack").and_then(Value::as_object_mut) {
            stack.remove("maxCompletionTokens");
        }
        if let Some(v) = r.pointer_mut("/detail/verify").and_then(Value::as_object_mut) {
            for k in ["fieldsCorrect", "fieldsExpected", "fieldsMissed"] {
                v.remove(k);
            }
        }
    }
    (BUNDLE_FORMAT_VERSION_BEFORE_FIELDS, runs)
}

/// What the site's strict parser (`parseBundle`) would refuse in a bundle,
/// by where: the bounds and forms a bundle written here can miss (a stated
/// value, a chip name, a repeated run). Empty means it takes it. The site
/// stays the authority; this saves an upload it would refuse.
pub fn site_refusals(bundle: &Value) -> Vec<String> {
    let forms = Forms::new();
    let mut r = Refusals::default();
    let f = &bundle["fixture"];
    r.text(&f["repo"], "fixture.repo", 200, Some(&forms.repo), false);
    r.text(&f["commitSha"], "fixture.commitSha", 200, Some(&forms.hex40), false);
    r.text(&f["name"], "fixture.name", 60, None, false);
    r.text(&f["version"], "fixture.version", 200, Some(&forms.semverish), false);
    let runs = bundle["runs"].as_array().cloned().unwrap_or_default();
    let mut keys = BTreeSet::new();
    for (i, run) in runs.iter().enumerate() {
        if !keys.insert(run["key"].as_str().unwrap_or_default().to_string()) {
            r.numbers.push(format!("runs[{i}]: the same run twice"));
        }
        r.run(i, run, &forms);
    }
    let mut errs = r.errs;
    errs.extend(r.numbers);
    if runs.is_empty() || runs.len() > 500 {
        errs.push("runs: 1 to 500 runs a bundle".to_string());
    }
    if serde_json::to_string(&f["manifest"]).map_or(0, |t| t.len()) > 64 * 1024 {
        errs.push("fixture.manifest: larger than 64 KB".to_string());
    }
    errs
}

/// The forms `parseBundle` holds a bundle's strings to.
struct Forms {
    semverish: regex::Regex,
    hex64: regex::Regex,
    hex40: regex::Regex,
    commit: regex::Regex,
    repo: regex::Regex,
    apple: regex::Regex,
    os: regex::Regex,
    model_id: regex::Regex,
    cluster: regex::Regex,
}

impl Forms {
    fn new() -> Self {
        let re = |p: &str| regex::Regex::new(p).expect("valid pattern");
        Forms {
            semverish: re(r"^[0-9A-Za-z.+-]{1,40}$"),
            hex64: re(r"^[0-9a-f]{64}$"),
            hex40: re(r"^[0-9a-f]{40}$"),
            commit: re(r"^[0-9a-f]{7,40}$"),
            repo: re(r"^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"),
            apple: re(r"^Apple M\d{1,2}(?: (?:Pro|Max|Ultra))?$"),
            os: re(r"^(?:macOS \d[0-9.]*|Omarchy\b[^\n]*)$"),
            model_id: re(r"^[A-Za-z]{1,20}\d{1,3},\d{1,3}$"),
            cluster: re(r"^[A-Z][A-Za-z]{0,19}$"),
        }
    }
}

/// What `site_refusals` has found: the strings' refusals, then the numbers'.
#[derive(Default)]
struct Refusals {
    errs: Vec<String>,
    numbers: Vec<String>,
}

impl Refusals {
    /// A non-blank string within `max` UTF-16 units, in `form` when given; null when `nullable`.
    fn text(&mut self, v: &Value, at: &str, max: usize, form: Option<&regex::Regex>, nullable: bool) {
        match v {
            Value::Null if nullable => {}
            Value::String(s) if !s.trim().is_empty() => {
                if s.encode_utf16().count() > max {
                    self.errs.push(format!("{at}: longer than {max}"));
                } else if form.is_some_and(|f| !f.is_match(s)) {
                    self.errs.push(format!("{at}: not in the form the site takes ({s})"));
                }
            }
            _ => self.errs.push(format!("{at}: expected a string{}", if nullable { " or null" } else { "" })),
        }
    }

    fn run(&mut self, i: usize, r: &Value, forms: &Forms) {
        let at = |k: &str| format!("runs[{i}].{k}");
        self.text(&r["darkmuxVersion"], &at("darkmuxVersion"), 200, Some(&forms.semverish), true);
        self.text(&r["darkmuxCommit"], &at("darkmuxCommit"), 200, Some(&forms.commit), true);
        let s = &r["stack"];
        let st = |k: &str| format!("runs[{i}].stack.{k}");
        self.text(&s["chip"], &st("chip"), 200, Some(&forms.apple), false);
        self.text(&s["accelerator"], &st("accelerator"), 32, None, true);
        self.text(&s["os"], &st("os"), 80, Some(&forms.os), true);
        for k in ["engine", "engineBackend", "engineVersion"] {
            self.text(&s[k], &st(k), 80, None, true);
        }
        self.text(&s["model"], &st("model"), 200, None, true);
        self.text(&s["weightsHash"], &st("weightsHash"), 200, Some(&forms.hex64), true);
        self.text(&s["quant"], &st("quant"), 40, None, true);
        self.text(&s["modelId"], &st("modelId"), 40, Some(&forms.model_id), true);
        if let Some(clusters) = s["cpuCores"].as_array() {
            if !(1..=4).contains(&clusters.len()) {
                self.errs.push(format!("{}: expected 1 to 4 CPU clusters, or null", st("cpuCores")));
            }
            for (j, c) in clusters.iter().enumerate() {
                self.text(&c["cluster"], &st(&format!("cpuCores[{j}].cluster")), 20, Some(&forms.cluster), false);
                if c["cores"].as_u64().is_none_or(|n| n > 1000) {
                    self.numbers.push(format!("{}: outside 0 to 1000", st(&format!("cpuCores[{j}].cores"))));
                }
            }
        }
        if s["gpuCores"].as_u64().is_some_and(|n| n > 1000) {
            self.numbers.push(format!("{}: outside 0 to 1000", st("gpuCores")));
        }
        for (k, max) in [("activeSeconds", MAX_SECONDS), ("tokensPerSecond", 100_000.0)] {
            if r[k].as_f64().is_some_and(|n| !(0.0..=max).contains(&n)) {
                self.numbers.push(format!("{}: outside 0 to {max}", at(k)));
            }
        }
    }
}

/// What the operator states by hand, for what the machine cannot say.
#[derive(Debug, Clone, Default)]
pub struct Stated {
    pub engine: Option<String>,
    pub engine_backend: Option<String>,
    pub engine_version: Option<String>,
    pub quant: Option<String>,
    pub weights_dir: Option<PathBuf>,
    pub darkmux_version: Option<String>,
    pub commit: Option<String>,
}

/// A bundle, and what it leaves out and why.
#[derive(Debug)]
pub struct Built {
    pub bundle: Value,
    /// One line per run: its verdict, its model, what it does not report.
    pub notes: Vec<String>,
    /// `owner/repo@sha7 (name version)`.
    pub fixture: String,
}

/// Reads the runs and the machine and builds one bundle. Reads only.
pub fn gather(runs: &[String], stated: &Stated, progress: &mut dyn FnMut(&str)) -> Result<Built> {
    gather_on(&Machine::here()?, runs, stated, &darkmux_types::config_access::flows_dir(), progress)
}

/// [`gather`] on a given machine, reading run stats against `flows_dir`.
pub fn gather_on(machine: &Machine, runs: &[String], stated: &Stated, flows_dir: &Path, progress: &mut dyn FnMut(&str)) -> Result<Built> {
    refuse_repeats(runs)?;
    let inputs = read_runs(runs, flows_dir)?;
    let source = one_fixture(&inputs, stated)?;
    let mut caches = Caches::default();
    let mut bundled = Vec::new();
    let mut notes = Vec::new();
    let mut starts = Vec::new();
    for input in &inputs {
        let (run, note) = machine.bundle_one(input, stated, &mut caches, progress)?;
        starts.push(input.started());
        bundled.push(run);
        notes.push(note);
    }
    let (repo, commit_sha, fixture_manifest) = fixture_at(&source, &starts, stated.commit.as_deref())?;
    let bundle = assemble(&repo, &commit_sha, &fixture_manifest, bundled)?;
    refuse_identifying(&bundle, &machine.identifying)?;
    refuse_unshareable(&bundle)?;
    let f = &bundle["fixture"];
    Ok(Built {
        fixture: format!(
            "{repo}@{} ({} {})",
            &commit_sha[..7],
            f["name"].as_str().unwrap_or_default(),
            f["version"].as_str().unwrap_or_default()
        ),
        bundle,
        notes,
    })
}

fn refuse_repeats(runs: &[String]) -> Result<()> {
    if runs.is_empty() {
        bail!("name at least one lab run");
    }
    if let Some(twice) = runs.iter().enumerate().find_map(|(i, r)| runs[..i].contains(r).then_some(r)) {
        bail!("{twice} is named twice");
    }
    Ok(())
}

/// The machine's serial, hardware id or names, or its user's, anywhere in a
/// run. The fixture is left out: it is a public repo's, approved as it is.
/// The refusal says where, never what.
fn refuse_identifying(bundle: &Value, names: &[String]) -> Result<()> {
    let mut found = Vec::new();
    names_in(&bundle["runs"], "bundle.runs", names, &mut found);
    if !found.is_empty() {
        bail!(
            "the bundle would carry something that identifies this machine or you (its serial, hardware id, host or user name), which is never shared; found at {}",
            found.join(", ")
        );
    }
    Ok(())
}

/// A file path anywhere in it, or anything the site would refuse.
fn refuse_unshareable(bundle: &Value) -> Result<()> {
    let mut paths = Vec::new();
    paths_in(bundle, "bundle", &mut paths);
    if !paths.is_empty() {
        bail!("the bundle would carry a file path, which is never shared; found at {}", paths.join(", "));
    }
    let refused = site_refusals(bundle);
    if !refused.is_empty() {
        bail!("the site would refuse this bundle:\n  {}", refused.join("\n  "));
    }
    Ok(())
}

/// One run as read: its id, its manifest, its stats.
struct Input {
    run_id: String,
    manifest: Value,
    stats: Value,
}

impl Input {
    fn started(&self) -> Option<u64> {
        self.stats.get("started_at_unix_ms").and_then(Value::as_u64)
    }

    fn model_id(&self) -> Option<String> {
        model_id_from(self.stats.get("model").and_then(Value::as_str))
    }
}

/// Reads and checks every run first, so a refusal names them all.
fn read_runs(runs: &[String], flows_dir: &Path) -> Result<Vec<Input>> {
    let mut inputs = Vec::new();
    let mut refused = Vec::new();
    for run in runs {
        match read_run(run, flows_dir)? {
            Ok(input) => inputs.push(input),
            Err(why) => refused.push(why),
        }
    }
    if !refused.is_empty() {
        bail!("not submitted:\n  {}", refused.join("\n  "));
    }
    Ok(inputs)
}

/// A run that can be submitted, or why not.
fn read_run(run: &str, flows_dir: &Path) -> Result<std::result::Result<Input, String>> {
    let dir = crate::lab::inspect::resolve_run_dir(run);
    let run_id = dir.file_name().map_or_else(|| run.to_string(), |n| n.to_string_lossy().into_owned());
    let Ok(text) = std::fs::read_to_string(dir.join(crate::lab::manifest::MANIFEST_FILE)) else {
        return Ok(Err(format!("{run_id}: no manifest.json, so it ended before the eval ran")));
    };
    let manifest: Value = serde_json::from_str(&text).with_context(|| format!("{run_id}: reading manifest.json"))?;
    let stats = match crate::lab::stats::compute_from_dir(&dir, flows_dir) {
        Ok(s) => serde_json::to_value(s)?,
        Err(e) => return Ok(Err(format!("{run_id}: its numbers cannot be read ({e:#})"))),
    };
    Ok(match refusal_for(&manifest, &stats) {
        Some(why) => Err(format!("{run_id}: {why}")),
        None => Ok(Input { run_id, manifest, stats }),
    })
}

/// The one fixture checkout the runs share; refuses runs of several, or a
/// model stated by hand when they used several.
fn one_fixture(inputs: &[Input], stated: &Stated) -> Result<PathBuf> {
    let sources: BTreeSet<Option<String>> = inputs.iter().map(|i| fixture_field(&i.manifest, "source_path")).collect();
    let baselines: BTreeSet<Option<String>> = inputs.iter().map(|i| fixture_field(&i.manifest, "baseline_hash")).collect();
    let source = match sources.iter().next() {
        Some(Some(s)) if sources.len() == 1 => PathBuf::from(s),
        _ => bail!("submit runs of one fixture checkout at a time (each run's manifest names it)"),
    };
    if baselines.len() != 1 {
        bail!("these runs started from different fixture contents; submit them separately");
    }
    let models: BTreeSet<Option<String>> = inputs.iter().map(Input::model_id).collect();
    if stated.describes_a_model() && models.len() > 1 {
        bail!(
            "--engine, --quant and --weights-dir describe one model; these runs used {}",
            models.iter().flatten().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(source)
}

impl Stated {
    /// Whether it states anything that holds for one model only.
    fn describes_a_model(&self) -> bool {
        [self.engine.is_some(), self.quant.is_some(), self.weights_dir.is_some(), self.engine_version.is_some()].contains(&true)
    }
}

/// What LM Studio says of a model, by its key.
pub type ProvenanceOf = Box<dyn Fn(&str) -> Option<darkmux_profiles::engine::ModelProvenance>>;

/// What the machine says of itself, read once.
pub struct Machine {
    pub chip: String,
    pub ram_gb: Option<u64>,
    pub accelerator: Option<String>,
    pub config: darkmux_platform::MachineConfig,
    pub os_release: Option<darkmux_platform::OsRelease>,
    pub registry: Option<darkmux_types::ProfileRegistry>,
    /// The darkmux installed here, and when its binary was written.
    pub installed: darkmux_types::BuildInfo,
    pub installed_at: Option<u64>,
    pub provenance: ProvenanceOf,
    /// What identifies this machine and its user (serial, hardware id, host
    /// and login names): read to check the bundle against, never sent.
    pub identifying: Vec<String>,
}

/// The platform's identifying strings and the user's names (`$USER`,
/// `$LOGNAME`, the home folder's, and git's `user.name`), each also by its
/// own words: a computer named "Kain’s MacBook Pro" is named in text by
/// "Kain" too. Words that name a product, not a person, are not counted.
pub fn identifying_here(
    platform: Vec<String>,
    user: Option<String>,
    logname: Option<String>,
    home: Option<PathBuf>,
    real_name: Option<String>,
) -> Vec<String> {
    let home = home.and_then(|h| h.file_name().map(|n| n.to_string_lossy().into_owned()));
    let mut out = Vec::new();
    let whole = platform
        .into_iter()
        .chain([user, logname, home, real_name].into_iter().flatten());
    for name in whole.filter(|n| !n.trim().is_empty()) {
        for one in std::iter::once(name.trim().to_string()).chain(name_words(&name)) {
            if !out.contains(&one) {
                out.push(one);
            }
        }
    }
    out
}

/// A name's words of three or more characters with a letter, possessive
/// dropped, less the words Apple names its machines with.
fn name_words(name: &str) -> Vec<String> {
    const PRODUCT: [&str; 10] = [
        "macbook", "pro", "air", "max", "mac", "mini", "studio", "imac", "ultra", "local",
    ];
    name.split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '.'))
        .map(|w| w.trim_end_matches("'s").trim_end_matches("\u{2019}s"))
        .filter(|w| {
            w.chars().count() >= 3
                && w.chars().any(char::is_alphabetic)
                && !PRODUCT.contains(&w.to_lowercase().as_str())
        })
        .map(str::to_string)
        .collect()
}

/// git's `user.name`, which a run's text may carry.
fn git_user_name() -> Option<String> {
    let out = Command::new("git").args(["config", "--get", "user.name"]).output().ok()?;
    user_name_from(out.status.success(), &out.stdout)
}

/// The name `git config --get user.name` printed: nothing when it failed or printed only space.
fn user_name_from(success: bool, stdout: &[u8]) -> Option<String> {
    let name = String::from_utf8_lossy(stdout).trim().to_string();
    (success && !name.is_empty()).then_some(name)
}

/// The chip, when it is Apple Silicon: the only results the site takes.
pub fn apple_chip(brand: Option<String>) -> Result<String> {
    match brand {
        Some(chip) if chip.starts_with("Apple M") => Ok(chip),
        other => bail!(
            "darkmux evals take Apple Silicon results only (macOS, or Omarchy Linux); this machine reports {}",
            other.filter(|c| !c.is_empty()).unwrap_or_else(|| "no Apple chip".to_string())
        ),
    }
}

impl Machine {
    fn here() -> Result<Self> {
        let platform = darkmux_platform::current();
        let lmstudio = darkmux_profiles::engine::lmstudio();
        Ok(Machine {
            chip: apple_chip(platform.cpu_brand())?,
            ram_gb: platform.ram_as_sold_gb(),
            accelerator: platform.gpu_api().map(str::to_string),
            config: platform.machine_config(),
            os_release: platform.os_release(),
            registry: darkmux_profiles::profiles::load_registry_quiet(None).ok().map(|l| l.registry),
            installed: darkmux_types::build_info(),
            installed_at: std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok()).and_then(|p| mtime_ms(&p)),
            provenance: Box::new(move |model| lmstudio.provenance(model).ok().flatten()),
            identifying: identifying_here(
                platform.identifying_strings(),
                std::env::var("USER").ok(),
                std::env::var("LOGNAME").ok(),
                dirs::home_dir(),
                git_user_name(),
            ),
        })
    }

    /// One run's bundle entry, and its note: its verdict, its model, what it does not report.
    fn bundle_one(&self, input: &Input, stated: &Stated, caches: &mut Caches, progress: &mut dyn FnMut(&str)) -> Result<(Value, String)> {
        let started = input.started();
        let mut why = Vec::new();
        let model_id = input.model_id().unwrap_or_default();
        let profile_model = self.profile_model(&input.manifest, &model_id);
        // A model the profile serves from an unmanaged endpoint is not LM Studio's (contract 1).
        let managed = profile_model.as_ref().is_none_or(|m| m.is_managed());
        let provenance = match stated.engine {
            None if managed => caches.provenance(&self.provenance, &model_id),
            _ => None,
        };
        if stated.engine.is_none() && provenance.is_none() {
            why.push("LM Studio does not list this model (pass --engine)".to_string());
        }
        let weights_path = stated.weights_dir.clone().or_else(|| provenance.as_ref().and_then(|p| p.weights_path.clone()));
        let (weights_hash, weights_changed) = match &weights_path {
            Some(path) => caches.weights(path, &input.run_id, &model_id, started, &mut why, progress)?,
            None => {
                why.push("weights unknown (pass --weights-dir)".to_string());
                (None, None)
            }
        };
        let ran_by = self.ran_by(&input.manifest, stated, started, &mut why);
        let os = self.os_as_of(started, &mut why);
        let engine = engine_of(stated, provenance.as_ref(), weights_changed, started, &mut why);
        let stack = Stack {
            chip: self.chip.clone(),
            ram_gb: self.ram_gb,
            accelerator: self.accelerator.clone(),
            os,
            engine: engine.engine,
            engine_backend: engine.backend,
            engine_version: engine.version,
            model: (!model_id.is_empty()).then(|| model_id.clone()),
            weights_hash,
            quant: engine.quant,
            context_length: profile_model.and_then(|m| m.n_ctx).map(u64::from),
            cpu_cores: self.config.cpu_clusters.clone(),
            gpu_cores: self.config.gpu_cores,
            model_id: self.config.model_id.clone(),
        };
        let run = bundle_run(&input.run_id, &input.manifest, &input.stats, &ran_by, &stack, &self.identifying);
        Ok((run, note_for(input, &model_id, &stack, &why)))
    }

    /// The model as the run's profile declares it, when it does.
    fn profile_model(&self, manifest: &Value, model_id: &str) -> Option<darkmux_types::ProfileModel> {
        let profile = self.registry.as_ref()?.profiles.get(manifest.get("profile").and_then(Value::as_str)?)?;
        profile.models.iter().find(|m| m.id == model_id || m.identifier.as_deref() == Some(model_id)).cloned()
    }

    /// The darkmux that ran it: stated, recorded in the manifest, or the
    /// installed one when it was installed before the run.
    fn ran_by(&self, manifest: &Value, stated: &Stated, started: Option<u64>, why: &mut Vec<String>) -> RanBy {
        if let Some(v) = &stated.darkmux_version {
            // A stated version says nothing of what built it.
            return RanBy { version: Some(v.clone()), ..Default::default() };
        }
        let recorded = manifest.get("darkmux").and_then(|d| serde_json::from_value::<darkmux_types::BuildInfo>(d.clone()).ok());
        let Some(b) = recorded.or_else(|| as_of_run(Some(self.installed.clone()), self.installed_at, started)) else {
            why.push("darkmux was updated after the run (pass --darkmux-version)".to_string());
            return RanBy::default();
        };
        RanBy { version: Some(b.version), commit: b.commit, build: b.build.map(build_word) }
    }

    fn os_as_of(&self, started: Option<u64>, why: &mut Vec<String>) -> Option<String> {
        let release = self.os_release.as_ref()?;
        let os = as_of_run(Some(release.name.clone()), release.installed_at_unix_ms, started);
        if os.is_none() {
            why.push("the OS may have been updated after the run".to_string());
        }
        os
    }
}

/// A run's note: its verdict, its model, what it does not report and why.
fn note_for(input: &Input, model_id: &str, stack: &Stack, why: &[String]) -> String {
    format!(
        "{}: {}, {model_id}; not reported: {}{}",
        input.run_id,
        if passed_from(&input.manifest) { "passed" } else { "failed" },
        stack.unreported().join(", "),
        if why.is_empty() { String::new() } else { format!(" ({})", why.join("; ")) }
    )
}

/// The engine fields of a stack: stated by hand, or read from LM Studio and
/// held only when they had not changed since the run.
#[derive(Default)]
struct EngineFields {
    engine: Option<String>,
    backend: Option<String>,
    version: Option<String>,
    quant: Option<String>,
}

fn engine_of(
    stated: &Stated,
    provenance: Option<&darkmux_profiles::engine::ModelProvenance>,
    weights_changed: Option<u64>,
    started: Option<u64>,
    why: &mut Vec<String>,
) -> EngineFields {
    if stated.engine.is_some() {
        return EngineFields {
            engine: stated.engine.clone(),
            backend: stated.engine_backend.clone(),
            version: stated.engine_version.clone(),
            quant: stated.quant.clone(),
        };
    }
    let Some(p) = provenance else {
        return EngineFields::default();
    };
    let version = as_of_run(p.runtime.clone(), p.runtime_installed_at_ms, started);
    if let (Some(r), None) = (&p.runtime, &version) {
        why.push(format!("{r} may not be the runtime that ran it"));
    }
    EngineFields {
        engine: Some("LM Studio".to_string()),
        backend: p.backend.clone(),
        version,
        // The quant is the weights': it holds for the run when they do.
        quant: as_of_run(p.quant.clone().filter(|q| !q.trim().is_empty()), weights_changed, started),
    }
}

/// What one submit has asked LM Studio and hashed, so each is done once.
#[derive(Default)]
struct Caches {
    provenance: BTreeMap<String, Option<darkmux_profiles::engine::ModelProvenance>>,
    /// A weights folder's hash, and its newest file's mtime.
    weights: BTreeMap<PathBuf, (Option<String>, Option<u64>)>,
}

impl Caches {
    fn provenance(&mut self, of: &ProvenanceOf, model_id: &str) -> Option<darkmux_profiles::engine::ModelProvenance> {
        self.provenance.entry(model_id.to_string()).or_insert_with(|| of(model_id)).clone()
    }

    /// The weights' hash as of the run (None when they changed after it), and when they last changed.
    fn weights(
        &mut self,
        path: &Path,
        run_id: &str,
        model_id: &str,
        started: Option<u64>,
        why: &mut Vec<String>,
        progress: &mut dyn FnMut(&str),
    ) -> Result<(Option<String>, Option<u64>)> {
        if !self.weights.contains_key(path) {
            let hashed = hash_weights(path, run_id, model_id, progress)?;
            self.weights.insert(path.to_path_buf(), hashed);
        }
        let (hash, changed) = self.weights[path].clone();
        let held = as_of_run(hash, changed, started);
        if held.is_none() {
            why.push("the weights changed after the run".to_string());
        }
        Ok((held, changed))
    }
}

/// A weights folder's hash, and its newest file's mtime.
fn hash_weights(path: &Path, run_id: &str, model_id: &str, progress: &mut dyn FnMut(&str)) -> Result<(Option<String>, Option<u64>)> {
    let files = model_files(path);
    if files.is_empty() {
        bail!("{run_id}: no weights found for {model_id}. Pass --weights-dir <the model's folder>");
    }
    progress(&format!("Hashing {} file(s) of {model_id} (a minute for a large model)…", files.len()));
    let mut hashed = Vec::new();
    let mut newest = Some(0u64);
    for f in &files {
        hashed.push((rel_path(path, f), sha256::file_hex(f).with_context(|| format!("hashing {}", f.display()))?));
        newest = newest.zip(mtime_ms(f)).map(|(a, b)| a.max(b));
    }
    Ok((weights_hash_of(&hashed), newest))
}

/// The fixture: a GitHub repo at the commit its checkout was on through the
/// runs, and its `.fixture.json` as it was at that commit.
pub fn fixture_at(source: &Path, starts: &[Option<u64>], stated_commit: Option<&str>) -> Result<(String, String, Value)> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(source).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if git(&["status", "--porcelain"]).as_deref() != Some("") {
        bail!("the fixture checkout has uncommitted changes (or is not a git checkout); its commit would not say what ran");
    }
    let commit_sha = match stated_commit {
        Some(c) => c.to_string(),
        None => commit_through(&git(&["reflog", "show", "--date=unix", "--format=%H %gd", "HEAD"]).unwrap_or_default(), starts)?,
    };
    let commit_sha = git(&["rev-parse", "--verify", "--quiet", &format!("{commit_sha}^{{commit}}")])
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("the fixture checkout has no commit {commit_sha}"))?;
    let repo = repo_from_remote(&git(&["remote", "get-url", "origin"]).unwrap_or_default())
        .ok_or_else(|| anyhow!("the fixture checkout's origin is not a GitHub repo"))?;
    let text = git(&["show", &format!("{commit_sha}:.fixture.json")])
        .ok_or_else(|| anyhow!("the fixture has no .fixture.json at {}", &commit_sha[..7]))?;
    let manifest: Value = serde_json::from_str(&text).context("reading the fixture's .fixture.json")?;
    Ok((repo, commit_sha, manifest))
}

/// The commit HEAD was on from the first run's start to the last's, by the
/// reflog. A run with no start time cannot be placed, so it is refused.
fn commit_through(reflog: &str, starts: &[Option<u64>]) -> Result<String> {
    let Some(starts) = starts.iter().copied().collect::<Option<Vec<u64>>>() else {
        bail!("the fixture commit: a run carries no start time; pass --commit <sha>");
    };
    let (Some(first), Some(last)) = (starts.iter().min(), starts.iter().max()) else {
        bail!("the fixture commit: the runs carry no start time; pass --commit <sha>");
    };
    commit_at_time(reflog, *first, *last).map_err(|e| anyhow!("the fixture commit: {e}"))
}

fn build_word(b: darkmux_types::BuildKind) -> String {
    match b {
        darkmux_types::BuildKind::Release => "release",
        darkmux_types::BuildKind::Dev => "dev",
    }
    .to_string()
}

fn fixture_field(manifest: &Value, key: &str) -> Option<String> {
    manifest.get("fixture")?.get(key)?.as_str().map(str::to_string)
}

fn mtime_ms(path: &Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// The model files under a folder (links followed, hidden ones left out), or the file itself.
pub fn model_files(path: &Path) -> Vec<PathBuf> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if meta.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(path) else {
        return found;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let child = e.path();
        match std::fs::metadata(&child) {
            Ok(m) if m.is_dir() => found.extend(model_files(&child)),
            Ok(m) if m.is_file() && is_model_file(&name) => found.push(child),
            _ => {}
        }
    }
    found
}

/// A file's path under the weights folder with `/` separators; the file's name when the weights are one file.
fn rel_path(root: &Path, file: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
        }
        _ => file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests;

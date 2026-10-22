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
/// 3 adds what built darkmux (`darkmuxCommit`, `darkmuxBuild`).
pub const BUNDLE_FORMAT_VERSION: u64 = 3;
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
    let at = |t: u64| entries.iter().find(|(_, when)| *when <= t).map(|(sha, _)| sha.clone());
    let Some(first) = at(first_start_ms) else {
        return Err("the reflog does not reach back to these runs; pass --commit <sha>".into());
    };
    if at(last_start_ms).as_deref() != Some(first.as_str()) {
        return Err("the fixture checkout moved between these runs; submit them separately".into());
    }
    Ok(first)
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
fn shareable(v: Option<&Value>, max: usize) -> Value {
    let Some(t) = v.and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
        return Value::Null;
    };
    if looks_like_path(t) {
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
pub fn verify_detail_from(manifest: &Value) -> Value {
    let Some(v) = manifest.get("verify").filter(|v| v.is_object()) else {
        return Value::Null;
    };
    let empty = Value::Object(Map::new());
    let g = v.get("work_gate").filter(|g| g.is_object()).unwrap_or(&empty);
    let added = g.get("tests_added").and_then(Value::as_f64).filter(|n| n.fract() == 0.0 && n.abs() <= MAX_COUNT);
    json!({
        "summary": shareable(v.get("details"), 500),
        "forcedFailureReason": shareable(g.get("forced_failure_reason"), 500),
        "baselineTests": count(g.get("baseline_test_count")),
        "testsTotal": count(g.get("tests_total")),
        "testsPassed": count(g.get("tests_passed")),
        "testsAdded": added.map_or(Value::Null, |n| json!(n as i64)),
        "testsFailed": count(g.get("tests_failed")),
        "sandboxChanged": flag(g.get("sandbox_changed")),
        "coveragePct": measure(g.get("coverage_pct"), 100.0),
        "coverageMinPct": measure(g.get("coverage_min_pct"), 100.0),
        "commandPassed": flag(g.get("command_passed")),
    })
}

/// The detail beyond the basics. Host figures need busy samples: with none
/// they are null, never zero.
pub fn detail_from(manifest: &Value, stats: &Value) -> Value {
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
        "verify": verify_detail_from(manifest),
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
pub fn bundle_run(run_id: &str, manifest: &Value, stats: &Value, ran_by: &RanBy, stack: &Stack) -> Value {
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
        "stack": stack.to_json(),
        "detail": detail_from(manifest, stats),
    })
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
    Ok(json!({
        "format": BUNDLE_FORMAT,
        "formatVersion": BUNDLE_FORMAT_VERSION,
        "fixture": { "repo": repo, "commitSha": commit_sha, "name": name, "version": version, "manifest": fixture_manifest },
        "runs": runs,
    }))
}

/// What the site's strict parser (`parseBundle`) would refuse in a bundle,
/// by where: the bounds and forms a bundle written here can miss (a stated
/// value, a chip name, a repeated run). Empty means it takes it. The site
/// stays the authority; this saves an upload it would refuse.
pub fn site_refusals(bundle: &Value) -> Vec<String> {
    fn re(p: &str) -> regex::Regex {
        regex::Regex::new(p).expect("valid pattern")
    }
    let semverish = re(r"^[0-9A-Za-z.+-]{1,40}$");
    let hex64 = re(r"^[0-9a-f]{64}$");
    let hex40 = re(r"^[0-9a-f]{40}$");
    let commit = re(r"^[0-9a-f]{7,40}$");
    let repo = re(r"^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$");
    let apple = re(r"^Apple M\d{1,2}(?: (?:Pro|Max|Ultra))?$");
    let os = re(r"^(?:macOS \d[0-9.]*|Omarchy\b[^\n]*)$");
    let mut errs = Vec::new();
    // `text()`: a non-blank string within `max` UTF-16 units, in `form` when given; null when `nullable`.
    let mut text = |v: &Value, at: &str, max: usize, form: Option<&regex::Regex>, nullable: bool| match v {
        Value::Null if nullable => {}
        Value::String(s) if !s.trim().is_empty() => {
            if s.encode_utf16().count() > max {
                errs.push(format!("{at}: longer than {max}"));
            } else if form.is_some_and(|f| !f.is_match(s)) {
                errs.push(format!("{at}: not in the form the site takes ({s})"));
            }
        }
        _ => errs.push(format!("{at}: expected a string{}", if nullable { " or null" } else { "" })),
    };
    let f = &bundle["fixture"];
    text(&f["repo"], "fixture.repo", 200, Some(&repo), false);
    text(&f["commitSha"], "fixture.commitSha", 200, Some(&hex40), false);
    text(&f["name"], "fixture.name", 60, None, false);
    text(&f["version"], "fixture.version", 200, Some(&semverish), false);
    let mut keys = BTreeSet::new();
    let mut numbers = Vec::new();
    let runs = bundle["runs"].as_array().cloned().unwrap_or_default();
    for (i, r) in runs.iter().enumerate() {
        let at = |k: &str| format!("runs[{i}].{k}");
        if !keys.insert(r["key"].as_str().unwrap_or_default().to_string()) {
            numbers.push(format!("runs[{i}]: the same run twice"));
        }
        text(&r["darkmuxVersion"], &at("darkmuxVersion"), 200, Some(&semverish), true);
        text(&r["darkmuxCommit"], &at("darkmuxCommit"), 200, Some(&commit), true);
        let s = &r["stack"];
        let st = |k: &str| format!("runs[{i}].stack.{k}");
        text(&s["chip"], &st("chip"), 200, Some(&apple), false);
        text(&s["accelerator"], &st("accelerator"), 32, None, true);
        text(&s["os"], &st("os"), 80, Some(&os), true);
        for k in ["engine", "engineBackend", "engineVersion"] {
            text(&s[k], &st(k), 80, None, true);
        }
        text(&s["model"], &st("model"), 200, None, true);
        text(&s["weightsHash"], &st("weightsHash"), 200, Some(&hex64), true);
        text(&s["quant"], &st("quant"), 40, None, true);
        for (k, max) in [("activeSeconds", MAX_SECONDS), ("tokensPerSecond", 100_000.0)] {
            if r[k].as_f64().is_some_and(|n| !(0.0..=max).contains(&n)) {
                numbers.push(format!("{}: outside 0 to {max}", at(k)));
            }
        }
    }
    errs.extend(numbers);
    if runs.is_empty() || runs.len() > 500 {
        errs.push("runs: 1 to 500 runs a bundle".to_string());
    }
    if serde_json::to_string(&f["manifest"]).map_or(0, |t| t.len()) > 64 * 1024 {
        errs.push("fixture.manifest: larger than 64 KB".to_string());
    }
    errs
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
    if runs.is_empty() {
        bail!("name at least one lab run");
    }
    if let Some(twice) = runs.iter().enumerate().find_map(|(i, r)| runs[..i].contains(r).then_some(r)) {
        bail!("{twice} is named twice");
    }
    let platform = darkmux_platform::current();
    let chip = platform.cpu_brand().unwrap_or_default();
    if !chip.starts_with("Apple M") {
        bail!(
            "darkmux evals take Apple Silicon results only (macOS, or Omarchy Linux); this machine reports {}",
            if chip.is_empty() { "no Apple chip" } else { chip.as_str() }
        );
    }
    let ram_gb = platform.ram_as_sold_gb();
    let os_release = platform.os_release();
    let registry = darkmux_profiles::profiles::load_registry_quiet(None).ok().map(|l| l.registry);

    // Read and check every run first, so a refusal names them all.
    let mut inputs = Vec::new();
    let mut refused = Vec::new();
    for run in runs {
        let dir = crate::lab::inspect::resolve_run_dir(run);
        let run_id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| run.clone());
        let manifest: Value = match std::fs::read_to_string(dir.join(crate::lab::manifest::MANIFEST_FILE)) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{run_id}: reading manifest.json"))?,
            Err(_) => {
                refused.push(format!("{run_id}: no manifest.json, so it ended before the eval ran"));
                continue;
            }
        };
        let stats = match crate::lab::stats::compute_from_dir(&dir, &darkmux_types::config_access::flows_dir()) {
            Ok(s) => serde_json::to_value(s)?,
            Err(e) => {
                refused.push(format!("{run_id}: its numbers cannot be read ({e:#})"));
                continue;
            }
        };
        match refusal_for(&manifest, &stats) {
            Some(why) => refused.push(format!("{run_id}: {why}")),
            None => inputs.push((run_id, manifest, stats)),
        }
    }
    if !refused.is_empty() {
        bail!("not submitted:\n  {}", refused.join("\n  "));
    }

    // One fixture checkout, one baseline.
    let sources: BTreeSet<Option<String>> = inputs.iter().map(|(_, m, _)| fixture_field(m, "source_path")).collect();
    let baselines: BTreeSet<Option<String>> = inputs.iter().map(|(_, m, _)| fixture_field(m, "baseline_hash")).collect();
    let source = match sources.iter().next() {
        Some(Some(s)) if sources.len() == 1 => PathBuf::from(s),
        _ => bail!("submit runs of one fixture checkout at a time (each run's manifest names it)"),
    };
    if baselines.len() != 1 {
        bail!("these runs started from different fixture contents; submit them separately");
    }
    let models: BTreeSet<Option<String>> =
        inputs.iter().map(|(_, _, s)| model_id_from(s.get("model").and_then(Value::as_str))).collect();
    let by_hand = stated.engine.is_some() || stated.quant.is_some() || stated.weights_dir.is_some() || stated.engine_version.is_some();
    if by_hand && models.len() > 1 {
        bail!(
            "--engine, --quant and --weights-dir describe one model; these runs used {}",
            models.iter().flatten().cloned().collect::<Vec<_>>().join(", ")
        );
    }

    let installed = darkmux_types::build_info();
    let installed_at = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(p).ok()).and_then(|p| mtime_ms(&p));
    let lmstudio = darkmux_profiles::engine::lmstudio();
    let mut provenance_cache: BTreeMap<String, Option<darkmux_profiles::engine::ModelProvenance>> = BTreeMap::new();
    let mut weights_cache: BTreeMap<PathBuf, (Option<String>, Option<u64>)> = BTreeMap::new();

    let mut bundled = Vec::new();
    let mut notes = Vec::new();
    let mut starts = Vec::new();
    for (run_id, manifest, stats) in &inputs {
        let started = stats.get("started_at_unix_ms").and_then(Value::as_u64);
        starts.extend(started);
        let mut why = Vec::new();
        let model_id = model_id_from(stats.get("model").and_then(Value::as_str)).unwrap_or_default();
        let profile_model = registry.as_ref().and_then(|r| {
            let profile = r.profiles.get(manifest.get("profile").and_then(Value::as_str)?)?;
            profile.models.iter().find(|m| m.id == model_id || m.identifier.as_deref() == Some(model_id.as_str())).cloned()
        });
        // A model the profile serves from an unmanaged endpoint is not LM Studio's (contract 1).
        let managed = profile_model.as_ref().is_none_or(|m| m.is_managed());
        let provenance = if stated.engine.is_none() && managed {
            provenance_cache
                .entry(model_id.clone())
                .or_insert_with(|| lmstudio.provenance(&model_id).ok().flatten())
                .clone()
        } else {
            None
        };
        if stated.engine.is_none() && provenance.is_none() {
            why.push("LM Studio does not list this model (pass --engine)".to_string());
        }

        let weights_path = stated.weights_dir.clone().or_else(|| provenance.as_ref().and_then(|p| p.weights_path.clone()));
        let mut weights_changed = None;
        let weights_hash = match &weights_path {
            Some(path) => {
                if !weights_cache.contains_key(path) {
                    let files = model_files(path);
                    if files.is_empty() {
                        bail!("{run_id}: no weights found for {model_id}. Pass --weights-dir <the model's folder>");
                    }
                    progress(&format!("Hashing {} file(s) of {model_id} (a minute for a large model)…", files.len()));
                    let mut hashed = Vec::new();
                    let mut newest = Some(0u64);
                    for f in &files {
                        let rel = rel_path(path, f);
                        hashed.push((rel, sha256::file_hex(f).with_context(|| format!("hashing {}", f.display()))?));
                        newest = newest.zip(mtime_ms(f)).map(|(a, b)| a.max(b));
                    }
                    weights_cache.insert(path.clone(), (weights_hash_of(&hashed), newest));
                }
                let (hash, changed) = weights_cache[path].clone();
                weights_changed = changed;
                let held = as_of_run(hash, changed, started);
                if held.is_none() {
                    why.push("the weights changed after the run".to_string());
                }
                held
            }
            None => {
                why.push("weights unknown (pass --weights-dir)".to_string());
                None
            }
        };

        let recorded = manifest.get("darkmux").and_then(|d| serde_json::from_value::<darkmux_types::BuildInfo>(d.clone()).ok());
        let ran_by = match (&stated.darkmux_version, recorded) {
            // A stated version says nothing of what built it.
            (Some(v), _) => RanBy { version: Some(v.clone()), ..Default::default() },
            (None, Some(b)) => RanBy { version: Some(b.version), commit: b.commit, build: b.build.map(build_word) },
            (None, None) => match as_of_run(Some(&installed), installed_at, started) {
                Some(b) => RanBy { version: Some(b.version.clone()), commit: b.commit.clone(), build: b.build.map(build_word) },
                None => {
                    why.push("darkmux was updated after the run (pass --darkmux-version)".to_string());
                    RanBy::default()
                }
            },
        };

        let os = as_of_run(os_release.as_ref().map(|r| r.name.clone()), os_release.as_ref().and_then(|r| r.installed_at_unix_ms), started);
        if os_release.is_some() && os.is_none() {
            why.push("the OS may have been updated after the run".to_string());
        }
        let engine_version = match &stated.engine {
            Some(_) => stated.engine_version.clone(),
            None => {
                let runtime = provenance.as_ref().and_then(|p| p.runtime.clone());
                let held = as_of_run(runtime.clone(), provenance.as_ref().and_then(|p| p.runtime_installed_at_ms), started);
                if let (Some(r), None) = (&runtime, &held) {
                    why.push(format!("{r} may not be the runtime that ran it"));
                }
                held
            }
        };
        let stack = Stack {
            chip: chip.clone(),
            ram_gb,
            accelerator: platform.gpu_api().map(str::to_string),
            os,
            engine: stated.engine.clone().or_else(|| provenance.as_ref().map(|_| "LM Studio".to_string())),
            engine_backend: match &stated.engine {
                Some(_) => stated.engine_backend.clone(),
                None => provenance.as_ref().and_then(|p| p.backend.clone()),
            },
            engine_version,
            model: (!model_id.is_empty()).then(|| model_id.clone()),
            weights_hash,
            // The quant is the weights': it holds for the run when they do.
            quant: match &stated.engine {
                Some(_) => stated.quant.clone(),
                None => as_of_run(
                    provenance.as_ref().and_then(|p| p.quant.clone()).filter(|q| !q.trim().is_empty()),
                    weights_changed,
                    started,
                ),
            },
            context_length: profile_model.and_then(|m| m.n_ctx).map(u64::from),
        };
        bundled.push(bundle_run(run_id, manifest, stats, &ran_by, &stack));
        let missing = stack.unreported();
        notes.push(format!(
            "{run_id}: {}, {model_id}; not reported: {}{}",
            if passed_from(manifest) { "passed" } else { "failed" },
            missing.join(", "),
            if why.is_empty() { String::new() } else { format!(" ({})", why.join("; ")) }
        ));
    }

    let (repo, commit_sha, fixture_manifest) = fixture_at(&source, &starts, stated.commit.as_deref())?;
    let bundle = assemble(&repo, &commit_sha, &fixture_manifest, bundled)?;

    let mut paths = Vec::new();
    paths_in(&bundle, "bundle", &mut paths);
    if !paths.is_empty() {
        bail!("the bundle would carry a file path, which is never shared; found at {}", paths.join(", "));
    }
    let refused = site_refusals(&bundle);
    if !refused.is_empty() {
        bail!("the site would refuse this bundle:\n  {}", refused.join("\n  "));
    }
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

/// The fixture: a GitHub repo at the commit its checkout was on through the
/// runs, and its `.fixture.json` as it was at that commit.
pub fn fixture_at(source: &Path, starts: &[u64], stated_commit: Option<&str>) -> Result<(String, String, Value)> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(source).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if git(&["status", "--porcelain"]).as_deref() != Some("") {
        bail!("the fixture checkout has uncommitted changes (or is not a git checkout); its commit would not say what ran");
    }
    let commit_sha = match stated_commit {
        Some(c) => c.to_string(),
        None => {
            let reflog = git(&["reflog", "show", "--date=unix", "--format=%H %gd", "HEAD"]).unwrap_or_default();
            let (first, last) = match (starts.iter().min(), starts.iter().max()) {
                (Some(a), Some(b)) => (*a, *b),
                _ => bail!("the fixture commit: the runs carry no start time; pass --commit <sha>"),
            };
            commit_at_time(&reflog, first, last).map_err(|e| anyhow!("the fixture commit: {e}"))?
        }
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

//! `GET /panel/:id` — allowlisted read-only CLI command views (#1569 packet B).
//!
//! The keystone mechanism of the CLI-panels epic (#1568): render a command's
//! own ANSI output in the viewer instead of re-implementing its logic in
//! JavaScript. The point is not saved code — it is that **twin-drift becomes
//! structurally impossible**: the viewer's missions board was a JS
//! re-implementation of `mission status` that had already diverged (#1561,
//! where the one row needing attention rendered as the one row with no
//! signal). A panel cannot diverge from the CLI because it IS the CLI.
//!
//! ## Invocation discipline
//!
//! - **Compile-time argv table.** The client sends an opaque id; the server
//!   maps it through [`panel_spec`]'s `match`. No shell, no PATH resolution
//!   (`std::env::current_exe()` — the daemon re-invokes its own binary), and
//!   **no user-supplied arguments**: the one client-influenced value is a
//!   render width, clamped and passed as `COLUMNS` env, never argv.
//! - **Read-only allowlist, by doctrine.** Nothing that dispatches a model
//!   (observability paths contain zero model dispatches, #1286) and nothing
//!   that mutates. The worst case of a bug here is a wrong reading, never a
//!   wrong action. If the list grows past a handful it is becoming a new
//!   accretion surface — that is the signal to stop, not to add a config.
//! - **`doctor` is manual-run only.** It PROBES (spawns checks, touches
//!   `lms`, reads disk) — an auto-polling doctor panel open on the measured
//!   host during a canon run is the observer joining the observed (#1286).
//!   Its entry is marked `auto_refresh: false` and the viewer must honor it;
//!   the TTL of 0 means even an explicit re-request never serves stale.
//!
//! ## Response shape
//!
//! `{ panel, argv, captured_ts_ms, gather_ms, exit_code, ansi_text,
//!    cache_ttl_ms, age_ms, auto_refresh }` — metadata AROUND the text,
//! never extraction FROM it (the moment the server parses the output, the
//! twin-drift this exists to kill is reborn server-side). `gather_ms` stamps
//! the observer's own cost into the payload (#1286 constraint 3), and
//! `cache_ttl_ms`/`age_ms` make the staleness story verifiable rather than
//! assumed (constraint 4).
//!
//! ## Caching + single-flight
//!
//! A per-panel TTL cache bounds the cost of an enthusiastic client to one
//! spawn per [`PANEL_CACHE_TTL`] per panel, and a per-panel single-flight
//! lock collapses concurrent misses into one spawn. Cache entries are
//! whole-response; `age_ms` reports how stale a served entry is.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{current_millis, AppState};

/// TTL for cached panel output. Short: panels are "state right now" views,
/// and the underlying commands are cheap disk reads — the cache exists to
/// bound a polling client, not to make data old.
pub(crate) const PANEL_CACHE_TTL: Duration = Duration::from_millis(3_000);

/// Hard wall-clock bound on one panel spawn. These are fast read-only CLI
/// verbs (disk + local probes); anything hitting this bound is wedged, and a
/// wedged child must never wedge the daemon route (#1570/#1573's class —
/// the same week this module was written, the crate sweep found two more
/// instances of unbounded external calls; this one is born bounded).
const PANEL_SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// One allowlist entry: the argv after the binary, whether the viewer may
/// auto-refresh it, and the cache TTL applied.
pub(crate) struct PanelSpec {
    pub(crate) argv: &'static [&'static str],
    pub(crate) auto_refresh: bool,
    pub(crate) cache_ttl: Duration,
}

/// The allowlist. Deliberately short — see the module doc. Ids are kebab-case
/// and OPAQUE to the client; the mapping to argv lives here and only here.
pub(crate) fn panel_spec(id: &str) -> Option<PanelSpec> {
    let (argv, auto_refresh, ttl): (&'static [&'static str], bool, Duration) = match id {
        "mission-status" => (&["mission", "status"], true, PANEL_CACHE_TTL),
        "role-list" => (&["role", "list"], true, PANEL_CACHE_TTL),
        "machine-status" => (&["machine", "status"], true, PANEL_CACHE_TTL),
        "config-list" => (&["config", "list"], true, PANEL_CACHE_TTL),
        "flow-status" => (&["flow", "status"], true, PANEL_CACHE_TTL),
        "lab-fixture-list" => (&["lab", "fixture", "list"], true, PANEL_CACHE_TTL),
        // Manual-run only (#1286): never auto-refreshed by the viewer, and
        // TTL 0 so an explicit re-run is always a real run.
        "doctor" => (&["doctor"], false, Duration::ZERO),
        _ => return None,
    };
    Some(PanelSpec { argv, auto_refresh, cache_ttl: ttl })
}

/// Whole-response cache entry.
struct CacheEntry {
    body: serde_json::Value,
    captured: Instant,
}

/// Panel state carried on [`AppState`]: the TTL cache plus the per-panel
/// single-flight locks. Both keyed by the panel id (a `&'static str` from
/// the allowlist, so no unbounded key growth).
#[derive(Clone, Default)]
pub(crate) struct PanelState {
    cache: Arc<tokio::sync::Mutex<HashMap<&'static str, CacheEntry>>>,
    flights: Arc<tokio::sync::Mutex<HashMap<&'static str, Arc<tokio::sync::Mutex<()>>>>>,
}

#[derive(Deserialize)]
pub(crate) struct PanelParams {
    /// Render width for the child's `COLUMNS`. Clamped hard — this is the
    /// ONE client-influenced input, and it never reaches argv.
    cols: Option<u16>,
}

fn clamp_cols(cols: Option<u16>) -> u16 {
    cols.unwrap_or(100).clamp(60, 200)
}

pub(crate) async fn panel_handler(
    Path(id): Path<String>,
    Query(params): Query<PanelParams>,
    State(state): State<AppState>,
) -> Result<axum::Json<serde_json::Value>, (StatusCode, String)> {
    let Some(spec) = panel_spec(&id) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("unknown panel \"{id}\" — panels are a fixed allowlist, not arbitrary commands\n"),
        ));
    };
    // Canonical id from the table (a &'static str for the cache keys).
    let id: &'static str = match panel_spec_key(&id) {
        Some(k) => k,
        None => unreachable!("panel_spec above already matched"),
    };
    let cols = clamp_cols(params.cols);

    // Serve fresh-enough cache without spawning.
    if let Some(body) = cached_if_fresh(&state.panels, id, spec.cache_ttl).await {
        return Ok(axum::Json(body));
    }

    // Single-flight: collapse concurrent misses into one spawn.
    let flight = {
        let mut flights = state.panels.flights.lock().await;
        flights.entry(id).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    };
    let _guard = flight.lock().await;
    // Re-check under the flight lock — a concurrent request may have filled
    // the cache while this one waited.
    if let Some(body) = cached_if_fresh(&state.panels, id, spec.cache_ttl).await {
        return Ok(axum::Json(body));
    }

    let started = Instant::now();
    let exe = std::env::current_exe().map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("resolving current_exe: {e}\n"))
    })?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(spec.argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Styled output on a pipe is the whole point (see style.rs's
        // CLICOLOR_FORCE tier). NO_COLOR is removed for the CHILD only: an
        // operator's NO_COLOR governs their terminal, and a panel is not
        // their terminal — leaving it set would silently blank every panel.
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .env("COLUMNS", cols.to_string())
        // A child must never inherit the daemon's own serve lifecycle env in
        // a way that could confuse it; everything else (DARKMUX_HOME, dirs)
        // is deliberately inherited — the panel must see the same state the
        // operator's own shell would.
        .kill_on_drop(true);

    let output = tokio::time::timeout(PANEL_SPAWN_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "panel \"{id}\" timed out after {}s — the CLI verb is wedged; \
                     the daemon killed it (kill_on_drop)\n",
                    PANEL_SPAWN_TIMEOUT.as_secs()
                ),
            )
        })?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("spawning panel \"{id}\": {e}\n")))?;

    let gather_ms = started.elapsed().as_millis() as u64;
    let ansi_text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_tail: String = String::from_utf8_lossy(&output.stderr)
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    let body = serde_json::json!({
        "panel": id,
        "argv": spec.argv,
        "captured_ts_ms": current_millis(),
        "gather_ms": gather_ms,
        "exit_code": output.status.code(),
        "ansi_text": ansi_text,
        // Non-empty only when something went to stderr — surfaced so a
        // failing verb is diagnosable from the panel itself, not just logs.
        "stderr_tail": stderr_tail,
        "cols": cols,
        "cache_ttl_ms": spec.cache_ttl.as_millis() as u64,
        "age_ms": 0,
        "auto_refresh": spec.auto_refresh,
    });

    if !spec.cache_ttl.is_zero() {
        let mut cache = state.panels.cache.lock().await;
        cache.insert(id, CacheEntry { body: body.clone(), captured: Instant::now() });
    }
    Ok(axum::Json(body))
}

/// The canonical `&'static str` key for a matched id.
fn panel_spec_key(id: &str) -> Option<&'static str> {
    match id {
        "mission-status" => Some("mission-status"),
        "role-list" => Some("role-list"),
        "machine-status" => Some("machine-status"),
        "config-list" => Some("config-list"),
        "flow-status" => Some("flow-status"),
        "lab-fixture-list" => Some("lab-fixture-list"),
        "doctor" => Some("doctor"),
        _ => None,
    }
}

/// Serve the cached body if it is within `ttl`, with `age_ms` restamped so
/// the client can SEE it got a cached copy (#1286 constraint 4 — cadence and
/// staleness are recorded knobs, never silent).
async fn cached_if_fresh(
    panels: &PanelState,
    id: &'static str,
    ttl: Duration,
) -> Option<serde_json::Value> {
    if ttl.is_zero() {
        return None;
    }
    let cache = panels.cache.lock().await;
    let entry = cache.get(id)?;
    let age = entry.captured.elapsed();
    if age > ttl {
        return None;
    }
    let mut body = entry.body.clone();
    body["age_ms"] = serde_json::json!(age.as_millis() as u64);
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_closed_and_argv_is_fixed() {
        // The table maps ids to argv; anything else is a 404, never a spawn.
        assert!(panel_spec("mission-status").is_some());
        assert!(panel_spec("doctor").is_some());
        assert!(panel_spec("rm -rf /").is_none());
        assert!(panel_spec("mission status").is_none(), "argv-looking ids are not ids");
        assert!(panel_spec("").is_none());
        // Every spec'd id has a canonical key and vice versa.
        for id in ["mission-status", "role-list", "machine-status", "config-list", "flow-status", "lab-fixture-list", "doctor"] {
            assert!(panel_spec(id).is_some(), "{id}");
            assert_eq!(panel_spec_key(id), Some(id));
        }
    }

    #[test]
    fn doctor_is_manual_only_and_uncached() {
        let d = panel_spec("doctor").unwrap();
        assert!(!d.auto_refresh, "doctor probes — auto-polling it is the observer joining the observed");
        assert!(d.cache_ttl.is_zero(), "an explicit doctor run must be a real run");
        // …and every other panel IS auto-refreshable with a real TTL.
        for id in ["mission-status", "role-list", "machine-status", "config-list", "flow-status", "lab-fixture-list"] {
            let s = panel_spec(id).unwrap();
            assert!(s.auto_refresh, "{id}");
            assert_eq!(s.cache_ttl, PANEL_CACHE_TTL, "{id}");
        }
    }

    #[test]
    fn cols_clamped_hard() {
        assert_eq!(clamp_cols(None), 100);
        assert_eq!(clamp_cols(Some(10)), 60);
        assert_eq!(clamp_cols(Some(5000)), 200);
        assert_eq!(clamp_cols(Some(120)), 120);
    }
}

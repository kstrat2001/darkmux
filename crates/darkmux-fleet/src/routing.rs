//! Fleet dispatch routing — local-vs-`--machine` selection, queue dispatch, and completion waiting.

use crate::queue::extract_field;
use crate::{publish_job, WorkJob};
use anyhow::{anyhow, Context, Result};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── Client-side --wait wrapper (PR-C.3) ──────────────────────────────
//
// After `publish_job` returns, the dispatching client can either return
// immediately (fire-and-forget; the operator polls flow stream from
// elsewhere) OR block until the runner's `dispatch.complete` flow
// record lands for the matching `session_id`. The `--wait` wrapper
// implements the blocking form by **polling the Redis flow stream**
// (`darkmux:flow`) — NOT the local file, because in a cross-machine
// dispatch the completion record lands on the RUNNER's local file,
// not the publisher's. The Redis stream is the only substrate both
// machines write to (via the shared TeeSink → RedisSink composition).
//
// This is the architectural pivot that makes cross-machine `--wait`
// actually work — a CRITICAL fix surfaced in PR-C.3 review where the
// initial local-file-polling implementation would always time out.

/// Poll interval for the `wait_for_completion` Redis polling. (#246 PR-C.3)
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Cap on XRANGE entries scanned per poll iteration. Matches the typical
/// Redis stream MAXLEN of 10000 (set via `DARKMUX_REDIS_MAXLEN`); covers
/// a full re-scan per poll without pagination. If the stream legitimately
/// exceeds this in a single poll window the caller will see a delayed
/// completion (corrects on the next iteration). (#246 PR-C.3)
const WAIT_XRANGE_COUNT: usize = 10000;

/// Result of `wait_for_completion`. Outcome is the dispatch's
/// `result_class` from the flow record's payload — typically `"ok"` or
/// `"error"` (see `crew::dispatch::dispatch` for the canonical values).
/// `wall_ms` is from the same payload.
#[derive(Debug, Clone)]
pub struct CompletionResult {
    pub session_id: String,
    pub result_class: String,
    pub wall_ms: Option<u64>,
    /// Raw payload JSON for downstream consumers that want richer
    /// fields (e.g. `exit_code`, `total_turns`, `result_class`).
    /// Currently surfaced via `--json` only (PR-D mission dispatch
    /// reads this for phase-level aggregation).
    #[allow(dead_code)] // consumed by PR-D mission dispatch fan-out aggregator
    pub payload: Option<serde_json::Value>,
}

/// Block until a `dispatch.complete` flow record for `session_id` lands
/// in the Redis flow stream, or `timeout` elapses. Returns the
/// completion result on success; bails when the timeout fires (the job
/// may still be running on the remote runner — the operator can re-tail
/// via `darkmux flow tail --session <id>` to keep watching).
///
/// Polls the Redis stream (default `darkmux:flow`; override via
/// `DARKMUX_REDIS_STREAM`) every `WAIT_POLL_INTERVAL` (250ms). Each
/// poll runs `XRANGE - + COUNT 10000` and scans for an entry whose
/// `record` field matches both the target `session_id` AND a
/// `dispatch complete` action. The full-scan-per-poll trades CPU for
/// correctness — the stream is bounded by `DARKMUX_REDIS_MAXLEN`
/// (typically 10000), so the worst-case scan is bounded too. v1 cost
/// model is fine; PR-E may add last-id tracking for efficiency.
///
/// **Why poll Redis, not the local file:** in a cross-machine dispatch
/// the runner writes the `dispatch.complete` record to its OWN local
/// `~/.darkmux/flows/<day>.jsonl`, not the publisher's. The Redis
/// stream is the only substrate both machines write to (the shared
/// `darkmux:flow` stream via the TeeSink → RedisSink composition).
/// (CRITICAL fix from PR-C.3 review)
pub fn wait_for_completion(
    redis_url: &darkmux_flow::RawRedisUrl,
    session_id: &str,
    timeout: Duration,
) -> Result<CompletionResult> {
    let client = redis::Client::open(redis_url.expose_for_probe())
        .with_context(|| format!("opening Redis to wait for completion of {session_id}"))?;
    let mut conn = client
        .get_connection()
        .with_context(|| format!("connecting to Redis to wait for completion of {session_id}"))?;

    // (#875) env > config.redis.stream > default, via config_access.
    let stream = darkmux_types::config_access::redis_stream();

    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "wait_for_completion: no dispatch.complete for session_id={session_id} \
                 within {}s in Redis stream {stream}. The job may still be running on the \
                 runner — tail `darkmux flow tail --session {session_id}` to keep watching.",
                timeout.as_secs()
            ));
        }

        // (#809) XREVRANGE (newest-first) — the completion record we're
        // waiting for is by definition RECENT. The old oldest-first XRANGE
        // dropped the newest entries once the stream rode at its `MAXLEN ~`
        // cap (XLEN floats above the cap; trimming is lazy), so a saturated
        // stream made this wait MISS the completion entirely and time out.
        // Scan order doesn't matter for a find; newest-first also returns
        // the match in the first entries scanned.
        let raw: redis::Value = redis::cmd("XREVRANGE")
            .arg(&stream)
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(WAIT_XRANGE_COUNT)
            .query(&mut conn)
            .with_context(|| format!("XREVRANGE on flow stream {stream}"))?;

        if let Some(result) = scan_flow_entries_for_completion(&raw, session_id)? {
            return Ok(result);
        }

        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

/// Walk XRANGE's nested-array response, scanning each entry's `record`
/// field for a `dispatch.complete` event matching `session_id`. Returns
/// the first match's CompletionResult, or `None` if no entry matches.
/// Pure function; unit-testable independent of live Redis.
pub(crate) fn scan_flow_entries_for_completion(
    raw: &redis::Value,
    session_id: &str,
) -> Result<Option<CompletionResult>> {
    use redis::Value as V;
    // Expected shape: Array([Array([id, Array([k, v, k, v, ...])])])
    let entries = match raw {
        V::Array(a) => a,
        V::Nil => return Ok(None),
        other => return Err(anyhow!("XRANGE: unexpected outer shape: {other:?}")),
    };
    for entry in entries {
        let parts = match entry {
            V::Array(p) => p,
            _ => continue,
        };
        if parts.len() < 2 {
            continue;
        }
        let fields = match &parts[1] {
            V::Array(f) => f,
            _ => continue,
        };
        let Some(record_str) = extract_field(fields, "record") else {
            continue;
        };
        if let Some(result) = match_completion(&record_str, session_id) {
            return Ok(Some(result));
        }
    }
    Ok(None)
}

/// Parse one record JSON; return `Some(CompletionResult)` when it's a
/// dispatch-completion event for the target `session_id`. Pure function;
/// unit-testable without live Redis.
///
/// Canonical action shape is `"dispatch complete"` (space, NOT dot) —
/// that's what every production emit site uses today
/// (`dispatch_internal::dispatch`, the internal-runtime path). The
/// dotted form `"dispatch.complete"` is
/// accepted as forward-compat in case a future cleanup migrates the
/// emitters to match the dotted-per-action-type convention of
/// `dispatch.turn` / `dispatch.tool` / etc. (PR-C.3 review HIGH-2)
pub(crate) fn match_completion(line: &str, target_session_id: &str) -> Option<CompletionResult> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let action = value.get("action").and_then(|v| v.as_str())?;
    if action != "dispatch complete" && action != "dispatch.complete" {
        return None;
    }
    let session = value.get("session_id").and_then(|v| v.as_str())?;
    if session != target_session_id {
        return None;
    }
    let payload = value.get("payload").cloned();
    let result_class = payload
        .as_ref()
        .and_then(|p| p.get("result_class"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let wall_ms = payload
        .as_ref()
        .and_then(|p| p.get("wall_ms"))
        .and_then(|v| v.as_u64());
    Some(CompletionResult {
        session_id: target_session_id.to_string(),
        result_class,
        wall_ms,
        payload,
    })
}

/// Convenience constructor — build a WorkJob from the components the
/// dispatching client has on hand. Centralizes the "always set X to Y"
/// defaults (attempt=1, published_at=now, etc.) so PR-C.3 doesn't
/// duplicate the shape.
#[allow(clippy::too_many_arguments)]
pub fn build_work_job(
    target_machine: Option<String>,
    role_id: String,
    message: String,
    session_id: String,
    deliver: Option<String>,
    workdir: Option<String>,
    phase_id: Option<String>,
    runtime: darkmux_crew::dispatch::Runtime,
    image: Option<String>,
    timeout_seconds: u32,
    published_by_machine: Option<String>,
    published_by_orchestrator: Option<String>,
) -> WorkJob {
    let published_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(|_| {
            // (#906) A pre-epoch / badly-NTP-skewed clock makes 0 (also the
            // "unset" sentinel) the stamp. Surface it rather than silently
            // mislabeling the record's publish time.
            eprintln!("darkmux: system clock is before the Unix epoch — stamping published_at_unix_ms=0");
            0
        });
    WorkJob {
        target_machine,
        role_id,
        message,
        session_id,
        deliver,
        workdir,
        phase_id,
        runtime,
        image,
        timeout_seconds,
        published_at_unix_ms,
        published_by_machine,
        published_by_orchestrator,
        attempt: 1,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Dispatch routing (#463 cycle-break)
//
// The local-vs-remote routing decision + the work-queue publish path moved
// here from `crew::dispatch` so `crew` no longer depends on `fleet` (the
// edge that made `crew` un-extractable as a crate). `crew::dispatch::dispatch`
// is now purely local; `dispatch_routed` is the front door for user-facing
// dispatch callers (main / phase_cli / mission_propose / notebook). The
// fleet runner calls `crew::dispatch::dispatch` directly — it's already on
// the chosen machine, so it must run locally and never re-route.
// ─────────────────────────────────────────────────────────────────────────

use darkmux_crew::dispatch::{self, DispatchOpts, DispatchResult, RoutingDecision};

/// Route a dispatch local-vs-remote, then run it. When `--machine` is set
/// (and isn't the local machine), publish to the single global work queue
/// and (if `--wait`) block on the runner's `dispatch.complete` flow
/// record. Otherwise fall through to the local dispatch path
/// (`crew::dispatch::dispatch`). After #590 there is no tier auto-route:
/// the only fleet-queue path is explicit `--machine`, and it's advisory
/// (any runner may claim; a non-target runner logs a soft warning and
/// proceeds). (#246 PR-C.3; relocated from `crew::dispatch::dispatch` in
/// #463; tier auto-route retired in #590.)
pub fn dispatch_routed(opts: DispatchOpts) -> Result<DispatchResult> {
    if let Some(target) = opts.machine.clone() {
        let local = darkmux_flow::resolve_machine_id();
        match dispatch::routing_decision(Some(target.as_str()), local.as_deref()) {
            RoutingDecision::Local {
                matches_was_explicit: true,
            } => {
                eprintln!(
                    "darkmux dispatch: --machine={target} matches local machine_id; \
                     routing locally."
                );
            }
            RoutingDecision::Remote {
                target,
                local_unknown: true,
            } => {
                // PR-C.3 review MEDIUM (Wave-E.7): local machine_id is
                // unresolvable (no DARKMUX_MACHINE_ID, hostname failed).
                // Routing via queue is the only option — surface the
                // ambiguity loudly so the operator sees what happened.
                eprintln!(
                    "{}",
                    darkmux_types::style::warn(&format!(
                        "darkmux dispatch: WARNING — local DARKMUX_MACHINE_ID is unresolvable. \
                         --machine={target} routes via the queue regardless. \
                         If you intended a local dispatch, set DARKMUX_MACHINE_ID to make \
                         tier-routing decisions deterministic."
                    ))
                );
                // #290 — emit the pinned route record so the audit
                // trail + topology UI see the operator-pinned routing
                // decision. Validation runs BEFORE the emit so a
                // role-load failure doesn't leave a misleading "pinned"
                // record in the audit chain.
                let session_id =
                    dispatch::emit_route_record_and_resolve_session(&opts, Some(&target));
                let mut opts = opts;
                opts.session_id = Some(session_id);
                return dispatch_via_queue(opts, Some(&target));
            }
            RoutingDecision::Remote {
                target,
                local_unknown: false,
            } => {
                let session_id =
                    dispatch::emit_route_record_and_resolve_session(&opts, Some(&target));
                let mut opts = opts;
                opts.session_id = Some(session_id);
                return dispatch_via_queue(opts, Some(&target));
            }
            RoutingDecision::Local {
                matches_was_explicit: false,
            } => {
                // Unreachable in this branch (we matched Some(target) above)
                // — but the enum's total shape covers it.
            }
        }
    }

    // Local fall-through — no `--machine` means run on this machine
    // (#590: the tier auto-route arm was removed; there's no tier to
    // trigger auto-routing).
    dispatch::dispatch(opts)
}

/// Publish a dispatch to the single global fleet work queue instead of
/// running it locally (#246 PR-C.3). Called from `dispatch_routed` when
/// `opts.machine` is set to a non-local id. If `opts.wait` is true (the
/// default for `dispatch`), blocks on the runner's
/// `dispatch.complete` flow record before returning; otherwise returns
/// immediately with a fire-and-forget synthetic result.
/// `target_machine: Some(id)` stamps the WorkJob's advisory hint field so
/// the audit trail and topology view see the operator-pinned target (#590:
/// advisory only — any runner may claim).
fn dispatch_via_queue(opts: DispatchOpts, target_machine: Option<&str>) -> Result<DispatchResult> {
    // (#703 Slice 4) `--image` now rides the WorkJob (`build_work_job` below)
    // and the runner injects into it — cross-machine dispatch honors it, so no
    // silent-drop warning here anymore.
    // The Redis URL is required for cross-machine dispatch. If it's
    // unset, the operator hasn't configured the fleet substrate — bail
    // loud with the fix-it pointer.
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5).
    let raw_url = darkmux_flow::redis_url().ok_or_else(|| {
        let context = match target_machine {
            Some(m) => format!("--machine={m}"),
            None => "fleet-queue dispatch".to_string(),
        };
        anyhow!(
            "{context} requires Redis (DARKMUX_REDIS_URL or config.redis.enabled) \
             — the fleet work queue lives on Redis. \
             Single-machine fleets shouldn't dispatch to the queue."
        )
    })?;

    // Resolve session_id up front — the runner needs it to stamp on
    // the dispatch.complete record, and --wait needs it as the join key.
    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| dispatch::fresh_session_id(&opts.role_id));

    // Build the WorkJob from DispatchOpts. The shape mirrors what the
    // runner side reconstructs via `WorkJob::into_dispatch_opts` —
    // round-trip parity matters for cross-machine dispatch.
    let job = build_work_job(
        target_machine.map(|s| s.to_string()),
        opts.role_id.clone(),
        opts.message.clone(),
        session_id.clone(),
        opts.deliver.clone(),
        opts.workdir.as_ref().map(|p| p.display().to_string()),
        opts.phase_id.clone(),
        opts.runtime,
        opts.image.clone(),
        opts.timeout_seconds,
        darkmux_flow::resolve_machine_id(),
        darkmux_flow::resolve_orchestrator(),
    );

    // Open the Redis client lazily here (not at darkmux startup) so the
    // local-dispatch path doesn't pay any connection cost. The same
    // `raw_url` (already resolved above) is reused by `wait_for_completion` below.
    let client = redis::Client::open(raw_url.expose_for_probe())
        .with_context(|| format!("opening Redis client {raw_url} for --machine dispatch"))?;

    // Publish — `publish_job` runs validate() before XADD, so a
    // malformed job bails before crossing the network.
    let work_id = publish_job(&client, &job).context("publishing WorkJob to fleet queue")?;

    eprintln!(
        "darkmux dispatch: published work_id={work_id} \
         target_machine={} session={session_id}",
        target_machine.unwrap_or("<any>"),
    );

    if !opts.wait {
        // Fire-and-forget. Return a synthetic success result; the
        // operator polls via `darkmux flow tail --session <id>`.
        return Ok(DispatchResult {
            exit_code: 0,
            stdout: format!("published; not waiting (session_id={session_id})\n"),
            stderr: String::new(),
            session_id,
            // Remote/queue path: the runtime's bookkeeping lands on the
            // runner, not on this dispatching host.
            out_dir: None,
        });
    }

    // Block on the runner's dispatch.complete. Timeout = the job's own
    // timeout + a small slack (the runner's clock starts at claim, so
    // the dispatching client's wait must outlast the runner's budget).
    let wait_timeout =
        std::time::Duration::from_secs((opts.timeout_seconds as u64).saturating_add(30));
    eprintln!(
        "darkmux dispatch: waiting for dispatch.complete (session={session_id}, \
         timeout={}s)…",
        wait_timeout.as_secs()
    );
    let completion = wait_for_completion(&raw_url, &session_id, wait_timeout)
        .context("waiting for remote dispatch completion")?;

    eprintln!(
        "darkmux dispatch: completed session={} result={} wall_ms={:?}",
        completion.session_id, completion.result_class, completion.wall_ms
    );

    // Translate completion → DispatchResult. We don't have stdout from
    // the runner side (it lives in the runner's flow records, not the
    // dispatching CLI's stdout); surface the result_class + wall_ms in
    // the synthetic stdout so the operator sees something useful.
    Ok(completion_to_dispatch_result(completion))
}

/// Translate a queue completion (from `wait_for_completion`) into the
/// `DispatchResult` shape the CLI returns. Pulls the actual `exit_code`
/// out of the dispatch.complete payload when present; falls back to a
/// binary 0/1 derived from `result_class` only when the payload lacks an
/// explicit exit_code. (#255 Wave-E.6)
pub(crate) fn completion_to_dispatch_result(c: CompletionResult) -> DispatchResult {
    let payload_exit_code = c
        .payload
        .as_ref()
        .and_then(|p| p.get("exit_code"))
        .and_then(|v| v.as_i64())
        .map(|n| n as i32);
    let exit_code = payload_exit_code.unwrap_or(if c.result_class == "ok" { 0 } else { 1 });
    let stdout = format!(
        "remote dispatch complete; result_class={} exit_code={exit_code} wall_ms={:?} session={}\n\
         (full output in runner's flow records — \
          tail `~/.darkmux/flows/<date>.jsonl` for session={})\n",
        c.result_class, c.wall_ms, c.session_id, c.session_id,
    );
    DispatchResult {
        exit_code,
        stdout,
        stderr: String::new(),
        session_id: c.session_id,
        // Remote/queue path: the runtime's bookkeeping lands on the
        // runner, not on this dispatching host.
        out_dir: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::dispatch::Runtime;

    // (#842) `build_work_job` is the single constructor for every WorkJob that
    // crosses the fleet wire, and had ZERO tests. A field-swap (workdir landing
    // in deliver), or `attempt` defaulting to something other than 1 (which the
    // re-publish logic relies on, PR-C.1), corrupts every cross-machine dispatch
    // and passes green CI.

    /// All distinct values so a field-swap (X landing where Y belongs) fails.
    fn sample_job() -> WorkJob {
        build_work_job(
            Some("studio".to_string()),       // target_machine
            "coder".to_string(),               // role_id
            "do the thing".to_string(),        // message
            "sess-42".to_string(),             // session_id
            Some("discord:123".to_string()),   // deliver
            Some("/work/repo".to_string()),    // workdir
            Some("phase-7".to_string()),      // phase_id
            Runtime::Internal,                  // runtime
            Some("rust:slim".to_string()),     // image
            900,                                // timeout_seconds
            Some("laptop".to_string()),        // published_by_machine
            Some("claude-code".to_string()),   // published_by_orchestrator
        )
    }

    #[test]
    fn build_work_job_sets_attempt_one() {
        // PR-C.1 invariant: a freshly-built job is attempt 1 (re-publish bumps
        // to 2+). A non-1 default would break re-dispatch accounting.
        assert_eq!(sample_job().attempt, 1);
    }

    #[test]
    fn build_work_job_passes_fields_through_without_swap() {
        let j = sample_job();
        assert_eq!(j.target_machine.as_deref(), Some("studio"));
        assert_eq!(j.role_id, "coder");
        assert_eq!(j.message, "do the thing");
        assert_eq!(j.session_id, "sess-42");
        assert_eq!(j.deliver.as_deref(), Some("discord:123"));
        assert_eq!(j.workdir.as_deref(), Some("/work/repo"));
        assert_eq!(j.phase_id.as_deref(), Some("phase-7"));
        assert_eq!(j.runtime, Runtime::Internal);
        assert_eq!(j.image.as_deref(), Some("rust:slim"));
        assert_eq!(j.timeout_seconds, 900);
        assert_eq!(j.published_by_machine.as_deref(), Some("laptop"));
        assert_eq!(j.published_by_orchestrator.as_deref(), Some("claude-code"));
    }

    #[test]
    fn build_work_job_preserves_none_optionals() {
        // The all-None shape must round-trip too — no field gets a spurious
        // default substituted for an absent optional.
        let j = build_work_job(
            None,
            "reviewer".to_string(),
            "m".to_string(),
            "s".to_string(),
            None,
            None,
            None,
            Runtime::Internal,
            None,
            60,
            None,
            None,
        );
        assert!(j.target_machine.is_none());
        assert!(j.deliver.is_none());
        assert!(j.workdir.is_none());
        assert!(j.phase_id.is_none());
        assert!(j.image.is_none());
        assert!(j.published_by_machine.is_none());
        assert!(j.published_by_orchestrator.is_none());
        assert_eq!(j.attempt, 1);
    }

    #[test]
    fn build_work_job_stamps_published_at() {
        // The #906 clock stamp: non-zero (0 is the pre-epoch sentinel) and
        // stamped DURING the build. Bracket the call between two clock reads so
        // the assertion can't flake on an NTP step or a suspended-VM resume —
        // the stamp must land in [before, after], which holds by construction.
        let now = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        };
        let before = now();
        let stamped = sample_job().published_at_unix_ms;
        let after = now();
        assert!(stamped > 0, "published_at should be stamped, not the 0 sentinel");
        assert!(
            stamped >= before && stamped <= after,
            "stamp {stamped} must fall within the call window [{before}, {after}]"
        );
    }

    // (#842) `match_completion` is the no-redispatch invariant: a waiting client
    // resolves when (and only when) its OWN session's terminal record lands.
    // Matching the wrong session (false-complete on a sibling) or missing the
    // canonical action shape (hang forever / re-dispatch) both corrupt fleet
    // routing and pass green CI without these.

    #[test]
    fn match_completion_matches_target_session_canonical_action() {
        let line = r#"{"action":"dispatch complete","session_id":"s-1","payload":{"result_class":"ok","wall_ms":1234,"exit_code":0}}"#;
        let c = match_completion(line, "s-1").expect("matches the canonical 'dispatch complete'");
        assert_eq!(c.session_id, "s-1");
        assert_eq!(c.result_class, "ok");
        assert_eq!(c.wall_ms, Some(1234));
    }

    #[test]
    fn match_completion_accepts_dotted_action_forwardcompat() {
        let line = r#"{"action":"dispatch.complete","session_id":"s-1","payload":{"result_class":"error"}}"#;
        let c = match_completion(line, "s-1").expect("dotted form accepted (forward-compat)");
        assert_eq!(c.result_class, "error");
        assert_eq!(c.wall_ms, None, "absent wall_ms → None");
    }

    #[test]
    fn match_completion_ignores_other_sessions_and_non_completions() {
        let complete = r#"{"action":"dispatch complete","session_id":"OTHER","payload":{}}"#;
        assert!(match_completion(complete, "s-1").is_none(), "a sibling session must NOT false-complete us");
        let turn = r#"{"action":"dispatch.turn","session_id":"s-1","payload":{}}"#;
        assert!(match_completion(turn, "s-1").is_none(), "a non-completion action is not a completion");
        assert!(match_completion("not json", "s-1").is_none(), "malformed line → None, never panic");
        let no_class = r#"{"action":"dispatch complete","session_id":"s-1"}"#;
        assert_eq!(
            match_completion(no_class, "s-1").unwrap().result_class,
            "unknown",
            "missing result_class defaults to 'unknown'"
        );
    }

    #[test]
    fn completion_to_dispatch_result_maps_exit_code_and_defaults() {
        // exit_code taken from payload when present.
        let c = CompletionResult {
            session_id: "s-1".into(),
            result_class: "error".into(),
            wall_ms: Some(9),
            payload: Some(serde_json::json!({"exit_code": 137})),
        };
        let r = completion_to_dispatch_result(c);
        assert_eq!(r.exit_code, 137, "payload exit_code wins");
        assert!(r.stdout.contains("result_class=error") && r.stdout.contains("session=s-1"));
        assert!(r.out_dir.is_none(), "remote path: no local bookkeeping");

        // No payload exit_code → derived from result_class (ok→0, else→1).
        let ok = CompletionResult {
            session_id: "s-2".into(),
            result_class: "ok".into(),
            wall_ms: None,
            payload: None,
        };
        assert_eq!(completion_to_dispatch_result(ok).exit_code, 0, "ok → 0");
        let bad = CompletionResult {
            session_id: "s-3".into(),
            result_class: "error".into(),
            wall_ms: None,
            payload: None,
        };
        assert_eq!(completion_to_dispatch_result(bad).exit_code, 1, "non-ok → 1");
    }
}


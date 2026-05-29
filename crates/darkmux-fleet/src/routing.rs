//! Fleet dispatch routing — auto-route target selection, queue dispatch, and completion waiting.

use crate::queue::extract_field;
use crate::{candidates_for_tier, load_roster, publish_job, WorkJob};
use anyhow::{anyhow, bail, Context, Result};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── Client-side --wait wrapper (PR-C.3) ──────────────────────────────
//
// After `publish_job` returns, the dispatching client can either return
// immediately (fire-and-forget; the operator polls flow stream from
// elsewhere) OR block until the worker's `dispatch.complete` flow
// record lands for the matching `session_id`. The `--wait` wrapper
// implements the blocking form by **polling the Redis flow stream**
// (`darkmux:flow`) — NOT the local file, because in a cross-machine
// dispatch the completion record lands on the WORKER's local file,
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
    /// reads this for sprint-level aggregation).
    #[allow(dead_code)] // consumed by PR-D mission dispatch fan-out aggregator
    pub payload: Option<serde_json::Value>,
}

/// Block until a `dispatch.complete` flow record for `session_id` lands
/// in the Redis flow stream, or `timeout` elapses. Returns the
/// completion result on success; bails when the timeout fires (the job
/// may still be running on the remote worker — the operator can re-tail
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
/// the worker writes the `dispatch.complete` record to its OWN local
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

    let stream = std::env::var("DARKMUX_REDIS_STREAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "darkmux:flow".to_string());

    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "wait_for_completion: no dispatch.complete for session_id={session_id} \
                 within {}s in Redis stream {stream}. The job may still be running on the \
                 worker — tail `darkmux flow tail --session {session_id}` to keep watching.",
                timeout.as_secs()
            ));
        }

        // XRANGE darkmux:flow - + COUNT 10000 — full-scan each poll. The
        // stream is bounded (MAXLEN ~ 10000) so the scan is bounded too.
        let raw: redis::Value = redis::cmd("XRANGE")
            .arg(&stream)
            .arg("-")
            .arg("+")
            .arg("COUNT")
            .arg(WAIT_XRANGE_COUNT)
            .query(&mut conn)
            .with_context(|| format!("XRANGE on flow stream {stream}"))?;

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
/// (`crew::dispatch::dispatch` openclaw path + `dispatch_internal::dispatch`
/// internal-runtime path). The dotted form `"dispatch.complete"` is
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
    target_tier: String,
    target_machine: Option<String>,
    role_id: String,
    message: String,
    session_id: String,
    deliver: Option<String>,
    workdir: Option<String>,
    sprint_id: Option<String>,
    runtime: darkmux_crew::dispatch::Runtime,
    timeout_seconds: u32,
    published_by_machine: Option<String>,
    published_by_orchestrator: Option<String>,
) -> WorkJob {
    let published_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    WorkJob {
        target_tier,
        target_machine,
        role_id,
        message,
        session_id,
        deliver,
        workdir,
        sprint_id,
        runtime,
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
// dispatch callers (main / sprint_cli / mission_propose / notebook). The
// fleet worker calls `crew::dispatch::dispatch` directly — it's already on
// the chosen machine, so it must run locally and never re-route.
// ─────────────────────────────────────────────────────────────────────────

use darkmux_crew::dispatch::{self, DispatchOpts, DispatchResult, RoutingDecision};

/// Route a dispatch local-vs-remote, then run it. When `--machine` is set
/// (and isn't the local machine) OR the role's tier auto-routes across the
/// fleet, publish to the work queue and (if `--wait`) block on the worker's
/// `dispatch.complete` flow record. Otherwise fall through to the local
/// dispatch path (`crew::dispatch::dispatch`). (#246 PR-C.3 / #247 PR-B;
/// relocated from `crew::dispatch::dispatch` in #463.)
pub fn dispatch_routed(opts: DispatchOpts) -> Result<DispatchResult> {
    if let Some(target) = opts.machine.clone() {
        let local = darkmux_flow::resolve_machine_id();
        match dispatch::routing_decision(Some(target.as_str()), local.as_deref()) {
            RoutingDecision::Local {
                matches_was_explicit: true,
            } => {
                eprintln!(
                    "darkmux crew dispatch: --machine={target} matches local machine_id; \
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
                    "darkmux crew dispatch: WARNING — local DARKMUX_MACHINE_ID is unresolvable. \
                     --machine={target} routes via the queue regardless. \
                     If you intended a local dispatch, set DARKMUX_MACHINE_ID to make \
                     tier-routing decisions deterministic."
                );
                // #290 — emit the pinned route record so the audit
                // trail + topology UI see the operator-pinned routing
                // decision. Validation runs BEFORE the emit so a
                // role-load failure OR an invalid tier doesn't leave a
                // misleading "pinned" record in the audit chain.
                let role_tier = dispatch::resolve_role_tier_for_record(&opts)?;
                let session_id =
                    dispatch::emit_route_record_and_resolve_session(&opts, &role_tier, Some(&target));
                let mut opts = opts;
                opts.session_id = Some(session_id);
                return dispatch_via_queue(opts, Some(&target));
            }
            RoutingDecision::Remote {
                target,
                local_unknown: false,
            } => {
                let role_tier = dispatch::resolve_role_tier_for_record(&opts)?;
                let session_id =
                    dispatch::emit_route_record_and_resolve_session(&opts, &role_tier, Some(&target));
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
    } else if let Some(auto_target_tier) = auto_route_target_tier(&opts)? {
        // #247 PR-B — auto-route by tier when no explicit --machine and the
        // role's tier doesn't match the local machine's tier (and the fleet
        // has a peer in the role's tier). The worker that claims it runs its
        // own preflight — we skip the local one.
        let local_tier = darkmux_flow::resolve_machine_tier();
        eprintln!(
            "darkmux crew dispatch: auto-routing role=`{}` via tier=`{}` \
             (local tier=`{}`, no --machine — consumer group claims).",
            opts.role_id,
            auto_target_tier,
            local_tier.as_deref().unwrap_or("<unknown>"),
        );
        let session_id =
            dispatch::emit_route_record_and_resolve_session(&opts, &auto_target_tier, None);
        let mut opts = opts;
        opts.session_id = Some(session_id);
        return dispatch_via_queue(opts, None);
    }

    // Local fall-through — run the dispatch on this machine.
    dispatch::dispatch(opts)
}

/// Publish a dispatch to the fleet work queue instead of running it
/// locally (#246 PR-C.3). Called from `dispatch_routed` when `opts.machine`
/// is set to a non-local id, or when tier auto-routing fires. If `opts.wait`
/// is true (the default for `crew dispatch`), blocks on the worker's
/// `dispatch.complete` flow record before returning; otherwise returns
/// immediately with a fire-and-forget synthetic result.
/// `target_machine: Some(id)` stamps the WorkJob's hint field so the
/// audit trail and topology view see the operator-pinned target.
/// `None` is the auto-route case (#247 PR-B).
fn dispatch_via_queue(opts: DispatchOpts, target_machine: Option<&str>) -> Result<DispatchResult> {
    // Determine the role's tier requirement (drives the work stream
    // selection). Roles MUST declare a concrete tier for cross-machine
    // dispatch — workers register on `darkmux:work:<inference|hub|client>`
    // streams; a role with `tier: None` would publish to
    // `darkmux:work:any` which has no consumer and the wait loop would
    // time out without explanation. Bail loud with operator-actionable
    // hints. (PR-C.3 review HIGH-1)
    let role = dispatch::load_role_or_bail(&opts.role_id)?;
    let role_tier = match role.tier.clone() {
        Some(t) if !t.trim().is_empty() && t != "any" => t,
        Some(t) => {
            bail!(
                "role `{}` has tier={:?} which has no fleet consumer (workers \
                 register on inference/hub/client streams). Either: (a) edit \
                 the role manifest to declare a concrete tier, or (b) omit \
                 --machine to dispatch locally.",
                opts.role_id,
                t
            );
        }
        None => {
            bail!(
                "role `{}` has no tier declaration in its manifest. \
                 Cross-machine dispatch requires the role to declare which \
                 machine class it runs on. Either: (a) add \"tier\": \
                 \"inference\" (or \"hub\") to the role's JSON manifest, or \
                 (b) omit --machine to dispatch locally.",
                opts.role_id
            );
        }
    };

    // The Redis URL is required for cross-machine dispatch. If it's
    // unset, the operator hasn't configured the fleet substrate — bail
    // loud with the fix-it pointer.
    let redis_url = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            let context = match target_machine {
                Some(m) => format!("--machine={m}"),
                None => format!("cross-tier auto-route (local tier != role tier=`{role_tier}`)"),
            };
            anyhow!(
                "{context} requires DARKMUX_REDIS_URL to be set \
                 (the fleet work queue lives on Redis). \
                 Single-machine fleets shouldn't dispatch cross-tier."
            )
        })?;

    // Resolve session_id up front — the worker needs it to stamp on
    // the dispatch.complete record, and --wait needs it as the join key.
    let session_id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| dispatch::fresh_session_id(&opts.role_id));

    // Build the WorkJob from DispatchOpts. The shape mirrors what the
    // worker side reconstructs via `WorkJob::into_dispatch_opts` —
    // round-trip parity matters for cross-machine dispatch.
    let job = build_work_job(
        role_tier,
        target_machine.map(|s| s.to_string()),
        opts.role_id.clone(),
        opts.message.clone(),
        session_id.clone(),
        opts.deliver.clone(),
        opts.workdir.as_ref().map(|p| p.display().to_string()),
        opts.sprint_id.clone(),
        opts.runtime,
        opts.timeout_seconds,
        darkmux_flow::resolve_machine_id(),
        darkmux_flow::resolve_orchestrator(),
    );

    // Open the Redis client lazily here (not at darkmux startup) so the
    // local-dispatch path doesn't pay any connection cost. The same
    // `raw_url` is reused by `wait_for_completion` below.
    let raw_url = darkmux_flow::RawRedisUrl::new(redis_url);
    let client = redis::Client::open(raw_url.expose_for_probe())
        .with_context(|| format!("opening Redis client {raw_url} for --machine dispatch"))?;

    // Publish — `publish_job` runs validate() before XADD, so a
    // malformed job bails before crossing the network.
    let work_id = publish_job(&client, &job).context("publishing WorkJob to fleet queue")?;

    eprintln!(
        "darkmux crew dispatch: published work_id={work_id} tier={} \
         target_machine={} session={session_id}",
        job.target_tier,
        target_machine.unwrap_or("<auto-route>"),
    );

    if !opts.wait {
        // Fire-and-forget. Return a synthetic success result; the
        // operator polls via `darkmux flow tail --session <id>`.
        return Ok(DispatchResult {
            exit_code: 0,
            stdout: format!("published; not waiting (session_id={session_id})\n"),
            stderr: String::new(),
            session_id,
            watched_state: Vec::new(),
        });
    }

    // Block on the worker's dispatch.complete. Timeout = the job's own
    // timeout + a small slack (the worker's clock starts at claim, so
    // the dispatching client's wait must outlast the worker's budget).
    let wait_timeout =
        std::time::Duration::from_secs((opts.timeout_seconds as u64).saturating_add(30));
    eprintln!(
        "darkmux crew dispatch: waiting for dispatch.complete (session={session_id}, \
         timeout={}s)…",
        wait_timeout.as_secs()
    );
    let completion = wait_for_completion(&raw_url, &session_id, wait_timeout)
        .context("waiting for remote dispatch completion")?;

    eprintln!(
        "darkmux crew dispatch: completed session={} result={} wall_ms={:?}",
        completion.session_id, completion.result_class, completion.wall_ms
    );

    // Translate completion → DispatchResult. We don't have stdout from
    // the worker side (it lives in the worker's flow records, not the
    // dispatching CLI's stdout); surface the result_class + wall_ms in
    // the synthetic stdout so the operator sees something useful.
    Ok(completion_to_dispatch_result(completion))
}

/// Decide whether the dispatch should auto-route via the work queue
/// because the role's declared tier doesn't match the local machine's
/// tier. Returns:
/// - `Ok(None)` — dispatch locally (role.tier ∈ {None, "any"}, OR
///   role.tier == local tier, OR roster is empty so there's no fleet
///   to route across)
/// - `Ok(Some(role_tier))` — fleet has a peer in `role_tier`; publish
///   via queue.
/// - `Err(_)` — operator HAS a fleet with peers in other tiers but
///   none in `role_tier`; bail loud (actionable misconfiguration).
///   (#247 PR-B; graceful-degradation refinement per LAB_NOTEBOOK
///   Beat 35.)
pub(crate) fn auto_route_target_tier(opts: &DispatchOpts) -> Result<Option<String>> {
    let role = dispatch::load_role_or_bail(&opts.role_id)?;
    let role_tier = match role.tier.as_deref().map(str::trim) {
        Some("") | Some("any") | None => return Ok(None), // local
        Some(t) => t.to_string(),
    };
    let local_tier = darkmux_flow::resolve_machine_tier();
    if local_tier.as_deref() == Some(role_tier.as_str()) {
        // Local matches role's tier — dispatch locally; no queue cost.
        return Ok(None);
    }
    // Tier mismatch — consult the fleet roster. `load_roster()` already
    // returns `Ok(FleetRoster::default())` for the missing-file case
    // (single-machine operator legitimately has no fleet.json), so we
    // only need to propagate genuine parse failures here.
    let roster = load_roster().with_context(|| {
        format!(
            "role `{}` requires tier=`{role_tier}` and the fleet roster failed to load. \
             Run `darkmux fleet status` to inspect.",
            opts.role_id
        )
    })?;
    let candidates = candidates_for_tier(&roster, &role_tier);
    if candidates.is_empty() {
        if roster.machines.is_empty() {
            // Single-machine operator — no fleet declared at all. Tier
            // constraints don't apply when there's nothing to route
            // across. Run locally with a teaching nudge. Operator-
            // sovereignty applied per Beat 35.
            eprintln!(
                "darkmux crew dispatch: role `{}` declares tier=`{role_tier}` but no \
                 fleet peers are declared — running locally. To enable multi-machine \
                 routing later: `darkmux fleet add <id> --tier <tier> --address <addr>`.",
                opts.role_id
            );
            return Ok(None);
        }
        // Roster has peers, but none in this tier — operator HAS a
        // fleet that's deliberately partitioned, just missing the
        // required tier. Actionable misconfiguration.
        let peer_count = roster.machines.len();
        let peer_word = if peer_count == 1 { "peer" } else { "peers" };
        bail!(
            "role `{}` requires tier=`{role_tier}` but no fleet peer is in that \
             tier (local tier=`{}`, {peer_count} other {peer_word} declared). Either: \
             (a) add a peer with `darkmux fleet add <id> --tier {role_tier} --address <addr>`, \
             or (b) edit the role manifest to declare `tier: \"any\"` if this work \
             belongs on whatever's available.",
            opts.role_id,
            local_tier.as_deref().unwrap_or("<unset>"),
        );
    }
    Ok(Some(role_tier))
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
         (full output in worker's flow records — \
          tail `~/.darkmux/flows/<date>.jsonl` for session={})\n",
        c.result_class, c.wall_ms, c.session_id, c.session_id,
    );
    DispatchResult {
        exit_code,
        stdout,
        stderr: String::new(),
        session_id: c.session_id,
        watched_state: Vec::new(),
    }
}


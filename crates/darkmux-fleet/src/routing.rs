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

/// (#2243) The read deadline for the next `wait_for_completion` poll: what is
/// LEFT of the declared wait budget, or `None` once the budget is spent.
///
/// Extracted from the loop so its one dangerous property is ASSERTABLE rather
/// than argued: **a returned `Some` is never `Duration::ZERO`.** A zero
/// `timeval` means BLOCK FOREVER in several socket APIs, and this value is
/// handed to `set_read_timeout` at the exact instant the wait's timeout is
/// supposed to fire — getting it wrong reintroduces the original #2243 hang
/// precisely when the operator is owed the timeout. `std` happens to reject a
/// zero duration outright (executed: `Err(InvalidInput, "cannot set a 0
/// duration timeout")`, leaving the previous deadline in force), but a
/// swallowed set on a platform that instead honored zero would hang, so the
/// invariant is enforced HERE and not left to the socket layer.
///
/// `checked_sub` covers `elapsed > timeout`; the `is_zero` filter covers the
/// exact-equality instant that `saturating_sub` would hand back as zero.
fn remaining_read_deadline(timeout: Duration, elapsed: Duration) -> Option<Duration> {
    timeout.checked_sub(elapsed).filter(|r| !r.is_zero())
}

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
    // (#2243) Bound BOTH phases, reusing darkmux-flow's connect definition
    // rather than re-deriving it here. Before this, a peer that accepts TCP and
    // never answers (measured live 2026-07-29, a Tailscale peer) blocked the
    // poll below forever, control never returned to the elapsed check at the top
    // of the loop, and the operator's declared `--wait` timeout could never fire.
    //
    // This bounded connect is paid BEFORE `start` is taken, so its own ceiling
    // (`REDIS_CONNECT_TIMEOUT * 2` = 1s) sits OUTSIDE the declared wait budget —
    // see the overshoot arithmetic at the read-deadline site below.
    let mut conn = darkmux_flow::open_redis_connection_bounded(
        &client,
        darkmux_flow::REDIS_CONNECT_TIMEOUT,
    )
    .with_context(|| format!("connecting to Redis to wait for completion of {session_id}"))?;
    // Bounds the WRITE side (and seeds a read deadline that the loop below
    // immediately replaces with the remaining wait budget, per-poll).
    darkmux_flow::bound_redis_response(&conn);

    // (#875) env > config.redis.stream > default, via config_access.
    let stream = darkmux_types::config_access::redis_stream();

    // (#2243) The one operator-facing timeout message, produced from BOTH
    // budget-exhaustion paths (the top-of-loop check and a read that hit the
    // deadline) so they cannot drift apart.
    let budget_exhausted = || {
        anyhow!(
            "wait_for_completion: no dispatch.complete for session_id={session_id} \
             within {}s in Redis stream {stream}. The job may still be running on the \
             runner — tail `darkmux flow tail --session {session_id}` to keep watching.",
            timeout.as_secs()
        )
    };

    let start = std::time::Instant::now();
    loop {
        // (#2243) Budget check and the ZERO-DURATION GUARD in one call:
        // `remaining_read_deadline` yields `None` once the budget is spent, and
        // its `Some` is guaranteed strictly positive (that guarantee is asserted
        // by `remaining_read_deadline_never_yields_a_zero_duration`). So
        // `remaining` is safe to hand to `set_read_timeout` below.
        let Some(remaining) = remaining_read_deadline(timeout, start.elapsed()) else {
            return Err(budget_exhausted());
        };

        // (#2243) The read deadline for THIS poll is the REMAINING WAIT BUDGET,
        // not a fixed constant. That is the difference between a bug and a fix:
        //
        // A fixed deadline shorter than a healthy peer's latency makes every
        // poll time out, and redis-rs makes that permanent. In redis-0.27.6
        // `connection.rs`, `Connection::read` responds to a read error that is
        // an IoError and is NOT `UnexpectedEof` by doing `messages_to_skip += 1`
        // for a RESPONSE read; the next `read()` then DISCARDS that many
        // successfully-parsed replies before returning one. Re-issuing the
        // command without draining the backlog creates and consumes the deficit
        // at the same rate, so it never closes — the client stays permanently
        // one reply behind and throws away every reply it receives. Measured
        // against a peer that answered every `XREVRANGE` correctly, in order,
        // with the completion record present: at 100ms latency `Ok` in 109ms;
        // at 1200ms latency against a 1000ms deadline, "no dispatch.complete"
        // after the full budget. Only the latency changed. That trades a loud
        // hang for a SILENT WRONG ANSWER — `mission dispatch --wait` reporting a
        // completed job as still running, which `src/main.rs` counts as a
        // failure. (Rebuilding the connection on timeout does NOT fix it; that
        // remedy was measured and disproved.)
        //
        // With the deadline equal to the remaining budget: a slow-but-healthy
        // poll completes normally, and a timeout can only mean the budget is
        // spent — so it ENDS the wait (below) rather than continuing it, and the
        // skip deficit is structurally unable to accumulate.
        //
        // ZERO-DURATION SAFETY. `Some(Duration::ZERO)` is the trap here: in
        // several socket APIs a zero `timeval` means BLOCK FOREVER, which would
        // reintroduce the original hang at the exact instant the timeout should
        // fire. redis-rs delegates straight to `std`'s socket
        // `set_read_timeout`, and `std` rejects it — executed on this platform:
        // `Err(InvalidInput, "cannot set a 0 duration timeout")`, with the
        // PREVIOUS deadline left in force (this call ignores the result, so a
        // zero would be a silent no-op, not a hang). We do not lean on that:
        // `remaining` is strictly positive by construction above. `std` also
        // clamps a sub-microsecond positive duration UP to 1µs rather than down
        // to zero, so the nanosecond tail is safe too.
        //
        // WHAT THIS DEADLINE IS NOT. `set_read_timeout` is `SO_RCVTIMEO`, a
        // per-`recv` deadline, not a per-command one: it fires on ZERO BYTES for
        // `remaining`, and any byte that arrives restarts the clock. So it bounds
        // a peer that goes SILENT (the #2243 failure mode) and does NOT bound a
        // peer that DRIBBLES — one byte every 400ms into a reply that never
        // terminates blocked 12s against a declared 2s wait when measured. Call
        // this bounded against silence, not bounded outright.
        //
        // AND IT IS NOT THE WHOLE OPERATOR SYMPTOM. `mission dispatch` publishes
        // every phase BEFORE it waits on any of them (`src/main.rs`), and
        // `queue.rs`'s `publish_job` still opens a plain unbounded
        // `get_connection()` — as does the `init_consumer_group` it calls first,
        // which is the actual first unbounded touch. That queue is deliberately
        // out of scope here: its `claim_job` issues `XREADGROUP ... BLOCK`, an
        // intentionally long-blocking read that a blanket socket deadline would
        // break, so it needs a per-call-site decision. Against the silent peer
        // #2243 describes, `--wait` therefore STILL hangs — earlier, in the
        // publish loop, before this function is ever reached. Fixing the wait
        // fixes the wait, not the end-to-end operator symptom.
        let _ = conn.set_read_timeout(Some(remaining));

        // (#809) XREVRANGE (newest-first) — the completion record we're
        // waiting for is by definition RECENT. The old oldest-first XRANGE
        // dropped the newest entries once the stream rode at its `MAXLEN ~`
        // cap (XLEN floats above the cap; trimming is lazy), so a saturated
        // stream made this wait MISS the completion entirely and time out.
        // Scan order doesn't matter for a find; newest-first also returns
        // the match in the first entries scanned.
        let polled: redis::RedisResult<redis::Value> = redis::cmd("XREVRANGE")
            .arg(&stream)
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(WAIT_XRANGE_COUNT)
            .query(&mut conn);

        let raw = match polled {
            Ok(raw) => raw,
            // (#2243) A poll that hits the deadline ENDS the wait with the
            // canonical timeout message, because a READ that hits it hit the
            // remaining budget. (Strictly, `bound_redis_response` above also
            // installed a FIXED 1s write deadline that this loop never
            // re-derives, and `is_timeout()` matches `TimedOut`/`WouldBlock`
            // on either side — so a WRITE expiry would claim the declared
            // budget was spent at ~1s. The arm is deliberately left wide
            // rather than narrowed to reads: no reachable path constructs
            // one, since a write expiry needs ~100KB+ of send-buffer backlog
            // and this loop issues a single ~50-byte command per poll.)
            // `continue` was round 1's answer and is wrong
            // here for two reasons: the budget is spent, so continuing only
            // re-derives the same message one loop later; and continuing after
            // a timed-out read is precisely what lets redis-rs's
            // `messages_to_skip` deficit persist (see the deadline site above).
            // Returning here means the deficit can never be created twice on
            // one connection, whatever the deadline actually was.
            //
            // `RedisError::is_timeout()` is the predicate: it is true exactly
            // for an `IoError` of kind `TimedOut`/`WouldBlock`, which is what
            // a `set_read_timeout` expiry surfaces as (verified against a live
            // silent peer in this module's tests, not assumed from the docs).
            // Every OTHER error — a connection reset, a protocol error, a
            // wrong-type reply — still propagates with today's diagnostics.
            // The disjointness matters: `ConnectionReset`/`BrokenPipe`/
            // `UnexpectedEof` belong to `is_connection_dropped()`, so nothing
            // fatal is swallowed as a timeout.
            //
            // The connection is deliberately NOT rebuilt on a timeout, and the
            // reason is NOT that a late reply gets picked up later — it does
            // not. redis-rs DISCARDS it, permanently, as a `messages_to_skip`
            // skip. The reason is simply that this connection has no next poll:
            // the wait is over on this line, and the connection is dropped.
            //
            // OVERSHOOT CEILING for a peer that returns whole replies promptly:
            //   `REDIS_CONNECT_TIMEOUT * 2` (1s, the bounded connect, paid
            //   BEFORE `start` and so outside the declared budget)
            //   + `timeout`
            //   + `WAIT_POLL_INTERVAL` (250ms — a poll can answer just under the
            //     budget and still sleep a full interval before the loop-top
            //     check fires).
            // That is PER CALL, and `src/main.rs`'s fan-out loops it over N
            // sessions serially, so the operator-visible ceiling is N times it.
            // A dribbling peer is NOT covered by it — see the deadline site's
            // `set_read_timeout` note and #2243's S1: `SO_RCVTIMEO` is a
            // per-`recv` deadline, not a per-command one.
            Err(e) if e.is_timeout() => return Err(budget_exhausted()),
            Err(e) => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("XREVRANGE on flow stream {stream}"))
            }
        };

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
    workdir: Option<String>,
    phase_id: Option<String>,
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
        workdir,
        phase_id,
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

/// Route a dispatch local-vs-remote, then run it locally via the raw
/// `crew::dispatch::dispatch` primitive — the pre-#1509 behavior, and still
/// what every caller other than the `darkmux dispatch` CLI verb wants
/// (`phase_cli`'s QA-gate dispatch, `mission_propose`, `notebook` — #1509's
/// scope is the CLI verb only; those three are a named follow-up, see
/// `dispatch_as_crew_of_one`'s module doc). Thin wrapper over
/// [`dispatch_routed_via`]; see that function's doc for the full routing
/// contract.
pub fn dispatch_routed(opts: DispatchOpts) -> Result<DispatchResult> {
    dispatch_routed_via(opts, dispatch::dispatch)
}

/// Route a dispatch local-vs-remote, then run it. When `--machine` is set
/// (and isn't the local machine), publish to the single global work queue
/// and (if `--wait`) block on the runner's `dispatch.complete` flow
/// record. Otherwise fall through to `local_dispatch` — a caller-injected
/// LOCAL execution primitive (#1509). Every caller except the `darkmux
/// dispatch` CLI verb passes the raw `crew::dispatch::dispatch` primitive
/// (via the [`dispatch_routed`] thin wrapper, unchanged pre-#1509 behavior);
/// the CLI verb passes `darkmux_crew::dispatch_as_crew_of_one::
/// dispatch_as_crew_of_one`, which runs the SAME primitive wrapped in a
/// crew-of-one Mission/Phase/Task/Step graph through `run_step_graph` — a
/// first-class run whose residency participates in the #1487 lease/
/// reconcile regime. Only the LOCAL fall-through switches; the `--machine`
/// routing decision, the queue-publish path, and every warning/route-record
/// emission below are unchanged for every caller (a `--machine` dispatch's
/// residency lives on the REMOTE runner machine, out of #1509's scope — see
/// its own module doc). After #590 there is no tier auto-route: the only
/// fleet-queue path is explicit `--machine`, and it's advisory (any runner
/// may claim; a non-target runner logs a soft warning and proceeds). (#246
/// PR-C.3; relocated from `crew::dispatch::dispatch` in #463; tier
/// auto-route retired in #590; `local_dispatch` injection added in #1509.)
pub fn dispatch_routed_via(
    opts: DispatchOpts,
    local_dispatch: impl FnOnce(DispatchOpts) -> Result<DispatchResult>,
) -> Result<DispatchResult> {
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
    // trigger auto-routing). `local_dispatch` is the caller-injected LOCAL
    // execution primitive (#1509) — see this function's own doc.
    local_dispatch(opts)
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
        opts.workdir.as_ref().map(|p| p.display().to_string()),
        opts.phase_id.clone(),
        opts.image.clone(),
        opts.timeout_seconds,
        darkmux_flow::resolve_machine_id(),
        // (#1758) `resolve_orchestrator()` was removed — it was write-only,
        // machine-scoped provenance for an invocation-scoped fact, and
        // nothing read `WorkJob.published_by_orchestrator` either (grepped:
        // producers + test fixtures only). Passing `None` here rather than
        // removing the field/param keeps `WorkJob`'s `deny_unknown_fields`
        // wire shape (`WORK_JOB_SCHEMA_VERSION`) unchanged — that field's
        // own removal is a separate, harder (hard-break, not lenient-read)
        // follow-up if it's ever worth doing.
        None,
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

    // (#842) `build_work_job` is the single constructor for every WorkJob that
    // crosses the fleet wire, and had ZERO tests. A field-swap (workdir landing
    // in image), or `attempt` defaulting to something other than 1 (which the
    // re-publish logic relies on, PR-C.1), corrupts every cross-machine dispatch
    // and passes green CI.

    /// All distinct values so a field-swap (X landing where Y belongs) fails.
    fn sample_job() -> WorkJob {
        build_work_job(
            Some("studio".to_string()),       // target_machine
            "coder".to_string(),               // role_id
            "do the thing".to_string(),        // message
            "sess-42".to_string(),             // session_id
            Some("/work/repo".to_string()),    // workdir
            Some("phase-7".to_string()),      // phase_id
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
        assert_eq!(j.workdir.as_deref(), Some("/work/repo"));
        assert_eq!(j.phase_id.as_deref(), Some("phase-7"));
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
            60,
            None,
            None,
        );
        assert!(j.target_machine.is_none());
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

    // (#1509) `dispatch_routed_via`'s local-dispatch injection seam. No
    // `opts.machine` means the local fall-through runs — never touches
    // Redis/the queue, so this is a fast, hermetic unit test even though
    // `dispatch_routed_via` is the same function a live `--machine` dispatch
    // uses.

    fn local_opts(role_id: &str) -> DispatchOpts {
        DispatchOpts {
            brief_refs: Vec::new(),
            workspace_read_only: false,
            record_context: None,
            resume_from: None,
            host_out: None,
            max_turns_override: None,
            role_id: role_id.to_string(),
            message: "hi".to_string(),
            session_id: None,
            timeout_seconds: 60,
            skip_preflight: false,
            json: true,
            workdir: None,
            phase_id: None,
            machine: None,
            wait: true,
            compaction: darkmux_crew::dispatch::CompactionDispatchArgs::default(),
            profile_name: None,
            config_path: None,
            force_container: false,
            max_completion_tokens: None,
            image: None,
            model_base_url_override: None,
            step_id: None,
            system_prompt_override: None,
        }
    }

    #[test]
    fn dispatch_routed_via_runs_the_injected_local_dispatch_on_the_local_fallthrough() {
        let called = std::cell::RefCell::new(false);
        let result = dispatch_routed_via(local_opts("coder"), |opts| {
            *called.borrow_mut() = true;
            assert_eq!(opts.role_id, "coder", "the SAME opts must reach the injected closure");
            Ok(DispatchResult {
                exit_code: 0,
                stdout: "injected stdout".to_string(),
                stderr: String::new(),
                session_id: "sess-injected".to_string(),
                out_dir: None,
            })
        })
        .unwrap();

        assert!(*called.borrow(), "the local fall-through must call the injected closure");
        assert_eq!(result.stdout, "injected stdout");
        assert_eq!(result.session_id, "sess-injected");
    }

    #[test]
    fn dispatch_routed_via_propagates_the_injected_closures_error() {
        let err = dispatch_routed_via(local_opts("coder"), |_opts| {
            Err(anyhow!("injected failure"))
        })
        .unwrap_err();
        assert!(err.to_string().contains("injected failure"), "{err}");
    }

    // ─── `wait_for_completion` against an accepts-but-never-answers peer (#2243) ───
    //
    // The failure mode measured live on 2026-07-29 (a Tailscale peer): the TCP
    // port accepts, the Redis handshake completes, and the command is never
    // answered. `wait_for_completion` checked its `--wait` deadline only at the
    // TOP of the loop and then blocked in an unbounded `XREVRANGE` read, so
    // control never returned to the check and the declared timeout could never
    // fire. `darkmux mission dispatch --wait 60` hung indefinitely.

    /// How long the fake peer below holds an accepted socket before dropping it.
    ///
    /// Deliberately LONGER than every wall-clock ceiling asserted here, and
    /// that is the whole point: when the peer CLOSES the socket the pending
    /// read returns EOF, which bounds the call *for free* and would make these
    /// tests pass with the response deadline removed. Same reasoning (and same
    /// vacuity trap) as `SILENT_PEER_HOLD` in `darkmux-flow`. (#2243)
    const SILENT_PEER_HOLD: Duration =
        Duration::from_millis(darkmux_flow::REDIS_RESPONSE_TIMEOUT.as_millis() as u64 * 10);

    /// Spawn a fake Redis peer that COMPLETES redis-rs's connection-setup
    /// handshake and then answers nothing. Copied in shape from
    /// `darkmux_flow::spawn_silent_redis_peer` (`#[cfg(test)]` there, so not
    /// reachable from this crate's test build).
    ///
    /// The two `+OK` replies are load-bearing: redis-rs 0.27 pipelines two
    /// ignored `CLIENT SETINFO` commands in `connection_setup_pipeline`. A peer
    /// that merely accepts TCP wedges at the HANDSHAKE, so every command-phase
    /// assertion written against it would pass vacuously against the connect
    /// phase instead. (#2243)
    fn spawn_silent_redis_peer(max_connections: usize) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(max_connections) {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    use std::io::Write;
                    let _ = stream.write_all(b"+OK\r\n+OK\r\n");
                    let _ = stream.flush();
                    std::thread::sleep(SILENT_PEER_HOLD);
                    drop(stream);
                });
            }
        });
        // Small settling margin only — `bind` already puts the socket in LISTEN.
        std::thread::sleep(Duration::from_millis(50));
        port
    }

    /// Anti-vacuity guard: prove the peer reaches the COMMAND phase, i.e. the
    /// connect SUCCEEDS and a command against it then times out. Costs one
    /// connection from the peer's budget. (#2243)
    fn assert_silent_peer_reaches_command_phase(port: u16) {
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str())
            .expect("open client against the fake peer");
        let mut conn = darkmux_flow::open_redis_connection_bounded(
            &client,
            darkmux_flow::REDIS_CONNECT_TIMEOUT,
        )
        .expect(
            "the fake peer must COMPLETE redis-rs's connection-setup pipeline — if the \
             connect fails, every wall-clock assertion here passes vacuously against the \
             CONNECT phase rather than the command phase #2243 is about",
        );
        darkmux_flow::bound_redis_response(&conn);
        let res: redis::RedisResult<String> = redis::cmd("PING").query(&mut conn);
        let err = res.expect_err("the fake peer answered a command; it must go silent");
        assert!(
            err.is_timeout(),
            "the response-deadline expiry must classify as `RedisError::is_timeout()` — \
             that predicate is what `wait_for_completion` keys on to END the wait with \
             its canonical timeout message. Got kind={:?} err={err:?}",
            err.kind()
        );
    }

    /// The predicate the fix turns on, verified against a REAL timing-out call
    /// rather than assumed from the docs. (#2243)
    #[test]
    fn response_deadline_expiry_classifies_as_a_redis_timeout_error() {
        let port = spawn_silent_redis_peer(2);
        assert_silent_peer_reaches_command_phase(port);
    }

    #[test]
    fn wait_for_completion_returns_within_a_bounded_wall_clock_against_a_silent_peer() {
        let port = spawn_silent_redis_peer(4);
        assert_silent_peer_reaches_command_phase(port);

        let url = darkmux_flow::RawRedisUrl::new(format!("redis://127.0.0.1:{port}"));
        let declared = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let err = wait_for_completion(&url, "sess-never-completes", declared)
            .expect_err("no completion record can ever arrive from a silent peer");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(6),
            "wait_for_completion must honor its declared --wait timeout even when the peer \
             accepts TCP and never answers; took {elapsed:?} for a {declared:?} wait. \
             Unbounded before #2243 (the read never returned to the elapsed check). \
             err={err:#}"
        );
    }

    #[test]
    fn wait_for_completion_ends_on_its_own_declared_timeout_not_a_per_poll_read_error() {
        // The bad trade this guards against: bounding the read makes a stalled
        // poll return `Err`, and surfacing that raw `Err` would tell the operator
        // "XREVRANGE on flow stream ...: Resource temporarily unavailable" —
        // losing the one message that says the job may still be running on the
        // runner and how to keep watching it.
        //
        // A read that hits the deadline now means the BUDGET is spent (the
        // deadline is the remaining budget), so it must produce that canonical
        // message and must not die early. Both halves are asserted below: the
        // wall clock reaches the declared wait, and the message is ours. (#2243)
        let port = spawn_silent_redis_peer(4);
        assert_silent_peer_reaches_command_phase(port);

        let url = darkmux_flow::RawRedisUrl::new(format!("redis://127.0.0.1:{port}"));
        let declared = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let err = wait_for_completion(&url, "sess-never-completes", declared).unwrap_err();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= declared,
            "the wait died at {elapsed:?}, BEFORE its declared {declared:?} — the read \
             deadline was shorter than the remaining budget, so a poll aborted the wait \
             early instead of the budget ending it. err={err:#}"
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no dispatch.complete"),
            "the wait must end on ITS OWN timeout error (which tells the operator the job \
             may still be running on the runner), not on a propagated per-poll read error. \
             Got: {msg}"
        );
    }

    // ─── `wait_for_completion` against a SLOW-BUT-HEALTHY peer (#2243) ───
    //
    // The three tests above all use a permanently SILENT peer, so every one of
    // them asserts on the failure path. The dangerous direction is the other
    // one: a peer that answers every command correctly and in order, just
    // slowly. Bounding the read with a FIXED deadline turns that peer's healthy
    // reply into a per-poll `Err`, and redis-rs then makes the damage permanent:
    //
    //   redis-0.27.6 `connection.rs` `Connection::read` — on a read error that
    //   is an IoError and is NOT `UnexpectedEof`, a RESPONSE read does
    //   `self.messages_to_skip += 1`. The next `read()` then DISCARDS that many
    //   successfully-parsed replies before returning one.
    //
    // A `continue` that re-issues the command without draining the backlog
    // creates and consumes the deficit at the same rate, so it never closes:
    // the client stays permanently one reply behind and throws every reply it
    // receives away as a skip. The wait then NEVER succeeds against a peer whose
    // completion record is right there — a loud hang traded for a silent wrong
    // answer, which `src/main.rs` counts as `failures += 1`.
    //
    // The fix derives the read deadline from the REMAINING wait budget, so a
    // healthy-but-slow poll completes normally and a timeout coincides with
    // budget exhaustion (ending the wait rather than continuing it, which is
    // what makes the deficit structurally unable to accumulate).

    /// The zero-duration guard, asserted rather than argued. `set_read_timeout`
    /// is handed this value at the exact instant the wait budget runs out; a
    /// zero would mean BLOCK FOREVER on a socket API that honors it, which is
    /// the original #2243 hang reappearing precisely when the operator is owed
    /// their timeout.
    ///
    /// The `elapsed == timeout` case is the one that matters and the one a
    /// `saturating_sub` gets wrong — it hands back `Duration::ZERO` where this
    /// must hand back `None`. (#2243)
    #[test]
    fn remaining_read_deadline_never_yields_a_zero_duration() {
        let budget = Duration::from_secs(5);

        // Budget spent: no deadline at all, so the caller ends the wait.
        assert_eq!(
            remaining_read_deadline(budget, budget),
            None,
            "elapsed EXACTLY equal to the budget must yield None, not \
             Some(Duration::ZERO) — this is the case `saturating_sub` gets wrong"
        );
        assert_eq!(remaining_read_deadline(budget, budget + Duration::from_secs(1)), None);

        // Budget left: a usable, strictly positive deadline.
        assert_eq!(
            remaining_read_deadline(budget, Duration::from_secs(2)),
            Some(Duration::from_secs(3))
        );

        // Sweep the whole boundary neighborhood at nanosecond grain: whatever
        // comes back must never be zero.
        for ns in 0..2_000u32 {
            let elapsed = budget - Duration::from_nanos(1_000) + Duration::from_nanos(ns as u64);
            if let Some(d) = remaining_read_deadline(budget, elapsed) {
                assert!(
                    !d.is_zero(),
                    "yielded a ZERO read deadline at elapsed={elapsed:?} of budget={budget:?} \
                     — `set_read_timeout(Some(Duration::ZERO))` means block-forever on socket \
                     APIs that honor it, which is #2243's hang at the worst possible moment"
                );
            }
        }

        // A zero-length wait can never produce a deadline either.
        assert_eq!(remaining_read_deadline(Duration::ZERO, Duration::ZERO), None);
    }

    /// The platform fact the guard above exists to not depend on, executed
    /// rather than quoted: `std` REJECTS a zero read deadline (it does not
    /// install a block-forever one), and the rejection is silently dropped by
    /// the `let _ =` at every call site — leaving whatever deadline was already
    /// in force. If this ever starts passing `Ok`, the guard in
    /// `remaining_read_deadline` is the only thing between #2243 and a hang.
    /// (#2243)
    #[test]
    fn std_rejects_a_zero_read_deadline_rather_than_blocking_forever() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                std::thread::sleep(Duration::from_secs(2));
                drop(stream);
            }
        });
        let sock = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to self");

        sock.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("a positive deadline installs");
        let err = sock
            .set_read_timeout(Some(Duration::ZERO))
            .expect_err("std must REJECT a zero read deadline");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert_eq!(
            sock.read_timeout().unwrap(),
            Some(Duration::from_secs(1)),
            "a rejected zero must leave the PREVIOUS deadline in force — which is \
             why the swallowed `let _ =` at the call site is not itself a hang"
        );
    }

    /// A real RESP2 `XREVRANGE` reply carrying one entry whose `record` field
    /// is a `dispatch complete` for `session_id` — the exact shape
    /// `scan_flow_entries_for_completion` walks. (#2243)
    fn xrevrange_reply_with_completion(session_id: &str) -> Vec<u8> {
        let record = serde_json::json!({
            "action": "dispatch complete",
            "session_id": session_id,
            "payload": { "result_class": "ok", "wall_ms": 42 },
        })
        .to_string();
        let mut out = Vec::new();
        out.extend_from_slice(b"*1\r\n"); // one entry
        out.extend_from_slice(b"*2\r\n"); // entry = [id, fields]
        out.extend_from_slice(b"$3\r\n1-0\r\n"); // id
        out.extend_from_slice(b"*2\r\n"); // fields = [k, v]
        out.extend_from_slice(b"$6\r\nrecord\r\n");
        out.extend_from_slice(format!("${}\r\n{record}\r\n", record.len()).as_bytes());
        out
    }

    /// Spawn a fake Redis peer that completes redis-rs's connection-setup
    /// handshake and then answers EVERY command correctly and in order — with a
    /// real completion-bearing `XREVRANGE` reply — after `latency`.
    ///
    /// This peer is HEALTHY. The only variable under test is how long its first
    /// byte takes relative to the read deadline. (#2243)
    fn spawn_slow_but_healthy_redis_peer(session_id: &str, latency: Duration) -> u16 {
        let reply = xrevrange_reply_with_completion(session_id);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let reply = reply.clone();
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    // The two `+OK`s redis-rs's `connection_setup_pipeline`
                    // expects for its two ignored `CLIENT SETINFO` commands
                    // (RESP2, no password, db 0 — verified in the crate source).
                    if stream.write_all(b"+OK\r\n+OK\r\n").is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {
                                std::thread::sleep(latency);
                                if stream.write_all(&reply).is_err() {
                                    return;
                                }
                                let _ = stream.flush();
                            }
                        }
                    }
                });
            }
        });
        // Small settling margin only — `bind` already puts the socket in LISTEN.
        std::thread::sleep(Duration::from_millis(50));
        port
    }

    /// CONTROL, and the anti-vacuity guard for the regression below: the same
    /// peer, the same reply bytes, at a latency well INSIDE any plausible read
    /// deadline. This proves the fake peer's reply actually parses into a
    /// `CompletionResult`, so a failure of the slow test is attributable to
    /// LATENCY alone rather than to a malformed fixture. (#2243)
    #[test]
    fn wait_for_completion_succeeds_against_a_fast_healthy_peer() {
        let session_id = "sess-fast-control";
        let port = spawn_slow_but_healthy_redis_peer(session_id, Duration::from_millis(100));

        let url = darkmux_flow::RawRedisUrl::new(format!("redis://127.0.0.1:{port}"));
        let got = wait_for_completion(&url, session_id, Duration::from_secs(5))
            .expect("a fast healthy peer's completion record must be found");

        assert_eq!(got.session_id, session_id);
        assert_eq!(got.result_class, "ok");
        assert_eq!(got.wall_ms, Some(42));
    }

    /// THE regression test for #2243's blocker. Same peer, same bytes, same
    /// completion record as the control above — only the latency changes, and
    /// it straddles the fixed per-command deadline round 1 used.
    ///
    /// With a fixed `REDIS_RESPONSE_TIMEOUT` deadline plus `continue`, this
    /// runs the full declared wait and returns the "no dispatch.complete"
    /// error for a job that completed. With the deadline derived from the
    /// remaining budget, the poll simply succeeds. (#2243)
    #[test]
    fn wait_for_completion_succeeds_against_a_slow_but_healthy_peer() {
        let session_id = "sess-slow-but-healthy";
        // Straddles the fixed deadline: longer than `REDIS_RESPONSE_TIMEOUT`,
        // far shorter than the declared wait budget below.
        let latency = darkmux_flow::REDIS_RESPONSE_TIMEOUT + Duration::from_millis(200);
        let port = spawn_slow_but_healthy_redis_peer(session_id, latency);

        let url = darkmux_flow::RawRedisUrl::new(format!("redis://127.0.0.1:{port}"));
        let declared = Duration::from_secs(5);
        let started = std::time::Instant::now();
        let got = wait_for_completion(&url, session_id, declared);
        let elapsed = started.elapsed();

        let got = got.unwrap_or_else(|e| {
            panic!(
                "a HEALTHY peer answered every XREVRANGE correctly and in order at {latency:?} \
                 with the completion record present, and the wait still failed after \
                 {elapsed:?} of its {declared:?} budget. This is the #2243 blocker: a fixed \
                 read deadline shorter than the peer's latency makes redis-rs bump \
                 `messages_to_skip` on every timed-out poll, and a `continue` that re-issues \
                 the command never drains that backlog — so every correct reply is discarded \
                 and the wait reports a completed job as still running. err={e:#}"
            )
        });

        assert_eq!(got.session_id, session_id);
        assert_eq!(got.result_class, "ok");
        assert_eq!(got.wall_ms, Some(42));
        assert!(
            elapsed < declared,
            "the wait must return as soon as the slow poll answers ({latency:?} plus connect), \
             not burn its whole {declared:?} budget; took {elapsed:?}"
        );
    }

    // ─── `wait_for_completion` against a HEALTHY peer with an EMPTY stream ───
    //
    // Every other fixture in this module is PATHOLOGICAL: permanently silent
    // (which exits through the inner `Err(e) if e.is_timeout()` arm) or
    // completion-bearing (which exits through `Ok`). Neither ever reaches the
    // LOOP-TOP budget check — the `let Some(remaining) = ... else { return
    // Err(budget_exhausted()) }` arm — so that arm had zero behavioral
    // coverage even though it is the arm #2243's operator symptom runs through.
    //
    // The path that reaches it is the ORDINARY one: Redis is fine, answers
    // every poll promptly, and the job simply has not finished yet, so the
    // stream holds no `dispatch.complete` and the wait must end when the
    // DECLARED BUDGET runs out. Break only the call site —
    //
    //     let remaining = remaining_read_deadline(timeout, start.elapsed())
    //         .unwrap_or(timeout);
    //
    // — and every read still succeeds, no timeout is ever raised, and the loop
    // spins forever: `--wait 60` never fires, which is #2243 verbatim. The pure
    // `remaining_read_deadline` unit test above stays green through that
    // mutation, which is exactly why this behavioral one has to exist.

    /// Spawn a fake Redis peer that completes redis-rs's connection-setup
    /// handshake and then answers EVERY command PROMPTLY with an empty RESP
    /// array (`*0\r\n`) — a healthy Redis whose stream holds no matching
    /// completion record yet. (#2243)
    fn spawn_healthy_empty_redis_peer() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    // The two `+OK`s redis-rs's `connection_setup_pipeline`
                    // expects for its two ignored `CLIENT SETINFO` commands.
                    if stream.write_all(b"+OK\r\n+OK\r\n").is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            // Empty array: a well-formed XREVRANGE reply that
                            // simply carries no entries. No latency at all —
                            // the read deadline must never be what ends this
                            // wait.
                            Ok(_) => {
                                if stream.write_all(b"*0\r\n").is_err() {
                                    return;
                                }
                                let _ = stream.flush();
                            }
                        }
                    }
                });
            }
        });
        // Small settling margin only — `bind` already puts the socket in LISTEN.
        std::thread::sleep(Duration::from_millis(50));
        port
    }

    /// Anti-vacuity guard for the test below: prove the peer ANSWERS, promptly
    /// and well-formed. If it went silent instead, the wait would exit through
    /// the read-timeout arm and the loop-top budget check would go untested
    /// again — the test would pass while covering nothing new. (#2243)
    fn assert_healthy_empty_peer_answers_promptly(port: u16) {
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}").as_str())
            .expect("open client against the fake peer");
        let mut conn = darkmux_flow::open_redis_connection_bounded(
            &client,
            darkmux_flow::REDIS_CONNECT_TIMEOUT,
        )
        .expect("the fake peer must COMPLETE redis-rs's connection-setup pipeline");
        darkmux_flow::bound_redis_response(&conn);

        let started = std::time::Instant::now();
        let got: redis::Value = redis::cmd("XREVRANGE")
            .arg("darkmux:flow")
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(WAIT_XRANGE_COUNT)
            .query(&mut conn)
            .expect(
                "the fake peer must ANSWER the command phase — a peer that times out here is a \
                 SILENT peer, and the wait below would then end through the read-timeout arm \
                 rather than the loop-top budget check this test exists to cover",
            );
        assert!(
            matches!(&got, redis::Value::Array(a) if a.is_empty()),
            "the peer must answer with an EMPTY stream (so no completion is ever found and the \
             budget is the only thing that can end the wait). Got {got:?}"
        );
        assert!(
            started.elapsed() < darkmux_flow::REDIS_RESPONSE_TIMEOUT,
            "the peer answered in {:?} — it must be PROMPT, so a read deadline can never be \
             what ends the wait below",
            started.elapsed()
        );
    }

    /// THE coverage for the loop-top budget check. A healthy peer answering
    /// every poll promptly with an empty stream is the single most common real
    /// `--wait` timeout: Redis is fine, the job is still running. The wait must
    /// end on the DECLARED budget.
    ///
    /// Run on a worker thread and collected with `recv_timeout` DELIBERATELY:
    /// the failure this guards against is an infinite loop, and a bare call
    /// would wedge the test binary (and CI) instead of going red. (#2243)
    #[test]
    fn wait_for_completion_ends_on_the_loop_top_budget_check_against_a_healthy_empty_peer() {
        let port = spawn_healthy_empty_redis_peer();
        assert_healthy_empty_peer_answers_promptly(port);

        let declared = Duration::from_secs(2);
        // Covers the documented overshoot ceiling (bounded connect 1s +
        // `declared` + one `WAIT_POLL_INTERVAL`) with room to spare, while
        // staying far below any plausible healthy return.
        let slack = Duration::from_secs(6);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let url = darkmux_flow::RawRedisUrl::new(format!("redis://127.0.0.1:{port}"));
            let started = std::time::Instant::now();
            let res = wait_for_completion(&url, "sess-still-running", declared);
            let _ = tx.send((res.map(|_| ()).map_err(|e| format!("{e:#}")), started.elapsed()));
        });

        let (res, elapsed) = rx.recv_timeout(declared + slack).unwrap_or_else(|_| {
            panic!(
                "wait_for_completion NEVER RETURNED within {:?} for a declared {declared:?}, \
                 against a HEALTHY peer answering every poll promptly with an empty stream. \
                 The loop-top budget check is the ONLY thing that can end this wait — no read \
                 ever times out and no completion is ever found — so this is #2243's original \
                 symptom: `--wait` that never fires.",
                declared + slack
            )
        });

        let err = res.expect_err("an empty stream can never yield a completion record");
        assert!(
            err.contains("no dispatch.complete"),
            "the wait must end with the canonical operator-facing timeout message (which names \
             the session and how to keep watching), not some propagated internal error. \
             Got: {err}"
        );
        assert!(
            elapsed >= declared,
            "the wait ended at {elapsed:?}, BEFORE its declared {declared:?} — a healthy peer's \
             prompt reply must never cut the budget short. err={err}"
        );
        assert!(
            elapsed < declared + slack,
            "the wait ran {elapsed:?} against a declared {declared:?}; the overshoot ceiling is \
             the bounded connect plus one poll interval, not this. err={err}"
        );
    }
}


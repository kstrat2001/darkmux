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
/// `crew::dispatch::dispatch` primitive — the pre-#1509, pre-#2628
/// behavior. This is the THIN WRAPPER's own default and stays the raw
/// primitive for every caller that reaches it: `phase_cli`'s QA-gate
/// dispatch is reached only from `MissionVerifyStepKind::run()`, an
/// already-wave-protected `StepKind` whose `seat()` has already resolved
/// residency for the WHOLE wave via `resolve_local_seat` — routing it
/// through `darkmux_crew::dispatch_reconciled::dispatch_reconciled`
/// instead would independently Exclusive-reconcile against a single
/// placement the scheduler already reconciled as part of a larger wave,
/// evicting concurrent wave siblings this call can't see (see that
/// module's own doc for the full hazard). `mission_propose` and `notebook
/// draft` are standalone (non-wave, non-`StepKind`) callers that DO want
/// #2628's Exclusive-reconcile + #1487 lease protection — they call
/// [`dispatch_routed_via`] directly with `dispatch_reconciled` as the
/// injected `local_dispatch`, rather than through this wrapper. Thin
/// wrapper over [`dispatch_routed_via`]; see that function's doc for the
/// full routing contract.
pub fn dispatch_routed(opts: DispatchOpts) -> Result<DispatchResult> {
    dispatch_routed_via(opts, dispatch::dispatch)
}

/// Route a dispatch local-vs-remote, then run it. When `--machine` is set
/// (and isn't the local machine), publish to the single global work queue
/// and (if `--wait`) block on the runner's `dispatch.complete` flow
/// record. Otherwise fall through to `local_dispatch` — a caller-injected
/// LOCAL execution primitive (#1509). `phase_cli`'s QA-gate dispatch passes
/// the raw `crew::dispatch::dispatch` primitive via the [`dispatch_routed`]
/// thin wrapper (unchanged pre-#1509 behavior — see that wrapper's doc for
/// why it stays raw); `mission_propose` and `notebook draft` call this
/// function directly with `darkmux_crew::dispatch_reconciled::
/// dispatch_reconciled` (#2628 — Exclusive-reconcile + a #1487 lease for a
/// standalone, non-wave dispatch); the CLI verb passes
/// `darkmux_crew::dispatch_as_crew_of_one::
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
                // (#2584, same class as #2561/#2580) `WorkJob` carries no
                // `resume_from` field at all, and the peer-side runner
                // (`runner.rs`) hardcodes it absent when it reconstructs
                // `DispatchOpts` — so a queued dispatch starts fresh on
                // the peer and exits 0 under a name the operator chose
                // because it looked like a resume. Refuse HERE, before
                // the route record is emitted or the queue is touched at
                // all: no flow record, no Redis connection, no WorkJob.
                // Carrying the checkpoint through the wire was considered
                // and rejected — a checkpoint is a directory on THIS
                // machine's filesystem, and the peer has no access to it,
                // so "resume on the peer" has no meaning to build toward
                // without a checkpoint-transfer feature this issue does
                // not ask for. Refusal is the correct behavior, not a
                // smaller compromise.
                if opts.resume_from.is_some() {
                    return Err(anyhow!(
                        "darkmux dispatch: --resume-from is not supported with \
                         --machine={target} (role `{}`): a queued dispatch runs on the \
                         PEER machine via the fleet work queue, which carries no \
                         checkpoint — the peer would start fresh and report success \
                         regardless. darkmux never silently starts a dispatch fresh under \
                         a name that looked like a resume: resume on THIS machine (drop \
                         --machine) or start this role fresh on the peer on purpose (drop \
                         --resume-from).",
                        opts.role_id
                    ));
                }
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
                // (#2584) Same refusal as the `local_unknown: true` arm
                // above — see its comment for the full mechanism and why
                // carrying the checkpoint through the wire is not the fix.
                if opts.resume_from.is_some() {
                    return Err(anyhow!(
                        "darkmux dispatch: --resume-from is not supported with \
                         --machine={target} (role `{}`): a queued dispatch runs on the \
                         PEER machine via the fleet work queue, which carries no \
                         checkpoint — the peer would start fresh and report success \
                         regardless. darkmux never silently starts a dispatch fresh under \
                         a name that looked like a resume: resume on THIS machine (drop \
                         --machine) or start this role fresh on the peer on purpose (drop \
                         --resume-from).",
                        opts.role_id
                    ));
                }
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
    use serial_test::serial;

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
            timeout_override_seconds: None, // (#2480)
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

    // ─── #2584: `--resume-from` routed to a peer via `--machine` must
    //     refuse BEFORE the fleet queue is ever touched ─────────────────
    //
    // `dispatch_via_queue` publishes a `WorkJob` that carries no
    // `resume_from` field at all (`queue.rs`'s `WorkJob` struct has none),
    // and the runner reconstructs `DispatchOpts` on the peer with
    // `resume_from: None` hardcoded (`runner.rs`). Before this fix, a
    // dispatch with `--machine <peer> --resume-from <dir>` sailed straight
    // past `dispatch()`'s own #2561/#2580 checkpoint refusals (which live
    // deep inside `crew::dispatch::dispatch`, never reached here) and
    // published a job that would start FRESH on the peer and exit 0 — the
    // exact promise-break #2561/#2580 closed on the other two routes,
    // reachable a third way.
    //
    // Same ORDER discipline as `dispatch_remote_refuses_resume_from_
    // before_the_http_call` (#2580, `darkmux-crew`): asserting only that
    // `dispatch_routed_via` returns an `Err` whose text mentions "resume"
    // cannot tell "refused before the queue was touched" apart from "the
    // queue rejected the job for an unrelated reason" — both produce an
    // `Err`. So this proves ORDER directly: a real loopback TCP listener
    // stands in for the fleet's Redis, and it must NEVER accept a
    // connection — `dispatch_via_queue`'s `redis::Client::open` +
    // `publish_job` is the only thing in this path that would ever dial
    // it.

    /// Spawn a bare TCP listener that records every accepted connection on
    /// `tx` and answers nothing (no Redis handshake, no protocol at all —
    /// it doesn't need to LOOK like Redis, it only needs to prove whether
    /// anything tried to connect). Deliberately simpler than
    /// `spawn_silent_redis_peer` above: this test's claim is "zero
    /// connections", not "the connection succeeds and then stalls", so
    /// there is nothing to gain from completing a real Redis handshake.
    fn spawn_connection_counting_peer() -> (u16, std::sync::mpsc::Receiver<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(_stream) = stream else { continue };
                let _ = tx.send(());
            }
        });
        // Small settling margin only — `bind` already puts the socket in LISTEN.
        std::thread::sleep(Duration::from_millis(50));
        (port, rx)
    }

    #[test]
    #[serial]
    fn dispatch_routed_via_refuses_resume_from_before_the_queue_is_touched() {
        let (port, rx) = spawn_connection_counting_peer();
        let flows_dir = tempfile::TempDir::new().unwrap();

        let prev_machine = std::env::var("DARKMUX_MACHINE_ID").ok();
        let prev_redis = std::env::var("DARKMUX_REDIS_URL").ok();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            // Local machine differs from the `--machine` target below, so
            // `routing_decision` resolves `Remote { local_unknown: false }`
            // — the ordinary cross-machine case, not the unresolvable-local
            // warning arm. That sibling arm can't be forced into
            // `local_unknown: true` from THIS shared unit-test binary (it
            // requires BOTH `DARKMUX_MACHINE_ID` unset AND the `hostname`
            // shell-out to fail — the latter is only forceable before
            // `darkmux_flow::resolve_machine_id()`'s process-wide
            // `OnceLock` caches a real hostname, which some earlier test in
            // this same binary has already done by the time this one runs).
            // It IS reachable from a dispatch, and is exercised end-to-end
            // in its own process by `resume_from_local_unknown_arm.rs`
            // (a separate integration-test binary in this crate's `tests/`).
            // Its guard is also proven by the structural conformance test
            // below, `every_dispatch_via_queue_call_site_is_guarded_against_
            // resume_from`, which reads this file's own source rather than
            // running it.
            std::env::set_var("DARKMUX_MACHINE_ID", "local-a");
            // Points the fleet queue's Redis client at the counting peer.
            // If the refusal did NOT run first, `dispatch_via_queue` would
            // dial this exact address.
            std::env::set_var("DARKMUX_REDIS_URL", format!("redis://127.0.0.1:{port}"));
            // Points the flow crate's LocalFileSink at a private, empty
            // directory. `local_sink_dir()` re-resolves this env var LIVE
            // on every `write()` (see its own doc — deliberately not
            // baked in at sink-construction time), so this reliably
            // targets THIS call's record, not whatever directory an
            // earlier test in this binary happened to freeze into the
            // process-wide sink singleton.
            std::env::set_var("DARKMUX_FLOWS_DIR", flows_dir.path());
        }

        let mut opts = local_opts("pr-reviewer");
        opts.machine = Some("peer-b".to_string());
        opts.resume_from = Some(std::path::PathBuf::from("/tmp/darkmux-2584-checkpoint"));

        let err = dispatch_routed_via(opts, |_opts| {
            panic!(
                "local_dispatch must never be invoked for a --machine=peer-b dispatch \
                 (this closure is the LOCAL fall-through seam; a remote target must never \
                 reach it regardless of resume_from)"
            );
        })
        .expect_err("--resume-from with --machine=<peer> must refuse, not route to the queue");
        let msg = format!("{err:#}");

        unsafe {
            match prev_machine {
                Some(v) => std::env::set_var("DARKMUX_MACHINE_ID", v),
                None => std::env::remove_var("DARKMUX_MACHINE_ID"),
            }
            match prev_redis {
                Some(v) => std::env::set_var("DARKMUX_REDIS_URL", v),
                None => std::env::remove_var("DARKMUX_REDIS_URL"),
            }
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }

        // POSITIVE: names the routed path and carries the same promise the
        // other two #2561/#2580 guards state — this makes it true here too.
        assert!(
            msg.contains("--machine=peer-b"),
            "must name the pinned target machine as the reason: {msg}"
        );
        assert!(
            msg.contains(
                "darkmux never silently starts a dispatch fresh under a name that looked \
                 like a resume"
            ),
            "must carry the same promise the other two guards state: {msg}"
        );

        // ORDER — no queue write, no job enqueued: the counting peer must
        // never have been dialed. A regression that deleted the check, or
        // moved it to run only after `dispatch_via_queue`'s
        // `redis::Client::open`/`publish_job`, would let this connect.
        match rx.recv_timeout(Duration::from_millis(300)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) => panic!(
                "dispatch_via_queue must never run for a refused resume, but the mock \
                 fleet-queue peer accepted a connection"
            ),
            Err(e) => panic!("unexpected mock channel state: {e:?}"),
        }

        // ORDER — no records emitted: `emit_route_record_and_resolve_
        // session` (the "dispatch route" flow record) must never have run
        // either. It writes through the LocalFileSink, which re-resolves
        // `DARKMUX_FLOWS_DIR` live per write (see the env-var comment
        // above) — so finding this private directory still empty proves
        // the emit call was never reached, not merely that a write to it
        // failed or landed elsewhere.
        let files: Vec<_> = std::fs::read_dir(flows_dir.path())
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            files.is_empty(),
            "no flow record may be written before the resume-from refusal fires; \
             found in {}: {files:?}",
            flows_dir.path().display()
        );
    }

    // ─── #2584 conformance: every call site of `dispatch_via_queue` must be
    //     guarded against `resume_from` ─────────────────────────────────
    //
    // The test above pins the `local_unknown: false` arm (the ordinary
    // cross-machine case) end-to-end. The sibling `local_unknown: true` arm
    // — local machine_id unresolvable — turns out ALSO to be reachable from
    // a real dispatch, in `resume_from_local_unknown_arm.rs` (a separate
    // integration-test binary in this crate's `tests/`): forcing it needs
    // only `DARKMUX_MACHINE_ID` unset and `PATH` emptied so the `hostname`
    // shell-out fails to spawn, and it must run in its OWN process because
    // `darkmux_flow::resolve_machine_id()` caches that shell-out's result in
    // a process-wide `OnceLock` — once any test in a shared binary resolves
    // it to `Some(<hostname>)`, every later test in that same process is
    // stuck with that cached value regardless of what env it mutates
    // afterward. An earlier version of this comment called that arm
    // "not producible portably" — that was true only of the EXISTING shared
    // unit-test binary, not of the arm itself; a fresh binary is enough.
    //
    // So this check is not the only thing standing between the operator and
    // the untested arm any more. It still earns its place as a SECOND,
    // structural layer: it does not run `dispatch_routed_via` at all, it
    // reads `routing.rs`'s own source and proves EACH match arm's own call
    // site carries the guard IN THAT ARM — a future edit that restores only
    // one arm's refusal, or drops a guard during a refactor of this match,
    // fails a fast test without needing a live dispatch to reach the arm
    // that lost it. **The per-arm scoping is load-bearing, not incidental,**
    // and has needed fixing TWICE, in opposite directions:
    // - The ORIGINAL version searched the whole ENCLOSING FUNCTION for a
    //   preceding guard rather than the call's own match-arm scope, which
    //   meant the FIRST arm's guard (textually earlier in the source)
    //   satisfied the search for the SECOND arm's call too — deleting only
    //   the second arm's guard left this check GREEN (too WIDE).
    // - The round-1 fix scoped the search to `nearest_enclosing_block`
    //   instead, which is itself wrong both ways: a brace-less arm has no
    //   block of its own, so the nearest enclosing block widens back out to
    //   the whole `match` (still too WIDE, same failure, different shape);
    //   and a call nested one level deeper than the guard WITHIN the same
    //   arm (an ordinary `if`, a closure body) shrinks the nearest
    //   enclosing block down PAST that arm's own guard, failing the check
    //   on correct code (too NARROW).
    // The round-2 fix (`guard_search_scope` / `match_arm_span_within`)
    // computes the arm's actual span structurally — from the enclosing
    // match's own depth-0 `=>` boundaries — rather than proxying it with
    // "nearest brace block". See `guard_search_scope`'s doc for the
    // mechanism and `resume_from_guard_precedes`'s doc for why `body` must
    // arrive pre-scoped either way.
    // - Round-2's own walk still broke, in the WIDE-only direction: it
    //   returned at the FIRST block it recognized as a match body, and a
    //   NESTED match between an arm's guard and its call also has its own
    //   depth-0 arrows — so the walk stopped at the inner match's arm
    //   instead of continuing out to the real, outer one, cutting the
    //   outer guard out of the search (a false ACCUSATION against correct
    //   code, never a false certification — see `guard_search_scope`'s
    //   doc for why the direction is structurally one-way). The round-3
    //   fix keeps walking through every containing block and remembers
    //   the OUTERMOST one recognized as a match, instead of stopping at
    //   the first.
    //
    // **Which layer is authoritative, for a maintainer facing one red and
    // one green:** the runtime test
    // (`dispatch_routed_via_refuses_resume_from_before_the_queue_is_touched`
    // below, plus its `local_unknown: true` sibling in
    // `resume_from_local_unknown_arm.rs`) is GROUND TRUTH — it runs the
    // real function against a real fake peer and observes whether a
    // connection was actually made. This structural scan is a CHEAP PROXY
    // for that ground truth, run on every `cargo test` without needing a
    // live dispatch; it exists to catch a regression FASTER, not to
    // out-rank the thing it approximates. A red scan with a green runtime
    // suite is worth investigating (the runtime tests only cover the two
    // arm shapes that exist TODAY, not every shape a future edit could
    // introduce) but is not proof of a live bypass by itself; a red
    // runtime test is.
    //
    // **Same shape as `darkmux-crew`'s `every_dispatch_remote_call_site_
    // is_guarded_against_resume_from` (#2580), NOT an extension of it.**
    // That check is hard-pinned to one file (`dispatch_internal.rs`) and
    // one identifier (`dispatch_remote`) in a DIFFERENT crate; generalizing
    // it to also cover `dispatch_via_queue` here would mean a
    // crate-or-workspace-wide scan — exactly the redesign its own doc
    // comment says a genuine visibility change would require, not
    // something worth building for a second, unrelated chokepoint. The
    // lexer discipline below (comment/string-aware, brace-matched,
    // structural if-block requirement) is copied in shape from that check
    // — same false-positive/false-negative traps apply to any text scan
    // over Rust source — but it is its own, separately-scoped check over
    // `routing.rs`, matching this file's much smaller premise (one caller
    // function, one private non-`pub` callee, no descendant module besides
    // its own inline `mod tests`).
    //
    // **What this cannot see, named plainly (same limits as #2580's
    // check, for the same reasons):**
    // - A `pub`/`pub(crate)` widening of `dispatch_via_queue`, or a new
    //   descendant module — both are pinned by the assertions below, so
    //   either fails LOUD rather than silently, but if `dispatch_via_queue`
    //   genuinely needs wider visibility this scan's premise is gone.
    // - A reimplementation of "publish this dispatch to the fleet queue"
    //   that never calls `dispatch_via_queue` itself. Not hypothetical: one
    //   already exists — `darkmux mission dispatch`'s per-phase fan-out
    //   loop (`src/main.rs`, around the `fleet::publish_job(&client, job)`
    //   call inside the `for (phase_id, session_id, job) in &jobs` loop)
    //   builds its own `WorkJob`s via `fleet::build_work_job` and calls
    //   `publish_job` directly, entirely outside this file. It is NOT a
    //   live bypass today only because `mission dispatch` has no
    //   `--resume-from` flag at all (only the single-dispatch `dispatch`
    //   verb does) — there is no checkpoint surface to silently drop. If
    //   `mission dispatch` ever grows one, it needs this same guard BEFORE
    //   its own publish loop, and this check will not notice either way.
    // - A call reached only through a function-pointer alias.
    // - A call inside an `impl` block method or a macro body (the function
    //   extractor only indexes column-0 `fn`/`pub fn` items) — this FAILS
    //   THE TEST LOUDLY (a panic naming the shape) rather than silently
    //   certifying it.
    // - A guard whose message text is factored into a helper function
    //   instead of inlined at the `return Err`/`bail!` site — the anchor
    //   match is textual against the CALLING function's own body.
    // - The anchor match inside a qualifying block is still TEXTUAL, not a
    //   real control-flow prover — accepted as vanishingly unlikely given
    //   how specific the anchor phrase is, same as #2580's check accepts.
    // - (#2609 review round 3 Also-fix 1) A call site sitting BEFORE the
    //   enclosing match's first arm's own `=>` — a match scrutinee, or a
    //   pattern guard's condition (`Foo(x) if call_here() => ...`) — falls
    //   back to `nearest_enclosing_block`'s WHOLE-match scope instead of a
    //   single arm's, so a sibling arm's guard is visible there too (too
    //   WIDE, same failure class as a brace-less arm, different trigger).
    //   Neither of this file's two real call sites is in a scrutinee or a
    //   pattern guard today — both are ordinary arm bodies — so this is
    //   correct by accident rather than by construction; see
    //   `match_arm_span_within`'s doc for the mechanism.

    /// If `cs[i]` begins a `//` line comment, `/* */` block comment, raw
    /// string, ordinary string, or char literal, returns the index just
    /// past it. Otherwise `None` — genuine code. Copied in shape from
    /// `darkmux-crew`'s `dispatch_internal_tests::skip_non_code_span`
    /// (different crate; that one isn't reachable from here).
    fn skip_non_code_span(cs: &[char], i: usize) -> Option<usize> {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        match c {
            '/' if next == Some('/') => {
                let mut j = i;
                while j < cs.len() && cs[j] != '\n' {
                    j += 1;
                }
                Some(j)
            }
            '/' if next == Some('*') => {
                let mut j = i + 2;
                while j + 1 < cs.len() && !(cs[j] == '*' && cs[j + 1] == '/') {
                    j += 1;
                }
                Some((j + 2).min(cs.len()))
            }
            'r' if next == Some('"') || next == Some('#') => {
                let mut hashes = 0usize;
                let mut j = i + 1;
                while cs.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if cs.get(j) != Some(&'"') {
                    return None;
                }
                j += 1;
                loop {
                    if j >= cs.len() {
                        break;
                    }
                    if cs[j] == '"' && (1..=hashes).all(|k| cs.get(j + k) == Some(&'#')) {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                Some(j)
            }
            '"' => {
                let mut j = i + 1;
                while j < cs.len() && cs[j] != '"' {
                    if cs[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                Some((j + 1).min(cs.len()))
            }
            '\'' => {
                let is_char_lit = match next {
                    Some('\\') => true,
                    Some(_) => cs.get(i + 2) == Some(&'\''),
                    None => false,
                };
                if is_char_lit {
                    let mut j = i + 1;
                    while j < cs.len() && cs[j] != '\'' {
                        if cs[j] == '\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    Some((j + 1).min(cs.len()))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// If a function-declaration keyword sequence (`fn `, `pub fn `, or a
    /// restricted-visibility form — `pub(crate) fn `, `pub(super) fn `,
    /// `pub(self) fn `, `pub(in a::b) fn `) begins at `cs[i]`, returns the
    /// index just past the trailing space of `fn `. Otherwise `None`.
    ///
    /// (#2584 review — Also-fix 1) The earlier version of this scan only
    /// recognized `fn ` and `pub fn `, so a `pub(crate) fn` at column 0
    /// (this very file already declares three: `scan_flow_entries_for_
    /// completion`, `match_completion`, `completion_to_dispatch_result`)
    /// silently dropped out of the function index — invisible to every
    /// consumer of `top_level_function_spans`, including the guard-search
    /// used by `every_dispatch_via_queue_call_site_is_guarded_against_
    /// resume_from` above, with no failure signal pointing at the real
    /// cause. Recognizing the restricted forms here closes that gap.
    fn fn_decl_prefix_len(cs: &[char], i: usize) -> Option<usize> {
        if cs[i..].starts_with(&['f', 'n', ' ']) {
            return Some(i + 3);
        }
        if !cs[i..].starts_with(&['p', 'u', 'b']) {
            return None;
        }
        let mut j = i + 3;
        if cs.get(j) == Some(&'(') {
            // Skip the parenthesized visibility qualifier — `(crate)`,
            // `(super)`, `(self)`, `(in some::path)` — by depth-matched
            // parens rather than a fixed keyword list, so a future Rust
            // edition's restricted-visibility syntax doesn't need a rewrite
            // here too.
            let mut depth = 1i32;
            let mut k = j + 1;
            while k < cs.len() && depth > 0 {
                match cs[k] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                k += 1;
            }
            if depth != 0 {
                return None; // unterminated — not genuine code; bail conservatively.
            }
            j = k;
        }
        // Require at least one space between the visibility keyword and `fn`.
        let space_start = j;
        while cs.get(j) == Some(&' ') {
            j += 1;
        }
        if j == space_start {
            return None;
        }
        if cs[j..].starts_with(&['f', 'n', ' ']) {
            Some(j + 3)
        } else {
            None
        }
    }

    /// Every top-level (column-0) function in `src`, as `(name,
    /// body_start, body_end)` byte-offset spans covering from the opening
    /// `{` through its matching closing `}`. Same algorithm as
    /// `darkmux-crew`'s `top_level_function_spans`, extended (#2584 review)
    /// to recognize restricted-visibility `fn` declarations via
    /// `fn_decl_prefix_len` above.
    fn top_level_function_spans(src: &str) -> Vec<(String, usize, usize)> {
        let mut spans = Vec::new();
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let at_line_start = i == 0 || cs[i - 1] == '\n';
            if at_line_start {
                if let Some(name_start) = fn_decl_prefix_len(&cs, i) {
                    let mut k = name_start;
                    while k < cs.len() && (cs[k].is_alphanumeric() || cs[k] == '_') {
                        k += 1;
                    }
                    let name: String = cs[name_start..k].iter().collect();
                    let mut j = k;
                    let mut paren_depth = 0i32;
                    while j < cs.len() {
                        match cs[j] {
                            '(' => paren_depth += 1,
                            ')' => paren_depth -= 1,
                            '{' if paren_depth == 0 => break,
                            ';' if paren_depth == 0 => break,
                            _ => {}
                        }
                        j += 1;
                    }
                    if j < cs.len() && cs[j] == '{' {
                        let mut depth = 0i32;
                        let mut m = j;
                        let body_start_char = j;
                        loop {
                            if m >= cs.len() {
                                break;
                            }
                            match cs[m] {
                                '{' => depth += 1,
                                '}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        let start_b = byte_offsets[body_start_char];
                                        let end_b = if m + 1 < byte_offsets.len() {
                                            byte_offsets[m + 1]
                                        } else {
                                            src.len()
                                        };
                                        spans.push((name.clone(), start_b, end_b));
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            m += 1;
                        }
                    }
                    i = j;
                    continue;
                }
            }
            i += 1;
        }
        spans
    }

    /// Every top-level `mod`/`pub mod`/`pub(crate) mod` DECLARATION line in
    /// `src` (comment/string-aware). Every module declared here is a
    /// DESCENDANT module, and Rust makes this file's private items
    /// (including `dispatch_via_queue`) visible to every descendant.
    fn top_level_mod_declarations(src: &str) -> Vec<String> {
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let at_line_start = i == 0 || cs[i - 1] == '\n';
            if at_line_start {
                let lookahead: String = cs[i..(i + 15).min(cs.len())].iter().collect();
                let is_mod_decl = lookahead.starts_with("mod ")
                    || lookahead.starts_with("pub mod ")
                    || lookahead.starts_with("pub(crate) mod ");
                if is_mod_decl {
                    let mut j = i;
                    while j < cs.len() && cs[j] != '\n' {
                        j += 1;
                    }
                    let b0 = byte_offsets[i];
                    let b1 = if j < byte_offsets.len() { byte_offsets[j] } else { src.len() };
                    out.push(src[b0..b1].trim().to_string());
                }
            }
            i += 1;
        }
        out
    }

    /// Every call-shaped occurrence of `name` in `src`: the whole
    /// identifier (not a longer identifier that merely contains it), found
    /// only at genuine code positions, followed by optional whitespace and
    /// then `(`. Excludes the `fn <name>(` / `pub fn <name>(` declaration
    /// itself. Returns the byte offset of the start of `name` for each hit.
    fn find_calls(src: &str, name: &str) -> Vec<usize> {
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let name_chars: Vec<char> = name.chars().collect();
        let mut hits = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let end = i + name_chars.len();
            let name_matches = end <= cs.len() && cs[i..end] == name_chars[..];
            if name_matches {
                let before_ok = i == 0 || !(cs[i - 1].is_alphanumeric() || cs[i - 1] == '_');
                let after_ok = cs.get(end).map(|c| !(c.is_alphanumeric() || *c == '_')).unwrap_or(true);
                if before_ok && after_ok {
                    let mut j = end;
                    while j < cs.len() && matches!(cs[j], ' ' | '\t' | '\r' | '\n') {
                        j += 1;
                    }
                    let is_call = cs.get(j) == Some(&'(');
                    let prefix: String = cs[i.saturating_sub(4)..i].iter().collect();
                    let is_declaration = prefix.ends_with("fn ");
                    if is_call && !is_declaration {
                        hits.push(byte_offsets[i]);
                    }
                }
            }
            i += 1;
        }
        hits
    }

    /// Every top-level-ish `if` in `body`, as `(cond_start, block_start,
    /// block_end)` byte offsets INTO `body`.
    fn find_if_blocks(body: &str) -> Vec<(usize, usize, usize)> {
        let cs: Vec<char> = body.chars().collect();
        let byte_offsets: Vec<usize> = body.char_indices().map(|(b, _)| b).collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let before_ok = i == 0 || !(cs[i - 1].is_alphanumeric() || cs[i - 1] == '_');
            let lookahead: String = cs[i..(i + 3).min(cs.len())].iter().collect();
            let is_if = before_ok && (lookahead.starts_with("if ") || lookahead.starts_with("if("));
            if is_if {
                let cond_start = i;
                let mut j = i + 2;
                let mut paren_depth = 0i32;
                loop {
                    if j >= cs.len() {
                        break;
                    }
                    if let Some(skip_to) = skip_non_code_span(&cs, j) {
                        j = skip_to;
                        continue;
                    }
                    match cs[j] {
                        '(' => paren_depth += 1,
                        ')' => paren_depth -= 1,
                        '{' if paren_depth <= 0 => break,
                        ';' if paren_depth <= 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                if j < cs.len() && cs[j] == '{' {
                    let block_start_char = j;
                    let mut depth = 0i32;
                    let mut m = j;
                    loop {
                        if m >= cs.len() {
                            break;
                        }
                        if let Some(skip_to) = skip_non_code_span(&cs, m) {
                            m = skip_to;
                            continue;
                        }
                        match cs[m] {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    let cond_start_b = byte_offsets[cond_start];
                                    let block_start_b = byte_offsets[block_start_char];
                                    let block_end_b =
                                        if m + 1 < byte_offsets.len() { byte_offsets[m + 1] } else { body.len() };
                                    out.push((cond_start_b, block_start_b, block_end_b));
                                    break;
                                }
                            }
                            _ => {}
                        }
                        m += 1;
                    }
                }
                i = j;
                continue;
            }
            i += 1;
        }
        out
    }

    /// Every matching brace-delimited block in `body` (comment/string-aware),
    /// at EVERY nesting depth — struct-pattern braces (`Foo { a, b } =>`)
    /// included, since this scan doesn't need to distinguish those from
    /// control-flow blocks: it only ever uses the result to find the
    /// SMALLEST block containing a given offset, and a struct-pattern brace
    /// never contains anything past its own `=>`.
    ///
    /// (#2584 review MUST-FIX) Added so the guard search below can be scoped
    /// to "the block immediately containing this call" instead of "anywhere
    /// in the whole enclosing function" — see `resume_from_guard_precedes`'s
    /// doc for why the wider scope was a false-negative trap.
    fn all_brace_block_spans(body: &str) -> Vec<(usize, usize)> {
        let cs: Vec<char> = body.chars().collect();
        let byte_offsets: Vec<usize> = body.char_indices().map(|(b, _)| b).collect();
        let mut stack: Vec<usize> = Vec::new();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            match cs[i] {
                '{' => stack.push(i),
                '}' => {
                    if let Some(start_char) = stack.pop() {
                        let start_b = byte_offsets[start_char];
                        let end_b =
                            if i + 1 < byte_offsets.len() { byte_offsets[i + 1] } else { body.len() };
                        out.push((start_b, end_b));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// The SMALLEST brace-delimited block in `body` that contains `at` —
    /// i.e. the nearest enclosing block (the containment test is half-open,
    /// `start <= at < end`, not a strict open interval). Falls back to the
    /// entire `body` span if `at` isn't inside any brace pair (shouldn't
    /// happen for a call site inside a real function body, but a scan
    /// bug here must fail wide-open rather than panic).
    ///
    /// (#2609 review round 2) This is no longer what scopes the guard
    /// search for a call inside a `match` arm — see `guard_search_scope`,
    /// which uses this only as its fallback for a call that isn't inside
    /// any `match` at all. Using this DIRECTLY as the arm-guard scope (the
    /// round-1 fix) was itself a proxy that broke in both directions: a
    /// brace-less arm has no block of its own, so the smallest enclosing
    /// block widens all the way out to the whole `match` — visible to a
    /// SIBLING arm's guard; and a call nested one level deeper than the
    /// guard WITHIN the same arm (an ordinary `if`, a closure body) shrinks
    /// the smallest enclosing block down PAST the arm's own guard. Neither
    /// failure is hypothetical — see `guard_search_scope`'s doc.
    fn nearest_enclosing_block(body: &str, at: usize) -> (usize, usize) {
        all_brace_block_spans(body)
            .into_iter()
            .filter(|(start, end)| *start <= at && at < *end)
            .min_by_key(|(start, end)| end - start)
            .unwrap_or((0, body.len()))
    }

    /// Every `=>` at bracket-depth 0 relative to `inner` (comment/string
    /// aware, depth tracked over `(){}[]` together), as the byte offset
    /// just PAST each one. `inner` is expected to be a brace block's
    /// content with its own wrapping `{`/`}` already stripped, so a
    /// `match`'s own arms — each `Pattern => body`, separated at the
    /// match's own top level — show up at depth 0 while anything inside a
    /// nested block (an arm's own `{ ... }` body, an `if`, a closure) does
    /// not.
    fn depth0_arrow_ends(inner: &str) -> Vec<usize> {
        let cs: Vec<char> = inner.chars().collect();
        let byte_offsets: Vec<usize> = inner.char_indices().map(|(b, _)| b).collect();
        let mut depth = 0i32;
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            match cs[i] {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                '=' if depth == 0 && cs.get(i + 1) == Some(&'>') => {
                    let end_char = i + 2;
                    let end_b = if end_char < byte_offsets.len() {
                        byte_offsets[end_char]
                    } else {
                        inner.len()
                    };
                    out.push(end_b);
                    i += 2;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// If `[block_start, block_end)` (a full brace-delimited span, braces
    /// included, as returned by `all_brace_block_spans`) is a `match`
    /// body — recognized STRUCTURALLY by having at least one `=>` at
    /// bracket-depth 0 relative to its own content, which is what a
    /// `match`'s own top level looks like and an ordinary `if`/closure/loop
    /// block does not — returns the byte span of the ARM that contains
    /// `at`: from just after that arm's own `=>` to just before the next
    /// arm's `=>` (or the block's closing brace, for the last arm).
    /// Returns `None` when `[block_start, block_end)` isn't a match body,
    /// so the caller keeps walking outward to a bigger ancestor block.
    ///
    /// This is deliberately generous at the arm's END boundary: it runs up
    /// to the NEXT arm's own `=>`, which over-includes that next arm's
    /// pattern (and any `if <guard>` on it) as part of "this" arm's
    /// span. Accepted as vanishingly unlikely to matter — the same kind of
    /// textual-heuristic tolerance `resume_from_guard_precedes`'s own doc
    /// already accepts — because a match arm's PATTERN never itself
    /// contains a `resume_from`-conditioned guard with this exact anchor.
    ///
    /// (#2609 review round 3 Also-fix 1, documentation only) When `at`
    /// sits inside `[block_start, block_end)` but BEFORE this match's
    /// first arm's own `=>` — the scrutinee, or a pattern guard's
    /// condition (`Foo(x) if call_here() => ...`) — none of the per-arm
    /// ranges built from `arrow_ends` cover it (they all start at or after
    /// the first arrow), so this returns `None` even though `at` genuinely
    /// is inside a match body. `guard_search_scope`'s walk then keeps going
    /// outward and can land on `nearest_enclosing_block`'s WHOLE-match
    /// fallback for that call, which makes a sibling arm's guard visible —
    /// too WIDE. Not fixed here: neither of this file's two real call
    /// sites sits in a scrutinee or a pattern guard (both are ordinary arm
    /// bodies), so this is a named gap, not a live bypass today.
    fn match_arm_span_within(
        body: &str,
        block_start: usize,
        block_end: usize,
        at: usize,
    ) -> Option<(usize, usize)> {
        if block_end < block_start + 2 {
            return None;
        }
        let inner = &body[block_start + 1..block_end - 1];
        let arrow_ends: Vec<usize> =
            depth0_arrow_ends(inner).into_iter().map(|off| off + block_start + 1).collect();
        if arrow_ends.is_empty() {
            return None;
        }
        for (idx, &arm_start) in arrow_ends.iter().enumerate() {
            let arm_end = arrow_ends.get(idx + 1).copied().unwrap_or(block_end - 1);
            if arm_start <= at && at < arm_end {
                return Some((arm_start, arm_end));
            }
        }
        None
    }

    /// The scope `resume_from_guard_precedes` should search for the call
    /// at `at` in `body`: when `at` sits inside a `match` arm (braced or
    /// brace-less), the byte span of THAT ARM alone — from
    /// `match_arm_span_within` — so a sibling arm's guard is invisible
    /// (the too-WIDE failure a brace-less arm produces against plain
    /// `nearest_enclosing_block`) while a nested block WITHIN the same arm
    /// (an ordinary `if`, a closure body) stays visible (the too-NARROW
    /// failure `nearest_enclosing_block` produces by shrinking to that
    /// nested block instead of the whole arm). Falls back to
    /// `nearest_enclosing_block` when `at` isn't inside any `match` at all
    /// — there is no arm to scope to.
    ///
    /// (#2609 review round 2) Walks every brace block containing `at`,
    /// SMALLEST first. Starting from the smallest is what fixes the
    /// too-narrow case: a nested `if`/closure block around the call is
    /// tried first, is correctly rejected (it has no depth-0 `=>` of its
    /// own), and the walk continues outward to the arm's own block, then —
    /// since an arm's own `{ ... }` body ALSO has no depth-0 `=>` of its
    /// own — outward again to the `match`'s own enclosing block, which
    /// does, and which is where `match_arm_span_within` computes the
    /// correct per-arm boundaries regardless of how deep the call sits
    /// inside that one arm.
    ///
    /// (#2609 review round 3 MUST-FIX) That reasoning silently assumed
    /// every block nested BETWEEN the arm's guard and the call has no
    /// `=>` of its own — true for an `if` or a closure, false for a
    /// NESTED `match`, which has depth-0 arrows relative to its own
    /// content just like the outer one does. The round-2 code returned at
    /// the FIRST block `match_arm_span_within` recognized, so a call
    /// wrapped in a nested match (with the real, outer arm's guard left
    /// untouched, earlier in the SAME outer arm) scoped to the inner
    /// match's own arm instead — cutting the outer guard out of the
    /// search and producing a false accusation against provably-correct
    /// code. Red-proven: wrapping `dispatch_via_queue(opts, Some(&target))`
    /// in `match true { true => return dispatch_via_queue(...), false =>
    /// {} }` inside the `local_unknown: false` arm, guard left in place,
    /// made the structural scan below FAIL while the runtime test
    /// (`dispatch_routed_via_refuses_resume_from_before_the_queue_is_
    /// touched`) stayed GREEN — ground truth says the guard fires, the
    /// scan accused it anyway.
    ///
    /// Fixed by not stopping at the first match: the walk now keeps going
    /// through every containing block, smallest to largest, and remembers
    /// the LAST (i.e. largest / outermost) span `match_arm_span_within`
    /// recognizes, falling back to `nearest_enclosing_block` only when
    /// NONE of them are. A nested match's own block is still recognized
    /// and still yields a span — that span is just no longer trusted as
    /// final the moment a bigger ancestor match also claims the offset.
    ///
    /// **Direction, corrected (round 4) — this paragraph previously had it
    /// backwards.** Taking the LAST (outermost) recognized arm span rather
    /// than the first does widen the chosen scope monotonically — an outer
    /// arm's span always contains every block nested inside it, never the
    /// reverse. But widening the search scope is what produces a false
    /// CERTIFICATION, not a false accusation: more text becomes visible to
    /// `resume_from_guard_precedes`, and — as round 3's own bug showed —
    /// that "more text" can be a SIBLING arm's guard inside a nested
    /// `match`, which the guard search then accepts as "preceding" a call
    /// that is genuinely unguarded in its own arm. A false ACCUSATION (a
    /// real guard the scan fails to see) is what a scope that is too
    /// NARROW produces — round 2's failure mode, not this one. So the
    /// security property this whole conformance check exists for did NOT
    /// hold throughout round 3; `nested_match_sibling_exclusions` below is
    /// what restores it, by excluding a nested match's sibling-arm text
    /// from the guard search even though that text sits inside the (still
    /// outermost) scope. This is also what the assertion's own failure
    /// message already said ("not a sibling arm's or an unrelated
    /// block's") while this comment claimed the opposite could never
    /// happen.
    fn guard_search_scope(body: &str, at: usize) -> (usize, usize) {
        let mut containing: Vec<(usize, usize)> = all_brace_block_spans(body)
            .into_iter()
            .filter(|(start, end)| *start <= at && at < *end)
            .collect();
        containing.sort_by_key(|(start, end)| end - start);
        let mut outermost_arm: Option<(usize, usize)> = None;
        for (start, end) in containing {
            if let Some(span) = match_arm_span_within(body, start, end, at) {
                outermost_arm = Some(span);
            }
        }
        outermost_arm.unwrap_or_else(|| nearest_enclosing_block(body, at))
    }

    /// Byte ranges within `body`, inside the outermost scope
    /// `[scope_start, scope_end)` computed by `guard_search_scope`, that
    /// must stay invisible to the guard search even though they sit inside
    /// that scope: the SIBLING-arm text of any `match` block that is
    /// nested strictly inside the scope and that also contains `at`.
    ///
    /// (#2609 review round 4 MUST-FIX) `guard_search_scope`'s outermost-arm
    /// widening (round 3) fixed round 2's too-NARROW failure but opened a
    /// too-WIDE one: when the call sits in one arm of a nested `match` and
    /// a DIFFERENT (sibling) arm of that SAME nested `match` carries a
    /// `resume_from` guard, the outer arm's span contains the nested
    /// match's whole brace block — every sibling arm included — so the
    /// sibling's guard reads as "preceding" the call even though the two
    /// arms are mutually exclusive at runtime and the call's own arm has
    /// no guard at all. Red-proven: wrapping the call in
    /// `match true { true => { <the real guard, moved here> } false => {
    /// dispatch_via_queue(...) } }`, with the guard genuinely absent from
    /// the `false` arm the call lives in, made the structural scan below
    /// PASS while the runtime test
    /// (`dispatch_routed_via_refuses_resume_from_before_the_queue_is_
    /// touched`, dialed against a real queue) FAILED — a live bypass the
    /// scan certified as safe.
    ///
    /// Fixed not by narrowing `guard_search_scope`'s scope (that would
    /// reopen round 2's bug) but by excluding, from WITHIN the unchanged
    /// outermost scope, every nested match's sibling-arm text: for each
    /// match block strictly inside the outer scope that also contains
    /// `at`, only the arm of THAT block containing `at` stays visible to
    /// the guard search — the rest of the block (its sibling arms) is
    /// excluded even though it lies inside the outer arm's byte range. A
    /// guard in the SAME inner arm as the call is unaffected (it isn't in
    /// an excluded range); a guard in a sibling arm is.
    fn nested_match_sibling_exclusions(
        body: &str,
        scope_start: usize,
        scope_end: usize,
        at: usize,
    ) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (block_start, block_end) in all_brace_block_spans(body) {
            // Only blocks STRICTLY nested inside the outer scope — this
            // naturally excludes the ancestor match block that produced
            // the outer scope itself (that block always starts at or
            // before `scope_start` and/or ends at or after `scope_end`,
            // since it also holds the outer arm's OWN siblings).
            if block_start < scope_start || block_end > scope_end {
                continue;
            }
            if !(block_start <= at && at < block_end) {
                continue;
            }
            if let Some((arm_start, arm_end)) = match_arm_span_within(body, block_start, block_end, at) {
                if arm_start > block_start {
                    out.push((block_start, arm_start));
                }
                if arm_end < block_end {
                    out.push((arm_end, block_end));
                }
            }
        }
        out
    }

    /// Collapse Rust string-literal line continuations (a `\` immediately
    /// followed by a newline strips the newline and the next line's
    /// leading whitespace) — this repo wraps long operator-facing messages
    /// that way, so a raw-byte substring search for a multi-word anchor
    /// would be brittle to wherever the literal happens to be wrapped.
    fn collapse_str_continuations(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let cs: Vec<char> = s.chars().collect();
        let mut i = 0usize;
        while i < cs.len() {
            if cs[i] == '\\' && cs.get(i + 1) == Some(&'\n') {
                i += 2;
                while i < cs.len() && matches!(cs[i], ' ' | '\t' | '\r' | '\n') {
                    i += 1;
                }
                continue;
            }
            out.push(cs[i]);
            i += 1;
        }
        out
    }

    /// The phrase every `--machine`+`--resume-from` guard must contain —
    /// the same promise `validate_resume_checkpoint` (container path) and
    /// the #2561/#2580 remote-single-shot guards state, restated true for
    /// the queued-peer route.
    const RESUME_FROM_GUARD_ANCHOR: &str =
        "darkmux never silently starts a dispatch fresh under a name that looked like a resume";

    /// True iff `body[..call_at_in_body]` contains an `if` block whose
    /// CONDITION mentions `resume_from`, whose block closes at or before
    /// `call_at_in_body`, and whose block text (after `\`-continuation
    /// collapsing) contains BOTH `RESUME_FROM_GUARD_ANCHOR` and a
    /// diverging construct (`return Err`/`bail!`/`panic!`).
    ///
    /// **`body` must already be scoped to the call site's own match-arm
    /// span (or nearest enclosing block, when the call isn't inside a
    /// match) — see `guard_search_scope` — never the whole enclosing
    /// FUNCTION and never just `nearest_enclosing_block` directly.**
    /// (#2584 review MUST-FIX; scoping mechanism replaced #2609 review
    /// round 2 — see `guard_search_scope`'s doc for why plain
    /// `nearest_enclosing_block` is itself insufficient) Both
    /// `dispatch_via_queue` call sites live in the same function
    /// (`dispatch_routed_via`'s two `Remote` match arms), and this function
    /// only checks "some guard occurs before this call ANYWHERE in `body`" —
    /// it has no notion of match-arm exclusivity. Called with the whole
    /// function body, the FIRST arm's guard (earlier in the source) would
    /// satisfy this check for the SECOND arm's call too, even after deleting
    /// the second arm's own guard — the two arms are mutually exclusive at
    /// runtime, but textually the first arm's guard still "precedes" the
    /// second arm's call. Scoping `body` to the call's own match-arm span
    /// closes this: the first arm's guard sits in a sibling span the scoped
    /// search never sees.
    ///
    /// `excluded` (#2609 review round 4 MUST-FIX) is a set of byte ranges
    /// INTO `body` — from `nested_match_sibling_exclusions` — that must be
    /// treated as if they weren't there, even though `body` is already
    /// scoped down to the outermost arm and these ranges sit inside it: a
    /// nested `match`'s sibling-arm text, which the outermost-scope fix
    /// (round 3) made visible again. A candidate guard whose `if` keyword
    /// starts inside one of these ranges is skipped, exactly as if it had
    /// never been found.
    fn resume_from_guard_precedes(
        body: &str,
        call_at_in_body: usize,
        excluded: &[(usize, usize)],
    ) -> bool {
        for (cond_start, block_start, block_end) in find_if_blocks(body) {
            if block_end > call_at_in_body {
                continue;
            }
            if excluded.iter().any(|(ex_start, ex_end)| cond_start >= *ex_start && cond_start < *ex_end) {
                continue;
            }
            let cond_text = &body[cond_start..block_start];
            if !cond_text.contains("resume_from") {
                continue;
            }
            let block_text = collapse_str_continuations(&body[block_start..block_end]);
            let diverges = block_text.contains("bail!")
                || block_text.contains("return Err")
                || block_text.contains("panic!");
            if diverges && block_text.contains(RESUME_FROM_GUARD_ANCHOR) {
                return true;
            }
        }
        false
    }

    /// Every genuine-code occurrence of the literal substring `needle` in
    /// `src` (comment/string-aware, via `skip_non_code_span`), as byte
    /// offsets. Needed because this conformance test's own source lives in
    /// the SAME FILE it scans (`routing.rs`'s `mod tests` is inline, unlike
    /// `dispatch_internal.rs` + its separate `dispatch_internal_tests.rs`
    /// in `darkmux-crew`) — a raw `str::matches` would also count this very
    /// test's own doc comments and the `DEFINITION_MARKER` string literal's
    /// VALUE as "occurrences", exactly the kind of false positive #2580's
    /// own review found and fixed for its sibling check.
    fn find_code_substring_occurrences(src: &str, needle: &str) -> Vec<usize> {
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let needle_chars: Vec<char> = needle.chars().collect();
        let mut hits = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let end = i + needle_chars.len();
            if end <= cs.len() && cs[i..end] == needle_chars[..] {
                hits.push(byte_offsets[i]);
            }
            i += 1;
        }
        hits
    }

    #[test]
    fn every_dispatch_via_queue_call_site_is_guarded_against_resume_from() {
        const DEFINITION_MARKER: &str = "fn dispatch_via_queue(";

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routing.rs");
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {} for resume_from conformance: {e}", path.display()));

        let functions = top_level_function_spans(&src);
        // (#2584 review — Also-fix 1) This file declares 9 top-level
        // (column-0, outside `mod tests`) functions today, 3 of them
        // `pub(crate)` — a shape `fn_decl_prefix_len` now recognizes
        // explicitly. The floor below is DELIBERATELY not pinned to
        // "today's count minus one": a floor with zero slack against the
        // real count means the FIRST legitimate function move out of this
        // file (a refactor, not a regression) trips this assertion, and a
        // message that only says "the extractor is broken" sends the
        // maintainer chasing the wrong cause — the real one being that this
        // scan's premise ("every relevant fn lives in this one file")
        // no longer holds. A few functions of real slack, plus a message
        // that names BOTH possibilities, keeps a genuine extractor
        // regression loud without turning an honest refactor into one.
        assert!(
            functions.len() > 5,
            "found only {} top-level fns in routing.rs (expected several more — this file \
             currently declares 9). Two different things produce this: (a) the extractor \
             regressed on a shape it should recognize (`fn_decl_prefix_len` — plain `fn`, \
             `pub fn`, or a restricted-visibility `pub(...) fn`), or (b) a function that used \
             to live at top-level in this file was genuinely moved or deleted, which means \
             this whole conformance scan's premise (everything relevant lives in THIS file) \
             may no longer hold. Check git blame on the delta before assuming either.",
            functions.len()
        );

        // ── pin: `dispatch_via_queue` stays module-PRIVATE ──────────────
        let definitions = find_code_substring_occurrences(&src, DEFINITION_MARKER);
        assert_eq!(
            definitions.len(),
            1,
            "expected exactly one code-position `{DEFINITION_MARKER}` declaration — found {}",
            definitions.len()
        );
        let def_at = definitions[0];
        let line_prefix = src[..def_at].rsplit('\n').next().unwrap_or("");
        assert_eq!(
            line_prefix, "",
            "`dispatch_via_queue` must stay module-PRIVATE for this scan's premise to hold — \
             found `{line_prefix}fn dispatch_via_queue(`, which reads as widened visibility. \
             If it genuinely needs wider visibility, this scan's premise is gone and it needs a \
             real redesign (a crate-or-workspace-wide scan), not a bigger pin."
        );

        // ── pin: this file declares no descendant module other than its
        //    own inline `#[cfg(test)] mod tests` ────────────────────────
        let mod_decls = top_level_mod_declarations(&src);
        assert_eq!(
            mod_decls,
            vec!["mod tests {".to_string()],
            "this scan's premise requires this file to declare NO descendant module other than \
             its own `#[cfg(test)] mod tests` — found: {mod_decls:?}. A new `mod` here is a \
             place a call to `dispatch_via_queue(` could live that this scan cannot see."
        );

        let call_offsets = find_calls(&src, "dispatch_via_queue");
        assert!(
            !call_offsets.is_empty(),
            "found zero calls to `dispatch_via_queue(` — either the extractor regressed or the \
             function was deleted; either way this test's premise no longer holds"
        );
        // (#2584 review — Also-fix 2) This is a bare count assertion, and
        // its failure cuts BOTH ways — don't just bump the constant to make
        // it pass again without asking which direction moved and why:
        // MORE than 2 is probably an honest new call site that needs the
        // same guard this test enforces on the existing two (bump the
        // constant once that guard is in place). FEWER than 2 is the
        // dangerous direction — it can mean a call site was hidden from
        // this scan rather than removed, e.g. a function-pointer alias
        // (`let f = dispatch_via_queue; ...; f(opts, ...)` — `find_calls`
        // only matches the identifier `dispatch_via_queue` immediately
        // followed by `(`, so an alias call never counts here at all).
        // Blindly lowering this constant to match a drop makes that
        // exact bypass permanent and silent.
        assert_eq!(
            call_offsets.len(),
            2,
            "expected exactly the two known call sites (both Remote arms of \
             `dispatch_routed_via`'s match) — found {}. If this went UP: a new call site needs \
             the same guard this test enforces on the existing two before you bump this \
             constant. If this went DOWN: do not just lower the constant — find out where the \
             missing call went first (a function-pointer alias is the known way a real call to \
             `dispatch_via_queue` can go invisible to this text scan).",
            call_offsets.len()
        );

        for call_at in call_offsets {
            let (fn_name, fn_start, fn_end) = functions
                .iter()
                .find(|(_, start, end)| *start <= call_at && call_at < *end)
                .unwrap_or_else(|| {
                    panic!(
                        "a `dispatch_via_queue(` call at byte offset {call_at} is not inside any \
                         top-level (column-0 `fn`/`pub fn`) function this scan indexes — extend \
                         `top_level_function_spans` before this test can vouch for it."
                    )
                });
            let body = &src[*fn_start..*fn_end];
            let call_at_in_body = call_at - fn_start;

            // (#2584 review MUST-FIX; scoping mechanism replaced #2609
            // review round 2) Scope the guard search to the call's own
            // match-arm SPAN — for these two call sites, that is each
            // `Remote` match arm's own extent, computed structurally from
            // the enclosing match's `=>` boundaries, not proxied by "the
            // nearest brace block" (which breaks in both directions — see
            // `guard_search_scope`'s doc) — NOT the whole `{fn_name}`
            // function. `dispatch_routed_via` holds BOTH `dispatch_via_
            // queue` call sites (one per match arm), and a function-wide
            // search would let one arm's guard vouch for the OTHER arm's
            // call, since match arms are mutually exclusive at runtime but
            // not textually ordered against each other. See
            // `resume_from_guard_precedes`'s doc for the full mechanism.
            let (scope_start, scope_end) = guard_search_scope(body, call_at_in_body);
            let scoped_body = &body[scope_start..scope_end];
            let call_at_in_scoped = call_at_in_body - scope_start;
            // (#2609 review round 4 MUST-FIX) The outermost scope above is
            // still right — narrowing it back would reopen round 2's bug —
            // but it can also make a nested match's SIBLING arm visible to
            // the guard search below. Exclude that sibling-arm text
            // explicitly; see `nested_match_sibling_exclusions`'s doc.
            let excluded_in_body =
                nested_match_sibling_exclusions(body, scope_start, scope_end, call_at_in_body);
            let excluded_in_scoped: Vec<(usize, usize)> = excluded_in_body
                .into_iter()
                .map(|(ex_start, ex_end)| (ex_start - scope_start, ex_end - scope_start))
                .collect();

            assert!(
                resume_from_guard_precedes(scoped_body, call_at_in_scoped, &excluded_in_scoped),
                "`{fn_name}` calls `dispatch_via_queue(` at file offset {call_at} without a \
                 `resume_from`-conditioned guard preceding it IN ITS OWN ENCLOSING SCOPE — the \
                 same match arm when the call sits in one, its own enclosing block otherwise. \
                 This is the #2561/#2580/#2584 bypass class: a caller can silently spend real \
                 tokens on a PEER machine under a --resume-from flag that was never honored. The \
                 guard must sit inside an `if` whose condition mentions `resume_from`, closes \
                 before the call, and contains both {RESUME_FROM_GUARD_ANCHOR:?} and a diverging \
                 bail!/return Err/panic! — all within that scope, not a sibling arm's or an \
                 unrelated block's. If this assertion is RED but the runtime tests \
                 (`dispatch_routed_via_refuses_resume_from_before_the_queue_is_touched` and \
                 `resume_from_local_unknown_arm.rs`) are GREEN: the runtime tests are ground \
                 truth (they run the real function against a real fake peer), this scan is a \
                 cheap proxy for them — investigate before assuming this scan is wrong, but a \
                 disagreement does not by itself mean this finding is a false positive."
            );
        }
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


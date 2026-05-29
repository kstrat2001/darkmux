//! Fleet worker loop — claims jobs off the tier work-stream and dispatches them.

use crate::{ack_job, claim_job, init_consumer_group, work_stream_name, ClaimedJob, WorkJob};
use std::path::PathBuf;
use std::time::Duration;

// ─── Daemon worker loop (PR-C.2) ──────────────────────────────────────
//
// Runs on a dedicated `std::thread` (not a tokio task) inside the
// `darkmux serve` daemon. Polls `darkmux:work:<own-tier>` via XREADGROUP
// with a short BLOCK budget; on claim, invokes the existing synchronous
// `crew::dispatch::dispatch(opts)` and acks on completion. The dispatch
// path is unchanged — whether work arrives via local CLI invocation OR
// queue claim, it lands at the same entry point.
//
// **Why a dedicated thread, not a tokio task:** the redis crate (sync)
// + `crew::dispatch::dispatch` (shells out to docker / openclaw, blocks
// 5+ minutes) would saturate the tokio executor. The thread runs
// independently of the axum server's runtime.

/// Consumer group name used by all darkmux workers. Per-tier; combined
/// with the stream name, every worker for a given tier shares the
/// group → exactly-one-consumer-per-job delivery.
pub(crate) const WORKER_CONSUMER_GROUP: &str = "darkmux-workers";

/// XREADGROUP BLOCK budget per poll. 2 seconds is short enough that
/// shutdown latency is bounded (the worker rechecks the shutdown flag
/// every BLOCK round) and long enough that a quiet queue doesn't
/// hot-spin Redis. (#246 PR-C.2)
const WORKER_BLOCK_MS: u64 = 2_000;

/// Spawn the daemon worker thread. Returns the JoinHandle so callers
/// can monitor (typically the daemon never joins — the worker runs
/// for the daemon's lifetime and dies when the process exits).
///
/// Reads three env vars at spawn time:
/// - `DARKMUX_REDIS_URL` — required; absent → worker doesn't start
/// - `DARKMUX_MACHINE_TIER` — required; absent → worker doesn't start
/// - `DARKMUX_MACHINE_ID` — used as consumer name (per-machine identity)
///
/// When prerequisites are missing, logs to stderr and returns a thread
/// that exits immediately (caller still gets a JoinHandle). This keeps
/// the daemon usable as an observability node even without queue
/// participation — same posture as the existing single-machine-fleet
/// default in `fleet status`.
pub fn spawn_worker_thread() -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("darkmux-worker".to_string())
        .spawn(worker_main)
        .expect("spawn darkmux-worker thread")
}

/// Entry point for the worker thread. Reads env config, opens Redis,
/// initializes the consumer group, then loops on claim/dispatch/ack.
fn worker_main() {
    let Some(redis_url) = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        eprintln!(
            "darkmux-worker: DARKMUX_REDIS_URL not set — fleet work queue disabled. \
             Daemon continues as observability/serve node only."
        );
        return;
    };

    let Some(tier) = darkmux_flow::resolve_machine_tier() else {
        eprintln!(
            "darkmux-worker: DARKMUX_MACHINE_TIER not set — fleet work queue disabled. \
             Set DARKMUX_MACHINE_TIER=<inference|hub|client> to enable."
        );
        return;
    };

    let machine_id = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());

    let url = darkmux_flow::RawRedisUrl::new(redis_url);
    let client = match redis::Client::open(url.expose_for_probe()) {
        Ok(c) => c,
        Err(e) => {
            // `{e:#}` walks the anyhow context chain — single-level
            // `{e}` would hide the underlying redis-rs cause behind
            // our `.with_context` wrapper. Operator needs the full
            // chain to diagnose. (PR-C.2 review carry-over)
            eprintln!(
                "darkmux-worker: failed to open Redis client ({url}): {e:#}. \
                 Queue worker disabled."
            );
            return;
        }
    };

    if let Err(e) = init_consumer_group(&client, &tier, WORKER_CONSUMER_GROUP) {
        eprintln!(
            "darkmux-worker: init_consumer_group on darkmux:work:{tier} failed: {e:#}. \
             Queue worker disabled."
        );
        return;
    }

    eprintln!(
        "darkmux-worker: started — tier={tier} consumer={machine_id} \
         stream={} group={}",
        work_stream_name(&tier),
        WORKER_CONSUMER_GROUP
    );

    loop {
        match claim_job(
            &client,
            &tier,
            WORKER_CONSUMER_GROUP,
            &machine_id,
            WORKER_BLOCK_MS,
        ) {
            Ok(None) => {
                // BLOCK timeout — no work. Loop and re-block.
                continue;
            }
            Ok(Some(claimed)) => {
                handle_claimed_job(&client, &tier, claimed);
            }
            Err(e) => {
                eprintln!("darkmux-worker: claim_job failed ({e}); backing off 1s");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Validate, dispatch, and ack one claimed job. Errors are logged and
/// the job is acked anyway — the `dispatch.complete` flow record (or
/// its absence) is the operator-visible signal; the ack just releases
/// the queue lease.
fn handle_claimed_job(client: &redis::Client, tier: &str, claimed: ClaimedJob) {
    let ClaimedJob { work_id, job } = claimed;
    let session_id = job.session_id.clone();
    let role_id = job.role_id.clone();
    eprintln!(
        "darkmux-worker: claimed work_id={work_id} role={role_id} \
         session={session_id} target_machine={:?} attempt={}",
        job.target_machine, job.attempt
    );

    // Boundary validation — reject malformed jobs at the consumer too,
    // even though `publish_job` validated. Belt-and-braces against a
    // hostile publisher who bypassed our publish path.
    if let Err(e) = job.validate() {
        eprintln!(
            "darkmux-worker: REJECTED claimed job {work_id}: {e:#}. \
             Acking to release queue lease; dispatch NOT invoked."
        );
        let _ = ack_job(client, tier, WORKER_CONSUMER_GROUP, &work_id);
        return;
    }

    // Workdir symlink-escape guard via the shared validator (Wave-E.2 /
    // #255). The dispatch path itself ALSO validates, but doing it
    // here is the canonical "queue boundary" check — operator sees
    // the rejection in the worker's flow records via dispatch.error,
    // not buried deep in the internal/openclaw dispatch path.
    if let Some(workdir_str) = &job.workdir {
        let path = std::path::Path::new(workdir_str);
        if let Err(e) = darkmux_types::workdir::validate_workdir(path) {
            eprintln!(
                "darkmux-worker: REJECTED claimed job {work_id}: workdir validation failed: {e:#}. \
                 Acking to release queue lease; dispatch NOT invoked."
            );
            let _ = ack_job(client, tier, WORKER_CONSUMER_GROUP, &work_id);
            return;
        }
    }

    // Optional target_machine pre-claim hint: when set, the publisher
    // asserted this specific machine should handle the job. If it
    // doesn't match the local machine_id, log a warning but proceed —
    // the queue already gave us the claim, refusing would orphan the
    // job (PR-E will handle this properly via lease re-publish).
    let local_machine = darkmux_flow::resolve_machine_id();
    if let Some(target) = &job.target_machine {
        if local_machine.as_deref() != Some(target.as_str()) {
            eprintln!(
                "darkmux-worker: target_machine={target:?} doesn't match \
                 local machine_id={local_machine:?}; proceeding (queue \
                 already claimed; PR-E will add lease re-publish)."
            );
        }
    }

    // Convert + dispatch. The dispatch function is synchronous and may
    // block several minutes for long-agentic dispatches.
    let opts = job.into_dispatch_opts();
    let dispatch_result = darkmux_crew::dispatch::dispatch(opts);

    match dispatch_result {
        Ok(outcome) => {
            eprintln!(
                "darkmux-worker: dispatched work_id={work_id} → exit_code={} \
                 stdout_bytes={} stderr_bytes={}",
                outcome.exit_code,
                outcome.stdout.len(),
                outcome.stderr.len(),
            );
        }
        Err(e) => {
            eprintln!(
                "darkmux-worker: dispatch ERROR work_id={work_id}: {e:#}. \
                 Acking to release queue lease; dispatch.complete flow \
                 record carries the failure detail."
            );
        }
    }

    if let Err(e) = ack_job(client, tier, WORKER_CONSUMER_GROUP, &work_id) {
        eprintln!("darkmux-worker: XACK failed for {work_id}: {e:#}");
    }
}

impl WorkJob {
    /// Convert a claimed `WorkJob` into the `DispatchOpts` shape the
    /// `crew::dispatch::dispatch` entry point consumes. Centralizes the
    /// queue → in-process boundary so PR-C.3's client path can be checked
    /// against this shape for round-trip parity.
    pub fn into_dispatch_opts(self) -> darkmux_crew::dispatch::DispatchOpts {
        use darkmux_crew::dispatch::DispatchOpts;
        DispatchOpts {
            role_id: self.role_id,
            message: self.message,
            deliver: self.deliver,
            session_id: Some(self.session_id),
            timeout_seconds: self.timeout_seconds,
            skip_preflight: false,
            // Worker-side dispatches preserve today's human-readable
            // stdout shape — JSON-envelope mode is operator-explicit
            // and only fires when the originating CLI used --json.
            // (When cross-machine plumbing eventually carries the flag
            // through WorkJob, this can read self.json.)
            json: false,
            watch_paths: vec![],
            workdir: self.workdir.map(PathBuf::from),
            sprint_id: self.sprint_id,
            runtime: self.runtime,
            // Worker-side runtime_cmd hardcoded to "openclaw" —
            // intentionally NOT threaded from the publisher's WorkJob.
            // A publisher-local binary path (`/opt/aider/aider` on the
            // dispatcher) doesn't translate to a remote worker's
            // filesystem; when cross-machine openclaw becomes a
            // first-class use case the field belongs to the WORKER's
            // local config (read at claim-time), not to the WorkJob
            // serialized off the publisher. Internal-runtime jobs
            // ignore the field entirely.
            runtime_cmd: "openclaw".to_string(),
            // Worker-side opts: never recurse into the queue (would
            // ping-pong jobs back to redis); always run local synchronous.
            machine: None,
            wait: true,
            // Fleet-deserialized dispatch jobs: producer didn't
            // capture compaction config (pre-#368 wire shape). Use
            // runtime defaults. Future iteration could propagate via
            // the job payload if cross-machine compaction tuning
            // becomes a real requirement.
            compaction: darkmux_crew::dispatch::CompactionDispatchArgs::default(),
        }
    }
}


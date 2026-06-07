//! Fleet runner loop — claims jobs off the global work-stream and dispatches them.

use crate::{ack_job, claim_job, init_consumer_group, ClaimedJob, WorkJob, WORK_STREAM};
use std::path::PathBuf;
use std::time::Duration;

// ─── Daemon runner loop (PR-C.2) ──────────────────────────────────────
//
// Runs on a dedicated `std::thread` (not a tokio task) inside the
// `darkmux serve` daemon. Polls the single global `darkmux:work` stream
// (#590) via XREADGROUP with a short BLOCK budget; on claim, invokes the
// existing synchronous `crew::dispatch::dispatch(opts)` and acks on
// completion. The dispatch path is unchanged — whether work arrives via
// local CLI invocation OR queue claim, it lands at the same entry point.
//
// **Why a dedicated thread, not a tokio task:** the redis crate (sync)
// + `crew::dispatch::dispatch` (shells out to docker / openclaw, blocks
// 5+ minutes) would saturate the tokio executor. The thread runs
// independently of the axum server's runtime.

/// Consumer group name used by all darkmux runners. Combined with the
/// single global stream name, every runner shares the group →
/// exactly-one-consumer-per-job delivery.
pub(crate) const RUNNER_CONSUMER_GROUP: &str = "darkmux-runners";

/// XREADGROUP BLOCK budget per poll. 2 seconds is short enough that
/// shutdown latency is bounded (the runner rechecks the shutdown flag
/// every BLOCK round) and long enough that a quiet queue doesn't
/// hot-spin Redis. (#246 PR-C.2)
const RUNNER_BLOCK_MS: u64 = 2_000;

/// Spawn the daemon runner thread. Returns the JoinHandle so callers
/// can monitor (typically the daemon never joins — the runner runs
/// for the daemon's lifetime and dies when the process exits).
///
/// Reads two env vars at spawn time:
/// - `DARKMUX_REDIS_URL` — required; absent → runner doesn't start
///   (Redis presence is the participation gate, #590)
/// - `DARKMUX_MACHINE_ID` — used as consumer name (per-machine identity)
///
/// When prerequisites are missing, logs to stderr and returns a thread
/// that exits immediately (caller still gets a JoinHandle). This keeps
/// the daemon usable as an observability node even without queue
/// participation — same posture as the existing single-machine-fleet
/// default in `fleet status`.
pub fn spawn_runner_thread() -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("darkmux-runner".to_string())
        .spawn(runner_main)
        .expect("spawn darkmux-runner thread")
}

/// Entry point for the runner thread. Reads env config, opens Redis,
/// initializes the consumer group, then loops on claim/dispatch/ack.
fn runner_main() {
    // env(DARKMUX_REDIS_URL) > config-assembled (#661 Slice 5).
    let Some(url) = darkmux_flow::redis_url() else {
        eprintln!(
            "darkmux-runner: Redis not configured (DARKMUX_REDIS_URL or \
             config.redis.enabled) — fleet work queue disabled. \
             Daemon continues as observability/serve node only."
        );
        return;
    };

    let machine_id = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());

    let client = match redis::Client::open(url.expose_for_probe()) {
        Ok(c) => c,
        Err(e) => {
            // `{e:#}` walks the anyhow context chain — single-level
            // `{e}` would hide the underlying redis-rs cause behind
            // our `.with_context` wrapper. Operator needs the full
            // chain to diagnose. (PR-C.2 review carry-over)
            eprintln!(
                "darkmux-runner: failed to open Redis client ({url}): {e:#}. \
                 Queue runner disabled."
            );
            return;
        }
    };

    if let Err(e) = init_consumer_group(&client, RUNNER_CONSUMER_GROUP) {
        eprintln!(
            "darkmux-runner: init_consumer_group on {WORK_STREAM} failed: {e:#}. \
             Queue runner disabled."
        );
        return;
    }

    eprintln!(
        "darkmux-runner: started — consumer={machine_id} \
         stream={WORK_STREAM} group={RUNNER_CONSUMER_GROUP}"
    );

    loop {
        match claim_job(&client, RUNNER_CONSUMER_GROUP, &machine_id, RUNNER_BLOCK_MS) {
            Ok(None) => {
                // BLOCK timeout — no work. Loop and re-block.
                continue;
            }
            Ok(Some(claimed)) => {
                handle_claimed_job(&client, claimed);
            }
            Err(e) => {
                eprintln!("darkmux-runner: claim_job failed ({e}); backing off 1s");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Validate, dispatch, and ack one claimed job. Errors are logged and
/// the job is acked anyway — the `dispatch.complete` flow record (or
/// its absence) is the operator-visible signal; the ack just releases
/// the queue lease.
fn handle_claimed_job(client: &redis::Client, claimed: ClaimedJob) {
    let ClaimedJob { work_id, job } = claimed;
    let session_id = job.session_id.clone();
    let role_id = job.role_id.clone();
    eprintln!(
        "darkmux-runner: claimed work_id={work_id} role={role_id} \
         session={session_id} target_machine={:?} attempt={}",
        job.target_machine, job.attempt
    );

    // Boundary validation — reject malformed jobs at the consumer too,
    // even though `publish_job` validated. Belt-and-braces against a
    // hostile publisher who bypassed our publish path.
    if let Err(e) = job.validate() {
        eprintln!(
            "darkmux-runner: REJECTED claimed job {work_id}: {e:#}. \
             Acking to release queue lease; dispatch NOT invoked."
        );
        let _ = ack_job(client, RUNNER_CONSUMER_GROUP, &work_id);
        return;
    }

    // Workdir symlink-escape guard via the shared validator (Wave-E.2 /
    // #255). The dispatch path itself ALSO validates, but doing it
    // here is the canonical "queue boundary" check — operator sees
    // the rejection in the runner's flow records via dispatch.error,
    // not buried deep in the internal/openclaw dispatch path.
    if let Some(workdir_str) = &job.workdir {
        let path = std::path::Path::new(workdir_str);
        if let Err(e) = darkmux_types::workdir::validate_workdir(path) {
            eprintln!(
                "darkmux-runner: REJECTED claimed job {work_id}: workdir validation failed: {e:#}. \
                 Acking to release queue lease; dispatch NOT invoked."
            );
            let _ = ack_job(client, RUNNER_CONSUMER_GROUP, &work_id);
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
                "darkmux-runner: target_machine={target:?} doesn't match \
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
                "darkmux-runner: dispatched work_id={work_id} → exit_code={} \
                 stdout_bytes={} stderr_bytes={}",
                outcome.exit_code,
                outcome.stdout.len(),
                outcome.stderr.len(),
            );
        }
        Err(e) => {
            eprintln!(
                "darkmux-runner: dispatch ERROR work_id={work_id}: {e:#}. \
                 Acking to release queue lease; dispatch.complete flow \
                 record carries the failure detail."
            );
        }
    }

    if let Err(e) = ack_job(client, RUNNER_CONSUMER_GROUP, &work_id) {
        eprintln!("darkmux-runner: XACK failed for {work_id}: {e:#}");
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
            // Runner-side dispatches preserve today's human-readable
            // stdout shape — JSON-envelope mode is operator-explicit
            // and only fires when the originating CLI used --json.
            // (When cross-machine plumbing eventually carries the flag
            // through WorkJob, this can read self.json.)
            json: false,
            watch_paths: vec![],
            workdir: self.workdir.map(PathBuf::from),
            sprint_id: self.sprint_id,
            runtime: self.runtime,
            // Runner-side runtime_cmd hardcoded to "openclaw" —
            // intentionally NOT threaded from the publisher's WorkJob.
            // A publisher-local binary path (`/opt/aider/aider` on the
            // dispatcher) doesn't translate to a remote runner's
            // filesystem; when cross-machine openclaw becomes a
            // first-class use case the field belongs to the RUNNER's
            // local config (read at claim-time), not to the WorkJob
            // serialized off the publisher. Internal-runtime jobs
            // ignore the field entirely.
            runtime_cmd: "openclaw".to_string(),
            // Runner-side opts: never recurse into the queue (would
            // ping-pong jobs back to redis); always run local synchronous.
            machine: None,
            wait: true,
            // Fleet-deserialized dispatch jobs: producer didn't
            // capture compaction config (pre-#368 wire shape). Use
            // runtime defaults. Future iteration could propagate via
            // the job payload if cross-machine compaction tuning
            // becomes a real requirement.
            compaction: darkmux_crew::dispatch::CompactionDispatchArgs::default(),
            // (#549) Fleet-deserialized jobs don't carry a `--profile`
            // override (pre-#549 wire shape) — the runner resolves against
            // its local `default_profile`. Future iteration could propagate
            // via the job payload if cross-machine profile selection becomes
            // a requirement.
            profile_name: None,
            // (#703 Slice 4) Honor the image the publisher requested (carried
            // on the WorkJob); the runner injects darkmux's binary into it.
            // `None` → the runner's default slim image.
            image: self.image,
        }
    }
}


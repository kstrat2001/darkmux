// Small, HAND-AUTHORED (not corpus-derived) dataset for the `peer` machine.
// The hub gets the full parity corpus (real recorded shape, real volume);
// the peer gets its own distinct, deliberately small board so the
// fleet-visible scenario (0b.a) has genuinely DIFFERENT content to prove
// is reachable from the hub's aggregated views, not a copy of the hub's
// own data. Every string here is synthetic by construction — no sanitizer
// input needed, but seed.mjs's tripwire pass scans it anyway (uniform
// treatment, not a special case).
const PEER_UID = "9F2C6A11-FLAT-SAT2-PEER-000000000002";

export function buildPeerFixture(nowMs) {
  const hourMs = 3_600_000;
  const t = (offsetHoursFromNow) => Math.floor((nowMs + offsetHoursFromNow * hourMs) / 1000);
  const iso = (offsetHoursFromNow) => new Date(nowMs + offsetHoursFromNow * hourMs).toISOString().replace(/\.\d{3}Z$/, "Z");

  const missions = [
    {
      id: "peer-demo-refactor",
      description: "Peer-machine demo mission: a small finalized refactor phase, seeded for the flatsat's fleet-visible scenario.",
      status: "Finalized",
      phase_ids: ["peer-demo-refactor-phase"],
      created_ts: t(-3),
      started_ts: t(-3),
      finalized_ts: t(-2),
    },
    {
      id: "peer-demo-docs",
      description: "Peer-machine demo mission: an in-progress docs pass, seeded so the peer board shows both a finished and a running row.",
      status: "Active",
      phase_ids: ["peer-demo-docs-phase"],
      created_ts: t(-1),
      started_ts: t(-1),
    },
  ];

  const phases = [
    {
      id: "peer-demo-refactor-phase",
      mission_id: "peer-demo-refactor",
      description: "Rename a helper module and update its call sites.",
      status: "Complete",
      created_ts: t(-3),
      started_ts: t(-3),
      completed_ts: t(-2),
      task_ids: [],
    },
    {
      id: "peer-demo-docs-phase",
      mission_id: "peer-demo-docs",
      description: "Write the peer machine's onboarding notes.",
      status: "Running",
      created_ts: t(-1),
      started_ts: t(-1),
      task_ids: [],
    },
  ];

  const flow = [];
  const emitPair = (missionId, handle, sessionId, startOffset, endOffset) => {
    flow.push({
      ts: iso(startOffset),
      level: "info",
      category: "work",
      tier: "local",
      stage: "dispatch",
      action: "dispatch start",
      handle,
      session_id: sessionId,
      source: "scheduler",
      mission_id: missionId,
      machine_id: "flatsat-peer",
      machine_uid: PEER_UID,
      orchestrator: "flatsat",
    });
    flow.push({
      ts: iso(endOffset),
      level: "info",
      category: "work",
      tier: "local",
      stage: "dispatch",
      action: "dispatch complete",
      handle,
      session_id: sessionId,
      source: "scheduler",
      mission_id: missionId,
      machine_id: "flatsat-peer",
      machine_uid: PEER_UID,
      model: "qwen3.6-35b-a3b",
      orchestrator: "flatsat",
    });
  };

  emitPair("peer-demo-refactor", "coder", "peer-refactor-coder-1", -3, -2.8);
  emitPair("peer-demo-refactor", "code-reviewer", "peer-refactor-review-1", -2.7, -2.6);
  emitPair("peer-demo-docs", "scribe", "peer-docs-scribe-1", -1, -0.9);
  // One record with NO terminal pair yet — a genuinely "still running" row
  // for scenarios that care about live/pending state (peer-asleep, in
  // particular, benefits from a record that hasn't completed at seed time).
  flow.push({
    ts: iso(-0.05),
    level: "info",
    category: "work",
    tier: "local",
    stage: "dispatch",
    action: "dispatch start",
    handle: "scribe",
    session_id: "peer-docs-scribe-2",
    source: "scheduler",
    mission_id: "peer-demo-docs",
    machine_id: "flatsat-peer",
    machine_uid: PEER_UID,
    orchestrator: "flatsat",
  });

  return { missions, phases, flow, machineUid: PEER_UID };
}

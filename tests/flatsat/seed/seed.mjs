#!/usr/bin/env bun
// Pre-populates BOTH machines' DARKMUX_HOME (tests/flatsat/.state/{hub,peer})
// so the fleet looks REAL on `docker compose up` — real missions/phases,
// real-shaped flow history, both machines visible on the shared Redis
// stream. Idempotent: wipes + rewrites .state/ every run (the compose
// volumes are bind mounts under this dir, gitignored).
//
// Data sources (packet brief, "Seeded state"):
//   - hub  <- the FULL parity corpus (tests/parity/corpus/*.json), reused
//             read-only, timestamps shifted to real "today" (see
//             lib/timeshift.mjs) so liveness/freshness windows work.
//   - peer <- a small hand-authored fixture (seed/peer-fixture.mjs) with
//             its own distinct mission ids, so the fleet-visible scenario
//             has genuinely different content to prove is reachable from
//             the hub, not a copy of the hub's own board.
//
// Every byte this script writes from corpus material is scanned by the
// parity harness's OWN tripwire (lib/sanitize.mjs's scanForSentinels) at
// the end of this run — reusing the check, not re-implementing it. That is
// the packet's sanitizer-gate requirement: "The seeding script must run
// the parity TRIPWIRE over anything it writes from corpus material."
import { existsSync, mkdirSync, rmSync, readFileSync, writeFileSync, appendFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import {
  HUB_STATE_DIR,
  PEER_STATE_DIR,
  STATE_DIR,
  PARITY_CORPUS_DIR,
  REDIS_URL,
  REDIS_STREAM,
  REDIS_MAXLEN,
} from "../lib/paths.js";
import { deltaMsFromCapturedDate, shiftIso, shiftTsFields, utcDateOf } from "../lib/timeshift.mjs";
import { connect, xaddRecord, xlen, flushStream } from "../lib/redis.mjs";
import { buildPeerFixture } from "./peer-fixture.mjs";
// Reuse of the parity harness's OWN sanitizer scan (not a re-implementation)
// — see tests/parity/tripwire.mjs, which this mirrors the contract of.
import { scanForSentinels } from "../../parity/lib/sanitize.mjs";

const HUB_UID = "9F2C6A11-FLAT-SAT1-HUB0-000000000001";

function readCorpus(name) {
  return JSON.parse(readFileSync(path.join(PARITY_CORPUS_DIR, name), "utf8"));
}

function writeJson(filePath, value) {
  mkdirSync(path.dirname(filePath), { recursive: true });
  writeFileSync(filePath, JSON.stringify(value, null, 2) + "\n");
}

function writeMissionsAndPhases(stateDir, missions, phases) {
  const missionsRoot = path.join(stateDir, "missions");
  for (const mission of missions) {
    writeJson(path.join(missionsRoot, mission.id, "mission.json"), mission);
  }
  for (const phase of phases) {
    writeJson(path.join(missionsRoot, phase.mission_id, "phases", `${phase.id}.json`), phase);
  }
}

/** Groups records by their (already-shifted) UTC date and appends each
 * group to `<stateDir>/flows/<date>.jsonl`, one compact JSON object per
 * line — the exact on-disk shape LocalFileSink produces. */
function writeFlowDayFiles(stateDir, records) {
  const flowsDir = path.join(stateDir, "flows");
  mkdirSync(flowsDir, { recursive: true });
  const byDate = new Map();
  for (const r of records) {
    const date = utcDateOf(r.ts);
    if (!byDate.has(date)) byDate.set(date, []);
    byDate.get(date).push(r);
  }
  for (const [date, recs] of byDate) {
    recs.sort((a, b) => (a.ts < b.ts ? -1 : a.ts > b.ts ? 1 : 0));
    const body = recs.map((r) => JSON.stringify(r)).join("\n") + "\n";
    appendFileSync(path.join(flowsDir, `${date}.jsonl`), body);
  }
  return byDate;
}

function writeConfig(stateDir, machineId, fleetMode) {
  writeJson(path.join(stateDir, "config.json"), {
    schema_version: "1.2",
    machine_id: machineId,
    orchestrator: "flatsat",
    lms_bin: "lms",
    lmstudio_url: "http://localhost:1234",
    redis: { enabled: true, host: "redis", port: 6379, stream: REDIS_STREAM, maxlen: REDIS_MAXLEN },
    audit: { enabled: false, dir: "~/.darkmux/audit" },
    runtime: { inactivity_timeout_seconds: 600, strict_selection: false, feedback_injection: true, check_updates: false },
    remote: { max_tokens_per_execution: 500000 },
    fleet: { mode: fleetMode },
  });
}

function walkFiles(dir) {
  let out = [];
  for (const name of readdirSync(dir)) {
    const p = path.join(dir, name);
    if (statSync(p).isDirectory()) out = out.concat(walkFiles(p));
    else out.push(p);
  }
  return out;
}

async function main() {
  console.log("flatsat seed: resetting .state/ ...");
  rmSync(STATE_DIR, { recursive: true, force: true });
  mkdirSync(HUB_STATE_DIR, { recursive: true });
  mkdirSync(PEER_STATE_DIR, { recursive: true });

  const meta = readCorpus("meta.json");
  const deltaMs = deltaMsFromCapturedDate(meta.captured_date);
  const schemaVersion = meta.daemon_health.flow_schema_version;
  console.log(`  corpus captured_date=${meta.captured_date}, shifting by ${(deltaMs / 86_400_000).toFixed(1)} days to reach real today`);

  // ── hub: full corpus, timestamps shifted, machine identity overwritten ──
  const corpusMissions = readCorpus("missions.json").missions;
  const corpusPhases = readCorpus("phases.json").phases;
  const shiftedMissions = corpusMissions.map((m) => shiftTsFields(m, deltaMs));
  const shiftedPhases = corpusPhases.map((p) => shiftTsFields(p, deltaMs));
  writeMissionsAndPhases(HUB_STATE_DIR, shiftedMissions, shiftedPhases);

  // Real production day-files can carry a `{_type:"schema", version,
  // darkmux_version}` schema-header line (crates/darkmux-serve/src/lib.rs's
  // `scan_flow_days` explicitly excludes it from record counts — "Schema-
  // header lines don't count as records") and the corpus's own captured
  // `/flow/<date>` responses include one as their last element. It has no
  // `ts`/`machine_id` (would blow up `shiftIso`/the machine rewrite below),
  // so it's excluded from the per-record rewrite here — reproducing its
  // exact write-time semantics isn't load-bearing for any of this packet's
  // six scenarios, so this stays a straight filter rather than a faithful
  // re-synthesis.
  const isFlowRecord = (r) => r && typeof r === "object" && r._type !== "schema";
  const flowToday = readCorpus("flow-today.json").filter(isFlowRecord);
  const flowYesterday = readCorpus("flow-yesterday.json").filter(isFlowRecord);
  const hubFlow = [...flowToday, ...flowYesterday].map((r) => ({
    ...r,
    ts: shiftIso(r.ts, deltaMs),
    machine_id: "flatsat-hub",
    machine_uid: HUB_UID,
  }));
  const hubBuckets = writeFlowDayFiles(HUB_STATE_DIR, hubFlow);
  console.log(`  hub: ${shiftedMissions.length} missions, ${shiftedPhases.length} phases, ${hubFlow.length} flow records across ${hubBuckets.size} day-file(s)`);

  // ── peer: small hand-authored fixture, own machine identity ──
  const nowMs = Date.now();
  const peer = buildPeerFixture(nowMs);
  writeMissionsAndPhases(PEER_STATE_DIR, peer.missions, peer.phases);
  const peerBuckets = writeFlowDayFiles(PEER_STATE_DIR, peer.flow);
  console.log(`  peer: ${peer.missions.length} missions, ${peer.phases.length} phases, ${peer.flow.length} flow records across ${peerBuckets.size} day-file(s)`);

  writeConfig(HUB_STATE_DIR, "flatsat-hub", "hub");
  writeConfig(PEER_STATE_DIR, "flatsat-peer", "peer");

  // ── shared Redis stream: mirrors TeeSink — every machine's own local
  // write ALSO lands on the shared stream, which is how a peer's work
  // becomes visible from the hub's aggregated views (/runs, /flow-missions,
  // the default fleet dashboard). Written in chronological order across
  // BOTH machines, matching how a real multi-machine XADD interleaving
  // would look.
  console.log(`  redis: XADD-ing ${hubFlow.length + peer.flow.length} records to ${REDIS_STREAM} at ${REDIS_URL} ...`);
  const client = connect();
  try {
    // Flush first — idempotent regardless of whether `redis` is a fresh
    // container or one `up.sh` found already running/healthy (`docker
    // compose up -d` no-ops on an already-healthy service, so a stale
    // stream from a prior interactive `inject.sh flood-stream` or a
    // scenario run that didn't go through a full `down` would otherwise
    // bleed into this seed — found live while validating this packet).
    await flushStream(client);
    const combined = [...hubFlow, ...peer.flow].sort((a, b) => (a.ts < b.ts ? -1 : a.ts > b.ts ? 1 : 0));
    for (const record of combined) {
      await xaddRecord(client, schemaVersion, record);
    }
    console.log(`  redis: stream length now ${await xlen(client)}`);
  } finally {
    client.close();
  }

  // ── sanitizer gate: independent re-scan of everything just written,
  // reusing tests/parity's own tripwire vocabulary. Structural, not
  // trust-the-corpus-was-clean — the corpus already passed this same
  // check when Packet 0a committed it, but a bug in THIS script's
  // rewriting could reintroduce something (e.g. failing to rewrite a
  // field), so the gate re-runs here rather than being assumed inherited.
  console.log("flatsat seed: tripwire re-scan of everything written to .state/ ...");
  const written = walkFiles(STATE_DIR).filter((f) => f.endsWith(".json") || f.endsWith(".jsonl"));
  let anyHit = false;
  for (const f of written) {
    const hits = scanForSentinels(readFileSync(f, "utf8"));
    if (hits.length) {
      anyHit = true;
      console.error(`TRIPWIRE HIT: ${path.relative(STATE_DIR, f)}`);
      for (const h of hits) console.error(`  "${h.sentinel}" x${h.count}`);
    }
  }
  if (anyHit) {
    console.error("flatsat seed: tripwire FAILED — refusing to leave seeded state on disk.");
    rmSync(STATE_DIR, { recursive: true, force: true });
    process.exit(2);
  }
  console.log(`flatsat seed: tripwire OK — scanned ${written.length} seeded files, zero hits.`);
  console.log("flatsat seed: done.");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});

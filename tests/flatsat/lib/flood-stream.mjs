#!/usr/bin/env bun
// Pads the shared `darkmux:flow` stream with N synthetic minimal records
// (default 10500) so a MAXLEN ~ 10000 trim actually fires — the
// stream-at-cap scenario's setup step (inject.sh flood-stream [n]). Kept
// OUT of the main seed (seed.mjs) deliberately: every other scenario reads
// against a realistic few-thousand-record dataset, and forcing 10k+ noise
// records into the shared stream for every `up` would slow every
// scenario's XREVRANGE reads for no benefit to five of the six.
import { connect, xaddRecord, xlen } from "./redis.mjs";

const count = Number(process.argv[2] || 10500);
const machineId = "flatsat-flood";
const uid = "9F2C6A11-FLAT-SAT9-FLOOD-00000000009";

async function main() {
  const client = connect();
  try {
    const startedAt = Date.now();
    for (let i = 0; i < count; i++) {
      const ts = new Date(startedAt - (count - i) * 1000).toISOString().replace(/\.\d{3}Z$/, "Z");
      await xaddRecord(client, "1.18.0", {
        ts,
        level: "info",
        category: "work",
        tier: "local",
        stage: "dispatch",
        action: i % 2 === 0 ? "dispatch start" : "dispatch complete",
        handle: "flood",
        session_id: `flood-${Math.floor(i / 2)}`,
        source: "scheduler",
        machine_id: machineId,
        machine_uid: uid,
        orchestrator: "flatsat",
      });
    }
    console.log(`flood-stream: XADD-ed ${count} synthetic records; stream length now ${await xlen(client)}`);
  } finally {
    client.close();
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});

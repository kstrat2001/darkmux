// Thin wrapper over Bun's NATIVE Redis client (`Bun.RedisClient` — no npm
// dependency; verified against a throwaway redis:7-alpine container before
// committing to this approach, see the packet report). Mirrors the exact
// on-wire shape `RedisSink::try_write` uses in
// crates/darkmux-flow/src/lib.rs, so seeded records are indistinguishable
// on read from ones a real darkmux process wrote:
//
//   XADD <stream> MAXLEN ~ <maxlen> * schema <FLOW_SCHEMA_VERSION> record <json>
//
// Two-field encoding: `schema` carries the flow schema version, `record`
// carries the JSON-serialized FlowRecord. The version is read from the
// parity corpus's own `meta.json` (`daemon_health.flow_schema_version`)
// rather than hardcoded here, so it can't silently drift from what the
// corpus was actually recorded against.
import { REDIS_URL, REDIS_STREAM, REDIS_MAXLEN } from "./paths.js";

export function connect(url = REDIS_URL) {
  return new Bun.RedisClient(url);
}

export async function xaddRecord(client, schemaVersion, record, { stream = REDIS_STREAM, maxlen = REDIS_MAXLEN } = {}) {
  const payload = JSON.stringify(record);
  return client.send("XADD", [
    stream,
    "MAXLEN",
    "~",
    String(maxlen),
    "*",
    "schema",
    schemaVersion,
    "record",
    payload,
  ]);
}

export async function xlen(client, stream = REDIS_STREAM) {
  return Number(await client.send("XLEN", [stream]));
}

export async function flushStream(client, stream = REDIS_STREAM) {
  return client.send("DEL", [stream]);
}

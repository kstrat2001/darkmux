#!/usr/bin/env bun
// Corpus recorder — Packet 0a (the parity harness). Records the operator's
// LIVE darkmux daemon into sanitized JSON fixtures under corpus/, which
// extract.spec.ts then replays through Playwright's route interception to
// take the legacy viewer.html down its real daemon-fetch code path with zero
// live network access. See tests/parity/README.md for the full picture.
//
// Endpoints recorded: the plan's named set (/runs /missions /phases
// /flow-days /flow-missions /flow/<today> /fleet/machines/live
// /fleet/sessions/live /machine/resources /machine/specs
// /panel/mission-status) PLUS three endpoints the plan's list didn't name
// but the viewer's own code demands to render a complete lens:
// /flow/<yesterday> (loadLiveWindow(), the live-mode boot path this harness
// exercises, fetches [prevDateUTC(today), today] — recording only <today>
// would starve the fleet lens's rolling 24h window of half its data);
// /lab/runs (the runs lens fetches it ALONGSIDE /runs on every entry —
// window.goRuns does `Promise.all([loadRuns(), loadLabRuns()])`, so a runs
// golden without it is an incomplete render, not a faithful one); and
// /flow-session/<id> for one concrete, non-sensitive session id (`task-list`)
// so the #session=<id> drill-in lens has something real to replay. All three
// additions are logged in the transcript below exactly like the named set.
//
// SANITIZATION IS MANDATORY AND UNCONDITIONAL: every response body is run
// through lib/sanitize.mjs's field-policy sanitizer BEFORE it touches disk,
// and the tripwire re-scans everything written before this script reports
// success. There is no flag to skip either step.
//
// ATOMIC WRITE (QA finding, post-0a review): everything is staged into a
// sibling temp directory first; corpus/ itself is only touched — via a
// delete-then-rename swap — AFTER every endpoint has fetched, sanitized, and
// passed the residual-canary check. A previous version deleted corpus/ up
// front, so a daemon hiccup or a sanitize failure partway through a re-record
// left the committed corpus destroyed instead of merely stale. Now a failed
// run leaves the existing corpus/ untouched and reports what failed.

import { mkdirSync, writeFileSync, rmSync, renameSync } from "node:fs";
import { sanitizeText, scanForSentinels } from "./lib/sanitize.mjs";
import { CORPUS_DIR } from "./lib/paths.js";

const DAEMON_URL = process.env.DARKMUX_DAEMON_URL || "http://127.0.0.1:8765";
const TMP_DIR = `${CORPUS_DIR}.tmp-${process.pid}`;

// Fixed, deterministic offset applied to the recorded capture time when the
// clock is frozen for golden extraction (see extract.spec.ts). Stored here
// (not invented at extraction time) so record + extract agree on one number
// without either having to import the other's internals.
const FREEZE_OFFSET_MS = 5000;

function todayUTC() {
  return new Date().toISOString().slice(0, 10);
}
function prevDateUTC(d) {
  const dt = new Date(d + "T00:00:00Z");
  dt.setUTCDate(dt.getUTCDate() - 1);
  return dt.toISOString().slice(0, 10);
}

async function fetchJson(pathAndQuery, { headers = {} } = {}) {
  const url = DAEMON_URL + pathAndQuery;
  const res = await fetch(url, { headers: { accept: "application/json", ...headers } });
  const text = await res.text();
  return { ok: res.ok, status: res.status, text, url };
}

async function recordEndpoint(entry, targetDir, { name, urlPath, file, headers, extra }) {
  const res = await fetchJson(urlPath, { headers });
  const rec = { name, path: urlPath, file, http_status: res.status, ok: res.ok, extra: extra || null };
  if (!res.ok) {
    rec.error = `HTTP ${res.status}`;
    entry.push(rec);
    console.error(`  FAIL  ${urlPath}  -> HTTP ${res.status}`);
    return rec;
  }
  let sanitized;
  try {
    sanitized = sanitizeText(res.text);
  } catch (e) {
    rec.error = `sanitize failed: ${e.message}`;
    entry.push(rec);
    console.error(`  FAIL  ${urlPath}  -> response was not valid JSON (${e.message}); refusing to write a fixture (no fabrication)`);
    return rec;
  }
  const residual = scanForSentinels(sanitized.text);
  if (residual.length) {
    // This should be structurally impossible given the field policy's
    // coverage, but the tripwire doctrine is "verify, don't assume" — if it
    // ever DID slip through, fail loud right here instead of writing the
    // file into the staging dir at all.
    rec.error = `TRIPWIRE: residual canary hits after sanitization: ${JSON.stringify(residual)}`;
    entry.push(rec);
    console.error(`  FAIL  ${urlPath}  -> ${rec.error}`);
    return rec;
  }
  const filePath = `${targetDir}/${file}`;
  writeFileSync(filePath, sanitized.text, "utf8");
  rec.bytes = Buffer.byteLength(sanitized.text, "utf8");
  rec.sanitized = sanitized.matched;
  entry.push(rec);
  const m = sanitized.matched;
  const unknownNote = m.unknownFields.length ? ` UNKNOWN-FIELDS=${JSON.stringify(m.unknownFields)}` : "";
  console.log(
    `  OK    ${urlPath.padEnd(38)} -> ${file.padEnd(28)} ${String(rec.bytes).padStart(8)}B  ` +
      `entity(id=${m.identifiers},ticket=${m.tickets},sha=${m.shas},ip=${m.ips}) prose=${m.prose} path=${m.paths} uuid=${m.uuids}${unknownNote}`
  );
  return rec;
}

async function main() {
  console.log(`Recording corpus from ${DAEMON_URL} ...`);
  const health = await fetchJson("/health");
  if (!health.ok) {
    console.error(`Daemon unreachable at ${DAEMON_URL} (health check: HTTP ${health.status}). Aborting — refusing to fabricate fixtures.`);
    process.exit(1);
  }
  let healthBody = null;
  try {
    healthBody = JSON.parse(health.text);
  } catch (_) {
    /* non-fatal; recorded as null */
  }
  console.log(`  daemon reachable: ${JSON.stringify(healthBody)}`);

  const recordedAtMs = Date.now();
  const capturedDate = todayUTC();
  const capturedPrevDate = prevDateUTC(capturedDate);

  // Stage into a sibling temp dir — corpus/ itself is not touched until the
  // final atomic swap at the bottom of main(). Clear any stale staging dir
  // left behind by a previous crashed run first.
  rmSync(TMP_DIR, { recursive: true, force: true });
  mkdirSync(TMP_DIR, { recursive: true });

  const endpoints = [];
  const rec = (spec) => recordEndpoint(endpoints, TMP_DIR, spec);

  await rec({ name: "missions", urlPath: "/missions", file: "missions.json" });
  await rec({ name: "phases", urlPath: "/phases", file: "phases.json" });
  await rec({ name: "runs", urlPath: "/runs", file: "runs.json" });
  await rec({ name: "flow-days", urlPath: "/flow-days", file: "flow-days.json" });
  await rec({ name: "flow-missions", urlPath: "/flow-missions", file: "flow-missions.json" });
  await rec({ name: "flow-today", urlPath: `/flow/${capturedDate}`, file: "flow-today.json", extra: { date: capturedDate } });
  await rec({ name: "flow-yesterday", urlPath: `/flow/${capturedPrevDate}`, file: "flow-yesterday.json", extra: { date: capturedPrevDate } });
  await rec({ name: "fleet-machines-live", urlPath: "/fleet/machines/live", file: "fleet-machines-live.json" });
  await rec({ name: "fleet-sessions-live", urlPath: "/fleet/sessions/live", file: "fleet-sessions-live.json" });
  await rec({ name: "machine-resources", urlPath: "/machine/resources", file: "machine-resources.json" });
  await rec({ name: "machine-specs", urlPath: "/machine/specs", file: "machine-specs.json" });
  await rec({
    name: "panel-mission-status",
    urlPath: "/panel/mission-status",
    file: "panel-mission-status.json",
    headers: { "x-darkmux-panel": "1" },
  });
  // The remaining seven panels in the daemon's allowlist (Packet 6 growth —
  // see crates/darkmux-serve/src/panel.rs::PANEL_IDS for the source of
  // truth). `doctor` is manual-run-only server-side (auto_refresh:false,
  // rate-floored at 30s between runs, ~2s to gather) — recording it here is
  // still safe: this script runs at most once per `bun run record`
  // invocation, an operator-initiated action, never a poll.
  const PANEL_IDS_EXCEPT_MISSION_STATUS = [
    "mission-status-all",
    "machine-status",
    "flow-status",
    "role-list",
    "config-list",
    "lab-fixture-list",
    "doctor",
  ];
  for (const id of PANEL_IDS_EXCEPT_MISSION_STATUS) {
    await rec({
      name: `panel-${id}`,
      urlPath: `/panel/${id}`,
      file: `panel-${id}.json`,
      headers: { "x-darkmux-panel": "1" },
    });
  }
  // Extensions beyond the plan's literal list — see module doc.
  await rec({ name: "lab-runs", urlPath: "/lab/runs", file: "lab-runs.json", extra: { reason: "runs lens fetches this alongside /runs on every entry" } });
  await rec({
    name: "flow-session-task-list",
    urlPath: "/flow-session/task-list",
    file: "flow-session-task-list.json",
    extra: { reason: "#session=<id> deep-link golden target; task-list chosen because it carries no client identifiers, sidestepping URL-encoding of a sanitized compound id" },
  });

  const failed = endpoints.filter((e) => e.error);
  const meta = {
    recorded_at_ms: recordedAtMs,
    recorded_at_iso: new Date(recordedAtMs).toISOString(),
    daemon_url: DAEMON_URL,
    daemon_health: healthBody,
    captured_date: capturedDate,
    captured_prev_date: capturedPrevDate,
    freeze_offset_ms: FREEZE_OFFSET_MS,
    frozen_clock_ms: recordedAtMs + FREEZE_OFFSET_MS,
    endpoints,
  };
  writeFileSync(`${TMP_DIR}/meta.json`, JSON.stringify(meta, null, 2) + "\n", "utf8");

  console.log("");
  console.log(`Recorded ${endpoints.length - failed.length}/${endpoints.length} endpoints.`);

  if (failed.length) {
    console.error(`FAILED endpoints: ${failed.map((f) => f.name).join(", ")}`);
    console.error(`Staged output left at ${TMP_DIR} for inspection; corpus/ is UNCHANGED (atomic swap only happens on full success).`);
    process.exit(1);
  }

  // Atomic swap: only now does corpus/ get touched, and only because every
  // endpoint above succeeded.
  rmSync(CORPUS_DIR, { recursive: true, force: true });
  renameSync(TMP_DIR, CORPUS_DIR);

  console.log(`meta.json: captured_date=${capturedDate} captured_prev_date=${capturedPrevDate} frozen_clock_ms=${meta.frozen_clock_ms}`);
  console.log(`corpus/ swapped into place at ${CORPUS_DIR}`);
}

main().catch((e) => {
  console.error("record.mjs crashed:", e);
  // Best-effort: leave the staging dir for post-mortem rather than deleting
  // it on an unexpected throw — corpus/ itself was never touched.
  process.exit(1);
});

#!/usr/bin/env bun
// Corpus recorder — Packet 0a (the parity harness). Records the operator's
// LIVE darkmux daemon into sanitized JSON fixtures under corpus/. The
// `next-parity*.spec.ts` suites replay these through Playwright's route
// interception against the React port, with zero live network access — the
// same replay shape the (now-retired, #1806) legacy extractor originally
// used to take `viewer.html` down its real daemon-fetch code path when the
// goldens under `goldens/` were first captured. See tests/parity/README.md
// for the full picture.
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
// (#1868 packet 1) TWO more, for the mission-graph parity fixture:
// /mission/<GRAPH_FIXTURE_MISSION_ID>/graph.json (the node/edge snapshot
// the graph lens's canvas + timeline renderers both read) and
// /flow-mission/<GRAPH_FIXTURE_MISSION_ID> (the mission-scoped event
// backfill the lens's events panel reads). Originally captured by
// `mission-graph-goldens.spec.ts` against the standalone mission-graph
// page, BEFORE the graph lens got folded into the React port; that capture
// suite and the standalone page are both retired (#1868 packet 3) — the
// same two endpoints are now read by `next-parity-graph.spec.ts`, which
// grades the ported lens against the goldens the retired suite captured.
// The fixture id lives once in `lib/graph-fixture.js`, imported here and
// by `lib/mock-routes.js` + that spec, so it can't drift between the
// three.
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
//
// A FULL re-record is NOT harmless (QA finding, #1868 packet 1 review): the
// next-parity* goldens are pinned to the exact DATA in the committed
// corpus/, not just its shape, so running plain `bun run record` changes
// every fixture's content (whatever the operator's live daemon happens to
// hold right now) and fails 3+5+5+3 next-parity assertions against the
// frozen goldens until someone deliberately rebaselines them. See
// README.md's "Re-recording the corpus" section for the real rule.
//
// TARGETED MODE (`--only graph`, #1868 packet 1): records ONLY the two
// mission-graph parity fixture endpoints and writes them into the EXISTING
// corpus/ in place, with no whole-directory delete-then-rename swap, and no
// change to meta.json's `frozen_clock_ms`/`recorded_at_ms`/`captured_date`/
// anything else, only that pair's own transcript entries. This is how a
// NEW fixture gets added (or the mission-graph fixture specifically
// refreshed) without invalidating the other 21 fixtures' goldens. See
// `recordGraphOnly()` below.

import { mkdirSync, writeFileSync, rmSync, renameSync, readFileSync, existsSync } from "node:fs";
import { sanitizeText, scanForSentinels } from "./lib/sanitize.mjs";
import { CORPUS_DIR, META_JSON } from "./lib/paths.js";
import { GRAPH_FIXTURE_MISSION_ID } from "./lib/graph-fixture.js";

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

/** `--only <mode>` / `--only=<mode>`. No other flags exist today. */
function parseArgs(argv) {
  let only = null;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--only") {
      only = argv[i + 1];
      i++;
    } else if (a.startsWith("--only=")) {
      only = a.slice("--only=".length);
    }
  }
  return { only };
}

/** The mission-graph parity fixture's own two endpoints (#1868 packet 1),
 * used both by a FULL record (below, folded into the named endpoint list)
 * and by `recordGraphOnly()`'s targeted `--only graph` mode. One definition
 * so the two paths can't drift. */
function graphFixtureEndpointSpecs() {
  return [
    {
      name: "mission-graph-sanity",
      urlPath: `/mission/${encodeURIComponent(GRAPH_FIXTURE_MISSION_ID)}/graph.json`,
      file: "mission-graph-sanity.json",
      extra: { reason: "next-parity-graph.spec.ts canvas/timeline node+edge snapshot (recorded by the now-retired mission-graph-goldens.spec.ts, #1868 packet 1)", mission_id: GRAPH_FIXTURE_MISSION_ID },
    },
    {
      name: "flow-mission-sanity",
      urlPath: `/flow-mission/${encodeURIComponent(GRAPH_FIXTURE_MISSION_ID)}`,
      file: "flow-mission-sanity.json",
      extra: { reason: "next-parity-graph.spec.ts events panel backfill (recorded by the now-retired mission-graph-goldens.spec.ts, #1868 packet 1)", mission_id: GRAPH_FIXTURE_MISSION_ID },
    },
  ];
}

async function fetchJson(pathAndQuery, { headers = {} } = {}) {
  const url = DAEMON_URL + pathAndQuery;
  const res = await fetch(url, { headers: { accept: "application/json", ...headers } });
  const text = await res.text();
  return { ok: res.ok, status: res.status, text, url };
}

/** Fetch, sanitize, and residual-canary-check one endpoint. Does NOT write
 * anything to disk; the caller decides where and how (a plain write into a
 * staging dir for a full record, or a per-file temp-then-rename for the
 * targeted `--only graph` mode). Returns `{ rec, text }`, where `text` is
 * `null` on any failure (HTTP, non-JSON, or a residual sentinel hit) and
 * `rec.error` names why. */
async function fetchAndSanitize({ name, urlPath, file, headers, extra }) {
  const res = await fetchJson(urlPath, { headers });
  const rec = { name, path: urlPath, file, http_status: res.status, ok: res.ok, extra: extra || null };
  if (!res.ok) {
    rec.error = `HTTP ${res.status}`;
    console.error(`  FAIL  ${urlPath}  -> HTTP ${res.status}`);
    return { rec, text: null };
  }
  let sanitized;
  try {
    sanitized = sanitizeText(res.text);
  } catch (e) {
    rec.error = `sanitize failed: ${e.message}`;
    console.error(`  FAIL  ${urlPath}  -> response was not valid JSON (${e.message}); refusing to write a fixture (no fabrication)`);
    return { rec, text: null };
  }
  const residual = scanForSentinels(sanitized.text);
  if (residual.length) {
    // This should be structurally impossible given the field policy's
    // coverage, but the tripwire doctrine is "verify, don't assume": if it
    // ever DID slip through, fail loud right here instead of writing the
    // file into the staging dir at all.
    rec.error = `TRIPWIRE: residual canary hits after sanitization: ${JSON.stringify(residual)}`;
    console.error(`  FAIL  ${urlPath}  -> ${rec.error}`);
    return { rec, text: null };
  }
  rec.bytes = Buffer.byteLength(sanitized.text, "utf8");
  rec.sanitized = sanitized.matched;
  const m = sanitized.matched;
  const unknownNote = m.unknownFields.length ? ` UNKNOWN-FIELDS=${JSON.stringify(m.unknownFields)}` : "";
  console.log(
    `  OK    ${urlPath.padEnd(38)} -> ${file.padEnd(28)} ${String(rec.bytes).padStart(8)}B  ` +
      `entity(id=${m.identifiers},ticket=${m.tickets},sha=${m.shas},ip=${m.ips}) prose=${m.prose} path=${m.paths} uuid=${m.uuids}${unknownNote}`
  );
  return { rec, text: sanitized.text };
}

/** Full-record wrapper: fetch/sanitize, then write straight into `targetDir`
 * (the staging temp dir `main()` later swaps into place). Preserves the
 * original `recordEndpoint` behavior byte-for-byte. */
async function recordEndpoint(entry, targetDir, spec) {
  const { rec, text } = await fetchAndSanitize(spec);
  if (text !== null) {
    writeFileSync(`${targetDir}/${spec.file}`, text, "utf8");
  }
  entry.push(rec);
  return rec;
}

/**
 * `--only graph` (#1868 packet 1): records ONLY `graphFixtureEndpointSpecs()`
 * and writes them into the EXISTING `corpus/` in place. No whole-directory
 * swap (there is nothing to swap: 21 other fixtures are untouched), and
 * `meta.json`'s `recorded_at_ms`/`recorded_at_iso`/`daemon_health`/
 * `captured_date`/`captured_prev_date`/`freeze_offset_ms`/`frozen_clock_ms`
 * are NEVER rewritten by this mode; only the two targeted entries in
 * `meta.endpoints` are added or replaced. Sanitization and the residual
 * tripwire check are exactly as unconditional as the full path (both run
 * through the same `fetchAndSanitize`).
 *
 * All-or-nothing, same invariant as a full record: if either endpoint
 * fails, NOTHING is written (no partial fixture, no partial meta.json
 * update) and the process exits non-zero. Each surviving write is its own
 * temp-file-then-rename (same filesystem, so the rename is atomic), so a
 * crash between the two files' writes still leaves each individual file
 * either fully old or fully new, never truncated.
 */
async function recordGraphOnly() {
  console.log(`Recording GRAPH-ONLY fixtures (--only graph) from ${DAEMON_URL} ...`);
  const health = await fetchJson("/health");
  if (!health.ok) {
    console.error(`Daemon unreachable at ${DAEMON_URL} (health check: HTTP ${health.status}). Aborting, refusing to fabricate fixtures.`);
    process.exit(1);
  }
  if (!existsSync(CORPUS_DIR) || !existsSync(META_JSON)) {
    console.error(
      `No existing corpus/meta.json found at ${CORPUS_DIR}. --only graph updates an EXISTING corpus in place; ` +
        `it does not create one from scratch. Run a full \`bun run record\` first (and then deliberately ` +
        `rebaseline the next-parity goldens, per README.md), or restore the committed corpus/.`
    );
    process.exit(1);
  }

  const results = [];
  for (const spec of graphFixtureEndpointSpecs()) {
    results.push(await fetchAndSanitize(spec));
  }

  const failed = results.filter((r) => r.rec.error);
  if (failed.length) {
    console.error(`FAILED endpoints: ${failed.map((r) => r.rec.name).join(", ")}`);
    console.error(`Nothing written: corpus/ and meta.json are UNCHANGED (all-or-nothing, same as a full record).`);
    process.exit(1);
  }

  // Per-file atomic write: temp file, then rename over the final path (same
  // filesystem, so the rename is atomic); no whole-directory swap needed
  // since only these two files are in scope.
  for (const { rec, text } of results) {
    const finalPath = `${CORPUS_DIR}/${rec.file}`;
    const tmpPath = `${finalPath}.tmp-${process.pid}`;
    writeFileSync(tmpPath, text, "utf8");
    renameSync(tmpPath, finalPath);
  }

  // meta.json: update ONLY these endpoints' transcript entries, in place.
  // Every other field stays exactly what the last FULL record wrote, most
  // importantly `frozen_clock_ms`, which every next-parity golden's
  // relative-time text is captured against; changing it here would
  // invalidate every OTHER golden even though their underlying fixture
  // files never moved.
  const meta = JSON.parse(readFileSync(META_JSON, "utf8"));
  const byName = new Map(meta.endpoints.map((e, i) => [e.name, i]));
  for (const { rec } of results) {
    const idx = byName.get(rec.name);
    if (idx === undefined) {
      meta.endpoints.push(rec);
    } else {
      meta.endpoints[idx] = rec;
    }
  }
  const metaTmpPath = `${META_JSON}.tmp-${process.pid}`;
  writeFileSync(metaTmpPath, JSON.stringify(meta, null, 2) + "\n", "utf8");
  renameSync(metaTmpPath, META_JSON);

  console.log("");
  console.log(`Recorded ${results.length}/${results.length} graph endpoint(s) in place; every other corpus/ fixture and meta.json field is unchanged.`);
}

async function main() {
  const { only } = parseArgs(process.argv.slice(2));
  if (only === "graph") {
    await recordGraphOnly();
    return;
  }
  if (only) {
    console.error(`Unknown --only mode "${only}" (recognized: "graph"). Aborting.`);
    process.exit(1);
  }

  console.log(`Recording corpus from ${DAEMON_URL} ...`);
  console.warn(
    "WARNING: this re-records the WHOLE corpus. The next-parity* goldens are pinned to the exact data " +
      "in the CURRENT committed corpus/, so this changes every fixture's content and will fail next-parity " +
      "assertions until someone deliberately rebaselines the goldens (see README.md's \"Re-recording the " +
      "corpus\" section). To add or refresh only the mission-graph fixture without touching anything else, " +
      "use `bun run record -- --only graph` instead."
  );
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
  // (#1868 packet 1) The mission-graph parity fixture's own two endpoints;
  // see the module doc above and `graphFixtureEndpointSpecs()` (the SAME
  // spec `--only graph` uses, so the two paths can't drift apart).
  for (const spec of graphFixtureEndpointSpecs()) {
    await rec(spec);
  }

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

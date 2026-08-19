// Route-interception tables. Originally shared by the legacy extractor's
// extract.spec.ts (serve the recorded, sanitized corpus) and
// redprove.spec.ts (serve nothing — the harness must fail every golden
// comparison against this) — both retired in #1806. Now shared by the
// `next-parity*.spec.ts` suites the same way: `installCorpusRoutes` serves
// the recorded corpus against the React port, `installBlankRoutes` proves
// the port's own red-prove assertions (a blank/unreachable daemon must not
// match a real golden). One handler function per mode, both driven by the
// SAME endpoint inventory so a new endpoint added to one can't silently be
// forgotten in the other.

const { readFileSync } = require("fs");
const path = require("path");
const { CORPUS_DIR, META_JSON } = require("./paths.js");
const { GRAPH_FIXTURE_MISSION_ID } = require("./graph-fixture.js");

function loadMeta() {
  return JSON.parse(readFileSync(META_JSON, "utf8"));
}

function fixture(file) {
  return readFileSync(path.join(CORPUS_DIR, file), "utf8");
}

/**
 * Install corpus-backed routes on `page`. Every fetch/EventSource the viewer
 * issues resolves from the sanitized corpus — no live network access.
 */
/**
 * (#1868 packet 1) `installCorpusRoutes` returns a per-fixture fulfillment
 * counter (`{ "<file>.json": n }`). A suite whose `baseURL` is a LIVE
 * daemon (this directory's `mission-graph-goldens` suite is the only one)
 * cannot tell "replayed from corpus/" from "fell through to the daemon" by
 * looking at the render: on the machine the corpus was recorded from, both
 * produce the same bytes. Asserting a fixture was actually SERVED is what
 * makes the interception load-bearing, so deleting a route branch fails the
 * suite instead of silently recording live daemon state into a golden.
 * Every other suite ignores the return value.
 */
function installCorpusRoutes(page, meta) {
  const served = {};
  page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;

    const json = (file) => {
      served[file] = (served[file] || 0) + 1;
      return route.fulfill({ status: 200, contentType: "application/json", body: fixture(file) });
    };
    const jsonInline = (body) => route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
    const notFound = (msg) => route.fulfill({ status: 404, contentType: "text/plain", body: msg || "not recorded in this corpus\n" });

    if (p === "/missions") return json("missions.json");
    if (p === "/phases") return json("phases.json");
    if (p === "/runs") return json("runs.json");
    if (p === "/flow-days") return json("flow-days.json");
    if (p === "/flow-missions") return json("flow-missions.json");
    if (p === "/lab/runs") return json("lab-runs.json");
    if (p === "/fleet/machines/live") return json("fleet-machines-live.json");
    if (p === "/fleet/sessions/live") return json("fleet-sessions-live.json");
    if (p === "/machine/resources") return json("machine-resources.json");
    if (p === "/machine/specs") return json("machine-specs.json");

    // (#1868 packet 1) The mission-graph parity fixture's node/edge snapshot.
    // Matched explicitly: unlike every other endpoint here, NOTHING
    // previously handled `/mission/:id/graph.json` at all, so it silently
    // fell through to `route.continue()` at the bottom of this handler and
    // hit the REAL daemon. Only the fixture id resolves to a recorded
    // fixture; any other id 404s, mirroring the `/flow-session/` pattern
    // below and matching the real daemon's own 404 for a mission with no
    // local graph on this box (crates/darkmux-serve/src/mission_graph.rs).
    const missionGraphJsonMatch = p.match(/^\/mission\/([^/]+)\/graph\.json$/);
    if (missionGraphJsonMatch) {
      if (decodeURIComponent(missionGraphJsonMatch[1]) === GRAPH_FIXTURE_MISSION_ID) return json("mission-graph-sanity.json");
      return notFound(`no recorded graph fixture for mission \`${missionGraphJsonMatch[1]}\`\n`);
    }

    const flowDateMatch = p.match(/^\/flow\/(\d{4}-\d{2}-\d{2})$/);
    if (flowDateMatch) {
      const date = flowDateMatch[1];
      if (date === meta.captured_date) return json("flow-today.json");
      if (date === meta.captured_prev_date) return json("flow-yesterday.json");
      return notFound(`no recorded fixture for /flow/${date}\n`);
    }
    // SSE tail: fulfill an empty, immediately-closed event-stream — the
    // brief's "record what it DOES show under an inert stream" case. The
    // viewer's render is already complete from the /flow/<date> fetches
    // above by the time this connects; the tail exists to APPEND live
    // records, and an inert stream means nothing ever gets appended, which
    // is exactly the legacy behavior on a stream with no activity.
    if (/^\/flow\/\d{4}-\d{2}-\d{2}\/stream$/.test(p)) {
      return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    }

    if (p === "/flow-session/task-list") return json("flow-session-task-list.json");
    if (p.startsWith("/flow-session/")) return notFound("no recorded fixture for this session id\n");
    // (Packet 4) The REAL `/flow-mission/:id` handler (catalog_records_response
    // in crates/darkmux-serve/src/lib.rs) never 404s for an unmatched id — it
    // always answers 200 with `{records:[],count:0,...}` (an actually
    // MALFORMED id shape gets 400 BAD_REQUEST instead — the
    // is_valid_catalog_id check at lib.rs:2975-2981 — never 404 either).
    // Packet 0a's
    // original 404-everything mock predates this packet (its own comment says
    // "out of scope for this packet" — mission-graph deep links, i.e. this
    // whole endpoint, were literally out of scope back then) and doesn't
    // match that contract. This corpus never recorded the per-mission record
    // bodies (only the flow-missions.json ROLLUP), so `records:[]`/`count:0`
    // here is the honest answer given what's actually on disk — not a claim
    // that the mission has zero real records, just that this fixture set
    // can't answer with more. Verified this doesn't change ANY legacy golden
    // (viewer.html's boot() folds both a non-ok response AND a
    // ok-with-empty-array response into the same `RAW=[]`, so `bun run check`
    // stays green byte-for-byte either way) — it only changes which BRANCH
    // `/next`'s MissionReplay takes (its real "empty" state instead of an
    // artificial "couldn't reach" error, which the real endpoint would never
    // actually produce for this case).
    //
    // (#1868 packet 1) The mission-graph fixture's OWN mission-scoped event
    // backfill is matched first, ahead of the generic empty-stub fallback
    // immediately below, so the fixture id replays the real recorded
    // records the events panel needs, and every OTHER id keeps getting the
    // generic `records:[]` stub this corpus has always answered with.
    if (p === `/flow-mission/${encodeURIComponent(GRAPH_FIXTURE_MISSION_ID)}`) return json("flow-mission-sanity.json");
    if (p.startsWith("/flow-mission/")) return jsonInline({ records: [], count: 0, truncated: false, generated_at_ms: meta.frozen_clock_ms });

    if (p === "/panel/mission-status") return json("panel-mission-status.json");
    if (p === "/panel/mission-status-all") return json("panel-mission-status-all.json");
    if (p === "/panel/machine-status") return json("panel-machine-status.json");
    if (p === "/panel/flow-status") return json("panel-flow-status.json");
    if (p === "/panel/role-list") return json("panel-role-list.json");
    if (p === "/panel/config-list") return json("panel-config-list.json");
    if (p === "/panel/lab-fixture-list") return json("panel-lab-fixture-list.json");
    if (p === "/panel/doctor") return json("panel-doctor.json");
    if (p.startsWith("/panel/")) return notFound('unknown panel "' + p.slice("/panel/".length) + '" — panels are a fixed allowlist, not arbitrary commands\n');

    if (p.startsWith("/lab/run/")) return notFound("lab-run drill-down not recorded in this corpus\n");
    if (p.startsWith("/worktree-summary/")) return notFound("not recorded in this corpus\n");

    // Static asset / favicon / manifest noise the browser requests on its
    // own — let the static file server answer (404s harmlessly, same as a
    // real daemon-less static context).
    return route.continue();
  });
  return served;
}

/**
 * Install "blank daemon" routes — every API path 404s or returns an empty
 * shell, exactly like an unreachable/freshly-initialized daemon with no
 * data. Used by redprove.spec.ts to prove the extraction can FAIL: if any
 * golden comparison passes against this, the harness is lying (operator
 * doctrine: "a probe that passes without executing is worse than no probe").
 */
function installBlankRoutes(page) {
  page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;
    const apiPaths = [
      "/missions",
      "/phases",
      "/runs",
      "/flow-days",
      "/flow-missions",
      "/lab/runs",
      "/fleet/machines/live",
      "/fleet/sessions/live",
      "/machine/resources",
      "/machine/specs",
    ];
    if (apiPaths.includes(p)) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness — nothing recorded\n" });
    // (#1868 packet 1) The mission-graph fixture's own endpoint, blanked the
    // same way every other id-scoped route below is: a 404, matching the
    // real daemon's own 404 for a mission with no local graph on this box.
    if (/^\/mission\/[^/]+\/graph\.json$/.test(p)) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness\n" });
    if (/^\/flow\/\d{4}-\d{2}-\d{2}$/.test(p)) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness\n" });
    if (/^\/flow\/\d{4}-\d{2}-\d{2}\/stream$/.test(p)) return route.fulfill({ status: 200, contentType: "text/event-stream", body: "" });
    if (p.startsWith("/flow-session/")) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness\n" });
    if (p.startsWith("/flow-mission/")) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness\n" });
    if (p.startsWith("/panel/")) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness\n" });
    if (p.startsWith("/lab/run/")) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness\n" });
    if (p.startsWith("/worktree-summary/")) return route.fulfill({ status: 404, contentType: "text/plain", body: "blank harness\n" });
    return route.continue();
  });
}

module.exports = { loadMeta, installCorpusRoutes, installBlankRoutes };

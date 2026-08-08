// Route-interception tables shared by extract.spec.ts (serve the recorded,
// sanitized corpus) and redprove.spec.ts (serve nothing — the harness must
// fail every golden comparison against this). One handler function per mode,
// both driven by the SAME endpoint inventory so a new endpoint added to one
// can't silently be forgotten in the other.

const { readFileSync } = require("fs");
const path = require("path");
const { CORPUS_DIR, META_JSON } = require("./paths.js");

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
function installCorpusRoutes(page, meta) {
  page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;

    const json = (file) => route.fulfill({ status: 200, contentType: "application/json", body: fixture(file) });
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
    // always answers 200 with `{records:[],count:0,...}` (a 404 is reserved
    // for an actually MALFORMED id shape, is_valid_catalog_id). Packet 0a's
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
    if (p.startsWith("/flow-mission/")) return jsonInline({ records: [], count: 0, truncated: false, generated_at_ms: meta.frozen_clock_ms });

    if (p === "/panel/mission-status") return json("panel-mission-status.json");
    if (p.startsWith("/panel/")) return notFound('unknown panel "' + p.slice("/panel/".length) + '" — panels are a fixed allowlist, not arbitrary commands\n');

    if (p.startsWith("/lab/run/")) return notFound("lab-run drill-down not recorded in this corpus\n");
    if (p.startsWith("/worktree-summary/")) return notFound("not recorded in this corpus\n");

    // Static asset / favicon / manifest noise the browser requests on its
    // own — let the static file server answer (404s harmlessly, same as a
    // real daemon-less static context).
    return route.continue();
  });
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

// Single source of truth for the mission id the mission-graph parity
// fixtures are recorded against and replayed with (#1868 packet 1:
// capturing PARITY GOLDENS from the standalone `mission-graph.html` before
// it gets folded into the React port). CommonJS, same reason `paths.js` is:
// required both by the bun-run ESM script (`record.mjs`, via Node's
// ESM-imports-CJS interop for a plain `module.exports = {...}` shape) and by
// `mission-graph-goldens.spec.ts`, which Playwright's TS loader transpiles
// to `require()` calls. `lib/mock-routes.js` imports it too, so the id lives
// in exactly one place rather than being repeated (and able to drift) across
// the recorder, the mock routes, and the spec.
//
// A synthetic sanity-check mission (3 phases, 5 tasks, 5 steps, 9 edges, 8
// flow-mission records, verified live against the daemon at authoring
// time). Chosen because it carries no client-identifying content, so the
// mandatory sanitizer pass (`lib/sanitize.mjs`) has nothing real to redact
// and the recorded corpus + goldens stay legible.
const GRAPH_FIXTURE_MISSION_ID = "sanity-review-shape-1784702742-76e0c1";

module.exports = { GRAPH_FIXTURE_MISSION_ID };

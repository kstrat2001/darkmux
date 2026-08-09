// Shared path constants for the flatsat harness. CommonJS (matching
// tests/parity/lib/paths.js's own convention exactly, and for the same
// reason): Playwright's TS loader transpiles .spec.ts files to CJS
// `require()` calls, and `require()` cannot load a real ESM `.mjs` file —
// so this has to be `.js`/CJS even though it's imported via `import {...}
// from "../lib/paths.js"` from both the ESM .mjs scripts (seed.mjs,
// redis.mjs) and the TS specs; both toolchains interop cleanly with a
// plain `module.exports = {...}` object.
const path = require("path");

const FLATSAT_DIR = __dirname.endsWith("lib") ? path.dirname(__dirname) : __dirname;
const REPO_ROOT = path.dirname(path.dirname(FLATSAT_DIR));
const STATE_DIR = path.join(FLATSAT_DIR, ".state");
const HUB_STATE_DIR = path.join(STATE_DIR, "hub");
const PEER_STATE_DIR = path.join(STATE_DIR, "peer");

// Read-only consumption of the parity harness's sanitized corpus (Packet
// 0a). Never written to.
const PARITY_DIR = path.join(REPO_ROOT, "tests", "parity");
const PARITY_CORPUS_DIR = path.join(PARITY_DIR, "corpus");

// Gitignored, repo-relative by default (`tests/flatsat/.gallery/`) — NOT an
// operator machine path. Fixed inherited defect (Packet 6 QA, 2026-08-09):
// this constant previously hardcoded the operator's absolute scratchpad
// path, INCLUDING a session-scoped UUID, straight into committed source —
// the same class of leak packets 2 and 3 already fixed in
// `tests/parity/next-parity-*.spec.ts` (see those files' own comments for
// the full "why this must never be an operator absolute path" rationale). A
// public repo committing one machine's home-directory layout (worse, a
// session UUID that will never resolve on any other machine) is the defect;
// `screenshot()`'s own `mkdirSync` already runs lazily inside the function
// body (never at module scope), so switching the default here to a
// repo-relative path carries no new "silently creates a directory on
// import" risk. Override with `DARKMUX_GALLERY_DIR` for a real run (e.g.
// the operator's own scratchpad) — same env var name `next-parity-runs.spec
// .ts`/`next-parity-console.spec.ts` already use, so one override works
// across every UI-port gallery.
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(FLATSAT_DIR, ".gallery", "0b-flatsat");

const HUB_URL = "http://127.0.0.1:18765";
const PEER_URL = "http://127.0.0.1:18766";
const REDIS_URL = "redis://127.0.0.1:16379";

// (finding, documented in README) darkmux serve's auth gate (#881) treats
// ANY peer without a loopback ConnectInfo as remote and requires a bearer
// token on every route but /health — and Docker's bridge-network port
// forwarding does NOT preserve loopback identity the way the real
// deployment's Tailscale Serve (which terminates AT the tailnet node and
// proxies to a loopback-bound daemon) does. So every flatsat container
// binds 0.0.0.0 (required for the host port-mapping to reach it at all)
// WITH this fixed, non-secret bench token configured (docker-compose.yml),
// and every request this harness makes — browser navigation via
// playwright.config.js's extraHTTPHeaders, plus every raw fetch() a spec
// makes directly — must carry it. A real fleet member reached via
// Tailscale Serve would NOT need this; it's a Docker-networking-topology
// artifact of the test environment, not a production behavior.
const SERVE_TOKEN = "flatsat-bench-token";

// Matches config.redis.maxlen's shipped default (darkmux's own config.json
// convention) — see docker-compose.yml + seed/seed.mjs's config.json write.
const REDIS_MAXLEN = 10000;
const REDIS_STREAM = "darkmux:flow";

module.exports = {
  FLATSAT_DIR,
  REPO_ROOT,
  STATE_DIR,
  HUB_STATE_DIR,
  PEER_STATE_DIR,
  PARITY_DIR,
  PARITY_CORPUS_DIR,
  GALLERY_DIR,
  HUB_URL,
  PEER_URL,
  REDIS_URL,
  REDIS_MAXLEN,
  REDIS_STREAM,
  SERVE_TOKEN,
};

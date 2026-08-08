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

const GALLERY_DIR =
  "/private/tmp/claude-501/-Users-kain-de-projects-darkmux-public/652b2a6d-51b7-4543-9ddf-8ef250dd2a4d/scratchpad/ui-port-gallery/0b-flatsat";

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

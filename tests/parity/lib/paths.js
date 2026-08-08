// CommonJS (not .mjs) — required both by the bun-run ESM scripts (via Node's
// ESM-imports-CJS interop, which both bun and Node support cleanly for a
// plain `module.exports = {...}` shape) and by extract.spec.ts/redprove.spec.ts,
// which Playwright's own TS loader transpiles to `require()` calls — a real
// .mjs file can't satisfy that `require()`, hence this file is CJS.
const path = require("path");

const PARITY_DIR = path.dirname(__dirname);
const CORPUS_DIR = path.join(PARITY_DIR, "corpus");
const GOLDENS_DIR = path.join(PARITY_DIR, "goldens");
const SERVED_DIR = path.join(PARITY_DIR, ".served");
const REPO_ROOT = path.dirname(path.dirname(PARITY_DIR));
const VIEWER_HTML = path.join(REPO_ROOT, "crates", "darkmux-serve", "assets", "viewer.html");
const META_JSON = path.join(CORPUS_DIR, "meta.json");

module.exports = { PARITY_DIR, CORPUS_DIR, GOLDENS_DIR, SERVED_DIR, REPO_ROOT, VIEWER_HTML, META_JSON };

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
// The committed BUILT artifact (`ui/`'s `bun run build` output) — see
// `next-parity.spec.ts`'s / `next-parity-runs.spec.ts`'s module docs.
// Distinct from `VIEWER_HTML`: this is the React port under test, not the
// legacy reference. Shared by both next-parity harnesses (Packet 2's and
// Packet 3's) — each config copies it into its OWN `.served-next`-style
// staging dir before serving, so the two never race on a shared file.
const NEXT_HTML = path.join(REPO_ROOT, "crates", "darkmux-serve", "assets", "next.html");
const SERVED_NEXT_DIR = path.join(PARITY_DIR, ".served-next");

module.exports = { PARITY_DIR, CORPUS_DIR, GOLDENS_DIR, SERVED_DIR, REPO_ROOT, VIEWER_HTML, META_JSON, NEXT_HTML, SERVED_NEXT_DIR };

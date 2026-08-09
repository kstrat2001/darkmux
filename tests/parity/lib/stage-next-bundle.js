// Stage the committed React bundle for a next-parity suite — and REFUSE to
// stage a stale one.
//
// ## Why this exists (#1737)
//
// The next-parity suites serve `crates/darkmux-serve/assets/next.html`, the
// COMMITTED build output. Each config used to check only that the file
// EXISTS. So when `bun run build` failed — a type error in a test file is
// enough, since the build typechecks first — the suites happily ran against
// the PREVIOUS bundle and reported green.
//
// That is not a slow-burn annoyance; it fires exactly when someone is
// verifying something. On 2026-08-09 it produced, in one session:
//
//   * a FALSE PASS ON A RED-PROVE — a QA agent deleted a code branch to
//     confirm a test had teeth, the build failed on the orphaned symbol, the
//     suite ran the stale bundle, and the mutation "didn't break anything".
//     The worst possible place for a false green.
//   * two pre-merge oracles where `build` exited non-zero and all six parity
//     suites reported green immediately after, caught only because the
//     orchestrator checked the exit code by hand.
//
// A stale-bundle green is indistinguishable from a real one, which turns the
// harness's strongest guarantee into its most confident lie. Hence: the
// staleness check is structural, and lives in ONE place that every config
// calls, rather than six copies of an existence check.
//
// ## What "stale" means here
//
// Newer than every input the bundle is built from. Deliberately mtime-based
// rather than hash-based: the build is deterministic (verified repeatedly —
// rebuilding an unchanged tree reproduces a byte-identical `next.html`), so
// mtime is sufficient and costs one `stat` per source file instead of a
// rebuild. A false ALARM here is cheap and self-correcting (`bun run build`
// and continue); a false all-clear is the thing being prevented.

const fs = require('fs');
const path = require('path');

const REPO_ROOT = path.join(__dirname, '..', '..', '..');
const NEXT_HTML = path.join(REPO_ROOT, 'crates', 'darkmux-serve', 'assets', 'next.html');
const UI_DIR = path.join(REPO_ROOT, 'ui');

/** Every file whose change should invalidate the bundle. */
function bundleInputs() {
  const out = [];
  const walk = (dir) => {
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return; // an absent optional input is not a staleness signal
    }
    for (const e of entries) {
      if (e.name === 'node_modules' || e.name === 'dist' || e.name.startsWith('.')) continue;
      const full = path.join(dir, e.name);
      if (e.isDirectory()) walk(full);
      else out.push(full);
    }
  };
  walk(path.join(UI_DIR, 'src'));
  for (const f of ['package.json', 'vite.config.ts', 'tsconfig.json', 'index.html']) {
    const full = path.join(UI_DIR, f);
    if (fs.existsSync(full)) out.push(full);
  }
  return out;
}

/**
 * Copy the committed bundle into `servedDir/index.html`, throwing loudly if
 * it is missing or older than any of its inputs.
 *
 * @param {string} servedDir  directory the suite's webServer serves from
 * @param {string} configName for the error message, so a failing run names
 *                            which harness refused rather than making the
 *                            reader guess.
 */
function stageNextBundle(servedDir, configName) {
  if (!fs.existsSync(NEXT_HTML)) {
    throw new Error(
      `${configName}: ${NEXT_HTML} does not exist — run \`cd ui && bun run build\` first ` +
        `(the artifact is committed, not built by this config).`,
    );
  }

  const bundleMs = fs.statSync(NEXT_HTML).mtimeMs;
  const newer = bundleInputs()
    .map((f) => ({ f, ms: fs.statSync(f).mtimeMs }))
    .filter((x) => x.ms > bundleMs)
    .sort((a, b) => b.ms - a.ms);

  if (newer.length) {
    const shown = newer.slice(0, 5).map((x) => `  - ${path.relative(REPO_ROOT, x.f)}`).join('\n');
    const more = newer.length > 5 ? `\n  …and ${newer.length - 5} more` : '';
    throw new Error(
      `${configName}: the committed bundle is STALE — ${newer.length} source file(s) are newer than ` +
        `${path.relative(REPO_ROOT, NEXT_HTML)}:\n${shown}${more}\n\n` +
        `Run \`cd ui && bun run build\` and CHECK ITS EXIT CODE. Running these suites against a stale ` +
        `bundle reports green for code that was never built (#1737) — including, notably, mutation ` +
        `proofs that then appear to prove the opposite of the truth.`,
    );
  }

  fs.mkdirSync(servedDir, { recursive: true });
  fs.copyFileSync(NEXT_HTML, path.join(servedDir, 'index.html'));
}

module.exports = { stageNextBundle, NEXT_HTML };

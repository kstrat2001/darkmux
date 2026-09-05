// Ad-hoc render harness (NOT a spec — Playwright's default `testMatch` only
// collects `*.spec.*`/`*.test.*`, so this is never collected by CI).
//
// Screenshots every lens at both viewports into a named directory, so a
// change with no assertable output — a stylesheet refactor, a spacing pass —
// can be pixel-diffed before against after. It earns its keep: the token
// promotion in #2310 swarm UI-2 left `--good: var(--good)` in `:root`, a
// cycle that silently voided every `var(--good)` across five lenses, and the
// diff caught it while three green test files did not.
//
// It shoots the PUBLIC DEMO, not the e2e harness, because the demo is the
// only committed dataset rich enough to render every lens with real content.
// You have to serve it yourself — nothing here starts a server:
//
//     # from the repo root, after any ui/ change:
//     cd ui && npm run build && cd .. && bash scripts/build-demo.sh
//     (cd docs/demo && python3 -m http.server 47955 &)
//     cd tests/e2e && node shots.mjs /tmp/shots/before
//     # ...make the change, rebuild both, then:
//     cd tests/e2e && node shots.mjs /tmp/shots/after
//
// Rebuild BOTH `next.html` and `docs/demo/index.html` between the two runs —
// `build-demo.sh` generates the demo page from the built viewer, so skipping
// it shoots the previous build and the diff reads as "no change".
//
// Override the page with SHOT_BASE (e.g. a live daemon at :8765). The two
// ids below are read out of the committed demo fixtures; if the demo dataset
// is regenerated they move, and the mission/dispatch routes will render an
// empty state instead of failing:
//
//     jq -r '.missions[].id' docs/demo/demo-missions.json | head -1
//     jq -rs 'map(.session_id) | map(select(.)) | first' docs/demo/demo-flow.jsonl
//
// Diffing is deliberately left to the caller — any per-pixel comparison will
// do; what matters is reading the top colour transitions, because a swap's
// OWN delta is expected and anything else is a finding.
import { chromium } from '@playwright/test';
import fs from 'node:fs';

const OUT = process.argv[2];
const BASE = process.env.SHOT_BASE || 'http://127.0.0.1:47955/index.html';
const MISSION = 'demo-review-nameof-recency';
const DISPATCH = 'darkmux-compactor-compact-trajectories-2619114180';

const ROUTES = [
  ['fleet', '#lens=fleet'],
  ['runs', '#lens=runs'],
  ['machine', '#lens=machine'],
  ['console', '#lens=console'],
  ['mission', `#mission=${MISSION}`],
  ['dispatch', `#dispatch=${DISPATCH}`],
];
const VIEWPORTS = [['desktop', 1456, 900], ['phone', 390, 844]];

fs.mkdirSync(OUT, { recursive: true });
const browser = await chromium.launch();
for (const [vname, width, height] of VIEWPORTS) {
  const ctx = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: 1,
    timezoneId: 'UTC',
    reducedMotion: 'reduce',
    hasTouch: vname === 'phone',
    isMobile: vname === 'phone',
  });
  const page = await ctx.newPage();
  for (const [rname, hash] of ROUTES) {
    await page.goto(BASE + hash);
    await page.waitForSelector('.app-shell');
    await page.waitForTimeout(2500);
    // Freeze anything that ticks so two runs are comparable.
    await page.addStyleTag({ content: '*,*::before,*::after{animation:none!important;transition:none!important}' });
    await page.screenshot({ path: `${OUT}/${rname}-${vname}.png`, fullPage: true });
  }
  await ctx.close();
}
await browser.close();
console.log('shots written to', OUT);

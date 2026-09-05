// Ad-hoc render harness (NOT a spec): screenshots every lens at both
// viewports into a named directory, so a stylesheet refactor can be
// pixel-diffed before/after. Run: node shots.mjs <outdir>
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

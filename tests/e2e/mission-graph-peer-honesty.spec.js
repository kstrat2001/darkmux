// (#1466) A peer's mission must say WHERE it ran, not 404 into a shrug.
//
// Three surfaces disagree about where mission data lives: `/flow/:date` and
// its SSE stream are Redis-MERGED (fleet-wide), the viewer builds its
// selectable mission list from that merged stream, and the graph builder
// reads LOCAL DISK only. So the picker offers a peer's mission and the graph
// 404s — the UI advertising a door it cannot open. The old message ("may
// have been an ephemeral or cleared run") was true-ish and useless: the run
// is fine, it's just not here.
//
// This is the operator-chosen interim (#1466 option c). Rendering the peer's
// graph locally is the real fix and needs peer routing + the shared serve
// token; it stays open on #1466.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'review-ran-elsewhere';
const PEER = 'm1-max-32gb-studio';
const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '..', '..', 'crates', 'darkmux-serve', 'assets', 'mission-graph.html'),
  'utf8'
);
const TODAY = new Date().toISOString().slice(0, 10);

async function open404(page, flowRows) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(`**/mission/${MISSION_ID}/graph`, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
  // The graph builder is local-disk-only — for a peer's mission it has nothing.
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) =>
    r.fulfill({ status: 404, contentType: 'text/plain', body: 'no mission with id found\n' })
  );
  // The flow stream is fleet-wide and DOES carry the peer's records.
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(flowRows) })
  );
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'text/event-stream', body: '' })
  );
  await page.goto(`/mission/${MISSION_ID}/graph`);
  await page.waitForSelector('.msg');
  return errors;
}

test('a peer mission names the machine it ran on', async ({ page }) => {
  const errors = await open404(page, [
    { ts: `${TODAY}T10:00:00Z`, action: 'mission start', mission_id: MISSION_ID, machine_id: PEER },
  ]);

  const msg = page.locator('.msg');
  await expect(msg, 'the operator needs to know WHICH machine to open').toContainText(PEER);
  await expect(
    msg,
    'and it must not still claim the run may be gone — it demonstrably is not'
  ).not.toContainText('ephemeral or cleared');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a genuinely unattributable 404 keeps the calm original wording', async ({ page }) => {
  // No flow record names this mission on any machine — a real ephemeral or
  // cleared run. Inventing a machine name here would be worse than the
  // vague message, so the fallback must survive.
  const errors = await open404(page, [
    { ts: `${TODAY}T10:00:00Z`, action: 'mission start', mission_id: 'some-other-mission', machine_id: PEER },
  ]);

  const msg = page.locator('.msg');
  await expect(msg).toContainText('ephemeral or cleared');
  await expect(msg, 'no machine may be named when none is known').not.toContainText(PEER);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

// #1868 packet 2 retarget of mission-graph-peer-honesty.spec.js against
// `MissionGraphLens` (`#mission=<id>`).
//
// (#1466) A peer's mission must say WHERE it ran, not 404 into a shrug.
// `/flow/:date` is fleet-wide (Redis-merged in production; here just a
// route-mock), while the mission-graph builder reads LOCAL DISK only — so a
// peer's mission 404s on `/mission/:id/graph.json` even though the flow
// stream still carries its records with `machine_id` stamped.
// `MissionGraphLens.tsx`'s own `lookupOwningMachine` ports this exactly:
// on a 404, it scans `/flow/<today>` for a record naming this mission with a
// `machine_id`, and names the peer instead of the generic "gone" wording.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'review-ran-elsewhere';
const PEER = 'm1-max-32gb-studio';
const TODAY = new Date().toISOString().slice(0, 10);

async function open404(page, flowRows) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
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
  await page.route(/\/flow-mission\/[^/?]+(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) })
  );
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await page.waitForSelector('[role="alert"]');
  return errors;
}

test('a peer mission names the machine it ran on', async ({ page }) => {
  const errors = await open404(page, [
    { ts: `${TODAY}T10:00:00Z`, action: 'mission start', mission_id: MISSION_ID, machine_id: PEER },
  ]);

  const msg = page.getByRole('alert');
  await expect(msg, 'the operator needs to know WHICH machine to open').toContainText(PEER);
  await expect(
    msg,
    'and it must not still claim the run may be gone — it demonstrably is not'
  ).not.toContainText('ephemeral or cleared');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a genuinely unattributable 404 keeps the calm original wording', async ({ page }) => {
  // No flow record names this mission on any machine — a real ephemeral or
  // cleared run. Inventing a machine name here would be worse than the vague
  // message, so the fallback must survive.
  const errors = await open404(page, [
    { ts: `${TODAY}T10:00:00Z`, action: 'mission start', mission_id: 'some-other-mission', machine_id: PEER },
  ]);

  const msg = page.getByRole('alert');
  await expect(msg).toContainText('ephemeral or cleared');
  await expect(msg, 'no machine may be named when none is known').not.toContainText(PEER);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

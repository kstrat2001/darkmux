// #238: the viewer must respect prefers-reduced-motion — the infinite
// live-badge pulse (`@keyframes lpulse` on `.pb.live`) is the vestibular
// concern. Under reduced-motion emulation, the animation must be neutralized
// (the badge stays green + "● live" labeled, just not pulsing).
const { test, expect } = require('@playwright/test');

// (#1622) Every other spec collects uncaught errors; this one did not, so a
// throw during render would go unreported while the assertions still passed.
test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

test('prefers-reduced-motion neutralizes the live-badge pulse animation', async ({ page }) => {
  await page.goto('/index.html'); // demo harness — any viewer page carries the CSS
  await page.emulateMedia({ reducedMotion: 'reduce' });

  const result = await page.evaluate(() => {
    const matches = matchMedia('(prefers-reduced-motion: reduce)').matches;
    const el = document.createElement('span');
    el.className = 'pb live';
    document.body.appendChild(el);
    const cs = getComputedStyle(el);
    return { matches, duration: cs.animationDuration, iterations: cs.animationIterationCount };
  });

  // The emulation is actually active...
  expect(result.matches, 'reduced-motion media query should match under emulation').toBe(true);
  // ...and the viewer's guard collapsed the infinite 1.6s pulse: parse the
  // duration to seconds and assert it's effectively zero (the .001ms override).
  const secs = result.duration.endsWith('ms')
    ? parseFloat(result.duration) / 1000
    : parseFloat(result.duration);
  expect(secs, `animation-duration was ${result.duration}`).toBeLessThan(0.01);
  expect(result.iterations).not.toBe('infinite');
});

// (U5-3) The fleet strip's loading shimmer was the ONE animation in the sheet
// with no guard — every other one (`--beat`, `.masthead__refresh.spin`,
// `.labrunrow`'s nudge, `.dialog`'s entrance) has had one for releases. An
// endless loading shimmer is exactly the motion this preference exists to
// stop. Same construct-and-measure shape as the pulse test above, because the
// pending state needs a daemon that is slow to answer and no fixture reaches
// it — the CSS is what is under test either way.
test('prefers-reduced-motion neutralizes the fleet strip shimmer', async ({ page }) => {
  await page.goto('/index.html');
  await page.emulateMedia({ reducedMotion: 'reduce' });

  const result = await page.evaluate(() => {
    const strip = document.createElement('div');
    strip.className = 'fleet-strip fleet-strip--pending';
    const skel = document.createElement('div');
    skel.className = 'fleet-skeleton';
    strip.appendChild(skel);
    document.body.appendChild(strip);
    const cs = getComputedStyle(skel);
    return { name: cs.animationName, iterations: cs.animationIterationCount };
  });

  expect(result.name, `animation-name was ${result.name}`).toBe('none');
  expect(result.iterations).not.toBe('infinite');
});

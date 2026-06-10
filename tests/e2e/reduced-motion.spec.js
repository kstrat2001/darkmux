// #238: the viewer must respect prefers-reduced-motion — the infinite
// live-badge pulse (`@keyframes lpulse` on `.pb.live`) is the vestibular
// concern. Under reduced-motion emulation, the animation must be neutralized
// (the badge stays green + "● live" labeled, just not pulsing).
const { test, expect } = require('@playwright/test');

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

import { test, expect } from "@playwright/test";
import { waitSettled, installFrozenClock, installCorpusRoutes, loadMeta } from "./lib/extract-next-lens.js";

test("PROBE: computed styles of the three .session-run tracks", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);
  await page.goto("/index.html#session=task-list");
  await waitSettled(page, expect, '.session-run[data-state="data"]');
  const out = await page.evaluate(() => {
    const tracks = [...document.querySelectorAll(".session-run .track")];
    return tracks.flatMap((t, ti) =>
      [...t.children].map((c) => {
        const cs = getComputedStyle(c as HTMLElement);
        return `T${ti} cls=${JSON.stringify((c as HTMLElement).className)} size=${cs.fontSize} color=${cs.color} :: ${((c as HTMLElement).innerText || "").slice(0, 34).replace(/\n/g, "\\n")}`;
      })
    );
  });
  for (const l of out) console.log(l);
  expect(out.length).toBeGreaterThan(0);
});

import { test, expect } from "@playwright/test";

// #2000's red-first render proof. The run-detail brief pairs a label with the
// WRONG value at 3-column widths (1280/1440) because `sessionRun.ts` pushes
// label and value as two SEPARATE grid children (`pushKv`) into
// `.session-run .track.brief-grid` (`ui/src/styles.css`'s
// `repeat(auto-fit, minmax(min(240px, 100%), 1fr))`); with row-major
// auto-flow, a pair only reads correctly when the column count is even (or
// 1) — at an odd count above 1 every row starts on the opposite parity, so
// every pair straddles a column boundary. jsdom does no layout, so this is
// invisible to `sessionRun.test.ts`'s unit coverage; it needs a REAL browser
// computing a REAL grid, which is exactly what this spec is for.
//
// Talks to the `bun run dev` vite server (port 5273 by default — see
// `ui/vite.config.ts`), NOT a throwaway or the operator's real daemon: the
// `/flow-session/<id>` fetch is mocked via `page.route` below, so no backend
// data is needed at all — every field in the brief (`route`/`runtime`/
// `image`/`model`/`workspace`/`timing`) is fully controlled here, matching
// the shape already exercised by `ui/src/lenses/session/sessionRun.test.ts`'s
// "a clean, completed local dispatch" case.
//
// Run with: `bun run dev` (in `ui/`, separate shell), then from `ui/`:
//   DARKMUX_VERIFY_PORT=5273 npx playwright test verify/brief-pairing.spec.ts --config verify/playwright.config.ts
//
// LOCAL-ONLY, deliberately, and worth knowing before trusting a green CI run:
// NOTHING in CI executes this file. `vitest.config.ts` includes only
// `src/**/*.test.{ts,tsx}`, and the Playwright jobs run `tests/e2e` and
// `tests/parity` — so this is a hand-run proof, the same standing as its
// neighbors here (`live-render.spec.ts`, `machine-render.spec.ts`,
// `task-row-name.spec.ts`). The CI-able half of #2000's guard therefore
// lives in `src/lenses/catalog/SessionReplay.test.tsx` ("wraps every brief
// label with its OWN value in one `.brief-pair` grid item"): jsdom cannot
// see the layout defect, but it CAN see the wrapper being deleted, which is
// the likelier regression by far. Neither file replaces the other.
//
// A vite caching trap, hit twice while red-proving this spec: vite serves
// CACHED transforms, so a before/after run against a reverted component can
// pass while still serving the NEW module. Kill vite and
// `rm -rf node_modules/.vite` between arms, then confirm what is actually
// being served (`curl <dev>/src/lenses/catalog/SessionReplay.tsx | grep -c
// groupBriefEntries`) before believing either result.

const SESSION_ID = "brief-pairing-2000";

async function mockSessionAndGoto(page: import("@playwright/test").Page, width: number, height: number) {
  await page.route(`**/flow-session/${SESSION_ID}`, (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        records: [
          {
            ts: "2026-09-08T21:45:00Z",
            session_id: SESSION_ID,
            action: "dispatch start",
            handle: "darkmux/coder",
            model: "qwen3.6-35b-a3b-turboquant-mlx",
            payload: {
              runtime: "internal",
              image: "darkmux-runtime:latest",
              // A long path — the issue's own reproduction used a long
              // workspace value, which is what makes the mismatch legible.
              workspace: "/home/demo/.darkmux/runs/crawl-discarded-locks/sandbox",
              prompt_chars: 500,
            },
          },
          {
            ts: "2026-09-08T21:45:29Z",
            session_id: SESSION_ID,
            action: "dispatch complete",
            payload: { prompt_tokens: 1000, completion_tokens: 200, wall_ms: 29000 },
          },
        ],
      }),
    }),
  );
  await page.setViewportSize({ width, height });
  await page.goto(`/#dispatch=${SESSION_ID}`);
  const grid = page.locator(".session-run .track.brief-grid");
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return grid;
}

// The exact field → value mapping the mocked dispatch-start/complete records
// above produce (matches `sessionRun.ts`'s `pushKv` order: route, runtime,
// image, model, workspace, timing, then the fallback prompt-length line).
//
// These are INCIDENTAL FACTS, not the contract: they are today's fixture
// crossed with today's UI copy (`RUNTIME_LABEL`'s "internal container", the
// `route` string's "LMStudio", `fmtElapsed`'s "→"). Any of them can change
// for a reason that has nothing to do with #2000, and when one does the fix
// is to update this map — a failure here is a copy/fixture drift report, not
// a pairing regression. The contract itself is the two assertions below.
const EXPECTED_VALUE_SUBSTRING: Record<string, string> = {
  route: "LMStudio",
  runtime: "internal container",
  image: "darkmux-runtime:latest",
  model: "qwen3.6-35b-a3b-turboquant-mlx",
  workspace: "/home/demo/.darkmux/runs/crawl-discarded-locks/sandbox",
  timing: "→", // "→" — the start/end arrow, never shared with any other field's value
  prompt: "500 chars",
};

/** Two complementary checks, both against the REAL rendered grid (never a
 *  CSS-string or class-name assertion — see the module doc above for why
 *  jsdom cannot see this class of defect at all):
 *
 *  1. CONTENT — for every `.brief-label` on screen, read the element right
 *     after it (`.nextElementSibling`) and assert it is a `.brief-value`
 *     carrying the value that field is supposed to carry.
 *
 *     STATED HONESTLY, because an earlier version of this comment claimed
 *     otherwise: this does NOT read the defect the issue reports, and it
 *     CANNOT go red on it. `pushKv` (`sessionRun.ts`) emits a label and its
 *     value as ADJACENT siblings, so `nextElementSibling` is the correct
 *     value however the grid lays the items out. The DOM was never wrong —
 *     only the painted PLACEMENT was, which makes GEOMETRY the assertion
 *     that carries the issue. (A run of this spec against `origin/main`'s
 *     component reported exactly that: red 5/5 on geometry, this half never
 *     firing.)
 *
 *     It stays because it guards a DIFFERENT regression cheaply: a future
 *     grouping/ordering change that pairs a label with somebody else's
 *     value, or drops the value element entirely, in the DOM itself.
 *  2. GEOMETRY — the assertion that carries #2000. The label and its value
 *     must share a left edge (same column) and the value must sit directly
 *     below the label, so the pair renders as one visually contiguous unit
 *     rather than two DOM-correct elements torn apart on screen.
 *
 *     Note what this is, exactly: a NEW invariant at EVERY width above one
 *     column, not a probe isolating the odd-column case. Against
 *     `origin/main`, where each entry is its own grid item, a label in
 *     column N always has its value in column N+1 — different left edges —
 *     so it fails at 768/1024/1680 as surely as at 1280/1440. The odd/even
 *     distinction only ever governed whether the CONTENT-visible symptom
 *     (a label reading against the wrong neighbor's value) appeared; the
 *     geometry was wrong at all of them. */
async function assertBriefLabelsPairWithTheirRealValue(grid: import("@playwright/test").Locator) {
  const rows = await grid.evaluate((el) =>
    Array.from(el.querySelectorAll(".brief-label")).map((label) => {
      const value = label.nextElementSibling as HTMLElement | null;
      const lBox = label.getBoundingClientRect();
      const vBox = value?.getBoundingClientRect();
      return {
        label: (label as HTMLElement).innerText.trim(),
        valueCls: value?.className ?? null,
        valueText: value?.innerText ?? null,
        lLeft: lBox.left,
        lBottom: lBox.bottom,
        vLeft: vBox?.left ?? null,
        vTop: vBox?.top ?? null,
      };
    }),
  );
  expect(rows.length, "the brief must have real label entries").toBeGreaterThan(0);

  for (const row of rows) {
    const expected = EXPECTED_VALUE_SUBSTRING[row.label];
    expect(expected, `unexpected label on screen: "${row.label}" — update EXPECTED_VALUE_SUBSTRING if this is intentional`).toBeDefined();

    // CONTENT: the element immediately after the label must BE a value
    // (never another label — that is exactly the straddle the issue
    // reports), and it must carry the RIGHT value for this field.
    expect(row.valueCls, `label "${row.label}" is not immediately followed by a .brief-value element`).toContain("brief-value");
    expect(
      row.valueText,
      `label "${row.label}" paired with "${row.valueText}", expected it to contain "${expected}"`,
    ).toContain(expected);

    // GEOMETRY: same column (left edges aligned within a few px — allows for
    // the label's own text-indent/letter-spacing, not for a full column
    // width of drift) and value directly below the label, not a full row
    // below (which would mean the pair spans two grid rows instead of
    // rendering as one compact unit).
    expect(row.vLeft, `label "${row.label}" and its value are not left-aligned (label at ${row.lLeft}, value at ${row.vLeft}) — they are not sharing a column`).toBeGreaterThanOrEqual(row.lLeft - 2);
    expect(row.vLeft!, `label "${row.label}" and its value are not left-aligned (label at ${row.lLeft}, value at ${row.vLeft})`).toBeLessThanOrEqual(row.lLeft + 2);
    expect(
      row.vTop! >= row.lBottom - 2 && row.vTop! < row.lBottom + 40,
      `label "${row.label}"'s value is not directly below it (label bottom ${row.lBottom}, value top ${row.vTop})`,
    ).toBe(true);
  }
}

test.describe("#2000 — run-detail brief pairs label with the right value at 3-column widths", () => {
  for (const width of [1280, 1440]) {
    test(`${width}px: every brief row reads as label-then-value pairs, never straddling a column`, async ({ page }) => {
      const grid = await mockSessionAndGoto(page, width, 900);
      await page.screenshot({ path: `verify/.gallery/2000-brief-${width}.png` });

      // Sanity: confirm this width actually resolves to the odd column count
      // the issue's own measured table names (3 at both 1280 and 1440) — so
      // a future CSS change that happens to dodge the odd-column case here
      // fails LOUDLY (this assertion) rather than the test silently stopping
      // to exercise the condition it exists to guard.
      const columns = await grid.evaluate((el) => getComputedStyle(el).gridTemplateColumns.trim().split(/\s+/).length);
      expect(columns, `expected ${width}px to resolve to an odd column count > 1 (issue's own table: 3) — got ${columns}, so this width no longer exercises the straddle condition`).toBe(3);

      await assertBriefLabelsPairWithTheirRealValue(grid);
    });
  }

  // Additional widths, so this proof doesn't depend on the specific ones
  // chosen above. NOT controls, despite what an earlier version of this
  // comment said: "must stay clean both before and after the fix" does not
  // hold. Against `origin/main` each entry is its own grid item, so at ANY
  // column count above 1 a label sits in column N and its value in column
  // N+1 — different left edges — and the GEOMETRY assertion fails here
  // exactly as it does at 1280/1440 (a run against main reported all five
  // widths red). What the issue's own table calls "already correct" at these
  // widths is the CONTENT symptom, which an even column count does dodge —
  // and content is the half that cannot go red here at all. So these widths
  // widen the same new invariant; they isolate nothing.
  for (const width of [768, 1024, 1680]) {
    test(`${width}px (even/other column counts — no CONTENT symptom here, same pairing invariant): still pairs correctly`, async ({ page }) => {
      const grid = await mockSessionAndGoto(page, width, 900);
      await assertBriefLabelsPairWithTheirRealValue(grid);
    });
  }
});

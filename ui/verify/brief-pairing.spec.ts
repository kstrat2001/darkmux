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
// This is the CONTENT half of the invariant — the issue's own title is "pairs
// label with the WRONG value", so the strongest possible proof is reading
// back what value each label is ACTUALLY next to on screen and comparing it
// against what it must be, not just "some value follows some label".
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
 *  1. CONTENT — for every `.brief-label` actually on screen, read the value
 *     rendered immediately after it (`.nextElementSibling`) and assert it is
 *     the value that field is SUPPOSED to carry (`EXPECTED_VALUE_SUBSTRING`).
 *     This is a direct read of the exact defect the issue reports: "`model`
 *     appears to name a filesystem path" is precisely
 *     `nextElementSibling.textContent` disagreeing with the expected
 *     mapping.
 *  2. GEOMETRY — every label/value pair that CONTENT found correct must also
 *     never straddle a column: the label and its value must share the same
 *     left edge (same column) and the value must sit directly below the
 *     label (small vertical gap), proving the pair renders as one visually
 *     contiguous unit rather than the two being correct in the DOM but torn
 *     apart on screen by the grid. */
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

  // Control widths (from the issue's own measured table) — must stay clean
  // both before and after the fix, so this proof doesn't accidentally
  // depend on the specific width chosen.
  for (const width of [768, 1024, 1680]) {
    test(`${width}px (control, already correct per the issue's own table): still pairs correctly`, async ({ page }) => {
      const grid = await mockSessionAndGoto(page, width, 900);
      await assertBriefLabelsPairWithTheirRealValue(grid);
    });
  }
});

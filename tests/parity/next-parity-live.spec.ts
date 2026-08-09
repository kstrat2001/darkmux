// @ts-nocheck
// Packet 5 acceptance: `/next`'s ported live-tail lens — the SSE flow tail
// (`useLiveTail.ts`), the reconcile backstop, UTC date-rollover, and the
// `#modebadge` live/reconnecting honesty (#1480) — against a REAL Chromium
// `EventSource`. Run with `bunx playwright test --config
// next-parity-live.playwright.config.js` (see `package.json`'s
// `next-parity-live` script). NOT picked up by `playwright.config.js`'s
// `testMatch` (the legacy extractor/red-prove pair) or any sibling
// next-parity config — this is its own suite, same convention every prior
// lens packet's own config file documents.
//
// This suite exists BECAUSE `useLiveTail.test.ts` (vitest, jsdom) cannot
// prove the thing it exists to prove: jsdom has no `EventSource`
// implementation at all, so that suite injects a hand-rolled mock whose
// `onopen`/`onerror`/`onmessage` are called DIRECTLY by test code, never by
// an actual browser networking stack. This file is the live proof the
// `sse.ts` module doc has been promising since Packet 1 — a real
// `EventSource`, in a real Chromium tab, against `page.route`-mocked (but
// otherwise unmodified) HTTP responses.
//
// TWO mechanical facts about the mock shape everything below, and both are
// worth stating up front rather than re-deriving per test:
//
// 1. `route.fulfill` delivers a COMPLETE HTTP response, not a true
//    open-ended stream — there is no Playwright primitive for "hold this
//    connection open and push more data later." So a `route.fulfill`-based
//    `/flow/:date/stream` response closes immediately after its body is
//    delivered, and the browser's `EventSource` — per spec — treats ANY
//    closure it didn't itself initiate as a dropped connection, firing
//    `error` and automatically reconnecting (re-issuing the SAME request,
//    which `page.route` intercepts again). This is exploited directly by
//    the "an SSE record lands" test below: it seeds an EMPTY first response
//    (closes immediately, and the reconnect is what carries the real
//    record) rather than fighting the mechanism.
// 2. For tests that just need a STABLE "connected" badge with no message
//    traffic (render-sanity, date-rollover), the trick above is the wrong
//    tool — a reconnect storm would just be noise. Instead, those routes
//    NEVER resolve the request at all (`await new Promise(() => {})`
//    inside the handler). A pending request never gets response headers, so
//    the browser's `EventSource` never reaches `onopen` OR `onerror` — it
//    just sits in `CONNECTING`. `useLiveTail`'s badge status starts at
//    `"live"` and only changes on one of those two callbacks (see that
//    hook's own doc for why — it mirrors legacy's `setBadges()` setting
//    "● live" BEFORE `startLiveTail` even runs), so a permanently-pending
//    request is the closest honest approximation this harness has to "a
//    real daemon's stream that's open and simply has nothing to say yet."
//    `page.on("request", ...)` still fires the instant the request is
//    DISPATCHED (not when it resolves), so request-arrival assertions
//    (which date a stream opened against) are unaffected by the hang.
const { test, expect } = require("@playwright/test");
const { installFrozenClock, installControllableClock, regionText } = require("./lib/extract-lens.js");
const { loadMeta, installCorpusRoutes, installBlankRoutes } = require("./lib/mock-routes.js");

/** Build an SSE response body. `retryMs`, when given, is emitted as the
 * FIRST line (`retry: <ms>\n\n`) — a real part of the SSE wire format (not a
 * test-only hack) that tells the browser how long to wait before
 * reconnecting after THIS connection closes; used to make the
 * fulfill-closes-immediately reconnect (see the module doc above) fast and
 * deterministic instead of waiting out the ~3s browser default. */
function sseBody(records, { retryMs } = {}) {
  const retryLine = retryMs != null ? `retry: ${retryMs}\n\n` : "";
  return retryLine + records.map((r) => `data: ${JSON.stringify(r)}\n\n`).join("");
}

/** `prevDateUTC()`'s inverse — `lib/flow.ts`'s own function, reimplemented
 * here in plain Node/CJS rather than importing the TS module (this file
 * runs outside the Vite/vitest toolchain, same as every other file in this
 * directory — see `lib/flow.ts` for the canonical, unit-tested version this
 * mirrors). */
function nextDateUTC(d) {
  const dt = new Date(d + "T00:00:00Z");
  dt.setUTCDate(dt.getUTCDate() + 1);
  return dt.toISOString().slice(0, 10);
}

function recordCountFrom(metaText) {
  const m = metaText.match(/(\d+)\s+records/);
  return m ? Number(m[1]) : null;
}

/** The corpus's two `/flow/<date>` fixtures are very different sizes
 * (`flow-yesterday.json` is ~2x `flow-today.json`) and `useFlowWindow`
 * fetches + renders each query independently — the `#meta` record count is
 * REAL, live React state, so it legitimately updates once for whichever day
 * resolves first and again when the second (slower) one lands. A "before"
 * snapshot taken the instant `#meta` first shows ANY count races that
 * second settle. This waits for the number to stop changing across
 * consecutive samples before treating it as the real baseline — the same
 * shape as this repo's other `toPass`-based settle-waits (see
 * `next-parity-console.spec.ts`'s `waitLoadedUnderFrozenClock`), just
 * polling a number instead of a locator. */
async function waitForStableRecordCount(page, { attempts = 30, intervalMs = 200 } = {}) {
  let last = null;
  for (let i = 0; i < attempts; i++) {
    const now = recordCountFrom(await regionText(page, "meta"));
    if (now !== null && now === last) return now;
    last = now;
    // eslint-disable-next-line no-await-in-loop -- deliberately sequential: each sample must see the PREVIOUS one's result.
    await page.waitForTimeout(intervalMs);
  }
  throw new Error(`meta record count never stabilized (last sample: ${last})`);
}

/** Registers a stream-path override that NEVER resolves — see the module
 * doc's point 2 for why this is the right tool for "stays live, no message
 * traffic" tests. Falls back to whatever routes are ALREADY installed
 * (expected: `installCorpusRoutes`, registered before this call) for every
 * other path. */
async function installHangingStream(page, matchesStreamPath) {
  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    if (matchesStreamPath(url.pathname)) {
      await new Promise(() => {}); // deliberately never resolves
      return;
    }
    return route.fallback();
  });
}

test.describe("next-parity: live/SSE lens (Packet 5)", () => {
  test("an SSE-delivered record raises the live window's record count by exactly one", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);

    const streamPath = `/flow/${meta.captured_date}/stream`;
    const record = {
      ts: new Date(meta.frozen_clock_ms).toISOString(),
      action: "flow.note",
      source: "next-parity-live",
      handle: "packet-5-sse-proof",
    };
    let deliverRecord = false;
    // OVERRIDE the corpus's inert empty stream (registered by
    // `installCorpusRoutes` above) for this ONE path — `page.route` checks
    // the LATEST-registered handler first, so this wins for the stream path
    // and defers (`route.fallback()`) to the corpus handler for every other
    // request, including the two-day `/flow/<date>` fetches this test's
    // baseline count depends on.
    await page.route("**/*", async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === streamPath) {
        return route.fulfill({
          status: 200,
          contentType: "text/event-stream",
          body: deliverRecord ? sseBody([record], { retryMs: 50 }) : sseBody([], { retryMs: 50 }),
        });
      }
      return route.fallback();
    });

    await page.goto("/index.html");
    await expect(page.locator("#meta")).toContainText(/records/, { timeout: 15000 });
    const before = await waitForStableRecordCount(page);
    expect(before, "the corpus's own /flow fixtures must produce a real baseline count").toBeGreaterThan(0);

    // Flip the flag AFTER the baseline has settled — the reconnect (forced
    // by the first, empty response closing) picks it up on its NEXT
    // request, not this one.
    deliverRecord = true;

    await expect(async () => {
      const nowCount = recordCountFrom(await regionText(page, "meta"));
      expect(nowCount).toBe(before + 1);
    }).toPass({ timeout: 8000, intervals: [100] });

    // Deliberately NOT asserting `#modebadge` here too: with `retry: 50`,
    // the SAME `route.fulfill` that carries the record ALSO closes
    // immediately (point 1 in the module doc) — so the badge is flickering
    // live/reconnecting on a ~50ms cadence at this point BY CONSTRUCTION,
    // and racing that flicker would make this assertion meaningless. The
    // badge's live/reconnecting honesty gets its own dedicated, non-racy
    // proof below (the broken-stream and pending-stream tests), where
    // nothing else is competing for the same connection.
  });

  test("a broken stream flips the badge to reconnecting — never silently claims live over a dead connection (#1480 part 2)", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);

    const streamPath = `/flow/${meta.captured_date}/stream`;
    await page.route("**/*", async (route) => {
      const url = new URL(route.request().url());
      if (url.pathname === streamPath) {
        return route.fulfill({ status: 500, contentType: "text/plain", body: "simulated stream failure" });
      }
      return route.fallback();
    });

    await page.goto("/index.html");
    await expect(page.locator("#modebadge")).toContainText("reconnecting", { timeout: 15000 });
    await expect(page.locator("#modebadge")).toHaveClass(/\bstale\b/);
  });

  test("render-sanity: zero pageerror, zero console.error, with the live stream connected (pending, per the module doc)", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await installHangingStream(page, (p) => p === `/flow/${meta.captured_date}/stream`);
    const pageErrors = [];
    const consoleErrors = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });

    await page.goto("/index.html");
    await expect(page.locator("#modebadge")).toContainText("live", { timeout: 15000 });

    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
    expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);
  });

  test("UTC date rollover: crossing midnight closes the old stream, reopens against the new day, and refetches the day-window pair", async ({ page }) => {
    const meta = loadMeta();
    await installControllableClock(page, meta.frozen_clock_ms);   // this test DRIVES time; see the helper's doc
    installCorpusRoutes(page, meta);
    await installHangingStream(page, (p) => /^\/flow\/\d{4}-\d{2}-\d{2}\/stream$/.test(p));

    const newDate = nextDateUTC(meta.captured_date);
    const seenStreamDates = new Set();
    const seenDayFetchDates = new Set();
    page.on("request", (r) => {
      const u = new URL(r.url());
      const streamMatch = u.pathname.match(/^\/flow\/(\d{4}-\d{2}-\d{2})\/stream$/);
      if (streamMatch) seenStreamDates.add(streamMatch[1]);
      const dayMatch = u.pathname.match(/^\/flow\/(\d{4}-\d{2}-\d{2})$/);
      if (dayMatch) seenDayFetchDates.add(dayMatch[1]);
    });

    await page.goto("/index.html");
    await expect(page.locator("#modebadge")).toContainText("live", { timeout: 15000 });
    expect(seenStreamDates.has(meta.captured_date), "the initial stream must open against the captured day").toBe(true);

    // Jump the frozen clock's `Date.now()` straight across UTC midnight
    // WITHOUT firing any of the intermediate 5s ticks (`setSystemTime` sets
    // the time but triggers nothing) — the live tail's own ticker
    // (`useLiveTail.ts`) only needs to observe `todayUTC()` having changed
    // the NEXT time it runs, not on every tick in between.
    const rolloverAtMs = Date.parse(`${newDate}T00:00:05.000Z`);
    await page.clock.setSystemTime(rolloverAtMs);

    // Pump the (still-frozen, now-jumped) clock forward one tick at a time
    // under a real-wall-clock `toPass` retry — robust to however many
    // `setInterval` firings a single `runFor` call actually processes after
    // a large jump, which isn't a contract this test needs to pin down.
    await expect(async () => {
      await page.clock.runFor(5000);
      expect(seenStreamDates.has(newDate), `stream requests seen: ${[...seenStreamDates].join(", ")}`).toBe(true);
    }).toPass({ timeout: 10000, intervals: [100] });

    // Retried, like the stream assertion above it. The day-window refetch is
    // a SEPARATE async hop from reopening the stream (invalidate -> refetch),
    // so asserting it un-retried immediately after a retried assertion pins
    // an ordering the code never promised. It failed exactly that way once
    // the harness stopped freezing timers: the stream had reopened, the
    // refetch simply had not landed yet. Still fails if the refetch never
    // happens at all — the timeout is the teeth.
    await expect(async () => {
      expect(seenDayFetchDates.has(newDate), `day-fetch requests seen: ${[...seenDayFetchDates].join(", ")}`).toBe(true);
    }).toPass({ timeout: 10000, intervals: [100] });
    // What this test does NOT (and structurally cannot, through this
    // harness) prove: that the OLD stream's `EventSource` was actually
    // closed rather than left open alongside the new one — Playwright has
    // no API to introspect a page-internal `EventSource`'s `readyState`,
    // and a pending mocked request (per `installHangingStream`) gives no
    // externally-observable signal on cancellation either. That half is
    // unit-level: `useLiveTail.test.tsx`'s own rollover test asserts
    // `firstStream.closed === true` directly against its mock, where the
    // assertion is actually meaningful. This test's job is the complementary
    // half units structurally can't do — proving a REAL browser issues the
    // new request and the day-window queries actually refetch — which the
    // two assertions above cover.
  });
});

test.describe("next-parity: live/SSE lens red-prove (harness self-test)", () => {
  // Unlike the sibling suites' 404-everything redprove (which proves their
  // STAGE-content goldens can fail), this lens has no stage-rendered golden
  // to falsify — its observable surface is `#modebadge` + the `#meta` record
  // count, both asserted directly above. What's worth proving here instead
  // is narrower and just as real: a blank/unreachable daemon's inert stream
  // (`installBlankRoutes`'s own `/flow/:date/stream` handler — an empty 200,
  // the same shape a genuinely quiet daemon produces) must not crash the
  // page or leave the badge in a state that isn't ONE of the two this hook
  // can actually report — see `useLiveTail.ts`'s own doc for why there are
  // only two (`live`/`reconnecting`), never a third silent default.
  test("a blank daemon's inert stream still resolves to a real, single badge state — never a crash, never an unrecognized state", async ({ page }) => {
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);
    const pageErrors = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html");
    await expect(page.locator("#modebadge")).toBeAttached({ timeout: 15000 });
    const stateAttr = await page.locator("#modebadge").getAttribute("data-state");
    expect(["live", "reconnecting"], `#modebadge reported an unrecognized data-state: ${stateAttr}`).toContain(stateAttr);
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
  });
});

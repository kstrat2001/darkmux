// @ts-nocheck
// Packet 5 acceptance: `/next`'s ported live-tail lens — the SSE flow tail
// (`useLiveTail.ts`), the reconcile backstop, UTC date-rollover, and the
// the pill's own dot — live/reconnecting honesty (#1480, folded into the masthead pill #2412) — against a REAL Chromium
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
// 2. For tests that just need a STABLE, side-effect-free stream with no
//    message traffic (render-sanity, date-rollover), the trick above is
//    the wrong tool — a reconnect storm would just be noise. Instead,
//    those routes NEVER resolve the request at all (`await new Promise(()
//    => {})` inside the handler). A pending request never gets response
//    headers, so the browser's `EventSource` never reaches `onopen` OR
//    `onerror` — it just sits in `CONNECTING`. (2026-09-06: `useLiveTail`'s
//    status USED to start at `"live"` and stay there through this hang —
//    mirroring legacy's `setBadges()` setting "● live" BEFORE
//    `startLiveTail` even ran — which made a permanently-pending request
//    read as a passable approximation of "a real daemon's stream that's
//    open and simply has nothing to say yet." That optimism was itself
//    dishonest: a connection genuinely stuck in `CONNECTING` has NOT
//    opened, and reporting "live" over it is exactly the false-positive
//    the pill's honesty is supposed to rule out — see `useLiveTail.ts`'s
//    own doc. The hook now starts at `"reconnecting"` and only flips to
//    `"live"` on a real `onOpen`, so a permanently-pending request now
//    correctly stays `"reconnecting"` for its whole life; the two tests
//    below that use this trick assert that stayed-`"reconnecting"` state
//    directly, rather than a `"live"` badge this mock cannot honestly
//    produce.) `page.on("request", ...)` still fires the instant the
//    request is DISPATCHED (not when it resolves), so request-arrival
//    assertions (which date a stream opened against) are unaffected by
//    the hang — and are what those two tests now use to confirm the
//    stream actually attempted to open, in place of waiting on a "live"
//    pill that this mock structurally cannot produce.
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

// The live record count MOVED out of `#meta` and into the event pane's own
// counter chip — the status bar stated it and so did the pane, and the
// duplicate cost the status a whole second line. The assertion is unchanged
// (an SSE-delivered record raises the count by exactly one); only where it
// reads the number moved. Chip reads `50 of 734 · last 24h`, or
// `734 · last 24h` when under the cap.
async function liveRecordCount(page) {
  const t = await page.locator(".eventlog__qcount, .qcount").first().textContent().catch(() => null);
  if (!t) return null;
  const m = t.match(/of\s+(\d+)/) || t.match(/^\s*(\d+)\b/);
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
 * polling a number instead of a locator.
 *
 * ZERO IS NOT SETTLED. "Two equal consecutive samples" is satisfied by
 * `0 === 0`, so this used to return a stable `0` and the caller failed on it
 * ("must produce a real baseline count") with a message accusing the CORPUS.
 * The `> 0` condition below is a BETTER FAILURE MESSAGE AND NOTHING MORE.
 * Saying so plainly matters: an earlier version of this comment claimed the
 * zero was a boot-time race that a positive-sample requirement "removes",
 * and instrumenting an actual failure disproved that.
 *
 * What a failing run really does (40 samples, 200ms apart, #2406 re-review):
 * the chip reads `0 events · 952 hidden` at t=286ms and STILL reads it at
 * t=8318ms. Every record has loaded; the event pane's curated default filter
 * is hiding all 952 of them, permanently, and no amount of waiting moves it.
 * A passing run on the same corpus goes `0 events · 952 hidden` -> `50 of 942
 * events · 10 hidden` by t=290ms.
 *
 * WHY that stuck state is reachable at all — reproduced DETERMINISTICALLY by
 * deleting the seed below: none of this corpus's 14 activity values is in
 * `DEFAULT_ACTIVITIES` (`ui/src/lib/eventFilters.ts`); they are all
 * lifecycle/telemetry (`dispatch start`, `step complete`, `host telemetry`,
 * ...). With no stored picks the curated default turns on NOTHING and the chip
 * reads exactly `0 events · 952 hidden`, forever. The only thing between this
 * suite and that state is the `beforeEach` below seeding a show-everything
 * payload; a failing run is one where those picks did not reach the pane, and
 * the pane has no recovery from it, because `absorbNewFacetValues` only ever
 * reconsiders a value its `seen` ledger has never recorded. A PRODUCT defect,
 * not a harness one, reproducible on `origin/main` (1 fail in 10) and filed
 * separately. Do not read this guard as its fix.
 *
 * (#2512, fixed) `eventFilters.ts`'s `resolveActivitySet` now backstops the
 * exact "curated default matches nothing the corpus offers" case this
 * describes — a fresh session over this lifecycle-only corpus now shows
 * all 952 records instead of hiding all of them. The dedicated regression
 * coverage for that fix is `tests/e2e/event-log-filters-lifecycle-only.spec.js`
 * (a from-scratch load, no seeded storage, over a small all-lifecycle
 * fixture) plus `ui/src/lib/eventFilters.test.ts`'s "#2512" describe block —
 * NOT this suite. The `beforeEach` seed below stays: removing it does not
 * reproduce #2512 any more (confirmed by hand, five runs, all landing on a
 * real 952 baseline instead of a stuck 0), but it DOES break this suite for
 * an unrelated, correct reason — the SSE-delivered `flow.note` record this
 * suite injects is a value `absorbNewFacetValues` has never seen before,
 * and #2416's own (deliberate, separately tested) policy is that a brand
 * new activity value absorbs OFF, not on. That is #2416's contract working
 * as designed, not #2512 recurring, and this suite exists to grade RENDER
 * PARITY, not re-litigate the filter default — hence the seed.
 *
 * The guard weakens nothing — the only caller asserts `> 0` on the very next
 * line, so a genuinely-zero run still fails, now with an honest message
 * instead of a wrong accusation.
 *
 * Bundle size changes how OFTEN it fires. Measured back-to-back on one
 * machine: this branch's bundle (619,414 B) failed 4 of 12 runs, and
 * `origin/main`'s (618,188 B) 1 of 16. The ~1.2KB this branch adds does not
 * cause the defect; it does make it likelier to be seen. */
async function waitForStableRecordCount(page, { attempts = 30, intervalMs = 200 } = {}) {
  let last = null;
  for (let i = 0; i < attempts; i++) {
    const now = await liveRecordCount(page);
    if (now !== null && now > 0 && now === last) return now;
    last = now;
    // eslint-disable-next-line no-await-in-loop -- deliberately sequential: each sample must see the PREVIOUS one's result.
    await page.waitForTimeout(intervalMs);
  }
  throw new Error(
    `meta record count never stabilized at a positive value (last sample: ${last}) — ` +
      `either the corpus's /flow fixtures are empty or the app never finished its day-window fetches`,
  );
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

// (#2416) The event filter now defaults to model activity only, and the
// `flow.note` record this suite delivers over SSE is an activity the default
// hides, so the count never rose. These goldens freeze RENDER parity of the events list, not the
// filter default, so the spec seeds the operator's "everything on" picks
// exactly as the e2e mission-lens specs do (one global stored payload,
// version 2). The default itself is pinned by event-log-filters-default.spec.
const SHOW_ALL_ACTIVITIES = [
  'reasoning',
  'checkpoint',
  'tool call',
  'turn',
  'heartbeat',
  'dispatch start',
  'dispatch end',
  'dispatch error',
  'feedback',
  'routing',
  'compaction',
  'note',
  'machine online',
  'machine offline',
  'session end',
  'detector',
  'runtime',
  'tokens',
  'lms',
  'host telemetry',
  'telemetry',
  'other',
  'step start',
  'phase start',
  'mission start',
  'step complete',
  'phase complete',
  'mission close',
  'step result',
  'step timing',
];
test.beforeEach(async ({ page }) => {
  await page.addInitScript((acts: string[]) => {
    window.sessionStorage.setItem("dmux.eventfilters", JSON.stringify({
      version: 2,
      act: { include: acts, exclude: [] },
      cat: { include: [], exclude: [] },
      tier: { include: [], exclude: [] },
      src: { include: [], exclude: [] },
      q: "",
    }));
  }, SHOW_ALL_ACTIVITIES);
});

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
    await expect(page.locator(".eventlog__qcount, .qcount").first()).toContainText(/\d/, { timeout: 15000 });
    const before = await waitForStableRecordCount(page);
    expect(before, "the corpus's own /flow fixtures must produce a real baseline count").toBeGreaterThan(0);

    // Flip the flag AFTER the baseline has settled — the reconnect (forced
    // by the first, empty response closing) picks it up on its NEXT
    // request, not this one.
    deliverRecord = true;

    await expect(async () => {
      const nowCount = await liveRecordCount(page);
      expect(nowCount).toBe(before + 1);
    }).toPass({ timeout: 8000, intervals: [100] });

    // Deliberately NOT asserting the pill's dot here too: with `retry: 50`,
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
    await expect(page.locator(".masthead__pilldot")).toHaveClass(/\bstale\b/, { timeout: 15000 });
    await expect(page.locator(".masthead__pilldot")).toHaveAttribute("title", "reconnecting");
  });

  test("render-sanity: zero pageerror, zero console.error, with the stream request pending (per the module doc — a stuck CONNECTING honestly stays reconnecting)", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await installHangingStream(page, (p) => p === `/flow/${meta.captured_date}/stream`);
    const pageErrors = [];
    const consoleErrors = [];
    const seenStreamDates = new Set();
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });
    page.on("request", (r) => {
      const m = new URL(r.url()).pathname.match(/^\/flow\/(\d{4}-\d{2}-\d{2})\/stream$/);
      if (m) seenStreamDates.add(m[1]);
    });

    await page.goto("/index.html");
    // (2026-09-06) The stream request never resolves (see the module doc),
    // so `onOpen` never fires — the pill honestly stays `reconnecting` for
    // this test's whole life, it never claims `live` over a connection
    // still stuck in `CONNECTING`. Confirming the boot actually happened
    // now goes through the request having been DISPATCHED, not the pill.
    await expect.poll(() => seenStreamDates.has(meta.captured_date), { timeout: 15000 }).toBe(true);
    await expect(page.locator(".masthead__pilldot")).toHaveClass(/\bstale\b/);

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
    // (2026-09-06) The stream request never resolves (see the module doc),
    // so the pill honestly stays `reconnecting` rather than claiming
    // `live` over a connection stuck in `CONNECTING` — confirming the
    // initial stream attempt now polls the request itself, same as the
    // render-sanity test above.
    await expect.poll(() => seenStreamDates.has(meta.captured_date), { timeout: 15000 }).toBe(true);

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
  // to falsify — its observable surface is the pill's dot + the `#meta` record
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
    await expect(page.locator(".masthead__pilldot")).toBeAttached({ timeout: 15000 });
    const stateAttr = await page.locator(".masthead__pilldot").getAttribute("data-state");
    expect(["live", "reconnecting"], `the pill's dot reported an unrecognized data-state: ${stateAttr}`).toContain(stateAttr);
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
  });
});

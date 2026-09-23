import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent, waitFor, act, within } from "@testing-library/react";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { EventLogColumn, compactCountLabel, fmtTok, fmtTurnDuration } from "./EventLogColumn";
import type { FlowRecord } from "../types/handwritten";
import { closeOpenModal } from "../lib/dialogManager";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
// `ui/src/components/` -> repo root is three levels up.
const REPO_ROOT = path.resolve(__dirname, "../../..");

/** (#2863 review round 2, finding 1) A trimmed, REAL flow-session fixture —
 * fetched once from `/flow-session/crew-dispatch-code-reviewer-1789963273339920-0`
 * (a run that left `turn 4` unfinished: a `dispatch.checkpoint` and a
 * `dispatch error`, no `dispatch.turn`), saved with most heartbeats/host
 * telemetry dropped but every action/payload shape kept verbatim from the
 * live daemon. Reused here (not a hand-built fixture) because the
 * duplicate-key bug this file's test below proves only reproduces against
 * the REAL record shapes — a hand-simplified version could accidentally
 * fix itself by construction. */
function readCorpus(name: string): FlowRecord[] {
  const raw = JSON.parse(readFileSync(path.join(REPO_ROOT, "tests/parity/corpus", name), "utf8"));
  return raw.records as FlowRecord[];
}

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return {
    ts: "2026-08-08T12:00:00.000Z",
    category: "dispatch",
    action: "dispatch.reasoning",
    machine_id: "MacBook-Pro",
    ...overrides,
  };
}

// `dialogManager`'s "which dialog is open" state is a module-level
// singleton, not React state — it survives across `render()` calls within
// this file (unmounting a component does not reset it). Without this, a
// test that opens the Filters modal and doesn't explicitly close it would
// leave it open for the NEXT test's freshly-rendered instance too.
afterEach(() => {
  // (#2018) Filters now persist to `sessionStorage`, so without this one
  // test's restrictive picks silently apply to the next — which is how
  // four unrelated tests started reporting an empty pane.
  try { window.sessionStorage.clear(); } catch { /* unavailable */ }
  closeOpenModal({ restore: false });
});

describe("EventLogColumn", () => {
  it("names the WINDOW in the header, and keeps #logscope present but empty", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    // (operator) The outer UI owns context now: the active tab or the crumb
    // already establishes it, and `#logscope` repeated that in six of its
    // eight legacy states. The element stays in the DOM, empty, so this
    // port's parity extraction agrees with legacy's; it dies with legacy at
    // the flip.
    // Present, HIDDEN, and still carrying its text: legacy's own span keeps
    // its text and `innerText` falls back to `textContent` when unrendered,
    // so emitting nothing here would make the two disagree in the parity
    // extraction. What changed is that it is no longer SHOWN.
    const scope = document.getElementById("logscope")!;
    expect(scope.hasAttribute("hidden")).toBe(true);
    expect(scope.textContent).toBe("fleet");
    expect(document.querySelector(".eventlog__head h3")?.textContent).toMatch(/events last \d+h/i);
  });

  it("renders every record (up to the cap) as a row, newest first", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(2);
    // newest first (viewer.html:2443's `slice(-50).reverse()`)
    expect(rows[0].textContent).toContain("s-new");
    expect(rows[1].textContent).toContain("s-old");
  });

  it("shows the empty-log message when there are no records", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(screen.getByText("no events yet")).toBeInTheDocument();
  });

  // RED-PROVED: with the search filter removed (query never applied), this
  // assertion fails because both rows would still be present after typing
  // "reasoning" — verified by temporarily deleting the `if (q && ...)`
  // guard in EventLogColumn.tsx and re-running this test, which then failed
  // on the `expect(rows.length).toBe(1)` line below; restored afterward.
  it("the search box filters the visible rows by substring", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-alpha" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "dispatch.tool", session_id: "s-beta" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-alpha");
  });

  it("shows 'no match' in the query count when the search matches nothing", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[rec({})]} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "nothing-matches-this" } });
    expect(screen.getByText("no match")).toBeInTheDocument();
  });

  // (#2770) The count chip's VISIBLE text stays a bare "N hidden" (the
  // desktop header's short-chip layout contract requires it — see
  // EventLogColumn.tsx's own doc), so the cause the operator's live report
  // needed lives in the EMPTY-STATE body message instead: the screen that
  // was actually indistinguishable from a broken viewer was "0 EVENTS" with
  // nothing in the UI to blame, not a nonzero chip reading "887 hidden".
  describe("the empty-state message names the responsible filter", () => {
    it("names the activity filter when unchecking the only present activity value empties the log", () => {
      const records = [rec({ action: "dispatch.reasoning" })];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      fireEvent.click(document.getElementById("fbtn")!);
      // Same gesture as "the modal's checkbox grid filters by
      // category/tier/source" above — the label is the facet value text.
      fireEvent.click(screen.getByLabelText("reasoning"));
      expect(screen.getByText("no events match your activity filter")).toBeInTheDocument();
    });

    it("names search when only the free-text query empties the log", () => {
      render(<EventLogColumn scopeLabel="fleet" records={[rec({})]} visible />);
      fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "nothing-matches-this" } });
      expect(screen.getByText("no events match your search")).toBeInTheDocument();
    });

    it("stays generic when a facet AND the query are both narrowing (mixed cause)", () => {
      const records = [rec({ action: "dispatch.reasoning", session_id: "s-alpha" })];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      fireEvent.click(document.getElementById("fbtn")!);
      fireEvent.click(screen.getByLabelText("reasoning"));
      fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
      expect(screen.getByText("no events match your filters")).toBeInTheDocument();
    });
  });

  // (#1891) The entire nonzero-match branch of `qcountText` had exactly
  // zero coverage before this — only the zero-match "no match" case above
  // was ever exercised. These four pin the grammar, the cap disclosure,
  // and the server-truncation marker this branch has to carry.

  it("shows a singular match count with no plural 's' for exactly one match", () => {
    const records = [rec({ session_id: "s-alpha" }), rec({ session_id: "s-beta" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    // (#2417 round 3) The one non-matching record is also "hidden" by the
    // active search — same `hiddenSuffix` the no-search chip carries.
    // (#2770 kept this chip's VISIBLE text plain — the cause label moved to
    // the empty-state message instead, see EventLogColumn.tsx's own doc on
    // why: naming it here widened the chip past the desktop header's
    // <180px short-form layout contract.)
    expect(document.getElementById("qcount")?.textContent).toBe("1 match · 1 hidden");
  });

  it("shows a plural match count for more than one match", () => {
    const records = [
      rec({ session_id: "s-alpha-1" }),
      rec({ session_id: "s-alpha-2" }),
      rec({ session_id: "s-beta" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    // (#2417 round 3) The one non-matching record is also "hidden" by the
    // active search — same `hiddenSuffix` the no-search chip carries.
    // (#2770 kept this chip's VISIBLE text plain — see the sibling test
    // above for why.)
    expect(document.getElementById("qcount")?.textContent).toBe("2 matches · 1 hidden");
  });

  it("appends the LOG_CAP disclosure once the match count exceeds what's shown", () => {
    const records = Array.from({ length: 60 }, (_, i) =>
      rec({ ts: `2026-08-08T12:${String(i).padStart(2, "0")}:00.000Z`, session_id: `s-alpha-${i}` }),
    );
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    expect(document.getElementById("qcount")?.textContent).toBe("60 matches · 50 shown");
  });

  // (#2417 round 3, CONSIDER-1) A live search used to drop the
  // hidden-by-facets context entirely — "12 matches" gave no sense of how
  // much a busy stream's curated default was ALSO hiding underneath the
  // search. Distinct from the LOG_CAP disclosure (`· 50 shown`): this
  // fixture has a query match count under LOG_CAP, so that segment is
  // absent and only the hidden-count segment appears.
  it("appends the hidden-by-filters count to a search match too, not just the no-search chip", () => {
    const records = [
      rec({ action: "dispatch.reasoning", session_id: "s-alpha-1" }),
      rec({ action: "dispatch.reasoning", session_id: "s-alpha-2" }),
      rec({ action: "dispatch.turn.heartbeat", session_id: "s-alpha-heartbeat" }), // hidden by the #2416 default, even though its id would match the query
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    expect(document.getElementById("qcount")?.textContent).toBe("2 matches · 1 hidden");
  });

  it("carries the server-truncation marker into a filtered match count too", () => {
    // (#1891 RED-proved defect) Before the fix, the search branch never
    // consulted `serverTruncated` at all — this "+" disappeared the moment
    // a search filter was active, even though the underlying `records`
    // slice was exactly as truncated as it was with no filter typed.
    const records = [rec({ session_id: "s-alpha-1" }), rec({ session_id: "s-alpha-2" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible serverTruncated />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    expect(document.getElementById("qcount")?.textContent).toBe("2+ matches");
  });

  it("clicking a row selects it (turns follow off) and shows it in the detail panel", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    // Default (follow=on) shows the newest record in the detail panel.
    expect(document.getElementById("detailbody")!.textContent).toContain("s-new");

    const rows = document.querySelectorAll('[data-act="rec"]');
    fireEvent.click(rows[1]); // the older row
    expect(document.getElementById("detailbody")!.textContent).toContain("s-old");
    // Clicking turned follow off.
    expect(document.getElementById("follow")!.className).not.toMatch(/\bon\b/);
  });

  it("(#2068) the detail pane is marked `following` while it tracks the newest record, and not once a row is picked", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    const detail = document.getElementById("detail")!;
    expect(detail.className).toMatch(/\bfollowing\b/);
    fireEvent.click(document.querySelectorAll('[data-act="rec"]')[1]);
    expect(detail.className).not.toMatch(/\bfollowing\b/);
    fireEvent.click(document.getElementById("follow")!);
    expect(detail.className).toMatch(/\bfollowing\b/);
  });

  it("(#2068) an empty log is never marked `following` — nothing streams into an empty pane", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(document.getElementById("detail")!.className).not.toMatch(/\bfollowing\b/);
  });

  it("(#2068) while following, the detail card holds a record for the throttle window even as newer ones stream in", () => {
    vi.useFakeTimers();
    vi.setSystemTime(10_000);
    try {
      const older = rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-one" });
      const { rerender } = render(<EventLogColumn scopeLabel="fleet" records={[older]} visible />);
      expect(document.getElementById("detailbody")!.textContent).toContain("s-one");
      // A burst: two newer records land within the hold window.
      rerender(<EventLogColumn scopeLabel="fleet" records={[older, rec({ ts: "2026-08-08T12:00:01.000Z", session_id: "s-two" })]} visible />);
      expect(document.getElementById("detailbody")!.textContent).toContain("s-two"); // first change lands at once
      rerender(<EventLogColumn scopeLabel="fleet" records={[older, rec({ ts: "2026-08-08T12:00:01.000Z", session_id: "s-two" }), rec({ ts: "2026-08-08T12:00:02.000Z", session_id: "s-three" })]} visible />);
      expect(document.getElementById("detailbody")!.textContent).toContain("s-two"); // held
      // The LIST already shows the newest; only the card holds.
      expect(document.querySelectorAll('[data-act="rec"]')[0].textContent).toContain("s-three");
      act(() => {
        vi.advanceTimersByTime(600);
      });
      expect(document.getElementById("detailbody")!.textContent).toContain("s-three");
    } finally {
      vi.useRealTimers();
    }
  });

  it("the follow toggle re-enables auto-selecting the newest record", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.querySelectorAll('[data-act="rec"]')[1]); // select the older one
    expect(document.getElementById("detailbody")!.textContent).toContain("s-old");

    fireEvent.click(document.getElementById("follow")!);
    expect(document.getElementById("follow")!.className).toMatch(/\bon\b/);
    expect(document.getElementById("detailbody")!.textContent).toContain("s-new");
  });

  it("shows the 'select an event' placeholder when nothing is selected and there are no records", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(screen.getByText("select an event from the log to inspect it")).toBeInTheDocument();
  });

  it("#fbtn opens the filters modal (#1640) — matching legacy's data-act=\"filters\" trigger", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(document.getElementById("modalbg")!.style.display).toBe("none");
    fireEvent.click(document.getElementById("fbtn")!);
    expect(document.getElementById("modalbg")!.style.display).toBe("flex");
    expect(document.querySelector('[aria-labelledby="filters-title"]')).toBeInTheDocument();
  });

  // (operator, 2026-09-06) The modal's standalone "model only" quick-action
  // button is REMOVED — replaced by the MODEL section's own header toggle
  // (`FiltersDialog.tsx`'s `SectionHeader`, tested directly in
  // `FiltersDialog.test.tsx`). This test used to click that button by its
  // text; it's superseded by the section-header coverage there rather than
  // rewritten here, since narrowing to "model activity only, everything else
  // off" is no longer a single gesture — it now happens per section.

  it("the modal's checkbox grid filters by category/tier/source, not just activity", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-local", tier: "local" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "dispatch.reasoning", session_id: "s-cloud", tier: "cloud" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    // Uncheck the "cloud" tier checkbox — its own accessible label is the
    // literal facet value text (see FiltersDialog.tsx).
    fireEvent.click(screen.getByLabelText("cloud"));
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-local");
  });

  // (operator, 2026-09-06) The modal's standalone "clear all" button is
  // REMOVED along with "model only" — see the note above. Re-selecting a
  // single unchecked value is exactly what the per-value checkbox already
  // covers (the test above); a whole-panel reset is no longer a single
  // gesture the panel offers.

  // (#2417 round 2, MF2) The button used to read "filters · 1" whether one
  // value or seventeen were hidden — a strict-subset-per-facet count capped
  // at one per facet. Seventeen distinct unmapped activities (none of them
  // in `DEFAULT_ACTIVITIES`, so all seventeen default OFF) beside three
  // that ARE in the curated default proves the count is real hidden values.
  it("the Filters button counts every hidden PRESENT value, not one per facet", () => {
    const records = [
      rec({ action: "dispatch.reasoning" }),
      rec({ action: "dispatch.checkpoint" }),
      rec({ action: "dispatch.turn" }),
      ...Array.from({ length: 17 }, (_, i) => rec({ action: `custom.action.${i}` })),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    expect(screen.getByRole("button", { name: /filters, 17 active/i })).toBeInTheDocument();
  });

  // (operator, 2026-09-06) The filters button's visible label moved from
  // "filters" text (+ an inline " · N" suffix when active) to an icon glyph
  // with the count as a separate badge — matching `.eventlog__follow`'s own
  // icon-only convention. `aria-label`/`title` (asserted above) already
  // carry the full semantics, so the glyph and badge are `aria-hidden` and
  // the button's own accessible name is unaffected by this markup change —
  // this test pins the DOM shape itself so a future edit can't silently
  // reintroduce the text label.
  it("the filters button renders an icon + a bare-digit badge, not a text label", () => {
    const records = [rec({ action: "dispatch.reasoning" }), rec({ action: "dispatch.turn.heartbeat" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    const fbtn = document.getElementById("fbtn")!;
    expect(fbtn.textContent).not.toContain("filters");
    expect(fbtn.querySelector(".eventlog__ficon")).not.toBeNull();
    const badge = fbtn.querySelector(".eventlog__fcount");
    expect(badge).not.toBeNull();
    // Bare digit — no " · " prefix carried over from the old inline suffix.
    expect(badge!.textContent).toBe("1");
    expect(badge!.getAttribute("aria-hidden")).toBe("true");
  });

  // (#2417 round 2, MF2) The pane chip used to print "50 of <filtered.length>"
  // — a POST-filter total that reads as the whole record count. A stream
  // that is mostly telemetry, with the curated activity default hiding it,
  // must say so.
  it("the pane chip names how many records the filters are hiding, not just the post-filter total", () => {
    const telemetry = Array.from({ length: 900 }, (_, i) =>
      rec({
        ts: `2026-08-08T${String(10 + Math.floor(i / 60)).padStart(2, "0")}:${String(i % 60).padStart(2, "0")}:00.000Z`,
        category: "telemetry",
        source: "process",
        action: undefined,
      }),
    );
    const reasoning = Array.from({ length: 100 }, (_, i) =>
      rec({ ts: `2026-08-08T20:${String(i % 60).padStart(2, "0")}:00.000Z`, action: "dispatch.reasoning" }),
    );
    render(<EventLogColumn scopeLabel="fleet" records={[...telemetry, ...reasoning]} visible />);
    expect(document.getElementById("qcount")?.textContent).toContain("900 hidden");
  });

  // (#2027, revised #2417 round 2 — one global store, written on gestures
  // only) Two `EventLogColumn`s ARE mounted at once on the `mission` route
  // historically (the App-level mainstay plus a lens's own pane — since
  // retired, but the invariant must hold regardless of which route reaches
  // it). The old fix scoped storage per-pane because a routine reconcile
  // tick on the hidden one clobbered the visible one's picks. The round-2
  // fix removes the need for scoping a different way: only an operator
  // gesture persists, so a pane nobody has touched never writes at all.
  //
  // The gesture here is the search box, not a facet checkbox — the
  // checkbox modal (`FiltersDialog`, via `Dialog`'s `createPortal`) is a
  // SINGLETON DOM id (`#modalbg`) shared by every mounted `EventLogColumn`
  // instance (see `App.tsx`'s own doc: "two live mounts... would fight over
  // dialogManager's modalbg id"), which is a separate, pre-existing
  // collision this test is not about. The search input has no such
  // singleton and exercises the identical `persistFilterState` gesture
  // path (`setQuery`).
  it("(#2027 dual-mount) an idle sibling pane's own reconcile never writes, so it cannot clobber a gesture made in the other", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-local", tier: "local" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "dispatch.reasoning", session_id: "s-cloud", tier: "cloud" }),
    ];
    const { container: paneA } = render(<EventLogColumn scopeLabel="fleet" paneId="a" records={records} visible />);
    const { rerender: rerenderB } = render(
      <EventLogColumn scopeLabel="mission m1" paneId="b" records={records} visible={false} />,
    );

    // Neither pane has been touched by the operator yet.
    expect(window.sessionStorage.getItem("dmux.eventfilters")).toBeNull();

    // The operator interacts with pane A only.
    fireEvent.change(within(paneA).getByPlaceholderText("filter events…"), { target: { value: "s-cloud" } });
    expect(paneA.querySelectorAll('[data-act="rec"]').length).toBe(1);

    const storedAfterA = JSON.parse(window.sessionStorage.getItem("dmux.eventfilters")!);
    expect(storedAfterA.q).toBe("s-cloud");

    // Pane B receives a fresh `records` array (a routine live-poll tick),
    // re-running its own facets/absorb reconcile effect — the exact path
    // that used to fire a mount/every-change persist under the old
    // per-scope keying. It must not write anything: pane B never had an
    // operator gesture of its own.
    //
    // (#2417 round 3, MF-B) The tick record carries a BRAND-NEW facet value
    // (`tier: "edge"` — neither "local" nor "cloud" was ever offered
    // before) rather than reusing an already-seen one. `absorbNewFacetValues`
    // returns the SAME `filters` reference when nothing is new (its own
    // "no spurious re-render" guarantee — see `eventFilters.ts`'s doc), so
    // a tick that introduces no new value never even reaches the
    // `setFilters` call inside the reconcile effect: the branch this test
    // exists to prove doesn't write is never actually exercised. A new
    // tier forces `absorbNewFacetValues` to return a NEW reference, which
    // takes the `next !== filters` branch and calls `setFilters` for real.
    rerenderB(
      <EventLogColumn
        scopeLabel="mission m1"
        paneId="b"
        records={[...records, rec({ ts: "2026-08-08T12:10:00.000Z", action: "dispatch.reasoning", session_id: "s-edge", tier: "edge" })]}
        visible={false}
      />,
    );

    const storedAfterB = JSON.parse(window.sessionStorage.getItem("dmux.eventfilters")!);
    expect(storedAfterB.q).toBe("s-cloud");
  });

  // (#2444 review finding) `setFacetMany` — the section header's gesture —
  // had no mounted-column coverage: `FiltersDialog.test.tsx` mocks the
  // callback, so flipping `set.delete(v)` to `set.add(v)` stayed green.
  // This drives the real header through the real column and asserts BOTH
  // the visible rows and the persisted store, the same store shape four
  // individual unchecks would write (the reviewer proved byte-equality).
  it("a section header sets exactly its present values, through the mounted column, and persists them", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s1" }),
      rec({ ts: "2026-08-08T12:01:00.000Z", action: "dispatch.tool", session_id: "s1" }),
      rec({ ts: "2026-08-08T12:02:00.000Z", action: "machine.online", session_id: "s1" }),
    ];
    const { container } = render(<EventLogColumn scopeLabel="fleet" paneId="a" records={records} visible />);
    // Default view: the two MODEL rows show; machine online is off by default.
    expect(container.querySelectorAll('[data-act="rec"]').length).toBe(2);
    fireEvent.click(document.getElementById("fbtn")!);
    // The dialog is display-toggled by class, which jsdom does not compute,
    // so role queries treat it as hidden; query the header by its label.
    const headerOf = () =>
      [...document.querySelectorAll<HTMLInputElement>("input[aria-label]")].find((i) =>
        /^model:/i.test(i.getAttribute("aria-label") ?? ""),
      )!;
    const header = headerOf();
    expect(header.checked).toBe(true);
    fireEvent.click(header);
    expect(container.querySelectorAll('[data-act="rec"]').length).toBe(0);
    const stored = JSON.parse(window.sessionStorage.getItem("dmux.eventfilters")!);
    expect([...stored.act.exclude].sort()).toEqual(["reasoning", "tool call"]);
    expect(stored.act.include).toEqual([]);
    // Back on: the same header, now unchecked, re-includes exactly those two.
    fireEvent.click(headerOf());
    expect(container.querySelectorAll('[data-act="rec"]').length).toBe(2);
  });

  // RED-PROVED (real regression, caught by
  // tests/parity/next-parity-live.spec.ts, not invented for this test): a
  // first draft seeded `filters` via a plain `useState(() =>
  // defaultFilterState(facets))` lazy initializer, which only ever runs
  // ONCE — at mount, when `records` is still `[]` (the shape every real
  // caller passes before its fetch resolves; `App.tsx` always mounts this
  // component before `useRouteRecords`/`useFlowWindow` have data). That
  // locked every facet Set to EMPTY forever, so `matchesFilters` rejected
  // every record once real data arrived — the log stayed permanently
  // empty. Confirmed by temporarily reverting the `useEffect` reseed in
  // `EventLogColumn.tsx` back to the bare lazy initializer and re-running
  // this test, which then failed with 0 rows instead of 1; restored
  // afterward.
  it("records that arrive AFTER the initial (empty) mount still render — filters must not lock onto an empty facet snapshot", async () => {
    const { rerender } = render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(0);

    const records = [rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-late" })];
    rerender(<EventLogColumn scopeLabel="fleet" records={records} visible />);

    await waitFor(() => expect(document.querySelectorAll('[data-act="rec"]').length).toBe(1));
    // (#2863) One session: its id is said once on the shared line, not on
    // the row. The point here is that the late record rendered at all.
    expect(document.querySelector(".eventlog")!.textContent).toContain("s-late");
  });

  // `.eventlog__rec` was a click-only div — no `role`, no `tabIndex`, no
  // key handler — so a keyboard user could not even TAB to a record, let
  // alone select one. Text-only assertions can't see this: the click
  // handler already worked and already produced the right text.
  describe("keyboard operability of a log row", () => {
    it("every row is a real role=button reachable by Tab", () => {
      const records = [rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-1" })];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      const row = document.querySelector('[data-act="rec"]')!;
      expect(row).toHaveAttribute("role", "button");
      expect(row).toHaveAttribute("tabIndex", "0");
    });

    it("Enter selects the row, the same as a click", () => {
      const records = [
        rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
        rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
      ];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      const rows = document.querySelectorAll('[data-act="rec"]');
      fireEvent.keyDown(rows[1], { key: "Enter" }); // the older row
      expect(document.getElementById("detailbody")!.textContent).toContain("s-old");
      expect(document.getElementById("follow")!.className).not.toMatch(/\bon\b/);
    });

    it("Space selects the row, the same as a click", () => {
      const records = [
        rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
        rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
      ];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      const rows = document.querySelectorAll('[data-act="rec"]');
      fireEvent.keyDown(rows[1], { key: " " });
      expect(document.getElementById("detailbody")!.textContent).toContain("s-old");
    });
  });

  // (#2863) Constants are said once. On a list that mixes machines or
  // sessions (the fleet), each row names its own: that is information. On a
  // list that is one session (a run's page), every row would repeat the same
  // two values, so they move to one line under the header.
  it("a list mixing sessions names each row's machine and session", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-1", machine_id: "MacBook-Pro" }),
      rec({ ts: "2026-08-08T12:01:00.000Z", session_id: "s-2", machine_id: "studio" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    expect(document.querySelectorAll(".eventlog__recmachine")).toHaveLength(2);
    expect(document.querySelectorAll(".eventlog__recsession")).toHaveLength(2);
    expect(document.querySelector(".eventlog__shared")).toBeNull();
  });

  it("a one-session list says its machine and session once, not on every row", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "darkmux-coding-x-1790125784225", machine_id: "MacBook-Pro", handle: "coder" }),
      rec({ ts: "2026-08-08T12:01:00.000Z", session_id: "darkmux-coding-x-1790125784225", machine_id: "MacBook-Pro", handle: "coder" }),
    ];
    render(<EventLogColumn scopeLabel="runs" records={records} visible />);
    expect(document.querySelector(".eventlog__recmachine")).toBeNull();
    expect(document.querySelector(".eventlog__recsession")).toBeNull();
    const shared = document.querySelector(".eventlog__shared")!;
    // (#2863, operator) The panel names its own scope: which session these
    // events are, and who ran it where, rather than leaving the reader to
    // infer it from the page beside it.
    expect(shared.querySelector(".eventlog__sharedlbl")!.textContent).toBe("session");
    expect(shared.querySelector(".eventlog__sharedwho")!.textContent).toBe("coder on MacBook-Pro");
    // The FULL id: the column truncates it only when it runs out of room,
    // and from the start, keeping the distinguishing tail (CSS). A fixed
    // six-character cut stayed cut however wide the column was dragged.
    expect(shared.querySelector(".eventlog__sharedsession")!.textContent).toBe("darkmux-coding-x-1790125784225");
    expect(shared.getAttribute("title")).toContain("darkmux-coding-x-1790125784225");
  });

  it("records that carry no session do not stop a list from being one session", () => {
    // Measured on a live run's page: machine telemetry rides the same list
    // with no session id, and every row still repeated the session.
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "sess-1", machine_id: "MacBook-Pro" }),
      rec({ ts: "2026-08-08T12:01:00.000Z", session_id: undefined, machine_id: "MacBook-Pro", action: "machine.telemetry" } as never),
    ];
    render(<EventLogColumn scopeLabel="runs" records={records} visible />);
    expect(document.querySelector(".eventlog__recsession")).toBeNull();
    expect(document.querySelector(".eventlog__sharedsession")!.textContent).toBe("sess-1");
  });

  it("a tool row is its tool, its object and its outcome, not its raw arguments", () => {
    const records = [
      rec({
        action: "dispatch.tool",
        session_id: "s-1",
        payload: { tool_name: "edit", args: '{"path":"/workspace/src/a.js","edits":[{"old_string":"x"}]}', result_chars: 88, outcome: "ok" },
      } as never),
    ];
    render(<EventLogColumn scopeLabel="runs" records={records} visible />);
    const row = document.querySelector('[data-act="rec"]')!;
    expect(row.querySelector(".eventlog__ractivity")!.textContent).toBe("edit");
    expect(row.querySelector(".eventlog__recobj")!.textContent).toBe("src/a.js");
    expect(row.querySelector(".eventlog__outcome--ok")).not.toBeNull();
    expect(row.textContent).not.toContain("old_string");
    expect(row.textContent).not.toContain("88ch");
  });

  // ── Collapse (#1066) ───────────────────────────────────────────────────
  //
  // Collapsed is NOT hidden, and the distinction is the feature. `visible=
  // false` is `display:none` with nothing left to click — a route decided.
  // Collapsed keeps a control, so the operator decided and can undo it.
  it("collapses and expands from a control that stays clickable in both states", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    const col = () => document.querySelector(".eventlog")!;
    expect(col().className).not.toMatch(/eventlog--collapsed/);

    const btn = screen.getByRole("button", { name: /collapse the event log/i });
    fireEvent.click(btn);
    expect(col().className).toMatch(/eventlog--collapsed/);

    // The control is still THERE — that is what separates this from hidden.
    fireEvent.click(screen.getByRole("button", { name: /expand the event log/i }));
    expect(col().className).not.toMatch(/eventlog--collapsed/);
  });

  it("remembers the collapsed choice across a remount, so a tab switch does not reopen it", () => {
    const { unmount } = render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    fireEvent.click(screen.getByRole("button", { name: /collapse the event log/i }));
    unmount();

    render(<EventLogColumn scopeLabel="runs" records={[]} visible />);
    expect(document.querySelector(".eventlog")!.className).toMatch(/eventlog--collapsed/);
  });

  it("reports its state to assistive tech, not by glyph alone", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    const btn = screen.getByRole("button", { name: /collapse the event log/i });
    expect(btn).toHaveAttribute("aria-expanded", "true");
    fireEvent.click(btn);
    expect(screen.getByRole("button", { name: /expand the event log/i })).toHaveAttribute("aria-expanded", "false");
  });

  it("one pane's collapse choice does not collapse the other mounted pane", () => {
    // (#2026 QA) Two EventLogColumns are mounted at once on the `mission`
    // route: the App-level mainstay and the one MissionGraphLens owns. With a
    // single global key, collapsing the mainstay ANYWHERE silently collapsed
    // the mission's own pane — which the operator never touched, and which
    // then showed a 28px rail with no explanation.
    //
    // Red-proven: reverting `collapseKeyFor` to one constant fails this.
    const { unmount } = render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    fireEvent.click(screen.getByRole("button", { name: /collapse the event log/i }));
    unmount();

    render(<EventLogColumn paneId="mission" scopeLabel="mission m1" records={[]} visible />);
    expect(document.querySelector(".eventlog")!.className).not.toMatch(/eventlog--collapsed/);
  });

  it("the mainstay's collapse choice DOES survive a route change (that is the point)", () => {
    // The counterpart to the test above, and the reason the key is the MOUNT
    // SITE rather than the scope label: the App-level pane's label changes
    // per route ("fleet" -> "runs"), so keying on it would reset the choice on
    // every tab switch and turn a mainstay back into a per-page toggle.
    const { unmount } = render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    fireEvent.click(screen.getByRole("button", { name: /collapse the event log/i }));
    unmount();

    render(<EventLogColumn scopeLabel="runs" records={[]} visible />);
    expect(document.querySelector(".eventlog")!.className).toMatch(/eventlog--collapsed/);
  });
});

// ── (#2107 tabbed-drawer packet, restyled #2108 round 5) `pushDetail` —
// the phone drawer's Events tab interaction model: selecting a record
// replaces the list with a full-height detail SCREEN. The selected-record
// strip at its top IS the back control (a separate `.eventlog__back` bar
// was tried and removed — the operator's own finding, "wastes a row").
// ───────────────────────────────────────────────────────────────────────

describe("EventLogColumn — pushDetail mode", () => {
  it("shows the list, not the split detail/list layout, when nothing is selected yet", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[rec({})]} visible pushDetail />);
    expect(document.querySelector(".eventlog__rec")).not.toBeNull();
    expect(document.querySelector("#detail")).toBeNull();
    expect(document.querySelector("#split")).toBeNull();
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
  });

  it("selecting a record replaces the list with a full-height detail screen — the strip IS the back control, no separate bar", () => {
    const records = [rec({ session_id: "s1" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible pushDetail />);
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    const pushed = document.querySelector('[data-act="eventlog-pushed"]');
    expect(pushed).not.toBeNull();
    expect(pushed!.textContent).toContain("s1");
    expect(document.querySelector('[data-act="rec"]')).toBeNull();
    expect(document.querySelector('[data-act="eventlog-back"]')).toBeNull();
    const strip = document.querySelector('[data-act="rec-strip"]')!;
    expect(strip).not.toBeNull();
    expect(strip.getAttribute("role")).toBe("button");
    expect(strip.getAttribute("aria-label")).toBe("Back to list");
    expect(strip.getAttribute("tabindex")).toBe("0");
  });

  it("tapping the strip returns to the list, and the record stays highlighted as selected", () => {
    const records = [rec({ session_id: "s1" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible pushDetail />);
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    fireEvent.click(document.querySelector('[data-act="rec-strip"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    const row = document.querySelector('[data-act="rec"]')!;
    expect(row).not.toBeNull();
    expect(row.className).toMatch(/\bsel\b/);
  });

  // (#2863 review round 2, finding 5) The pushed-detail strip's preview
  // text calls `recordDetail(selected)` DIRECTLY — a second render path
  // outside `recordObject()`, whose own fallback was already escaped.
  it("the strip's preview text escapes a bidi override in the selected record", () => {
    const records = [
      rec({
        session_id: "s1",
        action: "dispatch.reasoning",
        payload: { reasoning_text: "Let me ‮esrever siht‬ read." },
      }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible pushDetail />);
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    const preview = document.querySelector(".preview-text")!;
    expect(preview).not.toBeNull();
    expect(preview.textContent).not.toContain("‮");
    expect(preview.textContent).toContain("⟨U+202E⟩");
  });

  it("the strip is keyboard-activatable (Enter/Space), same as any other row", () => {
    const records = [rec({ session_id: "s1" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible pushDetail />);
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).not.toBeNull();
    fireEvent.keyDown(document.querySelector('[data-act="rec-strip"]')!, { key: "Enter" });
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
  });

  it("omits the collapse rail entirely — collapsing a drawer TAB makes no sense", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible pushDetail />);
    expect(document.querySelector('[data-act="togglelog"]')).toBeNull();
  });

  it("passive follow-latest re-selection never yanks the operator into the pushed detail screen", () => {
    // Only an explicit tap (`selectRecord`) opens the pushed screen — the
    // `follow` toggle keeps re-selecting the newest record on every new
    // event, and doing that in pushDetail mode too would fight any record
    // the operator is deliberately reading.
    const { rerender } = render(<EventLogColumn scopeLabel="fleet" records={[rec({ session_id: "s1" })]} visible pushDetail />);
    rerender(<EventLogColumn scopeLabel="fleet" records={[rec({ session_id: "s1" }), rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s2" })]} visible pushDetail />);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(2);
  });

  it("closes the pushed detail screen when the pane becomes invisible, so reopening lands on the list", () => {
    const records = [rec({ session_id: "s1" })];
    const { rerender } = render(<EventLogColumn scopeLabel="fleet" records={records} visible pushDetail />);
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).not.toBeNull();
    rerender(<EventLogColumn scopeLabel="fleet" records={records} visible={false} pushDetail />);
    rerender(<EventLogColumn scopeLabel="fleet" records={records} visible pushDetail />);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
  });

  it("a caller that omits pushDetail keeps the original split layout unchanged (regression guard)", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[rec({ session_id: "s1" })]} visible />);
    expect(document.querySelector("#detail")).not.toBeNull();
    expect(document.querySelector("#split")).not.toBeNull();
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    expect(document.querySelector('[data-act="rec"]')).not.toBeNull();
  });
});

// ── (#2108, operator finding — phone divider + one-tap expand) ──
//
// The split bar is re-enabled on the phone-width layout (no longer
// `display:none` there) with the SAME Pointer Event drag handlers desktop
// already has, plus a grip + an "Expand"/"Show list" control. These
// exercise the real component, not the CSS that makes it TALL on a
// phone (jsdom performs no layout) — see `PhoneDrawer.test.tsx`'s own
// stylesheet-content tests for that half.
describe("EventLogColumn — phone divider + one-tap expand (#2108)", () => {
  function drag(el: Element, startY: number, endY: number) {
    fireEvent.pointerDown(el, { clientY: startY, pointerId: 1 });
    fireEvent.pointerMove(el, { clientY: endY, pointerId: 1 });
    fireEvent.pointerUp(el, { clientY: endY, pointerId: 1 });
  }

  it("the divider bar is present with its touch-drag handlers, and dragging it resizes the pane", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[rec({ session_id: "s1" })]} visible />);
    const split = document.querySelector('[data-act="eventlog-split"]')!;
    expect(split).not.toBeNull();
    expect(document.querySelector(".eventlog__split-grip")).not.toBeNull();
    const detail = document.querySelector("#detail") as HTMLElement;
    const before = detail.style.flexBasis;
    drag(split, 300, 100); // drag up — grows the pane
    expect(detail.style.flexBasis).not.toBe(before);
  });

  it("dragging the divider persists the ratio to localStorage, scoped by paneId", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[rec({ session_id: "s1" })]} visible paneId="phone-drawer" />);
    const split = document.querySelector('[data-act="eventlog-split"]')!;
    drag(split, 300, 100);
    expect(window.localStorage.getItem("dmux.eventlog.detailpct.phone-drawer")).not.toBeNull();
  });

  it("re-mounting with a persisted ratio for this paneId restores it", () => {
    window.localStorage.setItem("dmux.eventlog.detailpct.phone-drawer", "55");
    render(<EventLogColumn scopeLabel="fleet" records={[rec({ session_id: "s1" })]} visible paneId="phone-drawer" />);
    const detail = document.querySelector("#detail") as HTMLElement;
    expect(detail.style.flexBasis).toBe("55%");
  });

  it("the Expand control toggles the pane to fill the sheet and the list to a 1-row strip showing the selected record, then back", () => {
    const records = [rec({ session_id: "s1", handle: "rec-1" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.querySelector('[data-act="rec"]')!);

    const expandBtn = document.querySelector('[data-act="eventlog-expand"]')!;
    expect(expandBtn.textContent).toBe("Expand");
    expect(document.querySelector('[data-act="eventlog-list-strip"]')).toBeNull();
    expect(document.querySelector("#logbody")).not.toBeNull();

    fireEvent.click(expandBtn);

    // Expanded: pane fills (no inline flexBasis — the CSS class takes
    // over), list collapses to a 1-row strip showing the selected record.
    const detail = document.querySelector("#detail") as HTMLElement;
    expect(detail.className).toContain("eventlog__detail--expanded");
    expect(detail.style.flexBasis).toBe("");
    expect(document.querySelector("#logbody")).toBeNull();
    const strip = document.querySelector('[data-act="eventlog-list-strip"]')!;
    expect(strip).not.toBeNull();
    expect(strip.querySelector('[data-act="rec-strip"]')).not.toBeNull();
    expect(expandBtn.textContent).toBe("Show list");

    fireEvent.click(document.querySelector('[data-act="eventlog-expand"]')!);

    // Back: full list restored, pane back to its ratio.
    expect(document.querySelector("#logbody")).not.toBeNull();
    expect(document.querySelector('[data-act="eventlog-list-strip"]')).toBeNull();
    expect(detail.className).not.toContain("eventlog__detail--expanded");
  });

  it("tapping Expand does not ALSO start a drag on the bar underneath it", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[rec({ session_id: "s1" })]} visible />);
    const detail = document.querySelector("#detail") as HTMLElement;
    const before = detail.style.flexBasis;
    fireEvent.click(document.querySelector('[data-act="eventlog-expand"]')!);
    // The pane switched to expanded mode (flexBasis cleared), not to some
    // arbitrary dragged value — proving no stray drag state leaked in.
    expect(detail.style.flexBasis).toBe("");
    expect(before).not.toBe("");
  });
});

// (operator, 2026-09-06) The phone drawer's Events toolbar row (follow +
// filters icon + the matches count) has no room for the full "50 of 684
// events · 12889 hidden" chip text beside two icon buttons at 320-390px —
// `compactCountLabel()` is the pure transform the mobile row applies to
// the exact same `qcountText` every other consumer reads (desktop, ARIA),
// so there is one source of truth for the wording and no parallel branch
// that could drift from it.
describe("compactCountLabel", () => {
  it("drops \"events\", swaps \"of\" for \"/\", and abbreviates 4+-digit runs to one-decimal k", () => {
    expect(compactCountLabel("50 of 684 events · 12889 hidden")).toBe("50/684 · 12.9k hidden");
  });

  it("leaves short text with no \"events\"/\"of\"/4-digit run untouched", () => {
    expect(compactCountLabel("2 matches · 1 hidden")).toBe("2 matches · 1 hidden");
    expect(compactCountLabel("60 matches · 50 shown")).toBe("60 matches · 50 shown");
    expect(compactCountLabel("no match")).toBe("no match");
  });

  it("abbreviates a hidden-count run even in the search-match branch", () => {
    expect(compactCountLabel("12 matches · 900 hidden")).toBe("12 matches · 900 hidden");
    expect(compactCountLabel("12 matches · 4952 hidden")).toBe("12 matches · 5.0k hidden");
  });
});

// (operator, 2026-09-06 — desktop screenshot) The count pill's DOM group
// is layout-load-bearing, and the two skins need it in DIFFERENT groups.
// On desktop the events column is ~380px and the pill reaches ~250px with
// `white-space: nowrap`; inside `.eventlog__headbtns` (`flex: none`) it
// could neither shrink nor wrap, so it squeezed the title into three
// stacked lines and still ran past the header's right edge. Outside that
// group it is a wrappable child of the `<h3>` and drops onto its own line.
// The phone needs the opposite (#2108): in the group, so follow + filters
// + count share ONE row and the header stays two rows.
describe("count pill placement (#2447)", () => {
  // Both axes, and the ORIGINAL DESCRIPTORS restored — not just the values.
  // `useIsMobile()` has a landscape branch (a phone rotated wide is still
  // phone chrome), so a test that sets only `innerWidth` lands on "desktop"
  // by way of jsdom's 768px `innerHeight` plus the absence of `matchMedia`,
  // not because it asked for a desktop viewport. Setting the pair says what
  // it means. And `defineProperty` replaces jsdom's own accessor with a data
  // property: writing the value back in `afterEach` would leave every later
  // test in this file with an `innerWidth` that no longer tracks the window.
  const ORIGINAL = {
    width: Object.getOwnPropertyDescriptor(window, "innerWidth"),
    height: Object.getOwnPropertyDescriptor(window, "innerHeight"),
  };
  function setViewport(width: number, height: number) {
    Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
    Object.defineProperty(window, "innerHeight", { configurable: true, value: height });
  }
  afterEach(() => {
    if (ORIGINAL.width) Object.defineProperty(window, "innerWidth", ORIGINAL.width);
    if (ORIGINAL.height) Object.defineProperty(window, "innerHeight", ORIGINAL.height);
  });

  const records = [rec({ action: "dispatch.reasoning" }), rec({ action: "tool.completed" })];

  it("desktop: the pill is a child of the header's h3, NOT of the button group", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    const qc = document.getElementById("qcount");
    expect(qc).toBeTruthy();
    expect(qc!.closest(".eventlog__headbtns")).toBeNull();
    expect(qc!.parentElement?.tagName).toBe("H3");
  });

  it("phone: the pill sits inside the button group, on the follow/filters row", () => {
    setViewport(390, 844);
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    const qc = document.getElementById("qcount");
    expect(qc).toBeTruthy();
    expect(qc!.closest(".eventlog__headbtns")).not.toBeNull();
  });

  it("renders exactly one pill in either skin — the two placements are exclusive", () => {
    setViewport(1440, 900);
    const desktop = render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    expect(desktop.container.querySelectorAll(".eventlog__qcount").length).toBe(1);
    desktop.unmount();
    setViewport(390, 844);
    const phone = render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    expect(phone.container.querySelectorAll(".eventlog__qcount").length).toBe(1);
  });
});

// ── (#2863) The vertical divider: the events column's width is the operator's ──
//
// Desktop only. 380px (the column's long-standing width) is the MINIMUM;
// dragging well past it collapses the column to the existing #1066 rail, so
// there is one collapsed state, not two. The width is kept per mount site,
// like the split ratio above, which makes the App-level column's width
// app-wide.
describe("EventLogColumn — resizable width (#2863)", () => {
  const ORIGINAL = {
    width: Object.getOwnPropertyDescriptor(window, "innerWidth"),
    height: Object.getOwnPropertyDescriptor(window, "innerHeight"),
  };
  function setViewport(width: number, height: number) {
    Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
    Object.defineProperty(window, "innerHeight", { configurable: true, value: height });
  }
  afterEach(() => {
    if (ORIGINAL.width) Object.defineProperty(window, "innerWidth", ORIGINAL.width);
    if (ORIGINAL.height) Object.defineProperty(window, "innerHeight", ORIGINAL.height);
    window.localStorage.clear();
    window.sessionStorage.clear();
  });

  const handle = () => document.querySelector('[data-act="eventlog-resize"]') as HTMLElement;
  const column = () => document.querySelector(".eventlog") as HTMLElement;
  const width = () => column().style.getPropertyValue("--eventlog-w");
  function drag(dx: number) {
    const h = handle();
    fireEvent.pointerDown(h, { clientX: 1000, pointerId: 1 });
    fireEvent.pointerMove(h, { clientX: 1000 + dx, pointerId: 1 });
    fireEvent.pointerUp(h, { clientX: 1000 + dx, pointerId: 1 });
  }

  it("renders a vertical separator at the 380px minimum", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(handle()).toHaveAttribute("role", "separator");
    expect(handle()).toHaveAttribute("aria-orientation", "vertical");
    expect(handle()).toHaveAttribute("aria-valuenow", "380");
    expect(width()).toBe("380px");
  });

  it("dragging left widens the column (it sits on the right)", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    drag(-100);
    expect(width()).toBe("480px");
    expect(handle()).toHaveAttribute("aria-valuenow", "480");
  });

  it("never narrower than 380px while short of the collapse point", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    drag(60);
    expect(width()).toBe("380px");
    expect(column()).not.toHaveClass("eventlog--collapsed");
  });

  it("dragging well past the minimum collapses to the existing rail", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    drag(120);
    expect(column()).toHaveClass("eventlog--collapsed");
    expect(width()).toBe("380px"); // reopening restores a usable width
  });

  it("never so wide that the page beside it drops under 420px", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    drag(-2000);
    expect(width()).toBe(`${1440 - 420}px`);
  });

  it("the width persists per mount site and is restored on the next mount", () => {
    setViewport(1440, 900);
    const { unmount } = render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    drag(-100);
    expect(window.localStorage.getItem("dmux.eventlog.width.app")).toBe("480");
    unmount();
    render(<EventLogColumn scopeLabel="runs" records={[]} visible />);
    expect(width()).toBe("480px");
  });

  it("double-click resets to 380px", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    drag(-100);
    fireEvent.doubleClick(handle());
    expect(width()).toBe("380px");
  });

  it("keyboard: arrows resize by 20px, Enter collapses", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    fireEvent.keyDown(handle(), { key: "ArrowLeft" });
    expect(width()).toBe("400px");
    fireEvent.keyDown(handle(), { key: "ArrowRight" });
    fireEvent.keyDown(handle(), { key: "ArrowRight" });
    expect(width()).toBe("380px");
    fireEvent.keyDown(handle(), { key: "Enter" });
    expect(column()).toHaveClass("eventlog--collapsed");
  });

  it("text selection is suspended across the page while dragging", () => {
    // Measured: a drag highlighted the run page's text beside the column.
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    fireEvent.pointerDown(handle(), { clientX: 1000, pointerId: 1 });
    expect(document.documentElement).toHaveClass("is-resizing");
    fireEvent.pointerUp(handle(), { clientX: 900, pointerId: 1 });
    expect(document.documentElement).not.toHaveClass("is-resizing");
  });

  it("a pane that unmounts mid-drag does not leave text selection off page-wide", () => {
    // (#2863 review) Navigating away during a drag skipped pointerup, so the
    // class stayed on <html>.
    setViewport(1440, 900);
    const { unmount } = render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    fireEvent.pointerDown(handle(), { clientX: 1000, pointerId: 1 });
    expect(document.documentElement).toHaveClass("is-resizing");
    unmount();
    expect(document.documentElement).not.toHaveClass("is-resizing");
  });

  it("only the primary button starts a drag", () => {
    setViewport(1440, 900);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    fireEvent.pointerDown(handle(), { clientX: 1000, pointerId: 1, button: 2 });
    expect(document.documentElement).not.toHaveClass("is-resizing");
  });

  it("is not offered on a phone, where the events live in the bottom sheet", () => {
    setViewport(390, 844);
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(handle()).toBeNull();
  });
});

// ── (#2863) A run's list is grouped by turn ─────────────────────────────────
describe("EventLogColumn — turns (#2863)", () => {
  const S = "darkmux-coding-x-1790125784225";
  const r = (sec: number, action: string, payload: Record<string, unknown> = {}) =>
    rec({ ts: new Date(Date.UTC(2026, 8, 23, 1, 9, sec)).toISOString(), action, session_id: S, machine_id: "MacBook-Pro", payload } as never);
  const records = [
    r(1, "dispatch.turn.heartbeat", { turn_seq: 1 }),
    r(9, "dispatch.turn", {
      turn_seq: 1,
      finish_reason: "tool_calls",
      tool_calls_count: 1,
      usage: { prompt_tokens: 15898, completion_tokens: 933, reasoning_tokens: 0 },
    }),
    r(9, "telemetry.context", { used: 15898, max: 262144, threshold: 131072 }),
    r(10, "dispatch.tool", { tool_name: "edit", args: '{"path":"/workspace/a.js"}', outcome: "ok" }),
    r(11, "dispatch.rest", { ms: 15000, reason: "thermal-duty-cycle", state: "fair" }),
  ];

  it("a turn is a header row: its number, what it did, its time, and its context", () => {
    render(<EventLogColumn scopeLabel="runs" records={records} visible />);
    const head = document.querySelector(".eventlog__rec--turn")!;
    expect(head.querySelector(".eventlog__turnname")!.textContent).toBe("Turn 1");
    expect(head.querySelector(".eventlog__turnwhy")!.textContent).toBe("1 tool");
    expect(head.querySelector(".eventlog__turndur")!.textContent).toBe("~8 s");
    expect(head.querySelector(".eventlog__ctxnums")!.textContent).toBe("in 15.9k · out 933");
    expect((head.querySelector(".eventlog__ctxfill") as HTMLElement).style.width).toMatch(/^6\.06/);
    expect((head.querySelector(".eventlog__ctxtick") as HTMLElement).style.left).toBe("50%");
    // The bar says what it is: a visible label (phones have no hover) and a
    // tooltip naming the yellow compaction mark.
    expect(head.querySelector(".eventlog__ctxlbl")!.textContent).toBe("context");
    expect(head.querySelector(".eventlog__ctxbar")!.getAttribute("title")).toBe(
      "context: 15,898 of 262,144 tokens (6%)\nyellow mark: compaction starts at 131,072 tokens",
    );
    // Still a row: clickable, counted, carries the handle title.
    expect(head).toHaveAttribute("data-act", "rec");
  });

  it("a rest is a divider between turns, still a selectable row", () => {
    render(<EventLogColumn scopeLabel="runs" records={records} visible />);
    const rest = document.querySelector(".eventlog__rec--rest")!;
    expect(rest.textContent).toBe("rested 15 s · thermal: fair");
    expect(rest).toHaveAttribute("data-act", "rec");
  });

  // (#2863 review, finding 2) The governor's own state-change record
  // (`emit_rest`, `thermal_governor.rs`) shares the `dispatch.rest` action
  // with a real rest but carries `{pause: false, delay_ms, state}` — no rest
  // ever happened, only the pacing changed. The `?? f.delay_ms` fallback
  // rendered it as "rested 15 s" too, so a run that never paused could show
  // an extra rest divider. Only the `ms` shape is a rest.
  it("a pacing-change record (no `ms`) reads as pacing, not a rest that happened", () => {
    const pacing = [
      ...records,
      r(20, "dispatch.rest", { pause: false, delay_ms: 15000, state: "fair" }),
    ];
    render(<EventLogColumn scopeLabel="runs" records={pacing} visible />);
    const rests = document.querySelectorAll(".eventlog__rec--rest");
    expect(rests).toHaveLength(1);
    expect(rests[0].textContent).toBe("rested 15 s · thermal: fair");
    const pacingRow = document.querySelector(".eventlog__rec--pacing")!;
    expect(pacingRow).not.toBeNull();
    expect(pacingRow.textContent).toBe("pacing · 15 s between turns · thermal: fair");
    expect(pacingRow).toHaveAttribute("data-act", "rec");
  });

  // (#2863 review round 2, finding 3) Four MORE `dispatch.rest` shapes,
  // read straight off the producers (`dispatch_internal.rs`): a real
  // pause carries neither `ms` nor `delay_ms` — the OLD code's `f.ms ===
  // null` fallthrough rendered every one of these as gray "pacing", which
  // is wrong in two ways: nothing is "between turns" (the run is STOPPED),
  // and the reason is not always thermal.
  it("a thermal pause (no ms/delay_ms) reads as paused, not pacing", () => {
    const paused = [...records, r(20, "dispatch.rest", { reason: "thermal", state: "serious", pause: true })];
    render(<EventLogColumn scopeLabel="runs" records={paused} visible />);
    expect(document.querySelector(".eventlog__rec--pacing")).toBeNull();
    const row = document.querySelector(".eventlog__rec--paused")!;
    expect(row).not.toBeNull();
    expect(row.textContent).toBe("paused · thermal: serious");
  });

  it("a thermal breaker trip reads as paused with the breaker's own reason", () => {
    const tripped = [...records, r(20, "dispatch.rest", { reason: "thermal-critical", state: "critical", pause: true })];
    render(<EventLogColumn scopeLabel="runs" records={tripped} visible />);
    expect(document.querySelector(".eventlog__rec--paused")!.textContent).toBe("paused · thermal-critical: critical");
  });

  it("an operator hold (tier 4) reads as paused with its own reason, not pacing", () => {
    const held = [
      ...records,
      r(20, "dispatch.rest", {
        reason: "thermal-episode-limit",
        state: "serious",
        pause: true,
        episode: 2,
        checklist: "worth checking: ...",
        resume_hint: "darkmux dispatch ...",
      }),
    ];
    render(<EventLogColumn scopeLabel="runs" records={held} visible />);
    expect(document.querySelector(".eventlog__rec--paused")!.textContent).toBe("paused · thermal-episode-limit: serious");
  });

  it("a battery pause reads as paused and does NOT say thermal", () => {
    const battery = [...records, r(20, "dispatch.rest", { reason: "battery", state: "12% (floor 20%)", pause: true })];
    render(<EventLogColumn scopeLabel="runs" records={battery} visible />);
    const row = document.querySelector(".eventlog__rec--paused")!;
    expect(row.textContent).toBe("paused · battery: 12% (floor 20%)");
    expect(row.textContent).not.toContain("thermal");
  });

  it("a resume/duty-cycle-exit record (pause:false, no delay_ms) reads as resumed, not pacing", () => {
    // Shares the quiet `--pacing` styling (both are low-severity, dim rows)
    // but the TEXT must not claim an ongoing delay that has ended.
    const resumed = [...records, r(20, "dispatch.rest", { reason: "thermal-duty-cycle", state: "fair", pause: false })];
    render(<EventLogColumn scopeLabel="runs" records={resumed} visible />);
    expect(document.querySelector(".eventlog__rec--paused")).toBeNull();
    const rows = [...document.querySelectorAll(".eventlog__rec")].filter((el) => el.textContent?.startsWith("resumed"));
    expect(rows).toHaveLength(1);
    expect(rows[0].textContent).toBe("resumed · thermal-duty-cycle: fair");
    expect(rows[0].textContent).not.toContain("pacing");
    expect(rows[0].textContent).not.toContain("between turns");
  });

  it("a list mixing sessions shows no turn headers", () => {
    const mixed = [...records, rec({ ts: "2026-09-23T01:09:30.000Z", action: "dispatch.turn", session_id: "other", payload: { turn_seq: 1 } } as never)];
    render(<EventLogColumn scopeLabel="fleet" records={mixed} visible />);
    expect(document.querySelector(".eventlog__rec--turn")).toBeNull();
  });

  it("formats durations with precision the data has", () => {
    expect(fmtTurnDuration(14217, false)).toBe("14.2 s");
    expect(fmtTurnDuration(74100, false)).toBe("1:14");
    expect(fmtTurnDuration(8000, true)).toBe("~8 s");
    expect(fmtTurnDuration(300, true)).toBe("~1 s");
    expect(fmtTok(933)).toBe("933");
    expect(fmtTok(18926)).toBe("18.9k");
  });
});

// (#2863 review round 2, finding 1) A synthesized turn header used to reuse
// its anchor record's OWN identity for its React key — proven on a real
// session where turn 4 left a checkpoint and an error but no `dispatch.turn`.
describe("EventLogColumn — synthesized header identity (#2863 review round 2)", () => {
  it("gives the synthesized header its own key, distinct from the row it borrowed a timestamp from", () => {
    const records = readCorpus("flow-session-unfinished-turn.json");
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    render(<EventLogColumn scopeLabel="runs" records={records} visible />);
    errSpy.mock.calls.forEach((call) => {
      const msg = String(call[0] ?? "");
      expect(msg, `console.error: ${call.map(String).join(" ")}`).not.toMatch(/same key|unique "key" prop/i);
    });
    errSpy.mockRestore();

    // The synthesized "Turn 4" header renders once, and the row it borrowed
    // its timestamp from (the checkpoint or the error) renders as its OWN,
    // separately selectable row underneath — not merged into one element.
    const heads = [...document.querySelectorAll(".eventlog__rec--turn")].filter(
      (h) => h.querySelector(".eventlog__turnname")?.textContent === "Turn 4",
    );
    expect(heads).toHaveLength(1);
    expect(heads[0]).not.toHaveAttribute("data-act");

    // Selecting the real error row must not highlight the synthesized
    // header too (the co-highlight the shared key produced).
    const errRow = [...document.querySelectorAll('[data-act="rec"]')].find((el) => el.textContent?.includes("dispatch error"));
    expect(errRow).toBeTruthy();
    fireEvent.click(errRow!);
    expect(heads[0]).not.toHaveClass("sel");
  });
});

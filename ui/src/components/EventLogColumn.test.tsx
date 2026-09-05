import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent, waitFor, act, within } from "@testing-library/react";
import { EventLogColumn } from "./EventLogColumn";
import type { FlowRecord } from "../types/handwritten";
import { closeOpenModal } from "../lib/dialogManager";

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

  // (#1891) The entire nonzero-match branch of `qcountText` had exactly
  // zero coverage before this — only the zero-match "no match" case above
  // was ever exercised. These four pin the grammar, the cap disclosure,
  // and the server-truncation marker this branch has to carry.

  it("shows a singular match count with no plural 's' for exactly one match", () => {
    const records = [rec({ session_id: "s-alpha" }), rec({ session_id: "s-beta" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    expect(document.getElementById("qcount")?.textContent).toBe("1 match");
  });

  it("shows a plural match count for more than one match", () => {
    const records = [
      rec({ session_id: "s-alpha-1" }),
      rec({ session_id: "s-alpha-2" }),
      rec({ session_id: "s-beta" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    expect(document.getElementById("qcount")?.textContent).toBe("2 matches");
  });

  it("appends the LOG_CAP disclosure once the match count exceeds what's shown", () => {
    const records = Array.from({ length: 60 }, (_, i) =>
      rec({ ts: `2026-08-08T12:${String(i).padStart(2, "0")}:00.000Z`, session_id: `s-alpha-${i}` }),
    );
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter events…"), { target: { value: "s-alpha" } });
    expect(document.getElementById("qcount")?.textContent).toBe("60 matches · 50 shown");
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

  it("the modal's 'model only' quick action keeps reasoning/tool-call/turn rows and drops others", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-reasoning" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "machine.online", session_id: "s-machine" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    fireEvent.click(screen.getByText("model only"));
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-reasoning");
  });

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

  it("'clear all' restores every facet and empties the search text", () => {
    const records = [rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-1", tier: "local" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    fireEvent.click(screen.getByLabelText("local"));
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(0);
    fireEvent.click(screen.getByText("clear all"));
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(1);
  });

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
    rerenderB(
      <EventLogColumn
        scopeLabel="mission m1"
        paneId="b"
        records={[...records, rec({ ts: "2026-08-08T12:10:00.000Z", action: "dispatch.reasoning", session_id: "s-cloud-2", tier: "cloud" })]}
        visible={false}
      />,
    );

    const storedAfterB = JSON.parse(window.sessionStorage.getItem("dmux.eventfilters")!);
    expect(storedAfterB.q).toBe("s-cloud");
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
    expect(document.querySelector('[data-act="rec"]')!.textContent).toContain("s-late");
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

  // Structural coverage for the two classNames that shipped matching
  // nothing in styles.css (rendered as default sans-serif text, invisible
  // to the innerText-based parity goldens).
  it("renders the machine/session meta spans with their styling classes", () => {
    const records = [rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-1", machine_id: "MacBook-Pro" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    expect(document.querySelector(".eventlog__recmachine")).toBeInTheDocument();
    expect(document.querySelector(".eventlog__recsession")).toBeInTheDocument();
    expect(document.querySelector(".eventlog__recmachine")!.textContent).toContain("MacBook-Pro");
    expect(document.querySelector(".eventlog__recsession")!.textContent).toContain("s-1");
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

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClientProvider, QueryClient } from "@tanstack/react-query";
import { FleetLens } from "./FleetLens";
import { todayUTC, prevDateUTC } from "../../lib/flow";
import { closeOpenModal } from "../../lib/dialogManager";

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
  // See EventLogColumn.test.tsx's own comment: dialogManager's open/close
  // state is a module-level singleton that outlives `render()`/unmount.
  closeOpenModal({ restore: false });
});

function renderFleetLens() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <FleetLens />
    </QueryClientProvider>,
  );
}

/** Routes `fetchJson` calls the same way the real daemon's endpoint set
 * does, keyed on the URL. `flowToday`/`flowYesterday` default to an empty
 * window so a test only has to name the records it actually cares about. */
function mockFleetFetch(opts: {
  flowToday?: unknown[];
  flowYesterday?: unknown[];
  machines?: unknown[];
  /** (#1809) This daemon's OWN `/machine/specs` — the confirmed-local
   * signal `localMachineUid` resolves against. Omitted (the default) keeps
   * the pre-existing 404 (no daemon has confirmed ANY card as local), so
   * every pre-#1809 test in this file is unaffected by this field's
   * addition. Set it to make ONE uid resolve as local — see the two
   * locality-split tests below for why that distinction now matters. */
  specs?: unknown;
} = {}) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const path = String(url);
      if (path === `/flow/${today}`) return Promise.resolve(new Response(JSON.stringify(opts.flowToday ?? []), { status: 200 }));
      if (path === `/flow/${yesterday}`) return Promise.resolve(new Response(JSON.stringify(opts.flowYesterday ?? []), { status: 200 }));
      if (path === "/fleet/machines/live") {
        return Promise.resolve(
          new Response(
            JSON.stringify({
              machines: opts.machines ?? [],
              meta: { sources: { fleet: { state: "off" } }, complete: true },
            }),
            { status: 200 },
          ),
        );
      }
      if (path === "/fleet/sessions/live") {
        return Promise.resolve(
          new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }),
        );
      }
      if (path === "/machine/specs") {
        if (opts.specs === undefined) return Promise.resolve(new Response("{}", { status: 404 }));
        return Promise.resolve(new Response(JSON.stringify(opts.specs), { status: 200 }));
      }
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
}

describe("FleetLens", () => {
  it("always renders the hero, even at zero — never hides it while there's no data yet", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    // Two "0" values (local + cloud tokens) render rather than the card
    // disappearing — the "hides late, pops in" defect this port guards
    // against (see `SavingsHero`'s own doc).
    expect(screen.getByText("local tokens")).toBeInTheDocument();
    expect(screen.getByText("cloud tokens")).toBeInTheDocument();
  });

  it("sums a locally-run session's telemetry into local tokens, and renders its machine card", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s1",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 500, completion_tokens: 100, total_tokens: 600 },
        },
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(screen.getByText("600")).toBeInTheDocument()); // fmtN(600) = "600" local tokens
    // Renders twice: the machine card AND the activity-timeline lane label.
    expect(screen.getAllByText("MacBook-Pro").length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("idle")).toBeInTheDocument(); // no live session -> idle, not "dispatch in flight"
  });

  it("renders a real 'history →' link (#1640) when notes history exists, and it opens the notes dialog", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T09:00:00.000Z`, action: "note", source: "orchestrator", handle: "shipped the thing" },
        { ts: `${today}T10:00:00.000Z`, action: "note", source: "orchestrator", handle: "shipped another thing" },
      ],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(screen.getByText(/shipped another thing/)).toBeInTheDocument());
    const link = container.querySelector('[data-act="notes"]');
    expect(link).toBeInTheDocument();
    expect(link!.textContent).toMatch(/history/i);

    expect(document.getElementById("nmodalbg")!.style.display).toBe("none");
    fireEvent.click(link!);
    expect(document.getElementById("nmodalbg")!.style.display).toBe("flex");
    // Both notes render, newest first (`openNotes()`'s `.reverse()`).
    const rows = document.querySelectorAll(".dialog__nrow");
    expect(rows.length).toBe(2);
    expect(rows[0].textContent).toContain("shipped another thing");
    expect(rows[1].textContent).toContain("shipped the thing");
  });

  it("no history link when there are no orchestrator notes", async () => {
    mockFleetFetch({});
    const { container } = renderFleetLens();
    await waitFor(() => expect(screen.getByText(/going hybrid takes nerve/i)).toBeInTheDocument());
    expect(container.querySelector('[data-act="notes"]')).not.toBeInTheDocument();
  });

  it("honesty about incomplete data: a session with no dispatch bookend renders as 'unattributed', not silently local", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s-orphan",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 },
        },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(screen.getByText("unattributed")).toBeInTheDocument());
    expect(screen.getByText("1,000")).toBeInTheDocument(); // the unattributed figure
    // Local tokens must NOT have absorbed it — it's excluded, not credited.
    const localBlock = screen.getByText("local tokens").previousSibling;
    expect(localBlock?.textContent).toBe("0");
  });

  // (drill-in packet, split by locality in #1809) The fleet-card click —
  // `data-act="machine" data-arg` in legacy (`ACTIONS.machine`,
  // `drillMachine(uid)`) — was previously a plain, non-interactive `<div>`,
  // and every card drilled to the SAME destination. #1809 splits that
  // destination by locality (see `FleetLens.tsx`'s own `machineDrillHash`
  // doc): the LOCAL card (confirmed against this daemon's OWN
  // `/machine/specs`) still reaches the residency room; anything this
  // daemon can't confirm as itself — including, but not limited to, a
  // genuinely remote machine — goes to the runs lens instead, pinned to
  // that machine. `MachineLens.test.tsx` covers what the residency-room
  // destination does with a REMOTE uid reached by a DIRECT deep-link
  // (still a real, supported route — see that file's own doc); these two
  // tests below are the LOCAL half. The REMOTE half — what a click on a
  // card this daemon can't confirm as itself actually does — is its own
  // test further down, the inverted case.
  it("clicking the LOCAL machine card navigates to the residency room via a real hash write", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      // This daemon's own /machine/specs identifies it AS the u1 machine —
      // the confirmed-local signal `localMachineUid` resolves against.
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    // "MacBook-Pro" renders TWICE — the machine card AND the activity-
    // timeline lane label (see the earlier test's own comment) — find the
    // CARD specifically via its ancestor class, not the singular query.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("MacBook-Pro");
    expect(card).toHaveAttribute("role", "button");
    fireEvent.click(card);
    expect(window.location.hash).toBe("#lens=machine&uid=u1");
  });

  it("Enter/Space also activates the LOCAL fleet-card drill-in (keyboard parity with the click)", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    // "MacBook-Pro" renders TWICE — the machine card AND the activity-
    // timeline lane label (see the earlier test's own comment) — find the
    // CARD specifically via its ancestor class, not the singular query.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("MacBook-Pro");
    fireEvent.keyDown(card, { key: "Enter" });
    expect(window.location.hash).toBe("#lens=machine&uid=u1");
  });

  // (#1809, merge-gate fix) The runs lens is for a machine POSITIVELY known
  // to be remote — nothing else. An earlier cut sent every unconfirmed card
  // there, which measured wrong on the first paint: `localUid` is null until
  // `/machine/specs` resolves, so the LOCAL card was clickable-and-wrong for
  // one frame (+0ms → runs, +100ms → machine). These two tests pin both
  // sides of the corrected rule, and the second is what stops the first from
  // being vacuous — without a confirmed-remote case, `machineDrillHash`
  // could always return the machine hash and every other test here would
  // still pass.
  it("an UNCONFIRMED card goes to the residency room — the destination that admits it is guessing", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      // No `specs` — locality unresolved, exactly the first-paint state.
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    fireEvent.click(document.querySelector(".mach")!);
    // The room names its own limitation for a remote machine and
    // self-corrects once specs land; the runs lens would have answered a
    // question the operator did not ask, confidently and unlabeled.
    expect(window.location.hash).toBe("#lens=machine&uid=u1");
  });

  it("a CONFIRMED-REMOTE card goes to the runs lens, pinned to that machine", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:01.000Z`, machine_uid: "u2", machine_id: "studio", session_id: "s2", action: "dispatch.start", handle: "coder" },
      ],
      // specs names THIS daemon as MacBook-Pro, so u2 is positively remote.
      specs: { machine_id: "MacBook-Pro" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelectorAll(".mach").length).toBe(2));
    const studio = [...document.querySelectorAll(".mach")].find((c) => c.textContent?.includes("studio"))!;
    fireEvent.click(studio);
    expect(window.location.hash).toBe("#lens=runs&machine=u2");
  });

  it("the savings hero renders tokens-only — no currency symbol or rate figure, even with non-zero savings (#803 regression coverage, restored post-#1806)", async () => {
    // Legacy's equivalent coverage
    // (`savings_hero_breakdown_is_classed_and_currency_free`, a source-text
    // scan of `viewer.html`'s `hybridNote`..`renderFleet` region) retired
    // with that file. This exercises the RENDERED hero instead — a fixture
    // with real, non-zero local tokens, so the assertion isn't vacuously
    // true against an empty "0" hero.
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s1",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 500, completion_tokens: 100, total_tokens: 600 },
        },
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
      ],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(screen.getByText("600")).toBeInTheDocument());
    const hero = container.querySelector(".savings");
    expect(hero).not.toBeNull();
    expect(hero!.textContent).not.toMatch(/[$€£]|USD|per million|\/M\b/);
  });

  // (#1869) The playback transport (`Scrubber`, rewind/play/speed/range)
  // only ever mounts inside `PlaybackLens`, which composes THIS component
  // rather than the other way around — `FleetLens` itself never renders it,
  // on any route. This is the live (default, no-hash) route's own coverage
  // of that: no `records`/`tMax`/`tMin`/`historical` props at all, the same
  // call every OTHER test in this file already makes.
  it("never renders a playback transport on the live route — the scrubber is PlaybackLens-only", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    expect(document.querySelector(".scrub")).toBeNull();
    expect(screen.queryByRole("slider")).not.toBeInTheDocument();
  });

  // (#1869, QA gate — caught against a real daemon, not by any prior test)
  // `tMax` (the day's FIXED ceiling, used for the activity axis span) and
  // `playhead` (the scrub position) are two separate props now. A first
  // cut passed only one number for both, which was invisible in every unit
  // test above because none of them separate the two — this is the
  // integration-level regression test that does. Scrubbing all the way
  // back to `tMin` must NOT collapse the "ACTIVITY" axis header down to a
  // single repeated instant; the day's whole recorded span stays on
  // screen, with only the playhead marker (and the token hero, and the bar
  // classes) moving.
  it("a scrubbed playhead moves the hero and the bars, but the activity axis stays the day's whole fixed span", async () => {
    const today = todayUTC();
    const dayTMin = Date.parse(`${today}T10:00:00.000Z`);
    const dayTMax = Date.parse(`${today}T12:00:00.000Z`);
    const records = [
      { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: `${today}T12:00:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
    ];

    const { rerender } = render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={dayTMax} tMin={dayTMin} playhead={dayTMax} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".fleettl")).not.toBeNull());
    // Read the un-scrubbed axis text off the DOM (rather than asserting a
    // literal) — `clkhm`/`clkrange` render in the runner's local timezone,
    // so the only portable assertion is "unchanged after scrubbing", below.
    const axisBefore = document.querySelector(".tlhdr span")!.textContent;
    expect(axisBefore).toBeTruthy();

    rerender(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={dayTMax} tMin={dayTMin} playhead={dayTMin} historical />
      </QueryClientProvider>,
    );

    // The axis header is BYTE-IDENTICAL before and after scrubbing to tMin —
    // the bug this test guards collapsed it to a single repeated instant.
    expect(document.querySelector(".tlhdr span")!.textContent).toBe(axisBefore);
    // The playhead marker DID move, to the left edge of that fixed axis.
    expect((document.querySelector(".ph") as HTMLElement).style.left).toBe("0%");
    // The hero moved too — the completion is no longer visible at tMin.
    expect(screen.getByText("local tokens").previousSibling?.textContent).toBe("0");
  });
});

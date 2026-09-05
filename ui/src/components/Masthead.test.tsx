import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Masthead } from "./Masthead";
import type { Route } from "../lib/route";
import { closeOpenModal, getOpenId } from "../lib/dialogManager";

function renderMasthead(route: Route, liveStatus: "live" | "reconnecting" = "live", replayDate: string | null = null) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <Masthead route={route} liveStatus={liveStatus} replayDate={replayDate} />
    </QueryClientProvider>,
  );
}

function clearInjectedMetas() {
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
}

afterEach(() => {
  clearInjectedMetas();
  // See EventLogColumn.test.tsx's own comment on why this is required:
  // dialogManager's open/close state outlives `render()`/unmount.
  closeOpenModal({ restore: false });
});

describe("Masthead", () => {
  it("renders the darkmux brand", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    expect(screen.getByText(/darkmux/)).toBeInTheDocument();
    expect(screen.getByText("darkmux")).toBeInTheDocument();
    vi.unstubAllGlobals();
  });

  it("renders the four topnav links", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    for (const label of ["home", "guide", "articles", "github"]) {
      expect(screen.getByRole("link", { name: label })).toBeInTheDocument();
    }
    vi.unstubAllGlobals();
  });

  it("#verbadge is empty when no darkmux-version meta is injected (every test harness in this repo — see the component's own doc)", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    expect(document.getElementById("verbadge")?.textContent).toBe("");
    vi.unstubAllGlobals();
  });

  it("(#2107) #verbadge renders ONLY the ⓘ affordance when the metas ARE present (a real daemon) — the inline text moved to the machine drawer's header", () => {
    const meta1 = document.createElement("meta");
    meta1.name = "darkmux-version";
    meta1.content = "2.7.0 (abc1234)";
    document.head.appendChild(meta1);
    const meta2 = document.createElement("meta");
    meta2.name = "darkmux-flow-schema";
    meta2.content = "1.16";
    document.head.appendChild(meta2);

    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    expect(document.getElementById("verbadge")?.textContent).toBe("ⓘ");
    // The full detail survives as a hover tooltip, not lost.
    expect(document.getElementById("verbadge")?.getAttribute("title")).toBe("darkmux 2.7.0 (abc1234) · flow schema 1.16 — about");
    vi.unstubAllGlobals();
  });

  it("shows the refresh control on a live route when the stream has dropped", () => {
    // Was "on a live route" unconditionally. The control now appears only
    // while the stream is NOT live — beside a pill reading `● LIVE` it
    // contradicts itself, and there is nothing to refresh.
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" }, "reconnecting");
    expect(screen.getByTitle("Refetch now")).toBeInTheDocument();
    vi.unstubAllGlobals();
  });

  it("hides the refresh control on a replay route (nothing live to refetch)", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "dispatch", dispatchId: "abc-123" });
    expect(screen.queryByTitle("Refetch now")).not.toBeInTheDocument();
    vi.unstubAllGlobals();
  });

  it("renders the catalog toggle (moved in from App.tsx — the existing, tested CatalogPanel)", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    expect(screen.getByRole("button", { name: /browse history/i })).toBeInTheDocument();
    vi.unstubAllGlobals();
  });

  // The parity goldens CANNOT cover this: the static harness has no working
  // stream, so its badge is permanently `reconnecting` and the button always
  // shows there. This is the state the goldens can never reach.
  it("hides the refresh control while the stream is live — it contradicts the badge", () => {
    const { container } = renderMasthead({ kind: "fleet" } as Route, "live");
    expect(container.querySelector(".masthead__refresh")).toBeNull();
  });

  it("shows it again when the stream drops, where a manual retry actually helps", () => {
    const { container } = renderMasthead({ kind: "fleet" } as Route, "reconnecting");
    expect(container.querySelector(".masthead__refresh")).toBeTruthy();
  });
});

describe("Masthead — about modal (#1640)", () => {
  function injectVersionMetas() {
    const meta1 = document.createElement("meta");
    meta1.name = "darkmux-version";
    meta1.content = "2.7.0 (abc1234)";
    document.head.appendChild(meta1);
    const meta2 = document.createElement("meta");
    meta2.name = "darkmux-flow-schema";
    meta2.content = "1.16";
    document.head.appendChild(meta2);
  }

  it("#verbadge is a real data-act=\"about\" button when it has content, and fires openModalEl(\"imodalbg\")", () => {
    // (#2107 "one modal" packet) This dialog's CONTENT (build/schema/
    // connection/mode/machine/hardware/links) moved to
    // `MachineDrawer.test.tsx` — `AboutDialog`, the sole former renderer of
    // `#imodalbg`, is retired, and Masthead alone (no `<MachineDrawer>`
    // mounted in this file's render tree) has nothing left to open. This
    // test keeps only what is genuinely THIS component's own job: the
    // button exists with the right affordances and calls the shared
    // trigger — `getOpenId()` is dialogManager's own state, provable
    // without needing a dialog mounted to observe it.
    injectVersionMetas();
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    const verbadge = document.getElementById("verbadge")!;
    expect(verbadge.tagName).toBe("BUTTON");
    expect(verbadge.getAttribute("data-act")).toBe("about");

    expect(getOpenId()).toBeNull();
    fireEvent.click(verbadge);
    expect(getOpenId()).toBe("imodalbg");
    vi.unstubAllGlobals();
  });

  it("#verbadge stays a non-interactive span (no about trigger) when empty — matching legacy's if(vb&&verMeta) gate", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    const verbadge = document.getElementById("verbadge")!;
    expect(verbadge.tagName).toBe("SPAN");
    expect(verbadge.getAttribute("data-act")).toBeNull();
    vi.unstubAllGlobals();
  });
});

/**
 * (#1801) On a static build (`darkmux-flow-src` injected), the source/date
 * badge must NOT become the catalog/history trigger — `CatalogPanel` fetches
 * `/flow-days`/`/flow-missions`, endpoints the static demo doesn't ship
 * fixtures for (out of scope per #1801's brief), so mounting the real
 * button there would 404 on click. Mirrors legacy's own gate:
 * `if(!flowSrc && mode!=="no-daemon"){ sb.dataset.act="catalog"; ... }`
 * (viewer.html:3936).
 */
describe("Masthead — static-build badge suppression (#1801)", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }

  it("(#2072) on a static build the badge names the replayed day on EVERY route, not TODAY/REPLAY per tab", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    injectMeta("darkmux-flow-date", "2026-08-26");
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    // The playback route is the demo's LANDING route: its own date resolves
    // only after the flow file loads, so it must read the meta too or it
    // flashes TODAY while every other tab already shows the day.
    for (const route of [{ kind: "playback", date: null }, { kind: "runs", runsKind: "all", run: null, machine: null }, { kind: "fleet" }, { kind: "mission", missionId: "m1", stepId: null }] as const) {
      const { container, unmount } = renderMasthead(route as never);
      const badge = container.querySelector(".masthead__srcbadge");
      expect(badge?.textContent).toBe("2026-08-26");
      unmount();
    }
    vi.unstubAllGlobals();
  });

  it("a daemon dispatch page reads LIVE (the live word) until the shell knows its day, then names that day — the pill is the ONE indicator, never a separate badge (#2412)", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const unknown = renderMasthead({ kind: "dispatch", dispatchId: "s1" } as never);
    // (header owns liveness, 2026-09-03) Day unknown ⇒ the subject is still
    // running ⇒ this is a live page ⇒ the pill's own dot carries it.
    expect(unknown.container.querySelector(".catalog-toggle")?.textContent).toContain("LIVE");
    expect(unknown.container.querySelector("#modebadge")).toBeNull();
    unknown.unmount();
    // A closed mission names ITSELF (operator, in #2412: "pill shows the
    // mission id"), not the day it happened to close on.
    const known = renderMasthead({ kind: "mission", missionId: "m1", stepId: null } as never, "live", "2026-08-07");
    expect(known.container.querySelector(".catalog-toggle")?.textContent).toBe("▣ m1");
    expect(known.container.querySelector("#modebadge")).toBeNull();
    vi.unstubAllGlobals();
  });

  it("a daemon playback of today's date still NAMES the day, never LIVE — that word belongs to the live view", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const today = new Date().toISOString().slice(0, 10);
    const { container } = renderMasthead({ kind: "playback", date: today } as never);
    expect(container.querySelector(".catalog-toggle")?.textContent).toBe(`▣ ${today}`);
    vi.unstubAllGlobals();
  });

  it("renders plain text, not the catalog-toggle button, when darkmux-flow-src is injected", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    // `route` here is whatever `App.tsx`'s already-resolved `displayRoute`
    // would be in production (see that component's own doc — the raw
    // `date: null` never reaches `Masthead` directly); `{kind:"fleet"}`
    // keeps this test focused on the badge-suppression gate itself, not on
    // date resolution, which is a separate concern this file doesn't own.
    const { container } = renderMasthead({ kind: "fleet" });
    expect(screen.queryByRole("button", { name: /browse history/i })).not.toBeInTheDocument();
    // The same VISIBLE text a live page would show still appears — this is
    // a suppression of the AFFORDANCE, not the text.
    const badge = container.querySelector(".masthead__srcbadge");
    expect(badge).toBeTruthy();
    expect(badge?.textContent).toBe("LIVE");
    vi.unstubAllGlobals();
  });

  // Inverted case: the exact same route shape, without the meta, must keep
  // rendering the real interactive toggle — matching the existing "renders
  // the catalog toggle" test above, restated here so the gate is proven
  // two-sided rather than inferred from that other test's unrelated route.
  it("without the meta, still renders the real interactive catalog toggle", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const { container } = renderMasthead({ kind: "fleet" });
    expect(screen.getByRole("button", { name: /browse history/i })).toBeInTheDocument();
    expect(container.querySelector(".masthead__srcbadge")).toBeNull();
    vi.unstubAllGlobals();
  });
});
// (header owns liveness, operator 2026-09-03) Liveness is GLOBAL chrome: the
// pill's own dot carries it on every daemon-backed route, the mission and
// dispatch pages included — no lens paints its own, and there is no second
// element (#2412 retired the separate `#modebadge`).
describe("live pill on every daemon-backed route (#2412)", () => {
  it("renders on the mission route", () => {
    const { container } = renderMasthead({ kind: "mission", missionId: "m1", stepId: null } as Route, "live");
    expect(container.querySelector(".catalog-toggle")?.textContent).toMatch(/live/i);
    expect(container.querySelector("#modebadge")).toBeNull();
  });
  it("renders on the dispatch route, and reflects a dropped stream", () => {
    const { container } = renderMasthead({ kind: "dispatch", dispatchId: "d1" } as Route, "reconnecting");
    expect(container.querySelector(".catalog-toggle")?.textContent).toMatch(/live/i);
    expect(container.querySelector("#modebadge")).toBeNull();
  });
});

/**
 * (#2412) Test-first: the failing-for-the-predicted-reason cases below were
 * run RED against the pre-#2412 tree (a `<LiveStatusBadge>` still mounted,
 * `pillLabel` not yet written) before the implementation landed — the
 * `.masthead__pilldot`/`aria-live` assertions had nothing to find, and
 * `.toContain("LIVE")` failed against the old literal "TODAY". Five states
 * named in the issue: live+connected, live+reconnecting, mission replay,
 * date replay, and the old badge's total absence in every one of them.
 */
describe("Masthead — the pill is the ONE transport control (#2412)", () => {
  it("(a) live + connected: pill reads LIVE, the dot carries the pulse class", () => {
    const { container } = renderMasthead({ kind: "fleet" } as Route, "live");
    const toggle = container.querySelector(".catalog-toggle")!;
    expect(toggle.textContent).toContain("LIVE");
    const dot = toggle.querySelector(".masthead__pilldot")!;
    expect(dot.className).toContain("live");
    expect(dot.className).not.toContain("stale");
    expect(dot.getAttribute("title")).toBeNull();
  });

  it("(b) live + reconnecting: the dot carries the reconnecting class, no pulse class, and announces via aria-live", () => {
    const { container } = renderMasthead({ kind: "fleet" } as Route, "reconnecting");
    const toggle = container.querySelector(".catalog-toggle")!;
    expect(toggle.textContent).toContain("LIVE");
    const dot = toggle.querySelector(".masthead__pilldot")!;
    expect(dot.className).toContain("stale");
    expect(dot.className).not.toContain(" live");
    expect(dot.getAttribute("title")).toBe("reconnecting");
    const live = toggle.querySelector('[aria-live="polite"]')!;
    expect(live.textContent).toBe("reconnecting");
  });

  it("(c) mission replay: the pill shows the mission id and the replay glyph, no dot", () => {
    const { container } = renderMasthead(
      { kind: "mission", missionId: "review-1785400940-136e76", stepId: null } as Route,
      "live",
      "2026-08-07",
    );
    const toggle = container.querySelector(".catalog-toggle")!;
    expect(toggle.textContent).toBe("▣ review-1785400940-136e76");
    expect(toggle.querySelector(".masthead__pilldot.live")).toBeNull();
    expect(toggle.querySelector(".masthead__pilldot.stale")).toBeNull();
    expect(toggle.querySelector(".masthead__pilldot--replay")?.textContent).toBe("▣");
  });

  it("(d) date replay: the pill shows the date and the replay glyph", () => {
    const { container } = renderMasthead({ kind: "playback", date: "2026-08-07" } as Route);
    const toggle = container.querySelector(".catalog-toggle")!;
    expect(toggle.textContent).toBe("▣ 2026-08-07");
    expect(toggle.querySelector(".masthead__pilldot--replay")?.textContent).toBe("▣");
  });

  it("(e) no element with the old badge's id exists, in any of the four states above", () => {
    for (const [route, liveStatus, replayDate] of [
      [{ kind: "fleet" }, "live", null],
      [{ kind: "fleet" }, "reconnecting", null],
      [{ kind: "mission", missionId: "m1", stepId: null }, "live", "2026-08-07"],
      [{ kind: "playback", date: "2026-08-07" }, "live", null],
    ] as const) {
      const { container, unmount } = renderMasthead(route as Route, liveStatus, replayDate);
      expect(container.querySelector("#modebadge")).toBeNull();
      expect(container.querySelector(".pb")).toBeNull();
      unmount();
    }
  });
});


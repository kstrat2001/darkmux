import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Masthead } from "./Masthead";
import type { Route } from "../lib/route";
import { closeOpenModal } from "../lib/dialogManager";
import type { MachineSpecs } from "../types/handwritten";

function renderMasthead(route: Route, liveStatus: "live" | "reconnecting" = "live", specs: MachineSpecs | null = null) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <Masthead route={route} liveStatus={liveStatus} specs={specs} />
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

  it("#verbadge renders the version + schema + info affordance when the metas ARE present (a real daemon)", () => {
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
    expect(document.getElementById("verbadge")?.textContent).toBe("v2.7.0 (abc1234) · schema 1.16  ⓘ");
    vi.unstubAllGlobals();
  });

  it("shows the refresh control on a live route when the stream has dropped", () => {
    // Was "on a live route" unconditionally. The control now appears only
    // while the stream is NOT live — beside a `● LIVE` badge it contradicts
    // itself, and there is nothing to refresh.
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

  it("#verbadge is a real data-act=\"about\" button when it has content, and opens #imodalbg", () => {
    injectVersionMetas();
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
    const verbadge = document.getElementById("verbadge")!;
    expect(verbadge.tagName).toBe("BUTTON");
    expect(verbadge.getAttribute("data-act")).toBe("about");

    expect(document.getElementById("imodalbg")!.style.display).toBe("none");
    fireEvent.click(verbadge);
    expect(document.getElementById("imodalbg")!.style.display).toBe("flex");
    expect(screen.getByText("about · darkmux")).toBeInTheDocument();
    expect(screen.getByText("2.7.0 (abc1234)")).toBeInTheDocument();
    expect(screen.getByText("1.16")).toBeInTheDocument();
    // The four external links legacy's modal footer carries — scoped to the
    // dialog body since the masthead's OWN topnav also has links with these
    // same accessible names.
    const dialogLinks = within(document.getElementById("infobody")!);
    for (const label of ["github", "guide", "articles", "home"]) {
      expect(dialogLinks.getByRole("link", { name: label })).toBeInTheDocument();
    }
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

  it("shows the machine/hardware rows on a live route when specs are available, and omits them on a replay", () => {
    injectVersionMetas();
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const specs = {
      darkmux_version: "2.7.0",
      flow_schema_version: "1.16",
      machine_id: "MacBook-Pro",
      os: "macOS",
      ram_total_bytes: 137438953472, // 128 GiB
      ram_free_for_ai_bytes: null,
      cpu_brand: "Apple M5 Max",
      loaded_models: [],
      lms_unreachable: false,
      utility_model: null,
      redis_url_redacted: null,
      generated_at_ms: 0,
    };
    renderMasthead({ kind: "fleet" }, "live", specs);
    fireEvent.click(document.getElementById("verbadge")!);
    expect(screen.getByText("MacBook-Pro")).toBeInTheDocument();
    expect(screen.getByText(/Apple M5 Max/)).toBeInTheDocument();
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
    for (const route of [{ kind: "playback", date: null }, { kind: "runs", runsKind: "all", run: null, machine: null }, { kind: "fleet" }, { kind: "mission", missionId: "m1" }] as const) {
      const { container, unmount } = renderMasthead(route as never);
      const badge = container.querySelector(".masthead__srcbadge");
      expect(badge?.textContent).toBe("FLOW · 2026-08-26");
      unmount();
    }
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
    // The same VISIBLE text a live page would show for "today" still
    // appears — this is a suppression of the AFFORDANCE, not the text.
    const badge = container.querySelector(".masthead__srcbadge");
    expect(badge).toBeTruthy();
    expect(badge?.textContent).toBe("TODAY");
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

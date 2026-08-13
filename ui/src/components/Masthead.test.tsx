import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Masthead } from "./Masthead";
import type { Route } from "../lib/route";

function renderMasthead(route: Route, liveStatus: "live" | "reconnecting" = "live") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <Masthead route={route} liveStatus={liveStatus} />
    </QueryClientProvider>,
  );
}

function clearInjectedMetas() {
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
}

afterEach(() => {
  clearInjectedMetas();
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
    renderMasthead({ kind: "session", sessionId: "abc-123" });
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

import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Masthead } from "./Masthead";
import type { Route } from "../lib/route";

function renderMasthead(route: Route) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <Masthead route={route} liveStatus="live" />
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

  it("shows the refresh control on a live route", () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderMasthead({ kind: "fleet" });
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
});

import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { FleetLens } from "./FleetLens";

// (#1729) The default view is entirely presence-derived: machine cards, the
// "N running" counts, the timeline's run colouring. When presence cannot be
// read, none of it goes blank — it renders confidently WRONG. This pins the
// marker that says so.
//
// It also guards a regression that already happened once: the marker existed
// on `FleetStrip`, and vanished when this lens replaced it on the route.
// Nothing went red, because FleetStrip's own tests kept passing while it
// stopped being mounted. These assertions live with the MOUNTED component.

function stub(fleetState: Record<string, unknown> | null) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (String(url).includes("/fleet/machines/live")) {
        return Promise.resolve(
          new Response(
            JSON.stringify({
              machines: [],
              meta: fleetState ? { sources: { fleet: fleetState }, complete: fleetState.state === "ok" || fleetState.state === "off" } : undefined,
            }),
            { status: 200 },
          ),
        );
      }
      return Promise.resolve(new Response("[]", { status: 200 }));
    }),
  );
}

function renderLens() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <FleetLens />
    </QueryClientProvider>,
  );
}

afterEach(() => vi.unstubAllGlobals());

describe("FleetLens — presence coverage marker", () => {
  it("says so when presence could not be read, instead of showing a confident empty fleet", async () => {
    stub({ state: "unavailable", detail: "could not read presence beats from Redis" });
    const { container } = renderLens();
    await waitFor(() => expect(container.querySelector('.fleetcov[data-state="unavailable"]')).toBeTruthy());
    expect(screen.getByText(/could not be read/i)).toBeInTheDocument();
  });

  it("reports staleness with the snapshot's age", async () => {
    stub({ state: "stale", age_ms: 41200, detail: "x" });
    const { container } = renderLens();
    await waitFor(() => expect(container.querySelector('.fleetcov[data-state="stale"]')).toBeTruthy());
    expect(screen.getByText(/41s old/)).toBeInTheDocument();
  });

  it("stays SILENT on a standalone machine — the inverted case", async () => {
    // `off` is a correct single-machine install. A permanent warning here
    // would be the bug, not the fix.
    stub({ state: "off" });
    const { container } = renderLens();
    await waitFor(() => expect(container.querySelector(".fleet-lens")).toBeTruthy());
    expect(container.querySelector(".fleetcov")).toBeNull();
  });

  it("stays silent on a healthy fleet", async () => {
    stub({ state: "ok" });
    const { container } = renderLens();
    await waitFor(() => expect(container.querySelector(".fleet-lens")).toBeTruthy());
    expect(container.querySelector(".fleetcov")).toBeNull();
  });
});

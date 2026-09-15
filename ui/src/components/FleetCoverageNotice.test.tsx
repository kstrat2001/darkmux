import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { FleetCoverageNotice } from "./FleetCoverageNotice";

// (#1729, moved here from `lenses/fleet/FleetCoverage.test.tsx` by #2683 when
// the component moved out of the fleet lens.) Presence is what machine cards,
// "N running" counts and the masthead headline are all derived from. When it
// cannot be read, none of those go blank — they render confidently WRONG.
// This pins the marker that says so.
//
// It also guards a regression that already happened once: the marker existed
// on `FleetStrip`, and vanished when `FleetLens` replaced it on the route.
// Nothing went red, because FleetStrip's own tests kept passing while it
// stopped being mounted. The component's own assertions live here; that it is
// still MOUNTED is `App.test.tsx`'s job (`presence coverage`). (#2725 deleted
// `FleetStrip` outright, along with the third copy of this wording it still
// carried.)

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

function renderNotice(props: { historical?: boolean } = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <FleetCoverageNotice {...props} />
    </QueryClientProvider>,
  );
}

afterEach(() => vi.unstubAllGlobals());

describe("FleetCoverageNotice — presence coverage marker", () => {
  it("says so when presence could not be read, instead of showing a confident empty fleet", async () => {
    stub({ state: "unavailable", detail: "could not read presence beats from Redis" });
    const { container } = renderNotice();
    await waitFor(() => expect(container.querySelector('.fleetcov[data-state="unavailable"]')).toBeTruthy());
    expect(screen.getByText(/could not be read/i)).toBeInTheDocument();
  });

  it("reports staleness with the snapshot's age", async () => {
    stub({ state: "stale", age_ms: 41200, detail: "x" });
    const { container } = renderNotice();
    await waitFor(() => expect(container.querySelector('.fleetcov[data-state="stale"]')).toBeTruthy());
    expect(screen.getByText(/41s old/)).toBeInTheDocument();
  });

  it("stays SILENT on a standalone machine — the inverted case", async () => {
    // `off` is a correct single-machine install. A permanent warning here
    // would be the bug, not the fix.
    stub({ state: "off" });
    const { container } = renderNotice();
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector(".fleetcov")).toBeNull();
  });

  it("stays silent on a healthy fleet", async () => {
    stub({ state: "ok" });
    const { container } = renderNotice();
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector(".fleetcov")).toBeNull();
  });

  it("says so when the presence READ itself failed — a daemon death mid-session (#2683)", async () => {
    // The shape the issue named, and the one with no coverage before this:
    // the daemon stops answering entirely. `fetchJson` returns a
    // discriminated result rather than throwing, so this is a SUCCESSFUL
    // query carrying `ok:false` — `useLiveMachines` hands its consumers an
    // empty map, which is byte-identical to "the fleet is genuinely empty".
    // The notice is what tells those two apart.
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response("nope", { status: 503, statusText: "Service Unavailable" }))),
    );
    const { container } = renderNotice();
    await waitFor(() => expect(container.querySelector('.fleetcov[data-state="unavailable"]')).toBeTruthy());
    expect(screen.getByText(/could not be read/i)).toBeInTheDocument();
  });

  it("makes NO claim while the first read is still in flight — the inverted case", async () => {
    // A pending query is not a failed one. Warning during the first poll of
    // every page load would be the "fires on a healthy system" failure this
    // whole change exists to avoid.
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise(() => {})),
    );
    const { container } = renderNotice();
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector(".fleetcov")).toBeNull();
  });

  it("stays silent on a replay, whatever coverage says about NOW", async () => {
    stub({ state: "unavailable", detail: "x" });
    const { container } = renderNotice({ historical: true });
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector(".fleetcov")).toBeNull();
  });
});

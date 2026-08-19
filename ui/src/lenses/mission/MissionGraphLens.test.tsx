import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MissionGraphLens } from "./MissionGraphLens";
import { todayUTC } from "../../lib/flow";
import type { MissionGraph } from "./graph";

function renderLens(missionId = "m1") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MissionGraphLens missionId={missionId} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
});

const GRAPH: MissionGraph = {
  mission_id: "m1",
  mission_status: "finalized",
  nodes: [
    { id: "p1", label: "Investigate", kind: "phase", status: "complete", depth: 0 },
    {
      id: "a",
      label: "bundle",
      kind: "task",
      status: "complete",
      parentId: "p1",
      depth: 0,
      steps: [{ id: "a-step", label: "Shell", kind: "procedural.shell", status: "complete" }],
    },
  ],
  edges: [{ id: "e1", source: "p1", target: "a", kind: "contains" }],
};

function mockFetch(opts: { graphStatus?: number; graph?: MissionGraph } = {}) {
  const graph = opts.graph ?? GRAPH;
  const graphStatus = opts.graphStatus ?? 200;
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url.includes("/graph.json")) {
        return Promise.resolve(
          graphStatus === 200
            ? new Response(JSON.stringify(graph), { status: 200 })
            : new Response("not found", { status: graphStatus }),
        );
      }
      if (url.includes("/flow-mission/")) {
        return Promise.resolve(new Response(JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }), { status: 200 }));
      }
      if (url.startsWith(`/flow/${todayUTC()}`)) {
        return Promise.resolve(new Response(JSON.stringify([]), { status: 200 }));
      }
      return Promise.resolve(new Response("not found", { status: 404 }));
    }),
  );
}

describe("MissionGraphLens", () => {
  it("renders a loading state before the graph fetch resolves", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderLens();
    expect(screen.getByRole("status", { name: /loading mission m1/i })).toBeInTheDocument();
  });

  it("renders the mission id, status badge and phase/task/step DOM once the graph loads", async () => {
    mockFetch();
    renderLens();
    // `EventLogColumn`'s hidden `#logscope` ALSO carries "m1" (its
    // `scopeLabel`) — scope this to the header's `.midname` element so the
    // query stays unambiguous (matches the same discipline other lens
    // tests use around this shared component).
    await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("m1"));
    expect(screen.getByText("finalized")).toBeInTheDocument();
    expect(document.querySelector(".phasegroup")).not.toBeNull();
    expect(document.querySelector(".mnode.k-task.s-complete")).not.toBeNull();
    expect(document.querySelector(".steprow.s-complete")).not.toBeNull();
  });

  it("renders an honest 'no mission found' message on a 404, never a blank page", async () => {
    mockFetch({ graphStatus: 404 });
    renderLens();
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/ephemeral or cleared run/i);
  });

  it("renders the raw diagnostic on a non-404 error, distinct from the 404 branch", async () => {
    mockFetch({ graphStatus: 500 });
    renderLens();
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/darkmux mission graph:/i);
  });

  it("the view-mode toggle switches between the canvas and the mobile timeline", async () => {
    mockFetch();
    renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    fireEvent.click(screen.getByTitle("switch renderer"));
    await waitFor(() => expect(document.querySelector(".tlphase")).not.toBeNull());
    expect(document.querySelector(".phasegroup")).toBeNull();
  });

  it("the events toggle shows/hides the mission-scoped EventLogColumn", async () => {
    mockFetch();
    renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    const evbtn = screen.getByTitle("mission events");
    expect(document.querySelector(".eventlog")?.className).not.toMatch(/eventlog--hidden/);
    fireEvent.click(evbtn);
    expect(document.querySelector(".eventlog")?.className).toMatch(/eventlog--hidden/);
  });

  it("a daemon-less static build shows an honest 'needs a daemon' notice, never attempts a fetch", () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    renderLens();
    expect(screen.getByText(/needs a running daemon/i)).toBeInTheDocument();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

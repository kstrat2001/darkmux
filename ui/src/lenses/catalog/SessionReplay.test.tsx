// See `sessionRun.test.ts`'s identical note: `clk()` reads the process
// timezone, and the real-corpus test below asserts on a `clk()`-derived
// value — pin it before anything runs.
process.env.TZ = "UTC";

import { describe, it, expect, vi, afterEach } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SessionReplay } from "./SessionReplay";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "../../../..");

function renderReplay(sessionId: string) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <SessionReplay sessionId={sessionId} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("SessionReplay", () => {
  it("renders pending while the fetch is in flight", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderReplay("s1");
    expect(screen.getByRole("status", { name: /loading session s1/i })).toBeInTheDocument();
  });

  it("renders a visible error on a non-2xx response", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("boom", { status: 404, statusText: "err" }))));
    renderReplay("s1");
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/404/);
  });

  it("renders an honest empty state when the session has no records", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }), { status: 200 }))),
    );
    renderReplay("s1");
    await waitFor(() => expect(screen.getByText(/no records found for session s1/i)).toBeInTheDocument());
  });

  it("renders the real run view — header, brief, metrics, detections — against the recorded corpus fixture", async () => {
    const raw = JSON.parse(readFileSync(path.join(REPO_ROOT, "tests/parity/corpus/flow-session-task-list.json"), "utf8"));
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(raw), { status: 200 }))));
    renderReplay("task-list");
    await waitFor(() => expect(screen.getByText(/RUNNING/)).toBeInTheDocument());
    expect(screen.getByText(/FETCH-RENDER/)).toBeInTheDocument();
    expect(screen.getByText(/task-list on/)).toBeInTheDocument();
    expect(screen.getByText("LMStudio · local · this machine")).toBeInTheDocument();
    expect(screen.getByText(/07:36:48 · running/)).toBeInTheDocument();
    expect(screen.getByText("1071:54 so far")).toBeInTheDocument();
    expect(screen.getByText("TURNS")).toBeInTheDocument();
    expect(screen.getByText("model (lms)")).toBeInTheDocument();
    expect(screen.getByText(/no telemetry yet/i)).toBeInTheDocument();
    expect(screen.getByText("detections")).toBeInTheDocument();
    expect(screen.getByText("✓ clean")).toBeInTheDocument();
    expect(screen.getByText(/no behavioral flags/i)).toBeInTheDocument();
  });

  it("URL-encodes the session id in the fetch path", async () => {
    const fetchMock = vi.fn((_url: string) =>
      Promise.resolve(new Response(JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }), { status: 200 })),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderReplay("a/b c");
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(fetchMock.mock.calls[0][0]).toBe("/flow-session/a%2Fb%20c");
  });
});

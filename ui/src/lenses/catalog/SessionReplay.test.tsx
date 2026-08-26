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
  // ── (#1973 / #1978) rendered-DOM assertions ────────────────────────
  //
  // These render the COMPONENT. `sessionRun.test.ts` does not — it compares
  // `flattenView`, a hand-written mirror of this DOM, against the legacy
  // golden. That mirror is unenforced (#1978): changing this component's
  // markup leaves all 993 tests green, which is exactly what happened while
  // building the disclosure below. So anything asserted about what the
  // operator actually SEES has to live here.

  const PROMPT_BODY = "line one of the brief\nline two names a file\nline three is the ask";

  function stubSession(over: Record<string, unknown> = {}) {
    const records = [
      {
        ts: "2026-08-26T07:36:48Z",
        action: "dispatch.start",
        session_id: "s-disc",
        machine_id: "MacBook-Pro",
        category: "work",
        source: "crew",
        payload: { role: "crawler", prompt: PROMPT_BODY, prompt_chars: PROMPT_BODY.length, ...over },
      },
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
  }

  it("(#1973) discloses the FULL prompt text, not just its length — the payload is reachable in the DOM", async () => {
    // The defect this guards: `sessionRun.ts` held `sp.prompt`, took its
    // `.length`, rendered `prompt · N chars`, and dropped the string. That is
    // the THIRD instance of that shape in this codebase (tool-call arguments
    // and session records were the first two, #1960).
    //
    // Asserting on the summary line would PASS against the bug — the summary
    // is what the bug rendered. So this asserts the BODY.
    stubSession();
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const disclosure = document.querySelector('[data-act="disclose-prompt"]');
    expect(disclosure, "the prompt disclosure must exist").toBeInTheDocument();
    expect(disclosure?.querySelector(".disclosure__body")?.textContent).toBe(PROMPT_BODY);
    // And the summary still reports the authoritative length.
    expect(disclosure?.querySelector(".disclosure__sum")?.textContent).toContain(`${PROMPT_BODY.length} chars`);
  });

  it("(#1973) reports the RECORD's char count when the carried text was truncated, never the surviving length", async () => {
    // A truncated payload must not under-report its real size — the operator
    // is reading this to know how big the brief was, not how much of it
    // survived transport.
    stubSession({ prompt_chars: 9999 });
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    const sum = document.querySelector('[data-act="disclose-prompt"] .disclosure__sum')?.textContent;
    expect(sum).toContain("9999 chars");
    expect(sum).toContain("truncated");
  });

  it("(#1973) offers NO expander when a record reports a length but carries no text", async () => {
    // Degrading to a dead expander onto an empty box would be worse than the
    // summary line it replaced.
    stubSession({ prompt: undefined });
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    expect(document.querySelector('[data-act="disclose-prompt"]')).not.toBeInTheDocument();
  });

  it("(#1973) separates MODEL metrics from HARNESS metrics into distinct panes", async () => {
    // The operator question that produced this: reading `model (lms)` beside
    // TURNS/TOKENS/WALL CLOCK, it was not knowable which numbers described the
    // model and which described darkmux around it.
    //
    // The split also matters for steps that ran no model at all: the model
    // pane can be ABSENT rather than showing `0 turns · 0 tokens`, which would
    // be a lie shaped like data.
    stubSession();
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const model = document.querySelector('.metrics[data-scope="model"]');
    const harness = document.querySelector('.metrics[data-scope="harness"]');
    expect(model, "model metric pane").toBeInTheDocument();
    expect(harness, "harness metric pane").toBeInTheDocument();

    expect(model?.textContent).toContain("TURNS");
    expect(model?.textContent).toContain("TOKENS IN");
    expect(model?.textContent).toContain("COMPACTIONS");
    // WALL CLOCK is the harness's measure of the run, not the model's work.
    expect(model?.textContent).not.toContain("WALL CLOCK");
    expect(harness?.textContent).toContain("WALL CLOCK");
  });

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

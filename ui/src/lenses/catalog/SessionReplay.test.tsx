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

  it("(#1973) prints the prompt summary ONCE — the brief note is gone now that the disclosure carries it", async () => {
    // The first live render showed `prompt · 467 chars` twice, a few pixels
    // apart: once in the run brief, once as the expander's summary. Same
    // duplication the brief's bare "run" heading was removed for.
    stubSession();
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    const occurrences = (document.querySelector(".session-run")?.textContent ?? "").match(
      new RegExp(`prompt · ${PROMPT_BODY.length} chars`, "g"),
    );
    expect(occurrences).toHaveLength(1);
  });

  it("(#1973) still prints a brief prompt line when there is a length but NO text to disclose", async () => {
    // The one case that keeps the brief line: nothing to expand, so the
    // summary has nowhere else to live. Guards against the de-duplication
    // above silently deleting the only report of a prompt's size.
    stubSession({ prompt: undefined });
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    expect(document.querySelector(".session-run")?.textContent).toContain("prompt");
    expect(document.querySelector('[data-act="disclose-prompt"]')).not.toBeInTheDocument();
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

  it("(#1973) keeps the two metric panes ADJACENT, so text order still matches the legacy golden", async () => {
    // CI caught what the screen did not. The first version of the pane split
    // rendered HARNESS *below* the model track, which sandwiched
    // `model (lms)` between two metric grids — and, because the parity spec
    // compares the rendered `#stage` text byte-for-byte against
    // `goldens/session-task-list.txt`, moved WALL CLOCK after `model (lms)`
    // and failed it.
    //
    // Adjacency is the fix for both: it reads as one grouped metric row, and
    // `innerText` order stays identical to legacy (the pane labels are
    // CSS-generated and never enter the text). This asserts it locally,
    // because the parity suite only runs in CI and a layout regression should
    // not need a full playwright run to surface.
    stubSession();
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const model = document.querySelector('.metrics[data-scope="model"]');
    const harness = document.querySelector('.metrics[data-scope="harness"]');
    expect(model?.nextElementSibling, "HARNESS must directly follow MODEL — nothing between them").toBe(harness);

    // And the text order the golden pins: COMPACTIONS ... WALL CLOCK ... model track.
    const txt = document.querySelector(".session-run")?.textContent ?? "";
    expect(txt.indexOf("COMPACTIONS")).toBeLessThan(txt.indexOf("WALL CLOCK"));
    expect(txt.indexOf("WALL CLOCK")).toBeLessThan(txt.indexOf("model ("));
  });

  it("(#1973) renders a signal group with its severity, count badge and run-relative time", async () => {
    // The rendered half of the SIGNALS redesign. `sessionRun.test.ts` pins the
    // DERIVATION; this pins what an operator actually sees, because that file
    // compares a hand-written mirror and never renders the component (#1978).
    const det = (ts: string, kind: string, severity: string) => ({
      ts,
      session_id: "s-sig",
      category: "telemetry",
      source: "detector",
      machine_id: "MacBook-Pro",
      payload: { severity, kind, detail: `${kind} detail` },
    });
    const records = [
      { ts: "2026-01-01T00:00:00Z", action: "dispatch.start", session_id: "s-sig", machine_id: "MacBook-Pro", payload: { role: "coder" } },
      det("2026-01-01T00:00:10Z", "cycle", "warn"),
      det("2026-01-01T00:00:20Z", "cycle", "warn"),
      det("2026-01-01T00:00:50Z", "intra-turn-stall", "info"),
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
    renderReplay("s-sig");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const groups = [...document.querySelectorAll(".signals .signal")];
    expect(groups).toHaveLength(2);

    // Severity is encoded THREE ways, so it survives monochrome and
    // colour-blind viewing: the attribute, the class, and the glyph.
    expect(groups[0].getAttribute("data-severity")).toBe("warn");
    expect(groups[0].querySelector(".signal__glyph")?.textContent).toBe("⚠");
    expect(groups[0].querySelector(".signal__kind")?.textContent).toBe("cycle");
    expect(groups[0].querySelector(".signal__count")?.textContent).toBe("×2");

    // A recovery is NOT a warning — the defect this redesign fixes.
    expect(groups[1].getAttribute("data-severity")).toBe("info");
    expect(groups[1].querySelector(".signal__glyph")?.textContent).toBe("✓");
    // ...and carries no count badge, because `×1` on every row is noise.
    expect(groups[1].querySelector(".signal__count")).toBeNull();

    // Run-relative times, newest first inside the group.
    expect([...groups[0].querySelectorAll(".signal__at")].map((e) => e.textContent)).toEqual(["+0:20", "+0:10"]);
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

  it("renders the real run view — header, brief, metrics, signals — against the recorded corpus fixture", async () => {
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
    expect(screen.getByText("signals")).toBeInTheDocument();
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

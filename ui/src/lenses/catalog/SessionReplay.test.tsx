// See `sessionRun.test.ts`'s identical note: `clk()` reads the process
// timezone, and the real-corpus test below asserts on a `clk()`-derived
// value — pin it before anything runs.
process.env.TZ = "UTC";

import { describe, it, expect, vi, afterEach } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { render, screen, waitFor, act } from "@testing-library/react";
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

  it("(#1973) separates MODEL metrics from SYSTEM metrics into distinct panes", async () => {
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
    const system = document.querySelector('.metrics[data-scope="system"]');
    expect(model, "model metric pane").toBeInTheDocument();
    expect(system, "system metric pane").toBeInTheDocument();

    expect(model?.textContent).toContain("TURNS");
    expect(model?.textContent).toContain("TOKENS IN");
    // COMPACTIONS is a HARNESS metric: the harness decides to compact and
    // performs it via a utility role. The specialist only experiences it.
    expect(model?.textContent).not.toContain("COMPACTIONS");
    expect(system?.textContent).toContain("COMPACTIONS");
    // WALL CLOCK is the harness's measure of the run, not the model's work.
    expect(model?.textContent).not.toContain("WALL CLOCK");
    expect(system?.textContent).toContain("WALL CLOCK");
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
    const system = document.querySelector('.metrics[data-scope="system"]');
    expect(model?.nextElementSibling, "HARNESS must directly follow MODEL — nothing between them").toBe(system);

    // And the text order the golden pins: COMPACTIONS ... WALL CLOCK ... model track.
    const txt = document.querySelector(".session-run")?.textContent ?? "";
    expect(txt.indexOf("TOKENS OUT")).toBeLessThan(txt.indexOf("WALL CLOCK"));
    expect(txt.indexOf("WALL CLOCK")).toBeLessThan(txt.indexOf("loaded models"));
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

  it("(#1972) a live, recently-active run TICKS its elapsed counter while no records arrive", async () => {
    // The defect: `runRegions` derived "now" from `computeTMax(data)` — the
    // newest record's timestamp — so the elapsed counter only advanced when a
    // record ARRIVED. A dispatch that went quiet showed a frozen clock, which
    // is exactly when an operator most wants to know how long it has been
    // quiet. It was not stale; it was structurally incapable of moving.
    //
    // The clock is frozen here deliberately (this project's own rule: never
    // mix a fixed fixture timestamp with a clock-relative assertion), and
    // advanced explicitly, so the distance between fixture and now is an
    // asserted parameter rather than inherited from whenever the suite runs.
    vi.useFakeTimers();
    const t0 = 1_800_000_000_000;
    vi.setSystemTime(t0);
    const records = [
      { ts: new Date(t0 - 30_000).toISOString(), action: "dispatch.start", session_id: "s-live", machine_id: "M", payload: { role: "coder" } },
      { ts: new Date(t0 - 3_000).toISOString(), action: "dispatch.turn.heartbeat", session_id: "s-live", machine_id: "M", payload: {} },
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
    renderReplay("s-live");
    await vi.waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const readWall = () =>
      [...document.querySelectorAll('.metrics[data-scope="system"] .mv')].map((e) => e.textContent).join("");
    const before = readWall();
    expect(before).toContain("so far");

    // No new records — only time passing. This is the whole point.
    act(() => {
      vi.advanceTimersByTime(5000);
    });
    expect(readWall()).not.toBe(before);
    vi.useRealTimers();
  });

  it("(#1972) a run silent longer than the watchdog's kill timeout does NOT tick — abandonment is not liveness", async () => {
    // A dispatch that died months ago also has no terminal record. Ticking it
    // would render `1071:54 so far` and climbing. The host watchdog hard-kills
    // after `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` (600s), so anything quieter
    // than that cannot still be running — and the recorded parity corpus is
    // exactly this shape, which is how the rule was found.
    vi.useFakeTimers();
    const t0 = 1_800_000_000_000;
    vi.setSystemTime(t0);
    const records = [
      { ts: new Date(t0 - 40 * 24 * 3600_000).toISOString(), action: "dispatch.start", session_id: "s-dead", machine_id: "M", payload: { role: "coder" } },
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
    renderReplay("s-dead");
    await vi.waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const readWall = () =>
      [...document.querySelectorAll('.metrics[data-scope="system"] .mv')].map((e) => e.textContent).join("");
    const before = readWall();
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(readWall()).toBe(before);
    // ...and the pulse says quiet rather than beating.
    expect(document.querySelector(".pulse")?.getAttribute("data-state")).not.toBe("beating");
    vi.useRealTimers();
  });

  it("(#1972) the header keeps EXACTLY one space between the pill and the role", async () => {
    // The parity golden compares `#stage` innerText byte-for-byte, and the
    // pulse contributes no text of its own — so inserting it is a whitespace
    // hazard, not a neutral addition. The first version shipped a doubled
    // space: invisible on screen, unmissable to the golden, a full CI
    // round-trip to discover.
    //
    // Scoped to the pill -> role boundary, which is where the pulse sits. A
    // whole-header whitespace check would fail on correct code, because an
    // empty role legitimately leaves a doubled space before `(sid on
    // machine)` — that predates this and is not what is guarded here.
    // Needs a record with a `handle` — the role is read from there, and the
    // shared fixture leaves it unset, which legitimately renders an empty
    // role and a doubled space further along the line.
    const records = [
      { ts: "2026-01-01T00:00:00Z", action: "dispatch.start", session_id: "s-hdr", machine_id: "M", handle: "coder", payload: {} },
      { ts: "2026-01-01T00:01:00Z", action: "dispatch.complete", session_id: "s-hdr", machine_id: "M", payload: {} },
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
    renderReplay("s-hdr");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    const header = document.querySelector(".session-run__header")?.textContent ?? "";
    // Exactly one space between the pill and the role — no noun between them.
    expect(header).toMatch(/^COMPLETE CODER /);
  });

  // ── (#1973 audit) accessibility ────────────────────────────────────

  it("(#1973) the pill and the pulse never tell CONTRADICTORY stories about the same run", async () => {
    // A run that opened and went silent for weeks has no terminal record, so
    // the pill says RUNNING — while liveness correctly says it cannot still
    // be executing. Feeding ONE boolean to both made the pulse announce
    // "finished" beside a green RUNNING pill: the same run, the same view,
    // opposite claims, and only a screen-reader user would ever have seen the
    // contradiction.
    vi.useFakeTimers();
    const t0 = 1_800_000_000_000;
    vi.setSystemTime(t0);
    const records = [
      { ts: new Date(t0 - 40 * 24 * 3600_000).toISOString(), action: "dispatch.start", session_id: "s-stale", machine_id: "M", payload: { role: "coder" } },
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
    renderReplay("s-stale");
    await vi.waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const pill = document.querySelector(".session-run__header .pill")?.textContent ?? "";
    const pulseLabel = document.querySelector(".pulse")?.getAttribute("aria-label") ?? "";
    expect(pill).toContain("RUNNING");
    // The pulse may say "may be abandoned"; it must NOT claim the run finished.
    expect(pulseLabel).not.toContain("finished");
    expect(document.querySelector(".pulse")?.getAttribute("data-state")).toBe("stale");
    vi.useRealTimers();
  });

  it("(#1973) signal severity is available WITHOUT sight — not only as colour, class and a data attribute", async () => {
    // The glyph was `aria-hidden`, and the other two carriers (a class and
    // `data-severity`) are invisible to assistive tech. So the whole
    // struggle-vs-recovery distinction — the reason this redesign exists —
    // reached sighted users only.
    const det = (ts: string, kind: string, severity: string) => ({
      ts, session_id: "s-sev", category: "telemetry", source: "detector", machine_id: "M",
      payload: { severity, kind, detail: `${kind} detail` },
    });
    const records = [
      { ts: "2026-01-01T00:00:00Z", action: "dispatch.start", session_id: "s-sev", machine_id: "M", payload: { role: "coder" } },
      det("2026-01-01T00:00:10Z", "cycle", "warn"),
      det("2026-01-01T00:00:50Z", "intra-turn-stall", "info"),
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
    renderReplay("s-sev");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    const glyphs = [...document.querySelectorAll(".signals .signal__glyph")];
    expect(glyphs.map((g) => g.getAttribute("aria-label"))).toEqual(["warning", "recovered"]);
    expect(glyphs.every((g) => g.getAttribute("aria-hidden") !== "true")).toBe(true);
  });

  it("(#1973) the metric panes carry accessible names — their labels are CSS-generated and reach no DOM text node", async () => {
    // `content: attr(data-scope)` renders MODEL/HARNESS into the box tree
    // only. It cannot be selected, copied, matched by find-in-page, or seen by
    // browser translation — and those two words are exactly the information
    // this redesign exists to convey. The `aria-label` restores the name
    // without adding text, so the parity goldens (which compare innerText
    // byte-for-byte) stay green.
    stubSession();
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    const model = document.querySelector('.metrics[data-scope="model"]');
    const system = document.querySelector('.metrics[data-scope="system"]');
    expect(model?.getAttribute("aria-label")).toBe("model metrics");
    expect(system?.getAttribute("aria-label")).toBe("system metrics");
    expect(model?.getAttribute("role")).toBe("group");
    // ...and the pane names are still absent from the text, which is what
    // keeps the goldens passing.
    expect(model?.textContent).not.toContain("model metrics");
  });

  it("(#1973) the pulse is NOT an ARIA live region — its state can flap once a second", async () => {
    // `role="status"` announces on every change, and the quiet threshold is a
    // bare cutoff with no hysteresis: a dispatch whose turn latency hovers
    // near it flips label every tick. That is the struggling run an operator
    // most needs a clean signal about.
    stubSession();
    renderReplay("s-disc");
    await waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    const pulse = document.querySelector(".pulse");
    expect(pulse?.getAttribute("role")).toBe("img");
    expect(pulse?.getAttribute("aria-live")).toBeNull();
  });

  it("(#1972) POLLS a LIVE session's records — the page used to fetch once and freeze", async () => {
    // Found by a live dogfood run, not by a test. The wall clock advanced
    // (it reads the browser clock) while turns, tokens, signals and the last
    // beat all froze at page load — so the pulse went quiet 5s in and could
    // never beat, on the page whose whole purpose is watching a run.
    //
    // Liveness comes from PRESENCE, not from the records: asking the records
    // whether to keep asking for records is circular.
    const calls: string[] = [];
    const fetchMock = vi.fn((url: string) => {
      calls.push(url);
      const body = url.startsWith("/fleet/sessions/live")
        ? // The hook reads presence BEATS and pulls `session_id` off each —
          // a bare id list parses to an empty set and the page never polls.
          { sessions: [{ session_id: "s-live" }], meta: {} }
        : { records: [], count: 0, truncated: false, generated_at_ms: 0 };
      return Promise.resolve(new Response(JSON.stringify(body), { status: 200 }));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderReplay("s-live");
    await waitFor(() => expect(calls.some((u) => u.includes("/flow-session/"))).toBe(true));
    const firstRound = calls.filter((u) => u.includes("/flow-session/")).length;
    // The refetch interval is real time; give it one cycle plus slack.
    await waitFor(
      () => expect(calls.filter((u) => u.includes("/flow-session/")).length).toBeGreaterThan(firstRound),
      { timeout: 9000 },
    );
  }, 12_000);

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
    // Same reason as the track below: this corpus did no model work, so the
    // MODEL pane is absent and TURNS with it. The SYSTEM pane still renders.
    expect(screen.queryByText("TURNS")).not.toBeInTheDocument();
    expect(screen.getByText("WALL CLOCK")).toBeInTheDocument();
    // This corpus is `step start`/`step complete` only — no dispatch, no
    // model telemetry — so it is a NON-MODEL unit and the loaded-models track
    // is absent by design (#1973). The golden asserted its presence for years,
    // which is the golden recording a defect rather than catching one.
    expect(screen.queryByText("loaded models")).not.toBeInTheDocument();
    expect(screen.queryByText(/no telemetry yet/i)).not.toBeInTheDocument();
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
    // (#1972) Order-independent: the page also queries `/fleet/sessions/live`
    // to decide whether to POLL, and that request can land first. Asserting
    // `calls[0]` pinned an incidental ordering rather than the behaviour.
    await waitFor(() =>
      expect(fetchMock.mock.calls.map((c) => c[0])).toContain("/flow-session/a%2Fb%20c"),
    );
  });
});

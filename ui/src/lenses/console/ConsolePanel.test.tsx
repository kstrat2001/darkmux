import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ConsolePanel } from "./ConsolePanel";
import type { PanelId } from "../../lib/route";

function renderPanel(initialPanelId: PanelId | "" = "") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <ConsolePanel initialPanelId={initialPanelId} />
    </QueryClientProvider>,
  );
}

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status });
}

const MISSION_STATUS_BODY = {
  panel: "mission-status",
  argv: ["mission", "status"],
  captured_ts_ms: Date.UTC(2026, 0, 1, 12, 0, 0),
  gather_ms: 8,
  exit_code: 0,
  ansi_text: "mission status — 0 missions",
  stderr_tail: "",
  cols: 100,
  cache_ttl_ms: 3000,
  age_ms: 0,
  auto_refresh: true,
};

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
});

// (#1904) `/runs` — the ONLY fetch a bare landing (no `panel=`) makes now
// that the default is the client-rendered activity view, not the
// `mission-status` CLI panel. Most tests below don't care about its
// content, so this stands in a well-shaped, empty response wherever a
// test's own fetch mock doesn't override `/runs` itself.
function runsJson(runs: unknown[] = []) {
  return jsonResponse({ runs, generated_at_ms: Date.UTC(2026, 0, 1, 12, 0, 0) });
}

describe("ConsolePanel", () => {
  // (#1904) Three false starts preceded this shape (documented in
  // `ActivityPanel.tsx`'s own module doc): a section bolted above the old
  // default, then a mission-status fallback, before the operator settled on
  // the console's default being the activity view itself — no panel fetch
  // at all until the operator explicitly picks a CLI panel tab.
  it("lands on the activity view by default — fetches ONLY /runs, no /panel/* call, until a CLI tab is explicitly picked", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());
    expect(fetchMock.mock.calls.some(([u]) => String(u).startsWith("/panel/"))).toBe(false);
    expect(screen.getByText("activity", { selector: ".runchip" })).toHaveClass("on");
    expect(screen.getByText("mission status", { selector: ".runchip" })).not.toHaveClass("on");
  });

  it("a running dispatch is visible on the DEFAULT console view, without picking any panel", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson([{ id: "d1", kind: "dispatch", status: "running", tracked: false, role: "coder", updated_ts: 1 }])
          : jsonResponse(MISSION_STATUS_BODY),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    // Same page, no interaction — the running row is already there.
    await waitFor(() => expect(document.querySelector(".consoleactivity .labrunrow")).not.toBeNull());
    expect(document.querySelector(".consoleactivity .labrunrow")!.textContent).toContain("d1");
  });

  it("selecting doctor (manual-only) does NOT auto-fetch — shows the not-yet-run placeholder", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("doctor");
    await waitFor(() => expect(screen.getByText(/not run yet — this panel probes the machine/)).toBeInTheDocument());
    // The manual-only contract (#1286) is about PANEL probes, never fired by
    // selecting the tab.
    expect(fetchMock.mock.calls.some(([u]) => String(u).startsWith("/panel/"))).toBe(false);
    expect(screen.getByRole("button", { name: "run" })).toBeInTheDocument();
  });

  it("clicking doctor's run button fetches it exactly once, then shows re-run", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({ ...MISSION_STATUS_BODY, panel: "doctor", argv: ["doctor"], ansi_text: "darkmux doctor — 0 checks", auto_refresh: false, cache_ttl_ms: 0 }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("doctor");
    await waitFor(() => expect(screen.getByRole("button", { name: "run" })).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "run" }));
    await waitFor(() => expect(screen.getByText(/darkmux doctor/)).toBeInTheDocument());
    expect(fetchMock.mock.calls.filter(([u]) => String(u).startsWith("/panel/"))).toHaveLength(1);
    expect(screen.getByRole("button", { name: "re-run" })).toBeInTheDocument();
    expect(screen.getByText("· manual-run only")).toBeInTheDocument();
  });

  it("clicking a CLI panel tab fetches it and marks it active, deactivating the activity tab", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/panel/role-list")) {
        return Promise.resolve(jsonResponse({ ...MISSION_STATUS_BODY, panel: "role-list", argv: ["role", "list"], ansi_text: "id description" }));
      }
      if (url.startsWith("/runs")) return Promise.resolve(runsJson());
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("id description")).toBeInTheDocument());
    expect(screen.getByText("roles").closest(".runchip")).toHaveClass("on");
    expect(screen.getByText("activity", { selector: ".runchip" })).not.toHaveClass("on");
  });

  it("a daemon error response renders the daemon's own message when explicitly selecting mission-status", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) =>
        Promise.resolve(url.startsWith("/runs") ? runsJson() : new Response('panel "mission-status" timed out\n', { status: 504 })),
      ),
    );
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText('panel "mission-status" timed out')).toBeInTheDocument());
  });

  it("re-visiting an already-loaded CLI panel reuses the cache (no second fetch)", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/panel/role-list")) {
        return Promise.resolve(jsonResponse({ ...MISSION_STATUS_BODY, ansi_text: "roles here" }));
      }
      if (url.startsWith("/runs")) return Promise.resolve(runsJson());
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());
    fireEvent.click(screen.getByText("mission status"));
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());
    // Both CLI panels have now been fetched once each (plus the one /runs
    // call from the initial activity landing) — this is the baseline every
    // further tab switch should reuse rather than add to.
    const callsAfterBothVisited = fetchMock.mock.calls.length;

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());

    expect(fetchMock.mock.calls.length).toBe(callsAfterBothVisited);
  });

  // (#1904 QA fix) Was: only the inline link was ever clicked, despite the
  // test's own title claiming both paths were exercised — the "all
  // activity" **tab** itself was never clicked (the comment at the end,
  // "Direct deep link to the tab reaches the same uncapped state," named
  // an assertion that was never made). This now genuinely drives both:
  // the inline link first, back to the capped default, then the "all
  // activity" tab chip itself.
  it("the 'all activity' tab and the default view's own 'show every run' link both reach the uncapped view", async () => {
    const runs = Array.from({ length: 12 }, (_, i) => ({ id: `r${i}`, kind: "mission", status: "complete", tracked: true, updated_ts: i }));
    vi.stubGlobal("fetch", vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson(runs) : jsonResponse(MISSION_STATUS_BODY))));
    renderPanel("");
    await waitFor(() => expect(document.querySelectorAll(".consoleactivity .labrunrow").length).toBe(10));

    fireEvent.click(screen.getByText("→ show every run"));
    await waitFor(() => expect(document.querySelectorAll(".consoleactivity .labrunrow").length).toBe(12));
    expect(screen.getByText("all activity", { selector: ".runchip" })).toHaveClass("on");

    // Back to the capped default, then the "all activity" TAB itself
    // (not the inline link) — the second, previously-unexercised path.
    fireEvent.click(screen.getByText("activity", { selector: ".runchip" }));
    await waitFor(() => expect(document.querySelectorAll(".consoleactivity .labrunrow").length).toBe(10));

    fireEvent.click(screen.getByText("all activity", { selector: ".runchip" }));
    await waitFor(() => expect(document.querySelectorAll(".consoleactivity .labrunrow").length).toBe(12));
    expect(screen.getByText("all activity", { selector: ".runchip" })).toHaveClass("on");
  });

  it("mission-status stays selectable and unaffected by the default's change", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());
    expect(screen.getByText("mission status", { selector: ".runchip" })).toHaveClass("on");
  });

  // (#1908) A command that fails with empty stdout used to render identically
  // to a command that succeeded and had nothing to say — header, timing,
  // empty box either way. The daemon already sends `exit_code` and
  // `stderr_tail`; the viewer just never read them. These three cover the
  // fix's own stated shapes: failure-with-stderr, honest-empty-success, and
  // failure-that-still-produced-stdout. The fourth shape from the issue (the
  // HTTP-refusal path, `viewer-panel.spec.js:119`) is the existing
  // "a daemon error response renders the daemon's own message…" test above,
  // unchanged by this fix.
  it("(#1908) a failed panel (non-zero exit, empty stdout, non-empty stderr) shows the stderr verbatim and reads as a failure", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({
              ...MISSION_STATUS_BODY,
              exit_code: 1,
              ansi_text: "",
              stderr_tail:
                "Error: running `lms ps --json`: host command failed: spawning `lms` (ps): No such file or directory (os error 2)",
            }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText(/host command failed: spawning `lms`/)).toBeInTheDocument());
    // Verbatim, in the same `.panelerr` treatment the HTTP-refusal path uses
    // — not reworded, and not the honest-empty-state wording either.
    expect(document.querySelector(".panelerr")!.textContent).toContain("No such file or directory");
    expect(screen.queryByText("no output")).not.toBeInTheDocument();
  });

  it("(#1908) a successful panel (zero exit, empty stdout) renders an honest empty state, not a failure", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({ ...MISSION_STATUS_BODY, exit_code: 0, ansi_text: "", stderr_tail: "" }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText("no output")).toBeInTheDocument());
    expect(document.querySelector(".panelerr")).toBeNull();
  });

  it("(#1908) a failed panel that DID produce stdout shows both the stdout and the stderr, not one instead of the other", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({
              ...MISSION_STATUS_BODY,
              exit_code: 2,
              ansi_text: "partial output before the failure\n",
              stderr_tail: "boom: something went wrong",
            }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText(/boom: something went wrong/)).toBeInTheDocument());
    expect(document.querySelector(".panelout")!.textContent).toContain("partial output before the failure");
    expect(document.querySelector(".panelerr")!.textContent).toContain("boom: something went wrong");
  });

  // (#1908 QA fix) A ZERO exit with empty stdout is the "no output" empty
  // state — but only when stderr is ALSO empty. A command that exits 0 and
  // still writes a warning to stderr is not silent; dropping that warning
  // in favor of the same "no output" wording a truly silent success gets
  // is the same dishonesty #1908 exists to kill, one branch over.
  it("(#1908) a successful panel that still wrote to stderr shows the stderr, not the 'no output' wording", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({
              ...MISSION_STATUS_BODY,
              exit_code: 0,
              ansi_text: "",
              stderr_tail: "warning: cache dir missing, using defaults",
            }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText(/warning: cache dir missing/)).toBeInTheDocument());
    expect(screen.queryByText("no output")).not.toBeInTheDocument();
  });

  // (#1908 QA fix) The synthesized-fallback branch (a failure that wrote
  // NOTHING to stderr either) is the one place this component invents its
  // own words rather than quoting the daemon — exactly the branch most
  // worth pinning so it can't drift into overclaiming a cause the response
  // never stated.
  it("(#1908) a failed panel with no stderr at all still names the failure by exit status, not a blank box", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({ ...MISSION_STATUS_BODY, exit_code: 17, ansi_text: "", stderr_tail: "" }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() =>
      expect(screen.getByText("command exited with status 17 and printed nothing to stderr")).toBeInTheDocument(),
    );
  });

  it("(#1908) a signal-killed panel (null exit_code) with no stderr names that instead of a blank box", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({ ...MISSION_STATUS_BODY, exit_code: null, ansi_text: "", stderr_tail: "" }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() =>
      expect(
        screen.getByText("command did not exit cleanly (killed) and printed nothing to stderr"),
      ).toBeInTheDocument(),
    );
  });

  // (#1921) A panel switch must never show the OUTGOING panel's body under
  // the incoming panel's own command line. Today's `CliPanelView` has no
  // `key` prop and this codebase sets no `placeholderData`/`keepPreviousData`
  // anywhere (`grep -rn keepPreviousData ui/src` is empty), so a query for a
  // never-before-fetched id starts with `data: undefined` and the loading
  // chrome renders — safe by the query library's own default, not by an
  // explicit guard. That default is exactly the kind of thing a "reuse the
  // previous panel while the next one loads" enhancement (`placeholderData:
  // keepPreviousData`, matching `useQuery`'s own naming) could reasonably
  // add later without anyone noticing it needs a compensating `key={id}` on
  // `CliPanelView` to keep panels from bleeding into each other. This test
  // exists to catch exactly that regression, not today's (absent) bug.
  it("(#1921) switching to a panel whose fetch never resolves does not render the prior panel's body under the new command line", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/runs")) return Promise.resolve(runsJson());
      if (url.startsWith("/panel/mission-status")) return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
      if (url.startsWith("/panel/machine-status")) return new Promise(() => {}); // never resolves
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText("mission status — 0 missions")).toBeInTheDocument());

    fireEvent.click(screen.getByText("machine", { selector: ".runchip" }));
    await waitFor(() => expect(screen.getByText("machine", { selector: ".runchip" })).toHaveClass("on"));

    // The command line switched to the new (not-yet-loaded) panel's own
    // command — `NotLoadedChrome`'s own `.pc-cmd`, distinct from
    // `LoadedChrome`'s (nothing has resolved for "machine status" yet)...
    expect(document.querySelector(".pc-cmd")!.textContent).toBe("$ darkmux machine status");
    // ...and the OLD panel's body text is gone, not left rendered underneath
    // the new command line.
    expect(screen.queryByText("mission status — 0 missions")).not.toBeInTheDocument();
    expect(document.body.textContent).not.toContain("mission status — 0 missions");
  });
});

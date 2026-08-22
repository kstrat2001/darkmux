import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ConsolePanel } from "./ConsolePanel";
import type { PanelId } from "../../lib/route";

function renderPanel(initialPanelId: PanelId | "" = "", initialOpts?: Readonly<Record<string, string>>) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <ConsolePanel initialPanelId={initialPanelId} initialOpts={initialOpts} />
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

function runsJson(runs: unknown[] = []) {
  return jsonResponse({ runs, generated_at_ms: Date.UTC(2026, 0, 1, 12, 0, 0) });
}

const RUN_LIST_DEFAULT_BODY = {
  panel: "run-list",
  argv: ["run", "list"],
  opts: { kind: "all", all: "recent" },
  captured_ts_ms: Date.UTC(2026, 0, 1, 12, 0, 0),
  gather_ms: 4,
  exit_code: 0,
  ansi_text: "KIND STATUS STARTED DURATION ID",
  stderr_tail: "",
  cols: 100,
  cache_ttl_ms: 3000,
  age_ms: 0,
  auto_refresh: true,
};

describe("ConsolePanel", () => {
  // (#1905 step 3) The console's default landing panel is `run-list`
  // (`panels.ts::DEFAULT_PANEL_ID`) — a real, ordinary CLI panel like any
  // other, fetched through `/panel/run-list` exactly like an explicit
  // `panel=run-list` deep link would be. This replaces #1904's
  // client-rendered `ActivityPanel` default (a THIRD renderer of `/runs`,
  // deleted along with its own escape-hatch pill — see `panels.ts`'s own
  // doc on `PANELS` for the full story). No `/runs` fetch happens at all
  // on a bare landing now; `run-list`'s own daemon endpoint is the ONLY
  // fetch.
  it("lands on run-list by default — a real /panel/run-list fetch, the run-list pill active", async () => {
    const fetchMock = vi.fn((url: string) => (url.startsWith("/panel/run-list") ? Promise.resolve(jsonResponse(RUN_LIST_DEFAULT_BODY)) : Promise.resolve(jsonResponse(MISSION_STATUS_BODY))));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/KIND STATUS/)).toBeInTheDocument());
    expect(fetchMock.mock.calls.some(([u]) => String(u).startsWith("/runs"))).toBe(false);
    expect(screen.getByText("run list", { selector: ".runchip" })).toHaveClass("on");
    expect(screen.getByText("mission status", { selector: ".runchip" })).not.toHaveClass("on");
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

  it("clicking a CLI panel tab fetches it and marks it active, deactivating the run-list default", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/panel/role-list")) {
        return Promise.resolve(jsonResponse({ ...MISSION_STATUS_BODY, panel: "role-list", argv: ["role", "list"], ansi_text: "id description" }));
      }
      if (url.startsWith("/panel/run-list")) return Promise.resolve(jsonResponse(RUN_LIST_DEFAULT_BODY));
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/KIND STATUS/)).toBeInTheDocument());

    fireEvent.click(screen.getByText("role list"));
    await waitFor(() => expect(screen.getByText("id description")).toBeInTheDocument());
    expect(screen.getByText("role list").closest(".runchip")).toHaveClass("on");
    expect(screen.getByText("run list", { selector: ".runchip" })).not.toHaveClass("on");
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
      if (url.startsWith("/panel/run-list")) return Promise.resolve(jsonResponse(RUN_LIST_DEFAULT_BODY));
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/KIND STATUS/)).toBeInTheDocument());

    fireEvent.click(screen.getByText("role list"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());
    fireEvent.click(screen.getByText("mission status"));
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());
    // Three CLI panels have now been fetched once each (run-list on the
    // default landing, role-list, mission-status) — this is the baseline
    // every further tab switch should reuse rather than add to.
    const callsAfterAllVisited = fetchMock.mock.calls.length;

    fireEvent.click(screen.getByText("role list"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());

    expect(fetchMock.mock.calls.length).toBe(callsAfterAllVisited);
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

// (#1911, opts-as-command-tokens redesign) The opts bar (a second pill row)
// is deleted — the operator rejected it live. A panel's declared options
// now render as TOKENS inside `.pc-cmd`'s own command line, driven by the
// same `PANEL_OPTS` table, never a switch on panel id.
describe("ConsolePanel — command-line tokens (#1911 redesign)", () => {
  /** The real server response shape for `run-list`'s two declared opts —
   * `argv` computed the SAME way a real daemon's `resolve_opts`/
   * `compose_argv` would, so a test asserting "no drift" isn't accidentally
   * proving it against a fixture that could never have matched in the
   * first place. Deliberately NOT built by calling this file's own
   * `composeArgv` import — an independent computation is what makes the
   * argv-drift test below meaningful. */
  function runListArgvFor(kind: string, all: string): string[] {
    const argv = ["run", "list"];
    if (kind !== "all") argv.push("--kind", kind);
    if (all === "all") argv.push("--all");
    return argv;
  }

  function runListBody(kind: string, all: string, over: Record<string, unknown> = {}) {
    return {
      panel: "run-list",
      argv: runListArgvFor(kind, all),
      opts: { kind, all },
      captured_ts_ms: Date.UTC(2026, 0, 1, 12, 0, 0),
      gather_ms: 4,
      exit_code: 0,
      ansi_text: `kind=${kind} all=${all}`,
      stderr_tail: "",
      cols: 100,
      cache_ttl_ms: 3000,
      age_ms: 0,
      auto_refresh: true,
      ...over,
    };
  }

  function runListFetchMock() {
    return vi.fn((url: string) => {
      if (url.startsWith("/runs")) return Promise.resolve(runsJson());
      if (url.startsWith("/panel/run-list")) {
        const u = new URL(url, "http://x");
        const kind = u.searchParams.get("opt.kind") ?? "all";
        const all = u.searchParams.get("opt.all") ?? "recent";
        return Promise.resolve(jsonResponse(runListBody(kind, all)));
      }
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
  }

  // (bullet 4 — the main regression risk named explicitly in the brief) A
  // panel with no declared opts renders BYTE-IDENTICAL to before this
  // whole opts feature existed: one plain, non-interactive `.pc-cmd` span,
  // no tokens, no `.optsbar`/`.pc-tok`/listbox/switch anywhere in the DOM.
  it("(byte-identical pin) a panel with no declared opts (role-list) has a plain static command line, no interactive tokens anywhere", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/panel/role-list") ? jsonResponse({ ...MISSION_STATUS_BODY, panel: "role-list", argv: ["role", "list"], ansi_text: "id description" }) : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("role-list");
    await waitFor(() => expect(screen.getByText("id description")).toBeInTheDocument());
    const cmd = document.querySelector(".pc-cmd")!;
    expect(cmd.textContent).toBe("$ darkmux role list");
    expect(cmd.children.length).toBe(0);
    expect(document.querySelector(".optsbar")).toBeNull();
    expect(document.querySelector(".pc-tok")).toBeNull();
    expect(document.querySelector('[role="listbox"]')).toBeNull();
    expect(document.querySelector('[role="switch"]')).toBeNull();
    expect(document.querySelector(".pc-drift")).toBeNull();
  });

  it("mission-status declares only the --all boolean token: role=switch, dim when off", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());
    const sw = screen.getByRole("switch", { name: "--all" });
    expect(sw).toHaveAttribute("aria-checked", "false");
    expect(sw).not.toHaveClass("on");
    expect(document.querySelector('[role="listbox"]')).toBeNull();
    expect(document.querySelector(".pc-cmd")!.textContent).toBe("$ darkmux mission status [--all]");
  });

  /// (#1922 review) The age-based `· stale (Ns)` note was DEAD CODE:
  /// `panelAgeLabel(body, Date.now())` made `age` identically zero, so the
  /// branch could never render. `format.ts`'s unit test kept passing
  /// because it calls `panelAgeLabel` directly with a synthetic timestamp
  /// and never exercises the call site — nothing asserted the chrome text.
  it("the chrome reports an aging body once it is past its TTL", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    try {
      const fetchMock = vi.fn((url: string) =>
        Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)),
      );
      vi.stubGlobal("fetch", fetchMock);
      renderPanel("mission-status");
      await waitFor(() => expect(document.querySelector(".pc-cmd")).toBeInTheDocument());
      expect(document.querySelector(".pc-stale")).toBeNull();

      // Past 3x the panel TTL, with no refetch — the body on screen is old
      // and the receipt has to say so.
      vi.setSystemTime(Date.now() + 30_000);
      fireEvent(window, new Event("resize"));
      await waitFor(() => expect(document.querySelector(".pc-stale")).not.toBeNull());
      expect(document.querySelector(".pc-stale")!.textContent).toMatch(/stale \(\d+s\)/);
    } finally {
      vi.useRealTimers();
    }
  });

  /// (#1922 review) The command line must show the command that would
  /// ACTUALLY run. Rendering the bare flag in both states made `.pc-cmd`
  /// read `$ darkmux mission status --all` above a body saying "8 of 93
  /// shown" — paste it and you get 93. The two parity goldens proved it:
  /// identical command lines, different bodies. State conveyed by colour
  /// alone was also a WCAG 1.4.1 failure on a control whose entire job is
  /// to be read.
  it("a boolean token's command text DIFFERS between off and on", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());

    const off = document.querySelector(".pc-cmd")!.textContent;
    fireEvent.click(screen.getByRole("switch", { name: "--all" }));
    await waitFor(() => expect(screen.getByRole("switch", { name: "--all" })).toHaveAttribute("aria-checked", "true"));
    const on = document.querySelector(".pc-cmd")!.textContent;

    expect(off).not.toBe(on);
    expect(off).toContain("[--all]");
    expect(on).toContain("--all");
    expect(on).not.toContain("[--all]");
  });

  it("run-list declares --kind (an enum token opening a listbox) AND --all (a switch)", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    const kindToken = screen.getByRole("button", { name: "--kind all ▾" });
    expect(kindToken).toHaveAttribute("aria-haspopup", "listbox");
    expect(kindToken).toHaveAttribute("aria-expanded", "false");
    expect(screen.getByRole("switch", { name: "--all" })).toHaveAttribute("aria-checked", "false");
    expect(document.querySelector(".pc-cmd")!.textContent).toBe("$ darkmux run list --kind all ▾ [--all]");
  });

  /// (#1911) The half that was missing: `parseRoute` could READ a selection
  /// out of a deep link, but nothing ever WROTE one, so a pick could not be
  /// shared, did not survive a reload, and left no history entry. Found by
  /// selecting a value against the live daemon and watching the address bar
  /// not move. `canonicalHash` already knew how to serialize `opt.*`; the
  /// call site was absent, exactly the gap `RunsBoard.selectKind` closes for
  /// its own out-of-route state.
  it("picking a value writes the selection into the URL", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    window.location.hash = "#lens=console&panel=run-list";
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "--kind all \u25be" }));
    fireEvent.click(screen.getByRole("option", { name: "lab" }));

    await waitFor(() => expect(window.location.hash).toContain("opt.kind=lab"));
    expect(window.location.hash).toContain("panel=run-list");
  });

  /// The default is not written, so the default variant's URL stays
  /// byte-identical to a bare panel link rather than accumulating noise.
  it("picking the default value again leaves no opt param behind", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    window.location.hash = "#lens=console&panel=run-list";
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "--kind all \u25be" }));
    fireEvent.click(screen.getByRole("option", { name: "lab" }));
    await waitFor(() => expect(window.location.hash).toContain("opt.kind=lab"));

    fireEvent.click(screen.getByRole("button", { name: "--kind lab \u25be" }));
    fireEvent.click(screen.getByRole("option", { name: "all" }));
    await waitFor(() => expect(window.location.hash).not.toContain("opt.kind"));
  });

  it("activating the --kind token opens a listbox of every legal value, current selection marked", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "--kind all ▾" }));
    const listbox = screen.getByRole("listbox", { name: "--kind" });
    expect(listbox).toBeInTheDocument();
    const options = screen.getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["all", "mission", "dispatch", "lab"]);
    expect(screen.getByRole("option", { name: "all" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("option", { name: "lab" })).toHaveAttribute("aria-selected", "false");
  });

  it("selecting a listbox option refetches, updates the token text, and closes the menu", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "--kind all ▾" }));
    fireEvent.click(screen.getByRole("option", { name: "lab" }));

    await waitFor(() => expect(screen.getByText(/kind=lab/)).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "--kind lab ▾" })).toBeInTheDocument();
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(fetchMock.mock.calls.some(([u]) => String(u).includes("/panel/run-list") && String(u).includes("opt.kind=lab"))).toBe(true);
  });

  it("clicking the --all switch toggles it on and refetches with --all applied", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("switch", { name: "--all" }));
    await waitFor(() => expect(screen.getByRole("switch", { name: "--all" })).toHaveAttribute("aria-checked", "true"));
    expect(screen.getByRole("switch", { name: "--all" })).toHaveClass("on");
    expect(fetchMock.mock.calls.some(([u]) => String(u).includes("opt.all=all"))).toBe(true);
  });

  it("keyboard: Enter/Space on the --all switch toggles it, same as a click", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.keyDown(screen.getByRole("switch", { name: "--all" }), { key: "Enter" });
    await waitFor(() => expect(screen.getByRole("switch", { name: "--all" })).toHaveAttribute("aria-checked", "true"));

    fireEvent.keyDown(screen.getByRole("switch", { name: "--all" }), { key: " " });
    await waitFor(() => expect(screen.getByRole("switch", { name: "--all" })).toHaveAttribute("aria-checked", "false"));
  });

  it("the command line reflects the current selection even before any fetch has landed (pending preview)", async () => {
    const fetchMock = vi.fn((url: string) => (url.startsWith("/panel/") ? new Promise(() => {}) : Promise.resolve(runsJson())));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(document.querySelector(".pc-cmd")!.textContent).toBe("$ darkmux run list --kind all ▾ [--all]"));

    fireEvent.click(screen.getByRole("button", { name: "--kind all ▾" }));
    fireEvent.click(screen.getByRole("option", { name: "dispatch" }));
    await waitFor(() => expect(document.querySelector(".pc-cmd")!.textContent).toBe("$ darkmux run list --kind dispatch ▾ [--all]"));
  });

  it("keyboard: Enter opens the menu focused on the current value, ArrowDown moves, Enter selects, focus returns to the token", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    const token = screen.getByRole("button", { name: "--kind all ▾" });
    token.focus();
    fireEvent.keyDown(token, { key: "Enter" });
    await waitFor(() => expect(screen.getByRole("option", { name: "all" })).toHaveFocus());

    fireEvent.keyDown(screen.getByRole("option", { name: "all" }), { key: "ArrowDown" });
    expect(screen.getByRole("option", { name: "mission" })).toHaveFocus();

    fireEvent.keyDown(screen.getByRole("option", { name: "mission" }), { key: "Enter" });
    await waitFor(() => expect(screen.getByText(/kind=mission/)).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "--kind mission ▾" })).toHaveFocus();
  });

  it("keyboard: Escape closes the menu without changing the selection and returns focus to the token", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    const token = screen.getByRole("button", { name: "--kind all ▾" });
    fireEvent.click(token);
    await waitFor(() => expect(screen.getByRole("option", { name: "all" })).toHaveFocus());

    fireEvent.keyDown(screen.getByRole("option", { name: "all" }), { key: "Escape" });
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "--kind all ▾" })).toHaveFocus();
    expect(fetchMock.mock.calls.some(([u]) => String(u).includes("opt.kind="))).toBe(false);
  });

  it("(#1911 selection state) flipping run-list -> mission-status -> run-list preserves the --kind selection (per-pill memory, no localStorage)", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "--kind all ▾" }));
    fireEvent.click(screen.getByRole("option", { name: "lab" }));
    await waitFor(() => expect(screen.getByText(/kind=lab/)).toBeInTheDocument());

    fireEvent.click(screen.getByText("mission status"));
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());

    fireEvent.click(screen.getByText("run list"));
    await waitFor(() => expect(screen.getByText(/kind=lab/)).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "--kind lab ▾" })).toBeInTheDocument();
    expect(localStorage.length).toBe(0);
  });

  it("a deep link carrying opts (#1911) renders the chosen token and fetches with it applied", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list", { kind: "dispatch" });
    await waitFor(() => expect(screen.getByText(/kind=dispatch/)).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "--kind dispatch ▾" })).toBeInTheDocument();
  });

  // ── Staleness: `placeholderData: keepPreviousData` (bullet 3) ──────────
  it("(stale output treatment) between changing a token and the fetch landing, the old output stays visible, dimmed, with an explicit note — never blanked, never silently current", async () => {
    let resolveSecond!: (r: Response) => void;
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/runs")) return Promise.resolve(runsJson());
      const u = new URL(url, "http://x");
      const kind = u.searchParams.get("opt.kind") ?? "all";
      if (kind === "all") return Promise.resolve(jsonResponse(runListBody("all", "recent")));
      return new Promise<Response>((resolve) => {
        resolveSecond = resolve;
      });
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "--kind all ▾" }));
    fireEvent.click(screen.getByRole("option", { name: "lab" }));

    // The command line already shows the NEW selection — that part is
    // never stale, it's a pure function of local state.
    await waitFor(() => expect(screen.getByRole("button", { name: "--kind lab ▾" })).toBeInTheDocument());
    // The OLD output is still on screen (not blanked to "running…"),
    // explicitly marked so it can't be read as current.
    expect(screen.getByText(/kind=all/)).toBeInTheDocument();
    expect(screen.getByText("· selection changed, refreshing…")).toBeInTheDocument();
    expect(document.querySelector(".panelout")).toHaveClass("pc-body-stale");

    resolveSecond(jsonResponse(runListBody("lab", "recent")));
    await waitFor(() => expect(screen.getByText(/kind=lab/)).toBeInTheDocument());
    expect(screen.queryByText("· selection changed, refreshing…")).not.toBeInTheDocument();
    expect(document.querySelector(".panelout")).not.toHaveClass("pc-body-stale");
  });

  it("(byte-identical pin, staleness half) a no-opts panel's output is never marked stale — its query key never changes, so isPlaceholderData can never fire", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/panel/role-list") ? jsonResponse({ ...MISSION_STATUS_BODY, panel: "role-list", argv: ["role", "list"], ansi_text: "roles here" }) : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("role-list");
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());
    expect(document.querySelector(".panelout")).not.toHaveClass("pc-body-stale");
    expect(screen.queryByText("· selection changed, refreshing…")).not.toBeInTheDocument();
  });

  // ── Drift: response.argv disagreeing with the composed argv (bullet 3) ──
  it("(argv-drift) when the response's own argv does not match what the client composed for the SAME selection, the response wins for display and the mismatch is surfaced, not hidden", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : // Same `opts` echo as the current (default) selection — NOT
            // stale — but a DIFFERENT argv than `composeArgv("run-list", {})`
            // would produce (`["run","list"]`). A real twin-drift shape:
            // the server resolved the identical selection to different argv
            // than this client's own table would.
            jsonResponse(runListBody("all", "recent", { argv: ["run", "list", "--verbose"] })),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());

    expect(document.querySelector(".pc-cmd")!.textContent).toBe("$ darkmux run list --verbose");
    expect(screen.getByText("⚠ argv mismatch — showing what actually ran")).toBeInTheDocument();
    // The response won for display — no live tokens rendered while the
    // client's own composed argv is provably wrong for this data.
    expect(document.querySelector('[role="listbox"]')).toBeNull();
    expect(document.querySelector('[role="switch"]')).toBeNull();
    expect(document.querySelector(".pc-tok")).toBeNull();
  });

  it("(argv-drift, no false positive) a matching response never shows the drift marker", async () => {
    const fetchMock = runListFetchMock();
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("run-list");
    await waitFor(() => expect(screen.getByText(/kind=all/)).toBeInTheDocument());
    expect(screen.queryByText(/argv mismatch/)).not.toBeInTheDocument();
    expect(document.querySelector(".pc-drift")).toBeNull();
  });
});

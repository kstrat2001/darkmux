import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MachineLens } from "./MachineLens";
import { todayUTC, prevDateUTC } from "../../lib/flow";

function renderMachine(uid: string | null) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens uid={uid} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
  // (#2019) A leaked static-build meta would silently put EVERY later test in
  // this file into daemon-less mode.
  document.head.querySelectorAll("meta[name^='darkmux-']").forEach((e) => e.remove());
});

const RESOURCES = {
  schema_version: "1",
  generated_at_ms: 1,
  gather_ms: 1,
  limit_bytes: 1000,
  limit_source: "budget",
  pool: { capacity_bytes: 2000, used_bytes: 1200, available_bytes: 1000, free_bytes: 800 },
  pressure: { swap_used_bytes: 0, compressor_bytes: 0, margin_percent: 90, red: false },
  models: [],
  machine: { potential_bytes: 500, unpriced_models: 0, current_bytes: 100, state: "green" },
  attribution: "test",
  messages: [],
  cache_ttl_ms: 2000,
};

/** Routes every endpoint the machine lens reads. `resourcesCalled` records
 * whether `/machine/resources` was EVER requested — the load-bearing
 * assertion for the local-only-probe gate (`enabled: isLocalMach`). */
/** (#2019) Inject a static-build meta for one test. `isStaticBuild()` and its
 * siblings read `document.head` live, so this is the real signal, not a
 * stand-in — see `injectedMeta.ts`'s own doc on why no test harness injects
 * these by default. Cleared in `afterEach` below. */
/** (#2021) `RESOURCES` carries `models: []`, which is exactly what let the
 * residency bug through: a lens that never builds a row looks identical to a
 * lens handed nothing to build one from. The static-fixture test needs a
 * payload with a REAL resident. */
const RESOURCES_WITH_RESIDENT = {
  ...RESOURCES,
  models: [
    {
      identifier: "darkmux:qwen3-4b-instruct-2507",
      model_key: "qwen3-4b-instruct-2507",
      owner: "darkmux",
      loaded_ctx: 120000,
      weights_bytes: 100,
      kv_per_token_bytes: 1,
      kv_bytes_at_ctx: 50,
      potential_bytes: 200,
      current_bytes: 150,
      state: "green",
    },
  ],
};

function staticMeta(name: string, content: string) {
  const el = document.createElement("meta");
  el.setAttribute("name", name);
  el.setAttribute("content", content);
  document.head.appendChild(el);
}

function mockMachineFetch(opts: {
  specs?: unknown;
  resources?: unknown;
  flowToday?: unknown[];
  flowYesterday?: unknown[];
  liveMachines?: unknown[];
  /** (#2019) The committed `{specs, resources}` a daemon-less build reads
   * from `darkmux-machine-src`, served here at that same path. */
  staticMachine?: unknown;
} = {}) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  const resourcesCalled = { value: false };
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const path = String(url);
      if (path === "/machine/specs") {
        return Promise.resolve(new Response(JSON.stringify(opts.specs ?? {}), { status: opts.specs === null ? 404 : 200 }));
      }
      if (path === "/machine/resources") {
        resourcesCalled.value = true;
        return Promise.resolve(new Response(JSON.stringify(opts.resources ?? RESOURCES), { status: 200 }));
      }
      if (path === `/flow/${today}`) return Promise.resolve(new Response(JSON.stringify(opts.flowToday ?? []), { status: 200 }));
      if (path === `/flow/${yesterday}`) return Promise.resolve(new Response(JSON.stringify(opts.flowYesterday ?? []), { status: 200 }));
      if (path === "/fleet/machines/live") {
        return Promise.resolve(
          new Response(
            JSON.stringify({ machines: opts.liveMachines ?? [], meta: { sources: { fleet: { state: "off" } }, complete: true } }),
            { status: 200 },
          ),
        );
      }
      if (path === "./demo-machine.json") {
        return Promise.resolve(
          new Response(JSON.stringify(opts.staticMachine ?? {}), { status: opts.staticMachine === undefined ? 404 : 200 }),
        );
      }
      if (path === "./demo-flow.jsonl") return Promise.resolve(new Response("", { status: 200 }));
      if (path === "/fleet/sessions/live") {
        return Promise.resolve(new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
      }
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
  return resourcesCalled;
}

describe("MachineLens", () => {
  it("uid: null (nav-tab/deep-link) is always the local machine — resources loads with real figures", async () => {
    const resourcesCalled = mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 } });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(resourcesCalled.value).toBe(true);
    expect(screen.queryByText(/not reported from here/i)).not.toBeInTheDocument();
  });

  it("a fleet-card drill into a REMOTE uid never fetches /machine/resources, and shows the honest not-reported line", async () => {
    const resourcesCalled = mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
    });
    renderMachine("remote-uid");
    // (#2108, operator finding) The machine NAME is no longer repeated in
    // this in-page header — `#crumb` (App.tsx, folded into the desktop tab
    // row) already states it. The header keeps "fleet › machine" plus the
    // hardware spec.
    await waitFor(() =>
      expect(document.querySelector(".machine-lens__hdr")?.textContent).toBe(
        "fleet › machine — M1 Max · 32 GB",
      ),
    );
    expect(screen.getByText(/residency \/ RAM not reported from here — local-probe only/i)).toBeInTheDocument();
    expect(screen.getByText(/View the machine page on studio directly/i)).toBeInTheDocument();
    expect(screen.queryByText(/limit source/i)).not.toBeInTheDocument();
    // The whole point of the gate — never even ISSUE the local probe request
    // for a page that can't honestly show its answer.
    expect(resourcesCalled.value).toBe(false);
    // A remote machine's own presence-beat specs string renders in the header.
    expect(screen.getByText(/M1 Max · 32 GB/)).toBeInTheDocument();
  });

  it("self-corrects to local figures when the drilled uid turns out to BE this machine (the OR-gate)", async () => {
    const resourcesCalled = mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 },
      flowToday: [{ ts: `${todayUTC()}T00:00:00Z`, machine_uid: "self-uid", machine_id: "MacBook-Pro" }],
    });
    // A fleet-card drill (uid explicit, machineIsLocal=false) into the uid
    // that resolves to THIS daemon's own specs.machine_id.
    renderMachine("self-uid");
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(resourcesCalled.value).toBe(true);
    expect(screen.queryByText(/not reported from here/i)).not.toBeInTheDocument();
  });

  it("(#1833) shows live CPU/GPU/MEM now/avg/max for this machine's own telemetry.process samples", async () => {
    // (rolling-window fix) The 10-minute window is measured against REAL
    // `Date.now()`, not the test's nominal "today" — a midnight-UTC
    // timestamp is routinely hours outside that window depending on when
    // the suite actually runs. Recent-relative-to-now instead.
    const oldIso = new Date(Date.now() - 5 * 60_000).toISOString();
    const newIso = new Date(Date.now() - 60_000).toISOString();
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 },
      flowToday: [
        { ts: newIso, machine_uid: "self-uid", machine_id: "MacBook-Pro" },
        // Distinct values per metric so each assertion below can only match
        // the ONE tile it names — cpu/mem/gpu never share an avg/now/max.
        { ts: oldIso, machine_uid: "self-uid", category: "telemetry", source: "process", action: "telemetry.process", payload: { cpu: 40, mem: 20, gpu: 60 } },
        { ts: newIso, machine_uid: "self-uid", category: "telemetry", source: "process", action: "telemetry.process", payload: { cpu: 80, mem: 50, gpu: 90 } },
        // A peer's own sample, same window — must NOT be averaged in.
        { ts: newIso, machine_uid: "peer-uid", category: "telemetry", source: "process", action: "telemetry.process", payload: { cpu: 999, mem: 999, gpu: 999 } },
      ],
    });
    renderMachine(null);
    // The section title renders unconditionally; the METER VALUES only once
    // specs has resolved `targetUid` to "self-uid" (an async render pass) —
    // wait on those, not the static title, so this isn't a false-pass on a
    // render that hasn't caught up yet.
    await waitFor(() => expect(screen.getByText("live load · last 10 min")).toBeInTheDocument());
    await waitFor(() => expect(document.querySelector(".mm-live-section .meter-now")?.textContent).toBe("80%"));

    const section = document.querySelector(".mm-live-section")!;
    const tile = (metric: string) => section.querySelector(`[data-meter="${metric}"]`)!;
    const avgmax = (metric: string) => tile(metric).querySelector(".meter-avgmax")!.textContent?.replace(/\s+/g, " ").trim();
    // cpu: [40, 80] — now (last) 80, avg 60, max 80.
    expect(tile("cpu").querySelector(".meter-now")!.textContent).toBe("80%");
    expect(avgmax("cpu")).toBe("60% avg · 80% max");
    // mem: [20, 50] — now 50, avg 35, max 50.
    expect(tile("mem").querySelector(".meter-now")!.textContent).toBe("50%");
    expect(avgmax("mem")).toBe("35% avg · 50% max");
    // gpu: [60, 90] — now 90, avg 75, max 90.
    expect(tile("gpu").querySelector(".meter-now")!.textContent).toBe("90%");
    expect(avgmax("gpu")).toBe("75% avg · 90% max");
    // The peer's 999s must never appear anywhere in this section.
    expect(section.textContent).not.toContain("999");
    // The VRAM gauge is untouched by this section — still rendered, same as
    // every other test in this file that reaches the health region.
    expect(screen.getByText(/limit source/i)).toBeInTheDocument();
  });

  it("an unrecognized/stale uid degrades gracefully — links to its (empty) runs lens by the raw uid, never crashes", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" } });
    renderMachine("totally-unknown-uid-nobody-has-ever-seen");
    // (#2108, operator finding) No name to show (unrecognized uid, no
    // display name resolved) and no hardware spec either — the header
    // degrades to plain "fleet › machine", never a crash or a stale label.
    await waitFor(() =>
      expect(document.querySelector(".machine-lens__hdr")?.textContent).toBe(
        "fleet › machine",
      ),
    );
    // (#1809) The old "no runs recorded for this machine" hint text is gone
    // with the runs list itself — a stale uid still gets a real, honestly
    // zero-count link out (never a crash, never a stale-looking count).
    // The raw uid survives HERE, in the link's own href, even though the
    // header above no longer names it.
    const link = screen.getByRole("link", { name: /runs on/i });
    expect(link).toHaveAttribute("href", "#lens=runs&machine=totally-unknown-uid-nobody-has-ever-seen");
    expect(screen.getByText(/not reported from here/i)).toBeInTheDocument();
  });

  /**
   * The merge-gate CONSIDER 5 finding: `data-state` is documented (see
   * `MachineLens.tsx`'s own comment above the health region) as the parity
   * harness's post-fetch SETTLED signal — but on a remote page `resources`
   * never fetches at all (`enabled: isLocalMach` gates the query off), so
   * the marker sat at "loading" forever even though the not-reported
   * placeholder had already rendered correctly. A future remote parity
   * test waiting on "loaded"/"error" would hang. A remote page must settle
   * on its own distinct value instead.
   */
  it("a remote machine page settles on data-state=\"remote\", never stuck at \"loading\"", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
    });
    renderMachine("remote-uid");
    await waitFor(() => expect(screen.getByText(/residency \/ RAM not reported from here/i)).toBeInTheDocument());
    expect(document.querySelector(".machine-lens__health")).toHaveAttribute("data-state", "remote");
  });

  it("the local machine page still settles on data-state=\"loaded\" once /machine/resources resolves", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 } });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(document.querySelector(".machine-lens__health")).toHaveAttribute("data-state", "loaded");
  });

  /**
   * (#2019) The SAME defect #1770 found for the remote page, on the OTHER
   * half of the same `enabled`. `resourcesQuery` is gated on
   * `isLocalMach && daemonBacked`; #1770 gave the first condition a settled
   * `"remote"` value, and the second never got one — so a daemon-less build
   * (darkmux.com/demo) left a disabled query at `status: "pending"` and the
   * lens rendered `loading…` forever, under a header reading "waiting for a
   * machine". Reported by the operator against the live site.
   *
   * Red-proven: reverting the `staticSettled` branch puts both of these back
   * at "loading".
   */
  it("a daemon-less build with NO captured fixture settles, never stuck at \"loading\"", async () => {
    staticMeta("darkmux-flow-src", "./demo-flow.jsonl");
    mockMachineFetch({ specs: { machine_id: "demo", cpu_brand: "M5 Ultra" } });
    renderMachine(null);
    await waitFor(() =>
      expect(document.querySelector(".machine-lens__health")).toHaveAttribute("data-state", "no-daemon"),
    );
  });

  it("a daemon-less build WITH a captured fixture reaches \"loaded\", same as a live probe", async () => {
    staticMeta("darkmux-flow-src", "./demo-flow.jsonl");
    staticMeta("darkmux-machine-src", "./demo-machine.json");
    mockMachineFetch({
      specs: { machine_id: "demo", cpu_brand: "M5 Ultra" },
      // Reuse the file's own realistic payload rather than hand-rolling a
      // thin one: `MachineHealthRegion` THREW on a minimal object, which is
      // its own small finding (a hand-edited `demo-machine.json` would crash
      // the lens rather than degrade) — but a fixture shaped unlike the real
      // response would be testing the fixture, not the code path.
      staticMachine: {
        specs: { machine_id: "m5-ultra-256gb", cpu_brand: "Apple M5 Ultra", ram_total_bytes: 274877906944 },
        resources: RESOURCES_WITH_RESIDENT,
      },
    });
    renderMachine(null);
    await waitFor(() =>
      expect(document.querySelector(".machine-lens__health")).toHaveAttribute("data-state", "loaded"),
    );

    // (#2021) `data-state="loaded"` alone did NOT catch the real defect. The
    // first cut fed the ledger from the fixture but never ran
    // `advanceResidency`, so the operator saw a correct gauge above a
    // residency section reading "no models loaded" while the fixture carried
    // four. Assert the ROWS, not just that the lens settled.
    await waitFor(() => expect(screen.getByText(/darkmux:qwen3-4b-instruct-2507/)).toBeInTheDocument());
    expect(screen.queryByText(/no models loaded/i)).not.toBeInTheDocument();
  });

  it("the 'fleet' back-link writes an empty hash", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" } });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    window.location.hash = "#lens=machine";
    screen.getByRole("button", { name: "fleet" }).click();
    expect(window.location.hash).toBe("");
  });
});

/**
 * The `darkmux/utility` CARD is gone from this page, and its absence is the
 * assertion. The operator's cut, after seeing it live: **it was config, not
 * machine state** — it described what the tier is responsible for, not how
 * it relates to this machine, and this page shows what is resident.
 *
 * Nothing needed re-homing. `resident` was already proven by the ledger
 * row's own existence; `not loaded` and `not configured` are config
 * questions `darkmux doctor` answers with a fix hint; `not reported`
 * duplicated the page-level not-local placeholder. What survives is one
 * badge on the row that was going to render anyway — covered in
 * `MachineHealthRegion.test.tsx`, with the id derivation in
 * `memoryLedgerLines.test.ts`.
 *
 * These tests exist so the card cannot quietly come back, and so the one
 * seam that replaced it — specs id → health region → row badge — is proven
 * end-to-end through the real component rather than only in unit isolation.
 */
describe("MachineLens — the utility tier is a row badge, not a card", () => {
  // A ledger carrying the configured tier as a real resident row — the only
  // arrangement in which a badge can legitimately appear.
  const RESIDENT_UTILITY = {
    ...RESOURCES,
    models: [
      {
        identifier: "darkmux:qwen3-4b",
        model_key: "qwen3-4b",
        owner: "darkmux",
        loaded_ctx: 120000,
        weights_bytes: 100,
        kv_per_token_bytes: 1,
        kv_bytes_at_ctx: 50,
        potential_bytes: 200,
        current_bytes: 150,
        state: "green",
      },
    ],
  };
  const withUtility = {
    specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472, utility_model: { id: "darkmux:qwen3-4b", loaded: true } },
    resources: RESIDENT_UTILITY,
  };

  it("renders NO utility card, in the state that used to render the fullest one", async () => {
    mockMachineFetch(withUtility);
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(container.querySelector(".machine-lens__util")).toBeNull();
    expect(container.querySelector(".mm-util-hdr")).toBeNull();
    // The card's own copy, gone with it — `handles` was the clearest case of
    // documentation pretending to be instrumentation.
    expect(container.textContent).not.toContain("internal small-model tier");
    expect(container.textContent).not.toContain("mission-compile");
  });

  it("badges the configured tier's row instead — the seam that replaced the card", async () => {
    mockMachineFetch(withUtility);
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    const row = [...container.querySelectorAll(".mm-row")].find((r) => r.textContent?.includes("darkmux:qwen3-4b"))!;
    expect(row).toBeTruthy();
    const chip = [...row.querySelectorAll(".mm-row-chip")].find((c) => c.textContent === "utility")!;
    expect(chip).toBeTruthy();
    // Identity, never a health verdict — no severity class.
    // Identity, never a health verdict: it carries the identity treatment
    // (filled + achromatic) and NONE of the severity classes.
    expect(chip.className).toBe("mm-row-chip is-identity");
    expect(chip.className).not.toMatch(/is-(green|amber|red|state|warn|new)\b/);
    // The gloss the card used to spend a line on survives as the title.
    expect(chip.getAttribute("title")).toContain("small-model tier");
  });

  it("the inverted case: a machine with no utility tier configured badges nothing", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472, utility_model: null },
      resources: RESIDENT_UTILITY, // the row is THERE; only the binding is absent
    });
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect([...container.querySelectorAll(".mm-row-chip")].some((c) => c.textContent === "utility")).toBe(false);
    expect(container.querySelector(".machine-lens__util")).toBeNull();
  });
});

function renderMachineLens(isMobileOverride?: boolean) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens uid={null} isMobileOverride={isMobileOverride} />
    </QueryClientProvider>,
  );
}

/** One flow record per session. */
function machineFlowRecord(sessionId: string) {
  return {
    ts: `${todayUTC()}T10:00:00.000Z`,
    machine_uid: "u1",
    machine_id: "MacBook-Pro",
    session_id: sessionId,
    handle: "coder",
    model: "qwen3.6-35b-a3b",
  };
}

function mockMachineRunsFetch(runCount: number) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  const records = Array.from({ length: runCount }, (_, i) => machineFlowRecord(`s${i}`));
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const path = String(url);
      if (path === `/flow/${today}`) return Promise.resolve(new Response(JSON.stringify(records), { status: 200 }));
      if (path === `/flow/${yesterday}`) return Promise.resolve(new Response(JSON.stringify([]), { status: 200 }));
      if (path === "/fleet/machines/live") {
        return Promise.resolve(
          new Response(JSON.stringify({ machines: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }),
        );
      }
      if (path === "/machine/specs") {
        return Promise.resolve(new Response(JSON.stringify({ machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" }), { status: 200 }));
      }
      if (path === "/machine/resources") {
        return Promise.resolve(
          new Response(
            JSON.stringify({
              schema_version: "1.0",
              generated_at_ms: 1,
              gather_ms: 1,
              limit_bytes: 1000,
              limit_source: "test",
              pool: { capacity_bytes: 1000, used_bytes: 600, available_bytes: 500, free_bytes: 400 },
              pressure: { swap_used_bytes: 0, compressor_bytes: 0, margin_percent: 50, red: false },
              models: [],
              machine: { potential_bytes: 0, unpriced_models: 0, current_bytes: 0, state: "green" },
            }),
            { status: 200 },
          ),
        );
      }
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
}

// (#1809, finishing #1508 step 4) The old "RUNS ON <MACHINE>" list — and
// the "show all"/"show fewer" expand-collapse control the a11y packet
// (#1767) hardened keyboard access for — is gone; this section replaces
// those tests with coverage of what replaced it: a link out to the runs
// lens, pinned to this machine.
//
// The link carries NO COUNT, and these tests pin that absence deliberately.
// An earlier cut labeled it with `sessionsOn(...).length` and this suite
// asserted the number — which passed while the live page LIED: that counts
// distinct session ids in the 24h flow window, the destination lists /runs
// rows over a 14-day window unioning missions, lab runs and ghosts. Measured
// on a real daemon: the link read "0 runs" while the destination listed 282.
// The tests were green the whole time, because they seeded the flow window
// and asserted against the same window — never against what the destination
// would actually show. A test that pins a number to its own fixture cannot
// catch a number that means the wrong thing.
describe("MachineLens — the runs-lens link (#1809)", () => {
  it("names the machine and points at the pinned runs lens", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link).toHaveAttribute("href", "#lens=runs&machine=u1");
  });

  // The regression guard for the lie described above: whatever the flow
  // window happens to hold, the label must not claim a quantity. Seeded with
  // three sessions precisely because the old implementation would have
  // rendered "3" here.
  it("claims no count, whatever this window's session total happens to be", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link.textContent).toBe("runs on MacBook-Pro →");
    expect(link.textContent).not.toMatch(/\d/);
  });

  it("renders the same label with an EMPTY window — no count to be wrong, nothing hidden", async () => {
    mockMachineRunsFetch(0);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link.textContent).toBe("runs on MacBook-Pro →");
  });

  it("clicking the link navigates via a real hash write, not a page reload", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    fireEvent.click(link);
    expect(window.location.hash).toBe("#lens=runs&machine=u1");
  });

  // Red-provable regression guard: if the old list ever comes back, this
  // fails — proving the removal actually happened, not just that the link
  // exists alongside it.
  it("the old per-run list markup is gone — no '.machine-lens__run' rows, no 'RUNS ON' header", async () => {
    mockMachineRunsFetch(3);
    const { container } = renderMachineLens();
    await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(container.querySelector(".machine-lens__run")).toBeNull();
    expect(screen.queryByText(/RUNS ON/)).not.toBeInTheDocument();
  });

  // (#2108, operator finding) On a phone this control sat at roughly half
  // the card's width with dead space beside it. On a narrow viewport it
  // gets the `--mobile` full-width/48px-touch-target class, and the
  // accessible name — arrow included — stays exactly what it was.
  it("gets the full-width mobile class on a narrow viewport, keeping the same accessible name", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens(true);
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link.className).toMatch(/\bmachine-lens__runslink--mobile\b/);
    expect(link.className).toMatch(/\bmachine-lens__runslink\b/);
    expect(link.textContent).toBe("runs on MacBook-Pro→");
  });

  it("stays the plain desktop class (no --mobile modifier) when not on a narrow viewport", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens(false);
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link.className).not.toMatch(/--mobile/);
    expect(link.textContent).toBe("runs on MacBook-Pro →");
  });
});

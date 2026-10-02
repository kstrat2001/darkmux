import type { BatteryCharge } from "../../types/generated/BatteryCharge";
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { MachineLens } from "./MachineLens";
import { todayUTC, prevDateUTC } from "../../lib/flow";

function renderMachine(uid: string | null) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens machineKey={uid} />
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
  /** (#2921 follow-up) `GET /fleet/roster` entries. */
  roster?: unknown[];
  /** (#3022) `GET /fleet/view` rows. */
  fleetView?: unknown[];
  /** (#2958) The flow window never answers (`true`), or answers once the
   *  promise resolves. */
  holdFlow?: boolean | Promise<void>;
  /** (#2958) Paths whose answer waits on the given promise. */
  hold?: Record<string, Promise<void>>;
  /** (#2965) Both `/flow/<day>` reads answer with this HTTP error status. */
  failFlow?: number;
} = {}) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  const resourcesCalled = { value: false };
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string): Promise<Response> => {
      const held = opts.hold?.[String(url)];
      if (held) return held.then(() => answer(url));
      return answer(url);
    }),
  );
  return resourcesCalled;
  function answer(url: string): Promise<Response> {
    const path = String(url);
    if (path === "/machine/specs") {
      return Promise.resolve(new Response(JSON.stringify(opts.specs ?? {}), { status: opts.specs === null ? 404 : 200 }));
    }
    if (path === "/machine/resources") {
      resourcesCalled.value = true;
      return Promise.resolve(new Response(JSON.stringify(opts.resources ?? RESOURCES), { status: 200 }));
    }
    if (opts.failFlow && (path === `/flow/${today}` || path === `/flow/${yesterday}`)) {
      return Promise.resolve(new Response("boom", { status: opts.failFlow, statusText: "Internal Server Error" }));
    }
    if (opts.holdFlow && (path === `/flow/${today}` || path === `/flow/${yesterday}`)) {
      if (opts.holdFlow === true) return new Promise<Response>(() => {});
      return opts.holdFlow.then(() => new Response(JSON.stringify(path === `/flow/${today}` ? (opts.flowToday ?? []) : (opts.flowYesterday ?? [])), { status: 200 }));
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
    if (path === "/fleet/dispatches/live") {
      return Promise.resolve(new Response(JSON.stringify({ dispatches: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
    }
    if (path === "/fleet/view" && opts.fleetView) {
      return Promise.resolve(new Response(JSON.stringify({ machines: opts.fleetView }), { status: 200 }));
    }
    if (path === "/fleet/roster" && opts.roster) {
      return Promise.resolve(new Response(JSON.stringify({ machines: opts.roster, error: null }), { status: 200 }));
    }
    return Promise.resolve(new Response("not recorded\n", { status: 404 }));
  }
}

/** (#2958) A promise and the function that resolves it. */
function gate(): { promise: Promise<void>; open: () => void } {
  let open = () => {};
  const promise = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { promise, open };
}

describe("MachineLens", () => {
  // (#2921 follow-up) A remote machine known only by its hardware uid: the
  // placeholder names it from the roster when it can, and otherwise says how
  // to name it; it never prints the uid.
  const FAKE_UID = "00000000-0000-4000-8000-ABCDEF000001";
  const uidOnly = () => [{ ts: new Date(Date.now() - 60_000).toISOString(), action: "dispatch.turn", machine_uid: FAKE_UID }];
  const UUID_RE = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;
  it("(#2921) a uid-only remote machine with a roster entry is named by it", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro" }, flowToday: uidOnly(), roster: [{ id: "studio", address: "a:1", added_unix_ms: 1, machine_uid: FAKE_UID }] });
    renderMachine(FAKE_UID);
    await waitFor(() => expect(screen.getByText(/View the machine page on studio directly/i)).toBeInTheDocument());
    expect(UUID_RE.test(document.body.textContent ?? "")).toBe(false);
  });
  it("(#2921) a uid-only remote machine with no name says how to name it", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro" }, flowToday: uidOnly() });
    renderMachine(FAKE_UID);
    await waitFor(() => expect(screen.getByText(/darkmux config set machine_id <name>/)).toBeInTheDocument());
    expect(UUID_RE.test(document.body.textContent ?? "")).toBe(false);
  });

  // (#3022) The header says HUB on the page of a machine whose card declares
  // it, and only there.
  describe("the HUB badge", () => {
    const row = (mode: "hub" | "peer", over: Record<string, unknown> = {}) => ({
      entry: null,
      is_this_machine: false,
      machine_uid: "u-mini",
      card: { state: "available", card: { specs: { machine_uid: "u-mini" }, fleet_mode: mode }, source: "listener" },
      ...over,
    });

    it("shows on a drilled machine that declares hub", async () => {
      mockMachineFetch({ specs: { machine_id: "MacBook-Pro", machine_uid: "u-self" }, fleetView: [row("hub")] });
      renderMachine("u-mini");
      await waitFor(() => expect(document.querySelector('.machine-lens__hdr [data-testid="hub-badge"]')).not.toBeNull());
    });

    it("shows on this machine's own page when its own row declares hub", async () => {
      mockMachineFetch({
        specs: { machine_id: "MacBook-Pro", machine_uid: "u-self" },
        fleetView: [row("hub", { is_this_machine: true, machine_uid: "u-self" })],
      });
      renderMachine(null);
      await waitFor(() => expect(document.querySelector('.machine-lens__hdr [data-testid="hub-badge"]')).not.toBeNull());
    });

    it("never shows on a peer's page, nor on the page of a machine other than the hub", async () => {
      mockMachineFetch({ specs: { machine_id: "MacBook-Pro", machine_uid: "u-self" }, fleetView: [row("hub")] });
      renderMachine("u-studio");
      await waitFor(() => expect(document.querySelector(".machine-lens__hdr")).not.toBeNull());
      // The view was asked and answered before the absence is asserted.
      const asked = () => (fetch as unknown as { mock: { calls: string[][] } }).mock.calls.some((c) => c[0] === "/fleet/view");
      await waitFor(() => expect(asked()).toBe(true));
      await new Promise((r) => setTimeout(r, 50));
      expect(document.querySelector('[data-testid="hub-badge"]')).toBeNull();
    });
  });

  // (5.0 R3) The header's hardware line is this machine's own, or "not reported".
  describe("the hardware line", () => {
    const peerRow = (specs: Record<string, unknown> | null) => ({
      entry: null,
      is_this_machine: false,
      machine_uid: "u-mini",
      liveness: "no_beat",
      card: specs ? { state: "available", card: { specs: { machine_uid: "u-mini", ...specs }, fleet_mode: "peer" }, source: "listener" } : { state: "unreachable", reason: "listener_off", detail: null },
    });
    const hdr = () => document.querySelector(".machine-lens__hdr")?.textContent;

    it("a peer with no beat reads its hardware off the card the view read", async () => {
      mockMachineFetch({ specs: { machine_id: "MacBook-Pro", machine_uid: "u-self" }, fleetView: [peerRow({ cpu_brand: "M1 Max", ram_total_bytes: 34359738368 })] });
      renderMachine("u-mini");
      await waitFor(() => expect(hdr()).toBe("fleet › machine — M1 Max · 32 GB"));
    });

    it("the card read now outranks a presence beat's older hardware string", async () => {
      mockMachineFetch({
        specs: { machine_id: "MacBook-Pro", machine_uid: "u-self" },
        liveMachines: [{ machine_uid: "u-mini", display_name: "mini", schema_version: "1", beat_ts_ms: 1, specs: "M1 · 16 GB" }],
        fleetView: [peerRow({ cpu_brand: "M4 Pro", ram_total_bytes: 51539607552 })],
      });
      renderMachine("u-mini");
      await waitFor(() => expect(hdr()).toBe("fleet › machine — M4 Pro · 48 GB"));
    });

    it("a peer nothing reported hardware for says so, never blank and never this machine's hardware", async () => {
      mockMachineFetch({ specs: { machine_id: "MacBook-Pro", machine_uid: "u-self", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 }, fleetView: [peerRow(null)] });
      renderMachine("u-mini");
      await waitFor(() => expect(hdr()).toBe("fleet › machine — hardware not reported"));
    });
  });

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
    expect(screen.getByText(/residency \/ RAM not reported from here: local-probe only/i)).toBeInTheDocument();
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

  // (#2814) The same OR-gate, with the flow window EMPTY — the state this
  // machine is in on a fresh install, with presence off, or once its last
  // record has aged out of retention.
  //
  // The test above resolves `localUid` from a flow record; that is an
  // OBSERVATION and it expires. With no record to observe, `localMachineUid`
  // used to fall through to `?? machineId` and hand back the NAME as if it
  // were a uid, so `targetUid === localUid` compared a uid to a name, was
  // false, and this machine classified ITSELF as remote: no residency
  // ledger, no utility model, and a note advising the operator to open the
  // machine page on the machine they were already sitting on.
  //
  // `/machine/specs` reporting `machine_uid` makes the comparison a uid-to-uid
  // one with no window involved. This exercises the CALL-SITE WIRING, not the
  // helper — a mutation sweep found `localMachineUid`'s own unit tests all
  // stayed green while both of its call sites dropped the argument entirely.
  it("(#2814) self-corrects on an EMPTY flow window, from the uid /machine/specs reports", async () => {
    const resourcesCalled = mockMachineFetch({
      specs: {
        machine_id: "MacBook-Pro",
        machine_uid: "self-uid",
        cpu_brand: "M5 Max",
        ram_total_bytes: 137438953472,
      },
      // No flow records, no presence beats. Only the daemon's own probe.
    });
    renderMachine("self-uid");
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(resourcesCalled.value).toBe(true);
    expect(screen.queryByText(/not reported from here/i)).not.toBeInTheDocument();
    expect(document.querySelector(".machine-lens__health")?.getAttribute("data-state")).not.toBe("remote");
  });

  // (#2814) `label` (`displayNameOf(targetUid)`) used to be user-visible
  // ONLY via the "runs on <machine> →" link (removed 2026-09-23); on a
  // LOCAL render (this test) it has no other visible consumer today, so
  // the positive "the resolved name shows up as text" half of this
  // regression check no longer has anywhere to assert against. The
  // NEGATIVE half — the raw uid must never leak into the page at all —
  // stays meaningful on its own and is still worth pinning. The pure
  // resolution logic itself (`displayNameOf` is a FLOOR, an observed name
  // outranks the specs name, self-vs-other) is fully covered without a
  // DOM render in `ui/src/lib/flow.test.ts`'s own `(#2814)`-tagged tests.
  it("(#2814) never leaks the raw hardware uid into the page, even when the window names nothing", async () => {
    const uid = "00000000-0000-4000-8000-ABCDEF000011";
    mockMachineFetch({
      specs: {
        machine_id: "MacBook-Pro",
        machine_uid: uid,
        cpu_brand: "M5 Max",
        ram_total_bytes: 137438953472,
      },
    });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(document.body.textContent).not.toContain(uid.slice(0, 8));
  });

  // Inverted, so the fix cannot be "treat every drilled uid as local": a
  // genuinely remote uid on the same empty window must still read remote.
  it("(#2814) a remote uid on an empty window still reads remote, with specs reporting its own uid", async () => {
    const resourcesCalled = mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", machine_uid: "self-uid", cpu_brand: "M5 Max" },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
    });
    renderMachine("remote-uid");
    await waitFor(() =>
      expect(document.querySelector(".machine-lens__health")?.getAttribute("data-state")).toBe("remote"),
    );
    expect(resourcesCalled.value).toBe(false);
  });

  it("(#1833) shows live CPU/GPU/MEM now/avg/max for this machine's own machine.telemetry samples", async () => {
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
        { ts: oldIso, machine_uid: "self-uid", category: "machinery", source: "host", action: "machine.telemetry", payload: { cpu_pct: 40, mem_pct: 20, gpu_pct: 60 } },
        { ts: newIso, machine_uid: "self-uid", category: "machinery", source: "host", action: "machine.telemetry", payload: { cpu_pct: 80, mem_pct: 50, gpu_pct: 90 } },
        // A peer's own sample, same window — must NOT be averaged in.
        { ts: newIso, machine_uid: "peer-uid", category: "machinery", source: "host", action: "machine.telemetry", payload: { cpu_pct: 999, mem_pct: 999, gpu_pct: 999 } },
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

  it("an unrecognized/stale uid degrades gracefully — never crashes", async () => {
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

  it("(#2915) the Utility section names the model, its window and residency with the row's footprint; no row badge", async () => {
    mockMachineFetch({ ...withUtility, specs: { ...withUtility.specs, utility_model: { id: "darkmux:qwen3-4b", loaded: true, n_ctx: 120000 } } });
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    const section = await waitFor(() => {
      const el = container.querySelector('[data-testid="machine-utility"]');
      expect(el).toBeTruthy();
      return el as HTMLElement;
    });
    expect(section.querySelector(".mm-utility__id")?.textContent).toBe("darkmux:qwen3-4b");
    expect(section.querySelector(".mm-utility__facts")?.textContent).toMatch(/^window 120,000 · resident · [\d.]+ (B|KiB|MiB|GiB)$/);
    expect(section.querySelector(".mm-utility__live")?.textContent).toBe("idle");
    // Every known job is listed, at zero with no usage in the window.
    expect([...section.querySelectorAll(".mm-utility__job")].map((r) => r.textContent)).toEqual([
      "compacting0 calls0 tokens",
      "radio routing0 calls0 tokens",
      "other0 calls0 tokens",
    ]);
    expect([...container.querySelectorAll(".mm-row-chip")].some((c) => c.textContent === "utility")).toBe(false);
  });

  it("(#2958) the Utility section says checking… and '—' counts while the flow window is unanswered, then its reading", async () => {
    let open = () => {};
    const flow = new Promise<void>((r) => {
      open = r;
    });
    mockMachineFetch({ ...withUtility, holdFlow: flow });
    const { container } = renderMachine(null);
    const section = await waitFor(() => {
      const el = container.querySelector('[data-testid="machine-utility"]');
      expect(el).toBeTruthy();
      return el as HTMLElement;
    });
    expect(section.querySelector(".mm-utility__live")?.textContent).toBe("checking…");
    expect(section.querySelector(".mach-util")?.getAttribute("aria-label")).toMatch(/checking…$/);
    // The model comes from /machine/specs, a reading: it shows at once.
    expect(section.querySelector(".mm-utility__id")?.textContent).toBe("darkmux:qwen3-4b");
    expect([...section.querySelectorAll(".mm-utility__job")].map((r) => r.textContent)).toEqual(["compacting——", "radio routing——", "other——"]);

    open();
    await waitFor(() => expect(section.querySelector(".mm-utility__live")?.textContent).toBe("idle"));
    expect([...section.querySelectorAll(".mm-utility__job")].map((r) => r.textContent)).toEqual([
      "compacting0 calls0 tokens",
      "radio routing0 calls0 tokens",
      "other0 calls0 tokens",
    ]);
  });

  it("(#2958) a remote machine page says checking…, not idle, while the flow window is unanswered", async () => {
    let open = () => {};
    const flow = new Promise<void>((r) => {
      open = r;
    });
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
      holdFlow: flow,
    });
    const { container } = renderMachine("remote-uid");
    await waitFor(() => expect(container.querySelector(".machine-drawer__idle-line")).not.toBeNull());
    expect(container.querySelector(".machine-drawer__idle-line")!.textContent).toBe("checking…");
    const section = container.querySelector('[data-testid="machine-utility"]')!;
    expect(section.querySelector(".mm-utility__id")?.textContent).toBe("—");
    expect(section.querySelector(".mm-utility__live")?.textContent).toBe("checking…");

    open();
    await waitFor(() => expect(container.querySelector(".machine-drawer__idle-line")!.textContent).toBe("idle · no samples in the last 10 min"));
    expect(section.querySelector(".mm-utility__id")?.textContent).toBe("no utility model seen");
  });

  // (5.0 R3) A peer nothing reaches this viewer from (no record, no beat) is
  // not "idle with 0 calls": the page says its data is not reported.
  it("(5.0 R3) a peer with no records and no beat reads not reported, never idle or 0 calls", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", machine_uid: "u-self" },
      roster: [{ id: "darkbook", address: "100.64.0.9:8765", added_unix_ms: 1, machine_uid: "u-quiet" }],
      flowToday: [{ ts: new Date().toISOString(), machine_uid: "U-SELF", machine_id: "MacBook-Pro", action: "dispatch.start", session_id: "s1" }],
    });
    const { container } = renderMachine("u-quiet");
    await waitFor(() => expect(container.querySelector(".machine-drawer__idle-line")!.textContent).toMatch(/not reported/));
    const section = container.querySelector('[data-testid="machine-utility"]')!;
    expect(section.querySelector(".mm-utility__id")?.textContent).toBe("not reported");
    expect(section.querySelector(".mm-utility__live")?.textContent).toBe("not reported");
    expect([...section.querySelectorAll(".mm-utility__job")].map((r) => r.textContent)).toEqual(["compacting——", "radio routing——", "other——"]);
    expect(container.textContent).not.toMatch(/0 calls|idle/);
  });

  // (#2965) A failed flow read settles the window, but it is not an answer
  // that nothing happened: the page's "idle" lines are negative claims about
  // exactly the records that are missing. They hold "checking…", as on the
  // fleet page, and the app-level `FlowReadNotice` names the failure.
  it("(#2965) a remote machine page says checking…, not idle, when the flow read fails", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
      failFlow: 500,
    });
    const { container } = renderMachine("remote-uid");
    await waitFor(() => expect(vi.mocked(fetch).mock.calls.some(([u]) => String(u) === `/flow/${todayUTC()}`)).toBe(true));
    await waitFor(() => expect(container.querySelector(".machine-drawer__idle-line")).not.toBeNull());
    // Let every read settle before judging the page: the failure is an answer.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(container.querySelector(".machine-drawer__idle-line")!.textContent).toBe("checking…");
    const section = container.querySelector('[data-testid="machine-utility"]')!;
    expect(section.querySelector(".mm-utility__live")?.textContent).toBe("checking…");
  });

  it("(#2965) this machine's Utility section says checking…, not idle, when the flow read fails", async () => {
    mockMachineFetch({ ...withUtility, failFlow: 500 });
    const { container } = renderMachine(null);
    const section = await waitFor(() => {
      const el = container.querySelector('[data-testid="machine-utility"]');
      expect(el).toBeTruthy();
      return el as HTMLElement;
    });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(section.querySelector(".mm-utility__live")?.textContent).toBe("checking…");
    expect([...section.querySelectorAll(".mm-utility__job")].map((r) => r.textContent)).toEqual(["compacting——", "radio routing——", "other——"]);
  });

  // (#2958 second review, point 2) Only the FIRST answer counts, as on the
  // fleet page: at UTC midnight the flow window rolls to a new day's key,
  // which starts out pending, and the page must not go back to "checking…".
  it("(#2958) does not return to 'checking…' when the flow window rolls to a new day at UTC midnight", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    try {
      vi.setSystemTime(new Date("2026-06-15T23:59:58.000Z"));
      const nextDay = gate();
      mockMachineFetch({ ...withUtility, hold: { "/flow/2026-06-16": nextDay.promise } });
      const { container } = renderMachine(null);
      const live = () => container.querySelector('[data-testid="machine-utility"] .mm-utility__live')?.textContent;
      await waitFor(() => expect(live()).toBe("idle"));
      vi.setSystemTime(new Date("2026-06-16T00:00:03.000Z"));
      await act(async () => {
        vi.advanceTimersByTime(5_000);
      });
      // The rollover really happened: the new day's window was asked for.
      await waitFor(() => expect(vi.mocked(fetch).mock.calls.some(([u]) => String(u) === "/flow/2026-06-16")).toBe(true));
      expect(live()).toBe("idle");
      expect([...container.querySelectorAll(".mm-utility__job")].map((r) => r.textContent)).not.toContain("compacting——");
    } finally {
      vi.useRealTimers();
    }
  });

  // (#2958 second review, point 3) A fleet-card drill into THIS machine is
  // told apart from a remote one by `/machine/specs`. Until it answers, the
  // page must not say "another machine" or give a remote machine's idle line.
  it("(#2958) a drill into this machine makes no remote claims while /machine/specs is unanswered", async () => {
    const specs = gate();
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", machine_uid: "self-uid", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 },
      flowToday: [{ ts: `${todayUTC()}T00:00:00Z`, machine_uid: "self-uid", machine_id: "MacBook-Pro" }],
      hold: { "/machine/specs": specs.promise },
    });
    const { container } = renderMachine("self-uid");
    const section = await waitFor(() => {
      const el = container.querySelector('[data-testid="machine-utility"]');
      expect(el).not.toBeNull();
      return el!;
    });
    await waitFor(() => expect(section.querySelector(".mm-utility__live")?.textContent).toBe("idle"));
    expect(container.textContent).not.toContain("another machine");
    expect(container.textContent).not.toContain("not reported from here");
    expect(container.querySelector(".machine-lens__health")?.getAttribute("data-state")).toBe("loading");
    expect(container.textContent).not.toContain("no samples in the last 10 min");
    expect(section.querySelector(".mm-utility__facts")?.textContent).toBe("window — · —");
    expect(container.querySelector(".machine-drawer__idle-line")?.textContent).toBe("checking…");

    specs.open();
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(container.textContent).not.toContain("another machine");
  });

  it("the inverted case: a machine with no utility tier configured says so", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472, utility_model: null },
      resources: RESIDENT_UTILITY, // the row is THERE; only the binding is absent
    });
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect([...container.querySelectorAll(".mm-row-chip")].some((c) => c.textContent === "utility")).toBe(false);
    expect(container.querySelector(".mm-utility__facts")?.textContent).toBe("window — · no utility model registered");
  });
});

// (#1809 shipped the runs-lens link this file used to test here; removed
// 2026-09-23 — operator: "the runs link is out of place. probably just
// remove it." `renderMachineLens`/`mockMachineRunsFetch`/
// `machineFlowRecord` and the whole "MachineLens — the runs-lens link"
// describe block existed solely to test it and are gone with it — a
// bordered `<a>` this lens no longer renders has nothing left to assert.)

// Red-provable regression guard, the same shape #1809's own now-deleted
// test used against the list it replaced: if the runs-lens link markup
// ever comes back, this fails.
describe("MachineLens — the runs-lens link is gone (2026-09-23)", () => {
  it("renders no runs-lens link anywhere on the page", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 } });
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(container.querySelector(".machine-lens__runslink")).toBeNull();
    expect(screen.queryByRole("link", { name: /runs on/i })).toBeNull();
  });
});

// ── (#2108, operator design rule — "the lens is a strict SUPERSET of the
//    sheet") The LOCAL machine page renders the SAME shared
//    `useMachineStatsContent` block (`liveBlock`) the global sheet/dialog
//    render, in place of the old flow-aggregation-only CPU/GPU/MEM
//    section — gaining thermal/power/CPU-cluster for free, fed by the
//    daemon ring. ──────────────────────────────────────────────────────

const LOAD_WITH_EXTRAS = {
  now: {
    sampled_at_ms: 4000,
    sampler_cost_ms: 4.2,
    cpu_pct: 22,
    cpu_clusters: [
      { name: "Super", cores: 6, pct: 46, mhz: 4400 },
      { name: "Performance", cores: 12, pct: 22, mhz: 3400 },
    ],
    mem_pct: 46,
    gpu_pct: 68,
    gpu_mhz: null,
    gpu_mem_bytes: null,
    thermal: { state: "fair", cpu_speed_limit_pct: 87 },
    power_mw: null,
    battery: { charge_pct: 78, on_ac: false, state: "discharging", minutes_to_empty: 130 },
  },
  window: {
    samples: 3,
    interval_ms: 2000,
    span_ms: 90_000,
    cpu_pct: { mean: 10, p95: 15, max: 20 },
    mem_pct: { mean: 30, p95: 35, max: 40 },
    gpu_pct: { mean: 50, p95: 55, max: 60 },
    power_mw: null,
    thermal: null,
    energy_mwh: null,
  },
  // (#2821) The Step-0 regression fixture, verbatim: the raw IOKit
  // `condition` disagrees with the computed `condition_word` — the lens
  // must render the LATTER as "condition", never the former.
  battery_health: {
    cycle_count: 28,
    design_capacity_mah: 6249,
    raw_max_capacity_mah: 5701,
    nominal_charge_capacity_mah: 5853,
    raw_capacity_pct: 91.2,
    nominal_capacity_pct: 93.7,
    condition: "Check Battery",
    condition_word: "Normal",
    permanent_failure_status: 0,
    temperature_c: 31.0,
    time_at_soc_ms: [36000000, 72000000, 144000000, 18000000],
    total_operating_ms: 19324800000,
  },
};

const LOAD_NO_BATTERY = {
  ...LOAD_WITH_EXTRAS,
  now: { ...LOAD_WITH_EXTRAS.now, battery: null },
  battery_health: null,
};

describe("MachineLens — battery surfaces (#2821, lens only)", () => {
  it("shows the charge bar (gradient fill, no icon while discharging), the time-left text, and the health row", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 },
      resources: { ...RESOURCES, load: LOAD_WITH_EXTRAS },
    });
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    // "Battery" — the raw DOM text; `.hx-section__title`'s uppercase is a
    // CSS `text-transform`, invisible to jsdom/RTL's textContent-based
    // queries (the parity golden, captured via a real browser's
    // `innerText`, is where "BATTERY" is the correct assertion instead).
    expect(screen.getByText("Battery")).toBeInTheDocument();
    expect(screen.getByText("78%")).toBeInTheDocument();
    expect(screen.getByText("2 h 10 m left")).toBeInTheDocument();
    expect(container.querySelector(".battery-bar-icon")).toBeNull(); // discharging: no icon
    expect(container.querySelector(".battery-bar-fill")!.getAttribute("fill")).toMatch(/^url\(#mm-battery-ramp\)$/);
    // The computed condition_word ("Normal"), never the raw unreliable
    // IOKit string ("Check Battery") — the Step-0 regression this issue
    // exists to fix.
    expect(screen.getByText("Normal")).toBeInTheDocument();
    expect(screen.queryByText(/Check Battery/)).toBeNull();
    expect(screen.getByText(/^max charge$/i)).toBeInTheDocument();
    expect(screen.getByText("5,701 mAh · 91.2%")).toBeInTheDocument();
    expect(screen.getByText(/^original capacity$/i)).toBeInTheDocument();
    expect(screen.getByText("6,249 mAh")).toBeInTheDocument();
    expect(screen.getByText("28")).toBeInTheDocument();
    expect(screen.getByText("31.0 °C")).toBeInTheDocument();
    // (#2821, operator, 2026-09-23) The per-bucket histogram was pulled —
    // `time_at_soc_ms` is an undocumented flat array that likely
    // collapses a 2D table, so a chart of it overclaims. Only the lifetime
    // cross-check total renders now.
    expect(screen.getByText("5,368 h")).toBeInTheDocument();
    expect(screen.queryByText(/state-of-charge bands/)).toBeNull();
  });

  it("renders no battery section at all on a machine with no battery", async () => {
    mockMachineFetch({
      specs: { machine_id: "Mac-Studio", cpu_brand: "M1 Max", ram_total_bytes: 34359738368 },
      resources: { ...RESOURCES, load: LOAD_NO_BATTERY },
    });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    expect(screen.queryByText("Battery")).toBeNull();
    expect(screen.queryByText(/operating time/i)).toBeNull();
  });

  it("warns on Service Battery", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      resources: {
        ...RESOURCES,
        load: {
          ...LOAD_WITH_EXTRAS,
          battery_health: { ...LOAD_WITH_EXTRAS.battery_health, condition_word: "Service Battery" },
        },
      },
    });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText("Service Battery")).toBeInTheDocument());
    expect(screen.getByText("Service Battery").closest(".dialog__kv")).toHaveClass("dialog__kv--warn");
  });

  // ── The battery BAR's own arc of history: dial -> solid bar -> reversed
  //    gradient (operator, 2026-09-24: "the solid meters do not indicate
  //    when things are getting tight... give every small meter the same
  //    gradient treatment the big memory gauge uses"). No discrete
  //    warn/critical threshold on the fill OR the percent text any more —
  //    the reversed ramp (red empty -> green full) carries that
  //    continuously, state-invariant (same on AC or discharging). ───────

  function machineWithBattery(battery: {
    charge_pct: number;
    on_ac: boolean;
    /** Test shorthand only: `true` means state "charging". Never sent on the wire. */
    charging?: boolean;
    state?: BatteryCharge["state"];
    minutes_to_empty?: number | null;
  }) {
    const { charging, ...wire } = battery;
    return {
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      resources: {
        ...RESOURCES,
        load: { ...LOAD_WITH_EXTRAS, now: { ...LOAD_WITH_EXTRAS.now, battery: { state: charging ? "charging" : "unknown", minutes_to_empty: null, ...wire } } },
      },
    };
  }

  it("fill width tracks the charge percent — 100% fills more than 35%", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 100, on_ac: true, charging: false }));
    const { container: c100 } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("100%")).toBeInTheDocument());
    const w100 = Number(c100.querySelector(".battery-bar-fill")!.getAttribute("width"));

    mockMachineFetch(machineWithBattery({ charge_pct: 35, on_ac: false, charging: false, minutes_to_empty: 60 }));
    const { container: c35 } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("35%")).toBeInTheDocument());
    const w35 = Number(c35.querySelector(".battery-bar-fill")!.getAttribute("width"));

    expect(w100).toBeGreaterThan(w35);
    expect(w35).toBeGreaterThan(0);
    // Exact scale: BATTERY_FILL_MAX_W is 44 viewBox units (machineStatsContent.tsx).
    expect(w100).toBeCloseTo(44, 5);
    expect(w35).toBeCloseTo(44 * 0.35, 5);
  });

  it("the fill and percent text carry NO threshold class at any percent — the gradient replaces it", async () => {
    for (const pct of [100, 35, 8, 15]) {
      mockMachineFetch(machineWithBattery({ charge_pct: pct, on_ac: false, charging: false, minutes_to_empty: 30 }));
      const { container } = renderMachine(null);
      await waitFor(() => expect(screen.getByText(`${pct}%`)).toBeInTheDocument());
      expect(container.querySelector(".battery-bar-fill")!.getAttribute("class")).not.toMatch(/mm-band-/);
      expect(container.querySelector(".battery-bar-pct")!.getAttribute("class")).not.toMatch(/mm-band-/);
    }
  });

  it("the fill always paints from the reversed battery ramp url, on AC or discharging alike — state-invariant", async () => {
    for (const battery of [
      { charge_pct: 8, on_ac: false, charging: false },
      { charge_pct: 8, on_ac: true, charging: true },
    ]) {
      mockMachineFetch(machineWithBattery(battery));
      const { container } = renderMachine(null);
      await waitFor(() => expect(screen.getByText("8%")).toBeInTheDocument());
      expect(container.querySelector(".battery-bar-fill")!.getAttribute("fill")).toBe("url(#mm-battery-ramp)");
      cleanup(); // same "8%" text in both iterations — must not match the PRIOR render's stale node
    }
  });

  // ── Icon states (operator, 2026-09-24: "a lightning bolt icon works
  //    inside the battery... on AC, not charging -> plug; on battery -> no
  //    icon"). ──────────────────────────────────────────────────────────

  it("bolt icon while charging", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 80, on_ac: true, charging: true }));
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("80%")).toBeInTheDocument());
    const icon = container.querySelector(".battery-bar-icon")!;
    expect(icon).not.toBeNull();
    expect(icon.getAttribute("data-kind")).toBe("bolt");
    expect(container.querySelector(".battery-bar text")).toBeNull(); // a drawn path, never an emoji
  });

  it("plug icon when on AC but not charging (topped off) — never a bolt", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 100, on_ac: true, charging: false }));
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("100%")).toBeInTheDocument());
    const icon = container.querySelector(".battery-bar-icon")!;
    expect(icon).not.toBeNull();
    expect(icon.getAttribute("data-kind")).toBe("plug");
    expect(container.querySelector(".battery-bar text")).toBeNull();
  });

  it("held: a marker at the held level and a labeled tooltip; the other states draw neither", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 80, on_ac: true, state: "held" }));
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("80%")).toBeInTheDocument());
    const mark = container.querySelector(".battery-bar-hold")!;
    expect(mark).not.toBeNull();
    // BATTERY_FILL_X (5) + 80% of the 44-unit fill width.
    expect(Number(mark.getAttribute("x1"))).toBeCloseTo(5 + 0.8 * 44, 5);
    const label = container.querySelector(".battery-bar-held")!;
    expect(label.textContent).toBe("held");
    expect(label.getAttribute("title")).toBe("Held at 80% by macOS (charge limit): plugged in, not charging");
    cleanup();

    for (const state of ["charging", "discharging", "full", "unknown"] as const) {
      mockMachineFetch(machineWithBattery({ charge_pct: 80, on_ac: true, state }));
      const r = renderMachine(null);
      await waitFor(() => expect(screen.getByText("80%")).toBeInTheDocument());
      expect(r.container.querySelector(".battery-bar-hold"), state).toBeNull();
      expect(r.container.querySelector(".battery-bar-held"), state).toBeNull();
      cleanup();
    }
  });

  it("held: an (i) button after the label explains the hold on focus, and is absent in other states", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 80, on_ac: true, state: "held" }));
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("80%")).toBeInTheDocument());
    const btn = screen.getByRole("button", { name: "what held means" });
    expect(container.querySelector(".battery-bar-held")!.nextElementSibling).toBe(btn.parentElement);
    expect(screen.queryByRole("tooltip")).toBeNull();
    act(() => btn.focus());
    const tip = await screen.findByRole("tooltip");
    expect(tip.textContent).toContain("Held at 80% by macOS's charge limit");
    expect(tip.textContent).toContain("not charging on purpose");
    expect(tip.textContent).toContain("not draining");
    expect(btn.getAttribute("aria-describedby")).toBe(tip.id);
    act(() => btn.blur());
    expect(screen.queryByRole("tooltip")).toBeNull();
    cleanup();

    mockMachineFetch(machineWithBattery({ charge_pct: 80, on_ac: true, charging: false, state: "full" }));
    renderMachine(null);
    await waitFor(() => expect(screen.getByText("80%")).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "what held means" })).toBeNull();
  });

  it("no icon while discharging", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 35, on_ac: false, charging: false, minutes_to_empty: 60 }));
    const { container } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("35%")).toBeInTheDocument());
    expect(container.querySelector(".battery-bar-icon")).toBeNull();
  });

  it("time-left text renders ONLY while discharging with an estimate — never on AC, never charging", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 80, on_ac: true, charging: true }));
    renderMachine(null);
    await waitFor(() => expect(screen.getByText("80%")).toBeInTheDocument());
    expect(screen.queryByText(/left$/)).toBeNull();
  });

  it("aria-label per state: on AC not charging / charging / on battery with estimate", async () => {
    mockMachineFetch(machineWithBattery({ charge_pct: 100, on_ac: true, charging: false }));
    const { container: c1 } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("100%")).toBeInTheDocument());
    expect(c1.querySelector(".battery-bar")!.getAttribute("aria-label")).toBe("battery 100%, on AC, not charging");

    mockMachineFetch(machineWithBattery({ charge_pct: 62, on_ac: true, charging: true }));
    const { container: c2 } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("62%")).toBeInTheDocument());
    expect(c2.querySelector(".battery-bar")!.getAttribute("aria-label")).toBe("battery 62%, charging");

    mockMachineFetch(machineWithBattery({ charge_pct: 35, on_ac: false, charging: false, minutes_to_empty: 130 }));
    const { container: c3 } = renderMachine(null);
    await waitFor(() => expect(screen.getByText("35%")).toBeInTheDocument());
    expect(c3.querySelector(".battery-bar")!.getAttribute("aria-label")).toBe("battery 35%, on battery, 2 h 10 m left");
  });
});

describe("MachineLens — the live block is the SAME shared component the sheet uses (#2108)", () => {
  it("the local machine page renders the thermal pill and CPU-cluster tiles from the daemon fixture", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 },
      resources: { ...RESOURCES, load: LOAD_WITH_EXTRAS },
    });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/limit source/i)).toBeInTheDocument());
    // Thermal pill + CPU-cluster tiles — content ONLY `HostExtras`
    // (inside `useMachineStatsContent`'s `liveBlock`) can produce; the
    // OLD flow-aggregation section never rendered either.
    await waitFor(() => expect(screen.getByText("Fair")).toBeInTheDocument());
    expect(screen.getByText("Super")).toBeInTheDocument();
    expect(screen.getByText("Performance")).toBeInTheDocument();
  });

  it("a REMOTE machine page still shows nothing from the daemon-fed block (no thermal pill) — the flow-aggregation fallback stays for that case", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      resources: { ...RESOURCES, load: LOAD_WITH_EXTRAS },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
    });
    renderMachine("remote-uid");
    await waitFor(() => expect(screen.getByText(/not reported from here/i)).toBeInTheDocument());
    expect(screen.queryByText("Fair")).toBeNull();
    expect(screen.queryByText("Super")).toBeNull();
  });

  it("MachineLens.tsx and MachineDrawer.tsx both source their live block from the SAME hook module", () => {
    // A source-level check alongside the behavioral one above: proves
    // this isn't two independently-built blocks that happen to render
    // similar text, but the literal SAME `useMachineStatsContent` export.
    const dir = path.dirname(fileURLToPath(import.meta.url));
    const lensSrc = readFileSync(path.join(dir, "MachineLens.tsx"), "utf-8");
    const drawerSrc = readFileSync(path.join(dir, "../../components/MachineDrawer.tsx"), "utf-8");
    expect(lensSrc).toMatch(/import \{ useMachineStatsContent \} from "\.\.\/\.\.\/components\/machineStatsContent"/);
    expect(drawerSrc).toMatch(/useMachineStatsContent/);
  });
});

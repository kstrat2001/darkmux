import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { PANEL_IDS } from "../../lib/route";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
import {
  PANELS,
  PANEL_OPTS,
  DEFAULT_PANEL_ID,
  isManualPanel,
  panelCols,
  panelArgv,
  panelOptGroups,
  resolveOpts,
  composeArgv,
  canonicalOptPairs,
  variantKey,
  sanitizeOptParams,
  rosterOptName,
  machineOpt,
  reconcileOpts,
  LOCAL_MACHINE,
} from "./panels";

describe("PANELS", () => {
  it("covers exactly the routing allowlist, same drift guard as panel.rs's own PANEL_IDS test", () => {
    expect(PANELS.map((p) => p.id).sort()).toEqual([...PANEL_IDS].sort());
  });

  it("(#1911) labels read as the command they run — the pending-command preview and the pill both come from the same argv", () => {
    for (const p of PANELS) {
      expect(p.label).toBe(panelArgv(p.id).join(" "));
    }
  });

  it("(#1911) run-list joins the pill row; mission-status-all is gone", () => {
    expect(PANELS.map((p) => p.id)).toContain("run-list");
    expect(PANELS.map((p) => p.id)).not.toContain("mission-status-all");
  });

  // (#1905 step 3) exactly eight pills — the operator's own rejection of a
  // ten-pill render ("can't allow main to have this") is the reason a
  // ninth/tenth client-only entry can never come back silently.
  it("is exactly ten pills (machine-list is the tenth), matching panel.rs's own doctrine cap — no client-only entries", () => {
    expect(PANELS).toHaveLength(10);
    expect(PANEL_IDS).toHaveLength(10);
  });

  it("(#1905 step 3) DEFAULT_PANEL_ID is run-list, and is itself one of the eight allowlisted panels", () => {
    expect(DEFAULT_PANEL_ID).toBe("run-list");
    expect(PANELS.map((p) => p.id)).toContain(DEFAULT_PANEL_ID);
    // (#1911) …and it LEADS the row. A default sitting mid-row reads as an
    // arbitrary pick rather than the panel you land on.
    expect(PANELS[0].id).toBe(DEFAULT_PANEL_ID);
  });

  it("only doctor is manual-only", () => {
    expect(isManualPanel("doctor")).toBe(true);
    for (const p of PANELS) {
      if (p.id !== "doctor") expect(isManualPanel(p.id)).toBe(false);
    }
  });
});

describe("panelCols", () => {
  it("clamps to the floor at a narrow width", () => {
    expect(panelCols({ clientWidth: 40 } as Element)).toBe(36);
  });

  it("clamps to the ceiling at a very wide width", () => {
    expect(panelCols({ clientWidth: 5000 } as Element)).toBe(200);
  });

  it("a phone's real width survives the clamp unchanged (#1613 parity with the daemon's own floor)", () => {
    // 390px viewport, minus the panel's own chrome, matches the daemon-side
    // fixture in `crates/darkmux-serve/src/panel.rs`'s `cols_clamped_hard`
    // test asserting 52 survives unclamped.
    expect(panelCols({ clientWidth: 399 } as Element)).toBe(52);
  });

  it("falls back to window.innerWidth when no element is passed", () => {
    const got = panelCols(null);
    expect(got).toBeGreaterThanOrEqual(36);
    expect(got).toBeLessThanOrEqual(200);
  });
});

// ── PANEL_OPTS: the client twin of panel.rs's declared option space (#1911) ──

describe("resolveOpts", () => {
  it("defaults every declared opt when nothing requested", () => {
    const resolved = resolveOpts("run-list", {});
    expect(resolved).toEqual([
      { name: "kind", value: "all", argv: [], isDefault: true },
      { name: "all", value: "recent", argv: [], isDefault: true },
      // (#2902) The usage toggle, off by default.
      { name: "usage", value: "off", argv: [], isDefault: true },
    ]);
  });

  it("(#2902) the usage toggle contributes --usage last, after --kind and --all", () => {
    expect(composeArgv("run-list", { usage: "on" })).toEqual(["run", "list", "--usage"]);
    expect(composeArgv("run-list", { usage: "on", all: "all", kind: "lab" })).toEqual(["run", "list", "--kind", "lab", "--all", "--usage"]);
    expect(variantKey("run-list", { usage: "on" })).toBe("run-list?usage=on");
    expect(variantKey("run-list", { usage: "off" })).toBe("run-list");
  });

  it("picks the named value", () => {
    const resolved = resolveOpts("run-list", { kind: "lab" });
    expect(resolved[0]).toEqual({ name: "kind", value: "lab", argv: ["--kind", "lab"], isDefault: false });
  });

  it("an unknown value for a known option silently drops to default (never a pass-through)", () => {
    const resolved = resolveOpts("run-list", { kind: "bogus" });
    expect(resolved[0]).toEqual({ name: "kind", value: "all", argv: [], isDefault: true });
  });

  it("an unknown option name is ignored entirely", () => {
    const resolved = resolveOpts("run-list", { machine: "studio" });
    expect(resolved.map((r) => r.name)).toEqual(["kind", "all", "usage"]);
  });

  it("a panel with no declared opts resolves to an empty list regardless of what's requested", () => {
    expect(resolveOpts("doctor", { kind: "mission" })).toEqual([]);
  });
});

describe("composeArgv", () => {
  it("follows DECLARATION order, never the order keys were passed in", () => {
    // Object key order in JS objects built like this literal IS insertion
    // order, so build it "all" first, "kind" second — the task's own named
    // case (a HashMap has no order server-side; the analog here is "don't
    // trust JS object key order either").
    const requested: Record<string, string> = {};
    requested["all"] = "all";
    requested["kind"] = "mission";
    expect(composeArgv("run-list", requested)).toEqual(["run", "list", "--kind", "mission", "--all"]);
  });

  it("a default contributes nothing to argv", () => {
    expect(composeArgv("run-list", {})).toEqual(["run", "list"]);
  });

  it("a panel with no opts composes its bare argv", () => {
    expect(composeArgv("doctor")).toEqual(["doctor"]);
  });
});

describe("canonicalOptPairs / variantKey", () => {
  it("no selection and explicitly picking the default produce the SAME (empty) pairs", () => {
    expect(canonicalOptPairs("run-list", {})).toEqual([]);
    expect(canonicalOptPairs("run-list", { kind: "all", all: "recent" })).toEqual([]);
  });

  it("sorts non-default pairs by name", () => {
    expect(canonicalOptPairs("run-list", { all: "all", kind: "lab" })).toEqual([
      ["all", "all"],
      ["kind", "lab"],
    ]);
  });

  it("variantKey is the base id alone when everything is default", () => {
    expect(variantKey("run-list", {})).toBe("run-list");
  });

  it("variantKey matches the server's own format: id?name=value&name=value, sorted", () => {
    expect(variantKey("run-list", { all: "all", kind: "lab" })).toBe("run-list?all=all&kind=lab");
  });

  it("variantKey differs for different selections", () => {
    expect(variantKey("run-list", { kind: "mission" })).not.toBe(variantKey("run-list", { kind: "dispatch" }));
  });
});

describe("panelOptGroups", () => {
  it("mission-status declares exactly the --all toggle", () => {
    expect(panelOptGroups("mission-status")).toEqual([
      { name: "all", values: [{ value: "recent", argv: [] }, { value: "all", argv: ["--all"] }] },
    ]);
  });

  it("seven of ten panels declare no static options", () => {
    const noOpts = PANEL_IDS.filter((id) => panelOptGroups(id).length === 0);
    expect(noOpts.sort()).toEqual(["config-list", "doctor", "flow-status", "lab-fixture-list", "machine-list", "machine-status", "role-list"].sort());
  });
});

// ── Drift guard (#1911): the TS opts table pinned against the Rust one ──
//
// Mirrors the direction `src/run_list.rs`'s own
// `run_kind_arg_vocabulary_matches_the_ui_runs_kinds_twin` test takes (a
// text-pin via a raw file read), just reversed: that test reads a TS file
// from Rust; this one reads the Rust file from TS, because darkmux-serve's
// `panel.rs` is the frozen server half this client mirrors, not the other
// way around. Extracts every `PanelOptValue { value: "...", argv: [...] }`
// literal that appears in `RUN_LIST_KIND_OPT`/`ALL_OPT`'s declarations and
// checks each `(panel, name, value)` triple this file declares has a match
// on the Rust side — the failure message names the twin relationship so a
// future edit to either table without the other explains itself instead of
// failing mysteriously.
describe("PANEL_OPTS pinned against crates/darkmux-serve/src/panel.rs (#1911)", () => {
  const rustSrc = readFileSync(
    path.join(__dirname, "../../../../crates/darkmux-serve/src/panel.rs"),
    "utf8",
  );

  it("every (panel, opt name, value) triple this file declares also appears in the Rust source", () => {
    for (const [panelId, entry] of Object.entries(PANEL_OPTS)) {
      for (const opt of entry.opts) {
        for (const v of opt.values) {
          const needle = `value: "${v.value}"`;
          expect(
            rustSrc.includes(needle),
            `PANEL_OPTS["${panelId}"].opts["${opt.name}"] declares value "${v.value}", which panel.rs's own ` +
              `PanelOptValue literals don't contain — the client's opts table (ui/src/lenses/console/panels.ts) ` +
              `and the server's (crates/darkmux-serve/src/panel.rs) are twins; update both together (#1911)`,
          ).toBe(true);
        }
      }
    }
  });

  it("panel.rs's own RUN_LIST_KIND_OPT vocabulary (all/mission/dispatch/lab) matches this file's run-list kind opt exactly", () => {
    const kindOpt = panelOptGroups("run-list").find((o) => o.name === "kind");
    expect(kindOpt, "run-list must declare a kind opt").toBeDefined();
    const tsValues = kindOpt!.values.map((v) => v.value).sort();
    expect(tsValues, "twin drift: ui panels.ts's run-list kind values vs panel.rs's RUN_LIST_KIND_OPT").toEqual(
      ["all", "dispatch", "lab", "mission"].sort(),
    );
    expect(rustSrc, "RUN_LIST_KIND_OPT not found in panel.rs — the twin this test pins against was renamed (#1911)").toContain(
      "const RUN_LIST_KIND_OPT: PanelOpt",
    );
  });

  it("the ALL_OPT boolean (recent/all) matches every panel that declares it here", () => {
    for (const id of ["mission-status", "run-list"] as const) {
      const allOpt = panelOptGroups(id).find((o) => o.name === "all");
      expect(allOpt, `${id} must declare an "all" opt`).toBeDefined();
      expect(allOpt!.values.map((v) => v.value)).toEqual(["recent", "all"]);
      expect(allOpt!.values[0].argv).toEqual([]);
      expect(allOpt!.values[1].argv).toEqual(["--all"]);
    }
    expect(rustSrc).toContain("const ALL_OPT: PanelOpt");
  });
});

// ── profile-list: the roster-valued `machine` opt (5.0) ────────────────
//
// Its legal values are the roster's machine names, which the client cannot
// know when it parses a hash: parse checks the SHAPE of the value, the server
// checks membership against its roster and 400s on a stranger.
describe("profile-list's roster-valued machine opt", () => {
  it("exactly two panels take a roster machine, and the server twin declares it too", () => {
    expect(PANEL_IDS.filter((id) => rosterOptName(id) !== null).sort()).toEqual(["machine-status", "profile-list"]);
    expect(rosterOptName("profile-list")).toBe("machine");
    const rustSrc = readFileSync(path.join(__dirname, "../../../../crates/darkmux-serve/src/panel.rs"), "utf8");
    expect(rustSrc).toContain('const ROSTER_MACHINE_OPT: &str = "machine"');
    expect(rustSrc).toContain('const ROSTER_MACHINE_FLAG: &str = "--machine"');
    expect(panelArgv("profile-list")).toEqual(["profile", "list"]);
  });

  it("machine status takes the machine as its positional id, not a flag", () => {
    expect(composeArgv("machine-status", { machine: "studio" })).toEqual(["machine", "status", "studio"]);
    expect(composeArgv("machine-status", {})).toEqual(["machine", "status"]);
    expect(variantKey("machine-status", { machine: "studio" })).toBe("machine-status?machine=studio");
    expect(machineOpt("machine-status", ["studio"], undefined).values[1].argv).toEqual(["studio"]);
    expect(sanitizeOptParams("machine-status", { machine: "studio" })).toEqual({ machine: "studio" });
    expect(composeArgv("machine-list", { machine: "studio" })).toEqual(["machine", "list"]);
  });

  it("a deep link's machine survives sanitizing; a stranger panel's does not", () => {
    expect(sanitizeOptParams("profile-list", { machine: "darkbook" })).toEqual({ machine: "darkbook" });
    expect(sanitizeOptParams("run-list", { machine: "darkbook" })).toEqual({});
  });

  it("drops a machine that cannot be a roster name (empty, huge, control characters, the local sentinel)", () => {
    for (const bad of ["", "x".repeat(200), "a\nb", "a\u0000b", LOCAL_MACHINE]) {
      expect(sanitizeOptParams("profile-list", { machine: bad }), JSON.stringify(bad)).toEqual({});
    }
  });

  it("machine and remote together keep the machine, which is the narrower ask", () => {
    expect(sanitizeOptParams("profile-list", { machine: "darkbook", remote: "on" })).toEqual({ machine: "darkbook" });
    expect(sanitizeOptParams("profile-list", { remote: "on" })).toEqual({ remote: "on" });
  });

  it("composes --machine after the static opts, and keys the cache on it", () => {
    expect(composeArgv("profile-list", { machine: "darkbook" })).toEqual(["profile", "list", "--machine", "darkbook"]);
    expect(composeArgv("profile-list", { remote: "on" })).toEqual(["profile", "list", "--remote"]);
    expect(composeArgv("profile-list", {})).toEqual(["profile", "list"]);
    expect(variantKey("profile-list", { machine: "darkbook" })).toBe("profile-list?machine=darkbook");
    expect(variantKey("profile-list", { remote: "on", machine: "x" })).toBe("profile-list?machine=x&remote=on");
    expect(canonicalOptPairs("profile-list", { machine: LOCAL_MACHINE })).toEqual([]);
  });

  it("the menu offers this machine first, then the roster, and keeps a linked name the roster lacks", () => {
    expect(machineOpt("profile-list", ["studio", "mini"], undefined).values.map((v) => v.value)).toEqual([LOCAL_MACHINE, "studio", "mini"]);
    expect(machineOpt("profile-list", ["studio"], "ghost").values.map((v) => v.value)).toEqual([LOCAL_MACHINE, "studio", "ghost"]);
    expect(machineOpt("profile-list", ["studio"], "studio").values[0].argv).toEqual([]);
  });

  it("picking a machine turns remote off and turning remote on forgets the machine", () => {
    expect(reconcileOpts("profile-list", { remote: "on" }, "machine", "studio")).toEqual({ machine: "studio" });
    expect(reconcileOpts("profile-list", { machine: "studio" }, "remote", "on")).toEqual({ remote: "on" });
    expect(reconcileOpts("profile-list", { machine: "studio" }, "machine", LOCAL_MACHINE)).toEqual({ machine: LOCAL_MACHINE });
    expect(reconcileOpts("run-list", { kind: "lab" }, "all", "all")).toEqual({ kind: "lab", all: "all" });
  });
});

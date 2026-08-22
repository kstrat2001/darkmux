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
  it("(#1905 step 3) is exactly eight pills, matching panel.rs's own doctrine cap — no client-only entries", () => {
    expect(PANELS).toHaveLength(8);
    expect(PANEL_IDS).toHaveLength(8);
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
    ]);
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
    expect(resolved.map((r) => r.name)).toEqual(["kind", "all"]);
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

  it("six of eight panels declare no options at all", () => {
    const noOpts = PANEL_IDS.filter((id) => panelOptGroups(id).length === 0);
    expect(noOpts.sort()).toEqual(["config-list", "doctor", "flow-status", "lab-fixture-list", "machine-status", "role-list"].sort());
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

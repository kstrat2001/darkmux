import { describe, it, expect, afterEach } from "vitest";
import { isStaticBuild, staticFlowSrc, resolveRunsSrc, resolveLabRunsSrc } from "./staticSource";

/**
 * (#1801) The one resolver every static-build consumer (`route.ts`,
 * `useRouteRecords.ts`, `PlaybackLens.tsx`, `RunsBoard.tsx`, `Masthead.tsx`)
 * imports from, instead of five independent `injectedMeta("darkmux-*-src")`
 * reads free to drift. Every test here injects the metas directly, matching
 * `route.test.ts`'s existing pattern for the same reason: none of this
 * repo's test harnesses (unit or parity) ever inject these in the ambient
 * environment (`injectedMeta.ts`'s own doc) — only a real
 * `scripts/build-demo.sh` output does.
 */

function injectMeta(name: string, content: string) {
  const el = document.createElement("meta");
  el.setAttribute("name", name);
  el.setAttribute("content", content);
  document.head.appendChild(el);
}

afterEach(() => {
  document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
});

describe("isStaticBuild / staticFlowSrc", () => {
  it("is false / null with no metas at all (every daemon-served page, every test harness)", () => {
    expect(isStaticBuild()).toBe(false);
    expect(staticFlowSrc()).toBeNull();
  });

  it("is true / the meta's content once darkmux-flow-src is injected", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    expect(isStaticBuild()).toBe(true);
    expect(staticFlowSrc()).toBe("./demo-flow.jsonl");
  });

  it("stays false for an unrelated darkmux meta — flow-src specifically is the signal", () => {
    injectMeta("darkmux-version", "2.7.0");
    expect(isStaticBuild()).toBe(false);
  });
});

describe("resolveRunsSrc", () => {
  it("defaults to /runs with no darkmux-runs-src meta (a real daemon)", () => {
    expect(resolveRunsSrc()).toBe("/runs");
  });

  it("resolves to the injected src when present (the static demo)", () => {
    injectMeta("darkmux-runs-src", "./demo-runs.json");
    expect(resolveRunsSrc()).toBe("./demo-runs.json");
  });
});

describe("resolveLabRunsSrc", () => {
  it("defaults to /lab/runs with no darkmux-lab-runs-src meta (a real daemon)", () => {
    expect(resolveLabRunsSrc()).toBe("/lab/runs");
  });

  it("resolves to the injected src when present (the static demo)", () => {
    injectMeta("darkmux-lab-runs-src", "./demo-lab-runs.json");
    expect(resolveLabRunsSrc()).toBe("./demo-lab-runs.json");
  });
});

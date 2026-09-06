import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { MachineLens } from "./MachineLens";

/**
 * (operator finding, 2026-09-05: "I'm a bit concerned about the wordiness in
 * machine lens. Is all the prose necessary?") A CEILING on how much text the
 * machine lens renders AT REST — nothing tapped, nothing expanded.
 *
 * The lens is SCANNED, not read. Every number keeps its label, unit and
 * chip; what this test bounds is the explanatory prose AROUND those numbers,
 * which had grown to the point where the operator could not find the figures
 * in it. Provenance the payload still carries (attribution, gather cost,
 * cache TTL, poll cadence) moved behind the region's own `how to read this`
 * disclosure rather than being deleted — "record exhaustively, display
 * selectively" applies to the RENDER, not to the payload.
 *
 * Two fixtures, because the demo machine is a healthy one and the wordiest
 * copy on this page only appears on a row that is NOT healthy:
 *   1. `docs/demo/demo-machine.json` verbatim — what darkmux.com/demo shows,
 *      and what an operator with four well-priced residents sees.
 *   2. the same fixture with an UNPRICED and an ESTIMATED resident — the two
 *      rows that used to carry a 52-word and an 18-word paragraph each.
 *
 * The ceilings are deliberately a little above the measured figure so an
 * honest one-word copy fix does not fail the suite; they are far below the
 * pre-trim counts, which is the assertion that matters. A ceiling that only
 * ever moves DOWN is the point — raising one is a decision, not a fix.
 */

const DEMO_MACHINE = JSON.parse(
  readFileSync(
    path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../../docs/demo/demo-machine.json"),
    "utf8",
  ),
) as { specs: Record<string, unknown>; resources: Record<string, unknown> };

/** Words rendered where the operator can actually read them. Skips the
 * screen-reader mirror of the odometer digits (`.mm-sr-only` — the same
 * figure twice would double-count it) and the BODY of a closed `<details>`
 * (present in the DOM, invisible until tapped — which is the entire point of
 * moving prose into one). A closed disclosure's `<summary>` still counts:
 * that line IS on screen. */
function visibleWords(root: HTMLElement): number {
  let n = 0;
  const walk = (el: Element) => {
    for (const node of el.childNodes) {
      if (node.nodeType === Node.TEXT_NODE) {
        const t = (node.textContent ?? "").trim();
        if (t) n += t.split(/\s+/).length;
        continue;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) continue;
      const child = node as Element;
      if (child.classList.contains("mm-sr-only")) continue;
      if (child.tagName === "DETAILS" && !(child as HTMLDetailsElement).open) {
        const summary = child.querySelector("summary");
        if (summary) walk(summary);
        continue;
      }
      walk(child);
    }
  };
  walk(root);
  return n;
}

function staticMeta(name: string, content: string) {
  const el = document.createElement("meta");
  el.setAttribute("name", name);
  el.setAttribute("content", content);
  document.head.appendChild(el);
}

function mount(machine: unknown) {
  staticMeta("darkmux-flow-src", "./demo-flow.jsonl");
  staticMeta("darkmux-machine-src", "./demo-machine.json");
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const p = String(url);
      if (p === "./demo-machine.json") return Promise.resolve(new Response(JSON.stringify(machine), { status: 200 }));
      if (p === "./demo-flow.jsonl") return Promise.resolve(new Response("", { status: 200 }));
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens uid={null} isMobileOverride={false} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
  document.head.querySelectorAll("meta[name^='darkmux-']").forEach((e) => e.remove());
});

/** The demo fixture's healthy machine plus one UNPRICED and one ESTIMATED
 * resident — the two rows that used to carry a 52-word and an 18-word
 * paragraph each, and the reason this file has a second fixture at all.
 *
 * `messages` is a PARAMETER because the server's own caveats are a separate
 * axis from the rows: `/machine/resources` computes `messages[]` itself, and
 * whether it emitted any is not something the row shapes decide. */
function unpricedEstimatedMachine(messages: { severity: string; text: string }[] = []) {
  const models = (DEMO_MACHINE.resources.models as Record<string, unknown>[]).slice();
  const unpriced = { ...models[3], identifier: "user:mystery-13b", model_key: "mystery-13b", owner: "user", potential_bytes: null, potential_source: null };
  const estimated = { ...models[3], identifier: "user:phi-4", model_key: "phi-4", owner: "user", potential_source: "estimated" };
  return {
    ...DEMO_MACHINE,
    resources: {
      ...DEMO_MACHINE.resources,
      models: [...models, unpriced, estimated],
      messages,
      machine: { ...(DEMO_MACHINE.resources.machine as object), unpriced_models: 1, estimated_models: 1, state: "unknown" },
    },
  };
}

describe("machine lens — at-rest prose budget", () => {
  it("the demo fixture's healthy machine renders under the at-rest word ceiling", async () => {
    const { container } = mount(DEMO_MACHINE);
    await waitFor(() => expect(document.querySelector(".machine-lens__health")).toHaveAttribute("data-state", "loaded"));
    await waitFor(() => expect(screen.getByText(/darkmux:qwen3-4b-instruct-2507/)).toBeInTheDocument());

    const words = visibleWords(container.querySelector(".machine-lens") as HTMLElement);
    // Measured 293 before the trim, 256 after. The ceiling sits just above
    // the post-trim figure: an honest one-word copy fix must not fail the
    // suite, and restoring either deleted paragraph must.
    expect(words).toBeLessThanOrEqual(262);
  });

  it("an unpriced resident adds a short row hint; an estimated resident adds one machine-wide line, not a paragraph each", async () => {
    const { container } = mount(unpricedEstimatedMachine());
    await waitFor(() => expect(screen.getByText(/user:phi-4/)).toBeInTheDocument());

    // The unpriced fact still marks its own row (#2440 left this hint
    // alone — only the ESTIMATED row's own duplicate hint was cut).
    const hints = [...container.querySelectorAll(".mm-hint")].map((h) => h.textContent ?? "");
    expect(hints.some((h) => /unprice/i.test(h))).toBe(true);
    // The estimated fact no longer lives on the row at all (#2440 cut 5) —
    // it is the ONE machine-wide summary line pointing at the disclosure.
    expect(hints.some((h) => /estimated:/i.test(h))).toBe(false);
    expect(container.textContent).toMatch(/1 model priced by estimate \(no config\.json\)/);

    const words = visibleWords(container.querySelector(".machine-lens") as HTMLElement);
    // Measured 423 before the #1854-era trim, 338 after #1819, 317 after
    // #2440 (the per-row estimated hint, the machine-level duplicate shrink
    // hint, and the always-open info-message paragraph are all gone from
    // the at-rest surface now). The ceiling only ever moves down.
    expect(words).toBeLessThanOrEqual(322);
  });

  // (#2440 cut 4 supersedes fix-loop 4's finding above) The operator's own
  // 2026-09-06 call reverses fix-loop 4's "messages render in the open"
  // rule for exactly the ONE severity that rule was protecting against a
  // false economy on: `info`. An `info` message is a disclosure, not an
  // alert (#1821 already drew this line for the WARN lamp); #2440 draws it
  // for on-page placement too. `warn`/`error` — real degradations — keep
  // fix-loop 4's guarantee unchanged: they still render in the open, never
  // behind a `<details>`. Only `info` moved, and the fact is not deleted:
  // it rides the disclosure verbatim, with an on-page one-liner pointing at
  // it — the inverse of the failure fix-loop 4 was guarding against (this
  // is a NAMED move with a replacement pointer, not a quiet deletion).
  it("keeps a warn/error message in the open; moves an info message into the disclosure, with an on-page pointer", async () => {
    const UNPRICED_TEXT =
      "1 loaded model reports no memory commitment — the machine total below is a floor, not a ceiling.";
    const ESTIMATED_TEXT =
      "1 loaded model's commitment is ESTIMATED from its weights, not reported by LMStudio.";
    const { container } = mount(
      unpricedEstimatedMachine([
        { severity: "warn", text: UNPRICED_TEXT },
        { severity: "info", text: ESTIMATED_TEXT },
      ]),
    );
    await waitFor(() => expect(screen.getByText(/user:phi-4/)).toBeInTheDocument());

    // The warn message: unchanged from fix-loop 4 — in the open, `.memmsg`.
    const warnEl = screen.getByText(UNPRICED_TEXT, { exact: false });
    expect(warnEl.closest("details")).toBeNull();
    expect(warnEl.className).toContain("memmsg");

    // The info message: now INSIDE `how this was measured`, not `.memmsg`.
    const infoEl = screen.getByText(ESTIMATED_TEXT, { exact: false });
    expect(infoEl.closest("details")).not.toBeNull();
    expect(infoEl.closest(".mm-about")).not.toBeNull();
    expect(container.querySelectorAll(".memmsg-info").length).toBe(0);

    // The inverted case, same fixture family: with NO server messages, no
    // `.memmsg` renders at all — a region that painted the string
    // unconditionally would satisfy every assertion above and pin nothing.
    const { container: bare } = mount(unpricedEstimatedMachine());
    await waitFor(() => expect(bare.querySelector(".mm-row")).not.toBeNull());
    expect(bare.querySelectorAll(".memmsg").length).toBe(0);
    // Only the warn entry renders as a `.memmsg` — the info entry does not,
    // even though the payload carries one of each.
    expect(container.querySelectorAll(".memmsg").length).toBe(1);
  });
});

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

  it("an unpriced + estimated resident adds a short hint each, not two paragraphs", async () => {
    const models = (DEMO_MACHINE.resources.models as Record<string, unknown>[]).slice();
    const unpriced = { ...models[3], identifier: "user:mystery-13b", model_key: "mystery-13b", owner: "user", potential_bytes: null, potential_source: null };
    const estimated = { ...models[3], identifier: "user:phi-4", model_key: "phi-4", owner: "user", potential_source: "estimated" };
    const machine = {
      ...DEMO_MACHINE,
      resources: {
        ...DEMO_MACHINE.resources,
        models: [...models, unpriced, estimated],
        machine: { ...(DEMO_MACHINE.resources.machine as object), unpriced_models: 1, estimated_models: 1, state: "unknown" },
      },
    };
    const { container } = mount(machine);
    await waitFor(() => expect(screen.getByText(/user:phi-4/)).toBeInTheDocument());

    // The facts themselves survive the trim — this is a prose budget, not a
    // deletion of the two states that most need naming.
    const hints = [...container.querySelectorAll(".mm-hint")].map((h) => h.textContent ?? "");
    expect(hints.some((h) => /unprice/i.test(h))).toBe(true);
    expect(hints.some((h) => /estimated/i.test(h))).toBe(true);

    const words = visibleWords(container.querySelector(".machine-lens") as HTMLElement);
    // Measured 423 before the trim, 338 after — the two row hints alone
    // were 70 words of it, now 20.
    expect(words).toBeLessThanOrEqual(345);
  });
});

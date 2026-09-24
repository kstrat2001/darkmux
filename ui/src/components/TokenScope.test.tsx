import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { TokenScope } from "./TokenScope";

// (#2890) What the operator SEES inside the tube, per state. The canvas
// itself cannot draw under jsdom (no 2D context), so these assert on the
// center overlay and the state the bezel exposes to CSS.

function center(container: HTMLElement) {
  return container.querySelector(".token-scope-center");
}

describe("TokenScope center, per state (#2890)", () => {
  it("TOOLS shows a glowing icon for the tool and no text", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="read" />);
    const icon = container.querySelector("[data-tool-icon]");
    expect(icon?.getAttribute("data-tool-icon")).toBe("read");
    expect(center(container)?.textContent).toBe("");
  });

  it("TOOLS falls back to the gear for any other tool or no name yet", () => {
    const a = render(<TokenScope tokensPerSec={0} size="card" state="tools" toolName="create_mod" />);
    expect(a.container.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("other");
    const b = render(<TokenScope tokensPerSec={0} size="card" state="tools" />);
    expect(b.container.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("other");
  });

  it("TOOLS ignores a center label: the icon is the whole message", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="bash" centerLabel="12" />);
    expect(container.querySelector(".token-scope-n")).toBeNull();
    expect(container.querySelector(".token-scope-screen")?.textContent).toBe("");
    expect(container.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("bash");
  });

  it("PROMPT shows the brain until a prompt size arrives, then the count", () => {
    const a = render(<TokenScope tokensPerSec={0} size="tile" state="prompt" centerLabel={null} />);
    expect(a.container.querySelector("[data-scope-icon]")?.getAttribute("data-scope-icon")).toBe("brain");
    expect(a.container.querySelector(".token-scope-n")).toBeNull();
    const b = render(<TokenScope tokensPerSec={0} size="tile" state="prompt" centerLabel="36k" centerUnit="reading" />);
    expect(b.container.querySelector("[data-scope-icon]")).toBeNull();
    expect(b.container.querySelector(".token-scope-n")?.textContent).toBe("36k");
  });

  it("no brain outside PROMPT", () => {
    for (const state of ["generating", "tools", "rest", "stalled", "finished"] as const) {
      const { container } = render(<TokenScope tokensPerSec={0} size="tile" state={state} />);
      expect(container.querySelector("[data-scope-icon]")).toBeNull();
    }
  });

  it("GEN shows the rate with its unit on the tile", () => {
    const { container } = render(<TokenScope tokensPerSec={180} size="tile" state="generating" centerLabel="180" centerUnit="tok/s" />);
    expect(container.querySelector(".token-scope-n")?.textContent).toBe("180");
    expect(container.querySelector(".token-scope-u")?.textContent).toBe("tok/s");
    expect(container.querySelector("[data-tool-icon]")).toBeNull();
  });

  it("a carried rate still dims the number", () => {
    const { container } = render(<TokenScope tokensPerSec={90} size="tile" state="generating" centerLabel="90" centerCarried />);
    expect(container.querySelector(".token-scope-n")?.getAttribute("data-carried")).toBe("true");
  });

  it("FINISHED keeps the average in the center with its unit", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="finished" centerLabel="64" centerUnit="avg tok/s" />);
    expect(container.querySelector(".token-scope-n")?.textContent).toBe("64");
    expect(container.querySelector(".token-scope-u")?.textContent).toBe("avg tok/s");
    expect(container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("finished");
  });

  it("REST, STALL and NO SIGNAL have an empty center with no label", () => {
    for (const state of ["rest", "stalled", "nosignal"] as const) {
      const { container, unmount } = render(<TokenScope tokensPerSec={0} size="tile" state={state} />);
      expect(center(container)).toBeNull();
      unmount();
    }
  });

  it("without a `state` prop, the existing props still pick the state", () => {
    const stalled = render(<TokenScope tokensPerSec={0} size="card" stalled tone="stalled" />);
    expect(stalled.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("stalled");
    const resting = render(<TokenScope tokensPerSec={0} size="card" resting tone="rest" />);
    expect(resting.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("rest");
    const tools = render(<TokenScope tokensPerSec={0} size="card" tone="tools" />);
    expect(tools.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("tools");
    const none = render(<TokenScope tokensPerSec={0} size="card" tone="none" />);
    expect(none.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("idle");
  });
});

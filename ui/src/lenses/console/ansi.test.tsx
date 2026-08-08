import { describe, it, expect, vi } from "vitest";
import { render } from "@testing-library/react";
import { parseAnsi, panelHref, panelSwitchId, AnsiText, ANSI_SGR_CLASS } from "./ansi";

describe("parseAnsi", () => {
  it("plain text with no escapes is a single unstyled segment", () => {
    const segs = parseAnsi("hello world");
    expect(segs).toEqual([{ text: "hello world", classes: [], link: null, switchTo: null }]);
  });

  it("honors a single SGR code (bold)", () => {
    const segs = parseAnsi("\x1b[1mbold\x1b[0m plain");
    expect(segs).toEqual([
      { text: "bold", classes: ["a-bold"], link: null, switchTo: null },
      { text: " plain", classes: [], link: null, switchTo: null },
    ]);
  });

  it("stacks multiple SGR codes from one escape (bold + green)", () => {
    const segs = parseAnsi("\x1b[1;32mok\x1b[0m");
    expect(segs).toEqual([{ text: "ok", classes: ["a-bold", "a-fg2"], link: null, switchTo: null }]);
  });

  it("a bare ESC[m resets like ESC[0m", () => {
    const segs = parseAnsi("\x1b[1mbold\x1b[mplain");
    expect(segs).toEqual([
      { text: "bold", classes: ["a-bold"], link: null, switchTo: null },
      { text: "plain", classes: [], link: null, switchTo: null },
    ]);
  });

  it("maps every SGR code darkmux emits to a class (parity with ANSI_SGR_CLASS)", () => {
    for (const [code, cls] of Object.entries(ANSI_SGR_CLASS)) {
      const segs = parseAnsi(`\x1b[${code}mx`);
      expect(segs[0].classes).toEqual([cls]);
    }
  });

  it("an unrecognized SGR code is silently dropped (no class), not an error", () => {
    const segs = parseAnsi("\x1b[123mx");
    expect(segs).toEqual([{ text: "x", classes: [], link: null, switchTo: null }]);
  });

  it("truncated CSI (no final byte) drops the remainder rather than throwing", () => {
    expect(() => parseAnsi("before\x1b[1")).not.toThrow();
    expect(parseAnsi("before\x1b[1")).toEqual([{ text: "before", classes: [], link: null, switchTo: null }]);
  });

  it("non-SGR CSI final bytes are consumed and discarded (only 'm' is honored)", () => {
    const segs = parseAnsi("a\x1b[2Jb");
    expect(segs).toEqual([{ text: "ab", classes: [], link: null, switchTo: null }]);
  });

  it("OSC 8 wraps a link, terminated by ST (ESC \\\\)", () => {
    const segs = parseAnsi("\x1b]8;;https://example.com/x\x1b\\click\x1b]8;;\x1b\\");
    expect(segs).toEqual([{ text: "click", classes: [], link: "https://example.com/x", switchTo: null }]);
  });

  it("OSC 8 also terminates on BEL", () => {
    const segs = parseAnsi("\x1b]8;;https://example.com/x\x07click\x1b]8;;\x07");
    expect(segs[0].link).toBe("https://example.com/x");
  });

  it("truncated OSC (no terminator) drops the remainder rather than throwing", () => {
    expect(() => parseAnsi("a\x1b]8;;https://x")).not.toThrow();
  });

  it("a same-origin console deep link resolves switchTo to the panel id", () => {
    const origin = window.location.origin;
    const segs = parseAnsi(`\x1b]8;;${origin}/#lens=console&panel=doctor\x1b\\link\x1b]8;;\x1b\\`);
    expect(segs[0].switchTo).toBe("doctor");
  });

  it("a console deep link with an unrecognized panel id does not resolve switchTo", () => {
    const origin = window.location.origin;
    const segs = parseAnsi(`\x1b]8;;${origin}/#lens=console&panel=rm-rf\x1b\\link\x1b]8;;\x1b\\`);
    expect(segs[0].switchTo).toBeNull();
    expect(segs[0].link).not.toBeNull(); // still a real link, just not an in-page switch
  });
});

describe("panelHref", () => {
  it("rejects a non-http(s) scheme", () => {
    expect(panelHref("javascript:alert(1)")).toBeNull();
  });

  it("rewrites a same-origin absolute URL to relative", () => {
    const origin = window.location.origin;
    expect(panelHref(`${origin}/mission/abc/graph`)).toBe("/mission/abc/graph");
  });

  it("rewrites a loopback daemon URL to relative regardless of the page's own origin", () => {
    expect(panelHref("http://127.0.0.1:8765/mission/abc/graph")).toBe("/mission/abc/graph");
    expect(panelHref("http://localhost:8765/mission/abc/graph")).toBe("/mission/abc/graph");
  });

  it("leaves a genuinely foreign origin absolute", () => {
    expect(panelHref("https://github.com/kstrat2001/darkmux")).toBe("https://github.com/kstrat2001/darkmux");
  });

  it("returns null for an unparseable URL", () => {
    expect(panelHref("http://")).toBeNull();
  });
});

describe("panelSwitchId", () => {
  it("returns null for a foreign-origin link", () => {
    expect(panelSwitchId("https://github.com/#lens=console&panel=doctor")).toBeNull();
  });

  it("returns null for a same-origin link that isn't the console lens", () => {
    expect(panelSwitchId("/mission/abc/graph")).toBeNull();
  });

  it("returns null for a console link naming no panel id", () => {
    expect(panelSwitchId("/#lens=console")).toBeNull();
  });
});

describe("AnsiText", () => {
  it("renders a styled span with the mapped class", () => {
    const { container } = render(<AnsiText text={"\x1b[32mgreen\x1b[0m"} onPanelSwitch={vi.fn()} />);
    const span = container.querySelector(".a-fg2");
    expect(span?.textContent).toBe("green");
  });

  it("renders a plain foreign link as a real anchor with a real href", () => {
    const { container } = render(
      <AnsiText text={"\x1b]8;;https://github.com/x\x1b\\repo\x1b]8;;\x1b\\"} onPanelSwitch={vi.fn()} />,
    );
    const a = container.querySelector("a.a-link");
    expect(a).toBeTruthy();
    expect(a?.getAttribute("href")).toBe("https://github.com/x");
    expect(a?.textContent).toBe("repo");
  });

  it("an in-page panel-switch link calls onPanelSwitch and does not navigate", () => {
    const origin = window.location.origin;
    const onSwitch = vi.fn();
    const { container } = render(
      <AnsiText text={`\x1b]8;;${origin}/#lens=console&panel=doctor\x1b\\switch\x1b]8;;\x1b\\`} onPanelSwitch={onSwitch} />,
    );
    const a = container.querySelector('a[data-act="setpanel"]');
    expect(a?.getAttribute("data-arg")).toBe("doctor");
    (a as HTMLAnchorElement).click();
    expect(onSwitch).toHaveBeenCalledWith("doctor");
  });

  it("innerText concatenates plain and styled segments with no stray markup artifacts", () => {
    const { container } = render(<AnsiText text={"a\x1b[1mb\x1b[0mc"} onPanelSwitch={vi.fn()} />);
    expect(container.textContent).toBe("abc");
  });
});

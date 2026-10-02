import { afterEach, describe, expect, it } from "vitest";
import { DRAG_THRESHOLD_PX, endedSelectionOrDrag, onIntentClick } from "./clickIntent";

function mount(): { card: HTMLElement; other: HTMLElement } {
  document.body.innerHTML = `<div id="card"><span id="t">hello world</span></div><p id="other">elsewhere text</p>`;
  return { card: document.getElementById("card")!, other: document.getElementById("other")! };
}

function press(x: number, y: number) {
  document.body.dispatchEvent(new MouseEvent("pointerdown", { clientX: x, clientY: y, bubbles: true }));
}

function click(el: HTMLElement, x = 10, y = 10, detail = 1) {
  return { currentTarget: el, clientX: x, clientY: y, detail };
}

function select(el: Node) {
  const r = document.createRange();
  r.selectNodeContents(el);
  const s = window.getSelection()!;
  s.removeAllRanges();
  s.addRange(r);
}

afterEach(() => window.getSelection()?.removeAllRanges());

describe("endedSelectionOrDrag", () => {
  it("is true for a selection inside the clicked element", () => {
    const { card } = mount();
    press(10, 10);
    select(card.querySelector("#t")!.firstChild!);
    expect(endedSelectionOrDrag(click(card))).toBe(true);
  });

  it("is false for a selection elsewhere", () => {
    const { card, other } = mount();
    press(10, 10);
    select(other);
    expect(endedSelectionOrDrag(click(card))).toBe(false);
  });

  it("is false for a collapsed selection", () => {
    const { card } = mount();
    press(10, 10);
    const s = window.getSelection()!;
    s.removeAllRanges();
    s.collapse(card, 0);
    expect(endedSelectionOrDrag(click(card))).toBe(false);
  });

  it("is true when the pointer moved past the threshold", () => {
    const { card } = mount();
    press(10, 10);
    expect(endedSelectionOrDrag(click(card, 10 + DRAG_THRESHOLD_PX + 3, 10))).toBe(true);
  });

  it("is false for jitter under the threshold", () => {
    const { card } = mount();
    press(10, 10);
    expect(endedSelectionOrDrag(click(card, 12, 11))).toBe(false);
  });

  it("ignores a stale pointerdown for a synthesized (detail 0) click", () => {
    const { card } = mount();
    press(500, 500);
    expect(endedSelectionOrDrag(click(card, 0, 0, 0))).toBe(false);
  });

  it("is true for the second click of a double-click", () => {
    const { card } = mount();
    press(10, 10);
    expect(endedSelectionOrDrag(click(card, 10, 10, 2))).toBe(true);
  });
});

describe("onIntentClick", () => {
  it("runs the handler on a plain click and skips it on a selection", () => {
    const { card } = mount();
    let n = 0;
    const h = onIntentClick(() => void n++);
    press(10, 10);
    h(click(card));
    expect(n).toBe(1);
    select(card.querySelector("#t")!.firstChild!);
    h(click(card));
    expect(n).toBe(1);
  });
});

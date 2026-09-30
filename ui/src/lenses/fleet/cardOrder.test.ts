import { describe, it, expect } from "vitest";
import { cardOrderKey, compareCardOrder, orderCards, type CardOrder } from "./cardOrder";

const item = (key: string, self = false): CardOrder => ({ key, self });

describe("cardOrderKey", () => {
  it("prefers the machine uid over the fallback id, and ignores case", () => {
    expect(cardOrderKey("ABC-1", "studio")).toBe("abc-1");
    expect(cardOrderKey(null, "Studio")).toBe("studio");
    expect(cardOrderKey("", "Studio")).toBe("studio");
  });
});

describe("orderCards", () => {
  it("puts this machine first, then the rest by key, whatever the input order", () => {
    const a = [item("b"), item("z", true), item("a")];
    const b = [item("a"), item("b"), item("z", true)];
    expect(orderCards(a, (x) => x).map((x) => x.key)).toEqual(["z", "a", "b"]);
    expect(orderCards(b, (x) => x).map((x) => x.key)).toEqual(["z", "a", "b"]);
  });

  it("does not mutate its input", () => {
    const input = [item("b"), item("a")];
    orderCards(input, (x) => x);
    expect(input.map((x) => x.key)).toEqual(["b", "a"]);
  });

  it("treats equal keys as equal", () => {
    expect(compareCardOrder(item("a"), item("a"))).toBe(0);
  });
});

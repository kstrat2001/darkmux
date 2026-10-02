import { describe, it, expect } from "vitest";
import { compareCardOrder, orderCards, type CardOrder } from "./cardOrder";

const item = (uid: string | null, fallback = uid ?? "?"): CardOrder => ({ uid, fallback });

describe("orderCards", () => {
  it("orders by uid whatever the input order, so every serving machine lays the fleet out alike", () => {
    const a = [item("UUID-C"), item("UUID-A"), item("UUID-B")];
    const b = [item("UUID-B"), item("UUID-C"), item("UUID-A")];
    expect(orderCards(a, (x) => x).map((x) => x.uid)).toEqual(["UUID-A", "UUID-B", "UUID-C"]);
    expect(orderCards(b, (x) => x).map((x) => x.uid)).toEqual(["UUID-A", "UUID-B", "UUID-C"]);
  });

  it("ignores the case of a uid", () => {
    expect(orderCards([item("b-2"), item("A-1")], (x) => x).map((x) => x.uid)).toEqual(["A-1", "b-2"]);
  });

  it("puts a card whose uid is unknown after every known one, in a stable order by its fallback", () => {
    const cards = [item(null, "studio"), item("UUID-Z"), item(null, "mini"), item("UUID-A")];
    expect(orderCards(cards, (x) => x).map((x) => x.uid ?? x.fallback)).toEqual(["UUID-A", "UUID-Z", "mini", "studio"]);
  });

  it("does not mutate its input", () => {
    const input = [item("b"), item("a")];
    orderCards(input, (x) => x);
    expect(input.map((x) => x.uid)).toEqual(["b", "a"]);
  });

  it("treats the same uid as equal", () => {
    expect(compareCardOrder(item("a"), item("A"))).toBe(0);
  });
});

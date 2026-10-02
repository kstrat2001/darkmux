import { describe, it, expect } from "vitest";
import { compareCardOrder, orderCards, type CardOrder } from "./cardOrder";

const item = (name: string, uid = name): CardOrder => ({ name, uid });

describe("orderCards", () => {
  it("orders by name whatever the input order, so every serving machine lays the fleet out alike", () => {
    const a = [item("studio"), item("MacBook-Pro"), item("darkbook")];
    const b = [item("darkbook"), item("studio"), item("MacBook-Pro")];
    const want = ["darkbook", "MacBook-Pro", "studio"];
    expect(orderCards(a, (x) => x).map((x) => x.name)).toEqual(want);
    expect(orderCards(b, (x) => x).map((x) => x.name)).toEqual(want);
  });

  it("ignores case in the name", () => {
    expect(orderCards([item("beta"), item("Alpha")], (x) => x).map((x) => x.name)).toEqual(["Alpha", "beta"]);
  });

  it("settles two machines that share a name by uid, in either case", () => {
    expect(orderCards([item("Mac", "UUID-B"), item("Mac", "uuid-a")], (x) => x).map((x) => x.uid)).toEqual(["uuid-a", "UUID-B"]);
  });

  it("does not mutate its input", () => {
    const input = [item("b"), item("a")];
    orderCards(input, (x) => x);
    expect(input.map((x) => x.name)).toEqual(["b", "a"]);
  });

  it("treats the same name and uid as equal", () => {
    expect(compareCardOrder(item("a"), item("a"))).toBe(0);
  });
});

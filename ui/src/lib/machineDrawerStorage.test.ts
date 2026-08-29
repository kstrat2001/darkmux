import { describe, expect, it } from "vitest";
import { initDrawerOpen, persistDrawerOpen } from "./machineDrawerStorage";

function memoryStorage(): Storage {
  const m = new Map<string, string>();
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
    clear: () => m.clear(),
    key: () => null,
    length: 0,
  } as Storage;
}

describe("machine drawer open/closed persistence (#2107)", () => {
  it("defaults closed when nothing is stored", () => {
    expect(initDrawerOpen(memoryStorage())).toBe(false);
  });

  it("round-trips an open write", () => {
    const s = memoryStorage();
    persistDrawerOpen(true, s);
    expect(initDrawerOpen(s)).toBe(true);
  });

  it("round-trips a closed write after an open one", () => {
    const s = memoryStorage();
    persistDrawerOpen(true, s);
    persistDrawerOpen(false, s);
    expect(initDrawerOpen(s)).toBe(false);
  });

  it("degrades to closed rather than throwing when storage access throws", () => {
    const throwing: Pick<Storage, "getItem"> = {
      getItem: () => {
        throw new Error("blocked");
      },
    };
    expect(initDrawerOpen(throwing)).toBe(false);
  });
});

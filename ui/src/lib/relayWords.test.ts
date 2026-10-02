import { describe, expect, it } from "vitest";
import { relayedFromText } from "./relayWords";

describe("relayedFromText", () => {
  it("words a relayed run as 'from <machine>': the executor owns the run, the sender is shown", () => {
    expect(relayedFromText({ asked_on_machine: "MacBook-Pro" })).toBe("from MacBook-Pro");
  });
});

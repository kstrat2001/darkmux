import { describe, it, expect } from "vitest";
import { loadFailureText, staleReadText } from "./fetcher";
import { clk, clkOn } from "./format";

const noon = new Date(2026, 10, 19, 12, 0, 0).getTime();

describe("loadFailureText", () => {
  it("names the status when there was one", () => {
    expect(loadFailureText("runs", { status: 500, message: "500 Internal Server Error" })).toBe(
      "couldn't load runs (HTTP 500): 500 Internal Server Error",
    );
  });

  it("leaves the status out when the daemon never answered", () => {
    expect(loadFailureText("runs", { status: null, message: "Failed to fetch" })).toBe("couldn't load runs: Failed to fetch");
  });
});

describe("staleReadText", () => {
  const failure = { status: 500, message: "500 Internal Server Error" };

  it("a read from earlier today is the bare time of day", () => {
    const readAt = noon - 10 * 60000;
    expect(staleReadText("runs", readAt, failure, noon)).toBe(`showing runs from ${clk(readAt)}; couldn't refresh (HTTP 500): 500 Internal Server Error`);
  });

  it("a read from an earlier day names its date, so an overnight failure does not pass for today", () => {
    const readAt = noon - 13 * 3600000; // 23:00 the day before
    const text = staleReadText("runs", readAt, failure, noon);
    expect(text).toBe(`showing runs from ${clkOn(readAt, noon)}; couldn't refresh (HTTP 500): 500 Internal Server Error`);
    expect(text).not.toContain(`from ${clk(readAt)};`);
  });
});

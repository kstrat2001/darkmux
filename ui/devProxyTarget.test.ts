import { describe, it, expect } from "vitest";
import { devProxyTarget, DEFAULT_PORT } from "./devProxyTarget";

// (#2782) The dev proxy's target was a hardcoded `http://127.0.0.1:8765`.
// These pin that it follows the env tier, and that the two address-shape
// rules it borrows from the Rust side (wildcard collapse, IPv6 bracketing)
// actually apply — a bracket bug here produces a URL that silently proxies
// nowhere, which is the failure mode the whole fix is about.
describe("devProxyTarget", () => {
  it("defaults to the built-in loopback address with no env set", () => {
    expect(devProxyTarget({})).toBe(`http://127.0.0.1:${DEFAULT_PORT}`);
  });

  it("follows DARKMUX_SERVE_PORT — the defect this exists to fix", () => {
    // The operator's actual configuration when this was found.
    expect(devProxyTarget({ DARKMUX_SERVE_PORT: "8799" })).toBe("http://127.0.0.1:8799");
  });

  it("follows DARKMUX_SERVE_BIND, together with the port", () => {
    expect(
      devProxyTarget({ DARKMUX_SERVE_BIND: "192.0.2.10", DARKMUX_SERVE_PORT: "8799" }),
    ).toBe("http://192.0.2.10:8799");
  });

  it("collapses a wildcard bind to loopback — a bind directive is not a destination", () => {
    expect(devProxyTarget({ DARKMUX_SERVE_BIND: "0.0.0.0", DARKMUX_SERVE_PORT: "8799" })).toBe(
      "http://127.0.0.1:8799",
    );
    expect(devProxyTarget({ DARKMUX_SERVE_BIND: "::", DARKMUX_SERVE_PORT: "8799" })).toBe(
      "http://127.0.0.1:8799",
    );
  });

  it("brackets an IPv6 literal so the URL parses", () => {
    const url = devProxyTarget({ DARKMUX_SERVE_BIND: "::1", DARKMUX_SERVE_PORT: "8799" });
    expect(url).toBe("http://[::1]:8799");
    // The point of bracketing: an unbracketed `::1` yields `http://::1:8799`,
    // which is not a parseable URL, so the proxy would silently go nowhere.
    expect(new URL(url).port).toBe("8799");
    expect(() => new URL("http://::1:8799")).toThrow();
  });

  it("does not double-bracket an already-bracketed literal", () => {
    expect(devProxyTarget({ DARKMUX_SERVE_BIND: "[::1]", DARKMUX_SERVE_PORT: "8799" })).toBe(
      "http://[::1]:8799",
    );
  });

  it("treats blank and whitespace-only env values as unset", () => {
    expect(devProxyTarget({ DARKMUX_SERVE_PORT: "", DARKMUX_SERVE_BIND: "  " })).toBe(
      `http://127.0.0.1:${DEFAULT_PORT}`,
    );
  });
});

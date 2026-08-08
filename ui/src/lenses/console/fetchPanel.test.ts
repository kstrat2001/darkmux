import { describe, it, expect, vi, afterEach } from "vitest";
import { fetchPanel } from "./fetchPanel";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchPanel", () => {
  it("sends the required X-Darkmux-Panel header and cols query param", async () => {
    const fetchMock = vi.fn((_url: string, _init?: RequestInit) =>
      Promise.resolve(new Response(JSON.stringify({ panel: "doctor" }), { status: 200 })),
    );
    vi.stubGlobal("fetch", fetchMock);
    await fetchPanel("doctor", 100);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("/panel/doctor?cols=100");
    expect(init?.headers).toMatchObject({ "X-Darkmux-Panel": "1" });
  });

  it("URL-encodes the panel id", async () => {
    const fetchMock = vi.fn((_url: string, _init?: RequestInit) => Promise.resolve(new Response("{}", { status: 200 })));
    vi.stubGlobal("fetch", fetchMock);
    await fetchPanel("weird id/x", 100);
    expect(fetchMock.mock.calls[0][0]).toBe("/panel/weird%20id%2Fx?cols=100");
  });

  it("returns ok:true with the parsed body on 200", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ panel: "doctor", ansi_text: "x" }), { status: 200 }))));
    const result = await fetchPanel("doctor", 100);
    expect(result).toEqual({ ok: true, data: { panel: "doctor", ansi_text: "x" } });
  });

  it("on a non-2xx response, uses the DAEMON'S OWN response body text as the message, not a generic status line", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          new Response('unknown panel "bogus" — panels are a fixed allowlist, not arbitrary commands\n', { status: 404 }),
        ),
      ),
    );
    const result = await fetchPanel("bogus", 100);
    expect(result).toEqual({ ok: false, message: 'unknown panel "bogus" — panels are a fixed allowlist, not arbitrary commands' });
  });

  it("falls back to a generic message only when the daemon's body is empty", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("", { status: 500 }))));
    const result = await fetchPanel("doctor", 100);
    expect(result).toEqual({ ok: false, message: "panel request failed: HTTP 500" });
  });

  it("a network failure (fetch throws) is reported as 'could not reach the daemon'", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new TypeError("Failed to fetch"))),
    );
    const result = await fetchPanel("doctor", 100);
    expect(result.ok).toBe(false);
    expect((result as { message: string }).message).toMatch(/could not reach the daemon/);
  });
});

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

  // (#1911) opt.* params
  it("appends opt.<name>=<value> for every provided selection, sorted by name", async () => {
    const fetchMock = vi.fn((_url: string, _init?: RequestInit) => Promise.resolve(new Response("{}", { status: 200 })));
    vi.stubGlobal("fetch", fetchMock);
    await fetchPanel("run-list", 100, { all: "all", kind: "lab" });
    expect(fetchMock.mock.calls[0][0]).toBe("/panel/run-list?cols=100&opt.all=all&opt.kind=lab");
  });

  it("sends no opt.* params when none are given (matches the pre-#1911 URL shape exactly)", async () => {
    const fetchMock = vi.fn((_url: string, _init?: RequestInit) => Promise.resolve(new Response("{}", { status: 200 })));
    vi.stubGlobal("fetch", fetchMock);
    await fetchPanel("doctor", 100);
    expect(fetchMock.mock.calls[0][0]).toBe("/panel/doctor?cols=100");
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

// ── The static-build path (#2019) ────────────────────────────────────────
//
// The regression these exist for: `/panel/:id` on GitHub Pages is answered by
// PAGES, not a daemon, and this module renders a failed response's BODY as its
// message by design. The console therefore printed 9,379 bytes of
// `<!DOCTYPE html>` as command output on darkmux.com/demo.
//
// The pair below is the point. It is not enough to stop rendering HTML; the
// daemon's own text MUST still come through, because that contract is why the
// body is read at all (a 404 names the allowlist, a 429 explains the floor).
// Breaking one to fix the other would trade one wrong reading for another.

function meta(name: string, content: string) {
  const el = document.createElement("meta");
  el.setAttribute("name", name);
  el.setAttribute("content", content);
  document.head.appendChild(el);
  return el;
}

describe("fetchPanel — a reply that did not come from a daemon", () => {
  afterEach(() => {
    document.head.querySelectorAll("meta[name^='darkmux-']").forEach((e) => e.remove());
  });

  it("does NOT render an HTML error body as the panel message", async () => {
    const pagesBody = '<!DOCTYPE html>\n<html><head><title>Page not found &middot; GitHub Pages</title>';
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response(pagesBody, { status: 404, headers: { "content-type": "text/html; charset=utf-8" } }))),
    );
    const out = await fetchPanel("run-list", 120);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.message).not.toContain("<!DOCTYPE");
    expect(out.message).not.toContain("Page not found");
    expect(out.message).toContain("static build");
  });

  it("STILL renders the daemon's own text/plain error verbatim", async () => {
    const daemonBody = 'unknown panel "nope" — panels are a fixed allowlist, not arbitrary commands\n';
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response(daemonBody, { status: 404, headers: { "content-type": "text/plain" } }))),
    );
    const out = await fetchPanel("nope", 120);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.message).toBe(daemonBody.trim());
  });

  it("treats an absent content-type as the daemon's (its 429 path sets none)", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("rate limited: retry in 30s", { status: 429 }))));
    const out = await fetchPanel("doctor", 120);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.message).toBe("rate limited: retry in 30s");
  });
});

describe("fetchPanel — static build reads its committed panel map", () => {
  afterEach(() => {
    document.head.querySelectorAll("meta[name^='darkmux-']").forEach((e) => e.remove());
  });

  it("serves a captured panel and never touches /panel/:id", async () => {
    meta("darkmux-panels-src", "./demo-panels.json");
    const captured = { "run-list": { panel: "run-list", ansi_text: "runs — 10 shown", cols: 120 } };
    const fetchMock = vi.fn((_url: string, _init?: RequestInit) =>
      Promise.resolve(new Response(JSON.stringify(captured), { status: 200 })),
    );
    vi.stubGlobal("fetch", fetchMock);
    const out = await fetchPanel("run-list", 120);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.data.ansi_text).toBe("runs — 10 shown");
    expect(fetchMock.mock.calls[0][0]).toBe("./demo-panels.json");
    expect(fetchMock.mock.calls.some((c) => String(c[0]).startsWith("/panel/"))).toBe(false);
  });

  it("names a panel the demo did not capture, rather than failing vaguely", async () => {
    meta("darkmux-panels-src", "./demo-panels.json");
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({}), { status: 200 }))));
    const out = await fetchPanel("doctor", 120);
    expect(out.ok).toBe(false);
    if (out.ok) return;
    expect(out.message).toContain("doctor");
    expect(out.message).toContain("static build");
  });
});

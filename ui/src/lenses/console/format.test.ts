import { describe, it, expect, vi, afterEach } from "vitest";
import { panelAgeLabel } from "./format";
import type { PanelResponse } from "../../types/handwritten";

function body(overrides: Partial<PanelResponse> = {}): PanelResponse {
  return {
    panel: "mission-status",
    argv: ["mission", "status"],
    captured_ts_ms: Date.UTC(2026, 0, 1, 12, 0, 0),
    gather_ms: 8,
    exit_code: 0,
    ansi_text: "",
    stderr_tail: "",
    cols: 100,
    cache_ttl_ms: 3000,
    age_ms: 0,
    auto_refresh: true,
    ...overrides,
  };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("panelAgeLabel", () => {
  it("formats captured_ts_ms as a UTC HH:MM:SS string", () => {
    const a = panelAgeLabel(body(), Date.now());
    expect(a.hhmmss).toMatch(/^\d{2}:\d{2}:\d{2}$/);
  });

  it("reports the daemon's own cache age when age_ms is set", () => {
    const a = panelAgeLabel(body({ age_ms: 4200 }), Date.now());
    expect(a.served).toBe(" · daemon cache 4s");
  });

  it("omits the daemon-cache suffix when age_ms is zero", () => {
    const a = panelAgeLabel(body({ age_ms: 0 }), Date.now());
    expect(a.served).toBe("");
  });

  it("is stale once the client-side age exceeds 3x the TTL", () => {
    vi.useFakeTimers();
    const fetchedAt = Date.now();
    vi.advanceTimersByTime(3000 * 3 + 1);
    const a = panelAgeLabel(body({ cache_ttl_ms: 3000 }), fetchedAt);
    expect(a.stale).toBe(true);
    expect(a.ageSec).toBe(9);
  });

  it("is not stale just under the 3x threshold", () => {
    vi.useFakeTimers();
    const fetchedAt = Date.now();
    vi.advanceTimersByTime(3000 * 3 - 1);
    const a = panelAgeLabel(body({ cache_ttl_ms: 3000 }), fetchedAt);
    expect(a.stale).toBe(false);
  });

  it("a manual panel's ttl of 0 never reports stale, regardless of age", () => {
    vi.useFakeTimers();
    const fetchedAt = Date.now();
    vi.advanceTimersByTime(10 * 60 * 1000);
    const a = panelAgeLabel(body({ cache_ttl_ms: 0, auto_refresh: false }), fetchedAt);
    expect(a.stale).toBe(false);
  });
});

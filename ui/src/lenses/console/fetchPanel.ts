/**
 * `GET /panel/:id` fetch — deliberately NOT routed through the shared
 * `lib/fetcher.ts::fetchJson` every other lens uses. `viewer.html`'s
 * `loadPanel()` reads the DAEMON'S OWN response-body TEXT as the error
 * message on a non-2xx response (a 404 names the allowlist; a 429 explains
 * the manual-run floor and its `Retry-After`) — its own comment: "Inventing
 * our own wording here would be the twin-drift this whole packet exists to
 * kill." `fetchJson`'s generic contract intentionally does NOT read the body
 * on failure (`message: \`${status} ${statusText}\``, e.g. "404 Not Found")
 * — correct for every OTHER endpoint here, wrong for this one specifically,
 * so this module ports `loadPanel`'s exact error-text behavior instead of
 * reusing the shared wrapper and losing it.
 */
import type { PanelResponse } from "../../types/handwritten";

export type PanelFetchOutcome = { ok: true; data: PanelResponse } | { ok: false; message: string };

/**
 * `opts` (#1911) — the panel's own resolved, NON-DEFAULT `(name, value)`
 * selections (`lenses/console/panels.ts::canonicalOptPairs` is the one
 * caller-side canonicalizer; this function just serializes whatever it's
 * handed), appended as `opt.<name>=<value>` query params. The server
 * validates every pair against its own declared table and 400s on an
 * unknown name/value — this function does no validation of its own, same
 * posture as the existing `cols` param.
 */
export async function fetchPanel(id: string, cols: number, opts: Readonly<Record<string, string>> = {}): Promise<PanelFetchOutcome> {
  try {
    const params = new URLSearchParams({ cols: String(cols) });
    for (const name of Object.keys(opts).sort()) params.set(`opt.${name}`, opts[name]);
    const res = await fetch(`/panel/${encodeURIComponent(id)}?${params.toString()}`, {
      headers: { accept: "application/json", "X-Darkmux-Panel": "1" },
    });
    if (!res.ok) {
      const text = (await res.text()).trim();
      // The daemon's own text is the message — see module doc.
      return { ok: false, message: text || `panel request failed: HTTP ${res.status}` };
    }
    const data = (await res.json()) as PanelResponse;
    return { ok: true, data };
  } catch (e) {
    return { ok: false, message: "could not reach the daemon: " + String(e) };
  }
}

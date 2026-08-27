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
import { staticPanelsSrc } from "../../lib/staticSource";

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
/** The daemon-less path: one committed `panelId -> PanelResponse` map.
 *
 * A MISS is a first-class outcome, not an error to paper over — the demo ships
 * the panels it has, and a panel it does not have must say so plainly rather
 * than render anything that arrived over HTTP. `cols` is deliberately ignored:
 * a fixture was captured at one width by the harness that shot it, and
 * pretending otherwise would silently mis-wrap fixed-width ANSI. */
async function fetchStaticPanel(src: string, id: string): Promise<PanelFetchOutcome> {
  try {
    const res = await fetch(src, { headers: { accept: "application/json" } });
    if (!res.ok) {
      return { ok: false, message: `this static build could not load its panel data (HTTP ${res.status}).` };
    }
    const all = (await res.json()) as Record<string, PanelResponse>;
    const hit = all[id];
    if (!hit) {
      return {
        ok: false,
        message:
          `\`${id}\` is not in this demo's captured panels. This page is a static ` +
          `build with no daemon behind it, so it can only show panels that were ` +
          `recorded when it was published.`,
      };
    }
    return { ok: true, data: hit };
  } catch (e) {
    return { ok: false, message: "could not load this static build's panel data: " + String(e) };
  }
}

export async function fetchPanel(id: string, cols: number, opts: Readonly<Record<string, string>> = {}): Promise<PanelFetchOutcome> {
  const staticSrc = staticPanelsSrc();
  if (staticSrc !== null) return fetchStaticPanel(staticSrc, id);
  try {
    const params = new URLSearchParams({ cols: String(cols) });
    for (const name of Object.keys(opts).sort()) params.set(`opt.${name}`, opts[name]);
    const res = await fetch(`/panel/${encodeURIComponent(id)}?${params.toString()}`, {
      headers: { accept: "application/json", "X-Darkmux-Panel": "1" },
    });
    if (!res.ok) {
      // The daemon's own text is the message — see module doc. But ONLY the
      // daemon's: that contract assumes a daemon answered, and the body is
      // rendered verbatim into the console pane. On GitHub Pages nothing
      // answers `/panel/:id`, so Pages did, and the console rendered its
      // entire `<!DOCTYPE html>` 404 page as command output.
      //
      // A daemon panel error is `text/plain`. Anything else did not come from
      // the contract this module ports, so its body is not a message and is
      // not shown. Checking the CONTENT TYPE rather than sniffing the body for
      // "<!DOCTYPE" keeps this a statement about who answered, not a guess
      // about what they said.
      const ctype = res.headers.get("content-type") ?? "";
      const fromDaemon = ctype === "" || ctype.startsWith("text/plain");
      const text = fromDaemon ? (await res.text()).trim() : "";
      if (!fromDaemon) {
        return {
          ok: false,
          message:
            `panel request failed: HTTP ${res.status} — and the reply was ` +
            `${ctype.split(";")[0] || "an unknown type"}, not a darkmux daemon. ` +
            `This page is probably a static build with no daemon behind it.`,
        };
      }
      return { ok: false, message: text || `panel request failed: HTTP ${res.status}` };
    }
    const data = (await res.json()) as PanelResponse;
    return { ok: true, data };
  } catch (e) {
    return { ok: false, message: "could not reach the daemon: " + String(e) };
  }
}

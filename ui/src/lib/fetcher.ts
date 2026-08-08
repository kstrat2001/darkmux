/**
 * The one typed fetch wrapper every query in this app goes through. Returns
 * a DISCRIMINATED RESULT rather than throwing — a lens component checks
 * `result.ok` and renders the failure explicitly (naming the status/message)
 * instead of leaning on TanStack Query's generic `isError` boolean, which
 * carries no detail a "never silent" empty/error state needs to show.
 *
 * Same-origin only by construction (every darkmux daemon endpoint is
 * same-origin with the served viewer — see `crates/darkmux-serve/src/lib.rs`'s
 * CORS doc) — no base-URL config, no credentials mode to reason about.
 */
export type FetchResult<T> =
  | { ok: true; data: T }
  | { ok: false; status: number | null; message: string };

export async function fetchJson<T>(path: string, init?: RequestInit): Promise<FetchResult<T>> {
  let response: Response;
  try {
    response = await fetch(path, init);
  } catch (err) {
    // Network failure (daemon unreachable, DNS, offline) — no HTTP status at
    // all. `status: null` distinguishes this from a real non-2xx response.
    return { ok: false, status: null, message: err instanceof Error ? err.message : "network error" };
  }
  if (!response.ok) {
    return { ok: false, status: response.status, message: `${response.status} ${response.statusText}` };
  }
  try {
    const data = (await response.json()) as T;
    return { ok: true, data };
  } catch (err) {
    return {
      ok: false,
      status: response.status,
      message: `response was not valid JSON: ${err instanceof Error ? err.message : String(err)}`,
    };
  }
}

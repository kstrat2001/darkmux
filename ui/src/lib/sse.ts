import type { QueryClient, QueryKey } from "@tanstack/react-query";

/**
 * SSE spike (packet brief: prove the wiring pattern here; the LIVE proof
 * against the flatsat's real `/flow/:date/stream` route is Packet 5's job,
 * not this one). Feeds an `EventSource` into `queryClient.setQueryData` for a
 * flow-tail query, with reconnect-on-visibilitychange — the same shape the
 * legacy viewer's `startLiveTail`/date-rollover handling implements
 * (`viewer.html` around `LIVE_ES`), ported to the Query cache instead of a
 * hand-rolled render call.
 *
 * Deliberately NOT a React hook — a hook wrapper is a lens-packet concern
 * once a real flow-tail component exists to own the effect's lifecycle. This
 * module is the reusable primitive underneath one.
 */

export interface FlowTailHandle {
  close: () => void;
}

/**
 * Open (or reuse) an `EventSource` against `/flow/:date/stream`, appending
 * each parsed record onto the `queryKey`'s cached array via
 * `setQueryData`. Reconnects when the tab becomes visible again after having
 * gone hidden — a backgrounded tab's `EventSource` can silently stop
 * receiving events on some platforms; visibility is the same signal the
 * legacy viewer's date-rollover poll loop already treats as "worth
 * re-checking state over."
 */
export function startFlowTail(
  queryClient: QueryClient,
  queryKey: QueryKey,
  date: string,
  eventSourceFactory: (url: string) => EventSource = (url) => new EventSource(url),
): FlowTailHandle {
  let source: EventSource | null = null;

  const appendRecord = (raw: string) => {
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      // A malformed line from the stream is dropped, not thrown — the tail
      // must not die because one record failed to parse.
      return;
    }
    queryClient.setQueryData<unknown[]>(queryKey, (prev) => [...(prev ?? []), parsed]);
  };

  const open = () => {
    source?.close();
    source = eventSourceFactory(`/flow/${date}/stream`);
    source.onmessage = (event: MessageEvent<string>) => appendRecord(event.data);
  };

  const onVisibilityChange = () => {
    if (document.visibilityState === "visible") {
      open();
    }
  };

  open();
  document.addEventListener("visibilitychange", onVisibilityChange);

  return {
    close: () => {
      source?.close();
      source = null;
      document.removeEventListener("visibilitychange", onVisibilityChange);
    },
  };
}

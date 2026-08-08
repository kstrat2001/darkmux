import { QueryClient } from "@tanstack/react-query";

/**
 * Shared QueryClient. Defaults are deliberately conservative — per-query
 * `refetchInterval`s (see `queryKeys.ts`'s cadence table) override this where
 * a lens needs live polling; the global default just avoids an accidental
 * refetch storm on window refocus for a personal fleet dashboard, matching
 * the legacy viewer's own "poll on a fixed timer, not on every tab focus"
 * posture.
 */
export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        refetchOnWindowFocus: false,
        retry: 1,
        staleTime: 1_000,
      },
    },
  });
}

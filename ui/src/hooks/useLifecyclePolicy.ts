import { useEffect, useSyncExternalStore } from "react";
import { fetchJson } from "../lib/fetcher";
import { DEFAULT_POLICY, policyOf, type LifecyclePolicy } from "../lib/lifecycle";
import { getSource } from "../lib/source";
import type { HealthResponse } from "../types/generated/HealthResponse";
import type { RunsPolicy } from "../types/generated/RunsPolicy";

/** The lifecycle policy the daemon judges runs by, read once per page from
 * `/health`'s `lifecycle_policy` (cheap: it builds nothing), so every surface
 * judges a run by the same numbers the runs board does. Until it answers,
 * the defaults; a static build has no daemon and keeps them. A daemon that
 * publishes none (one from before it did, or a failed read) is said once on
 * the console, never silently. */
let current: LifecyclePolicy = DEFAULT_POLICY;
let requested = false;
const listeners = new Set<() => void>();

function isPolicy(p: unknown): p is RunsPolicy {
  const o = p as { stale_after_ms?: unknown; budget_wait_grace_ms?: unknown } | null;
  return typeof o?.stale_after_ms === "number" && typeof o.budget_wait_grace_ms === "number";
}

async function load(): Promise<void> {
  const res = await fetchJson<HealthResponse>("/health");
  // A static build or a failed read has no body; the guard keeps a daemon
  // that answered without the policy from being trusted on its type alone.
  const p: unknown = res.ok ? res.data.lifecycle_policy : undefined;
  if (!isPolicy(p)) {
    console.warn("darkmux viewer: the daemon published no lifecycle policy on /health; runs are judged by the default (a run silent for 20 minutes has stopped).");
    return;
  }
  current = policyOf(p);
  for (const l of listeners) l();
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

const snapshot = () => current;

export function useLifecyclePolicy(): LifecyclePolicy {
  useEffect(() => {
    if (requested || getSource().kind !== "daemon") return;
    requested = true;
    void load();
  }, []);
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

/** Test-only: forget the policy read, so a test starts from the defaults. */
export function __resetLifecyclePolicy(): void {
  current = DEFAULT_POLICY;
  requested = false;
}

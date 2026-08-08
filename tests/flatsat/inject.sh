#!/usr/bin/env bash
# Failure injection for the flatsat fleet — plain, composable docker verbs
# plus one bun helper for stream volume. Run from tests/flatsat/ (or
# anywhere; paths below are relative to this script).
#
# Usage: ./inject.sh <subcommand> [args]
#
# Subcommands:
#   pause-peer         Freeze the peer container (SIGSTOP via `docker pause`)
#                       — simulates a machine that's asleep/unreachable
#                       without tearing down its state. Presence heartbeats
#                       (if any) stop; the process resumes exactly where it
#                       left off on unpause-peer.
#   unpause-peer        Resume a paused peer.
#   stop-redis          Stop the shared Redis container — simulates the
#                       coordination substrate dropping mid-view.
#   start-redis          Restart Redis (compose keeps its container, so this
#                       is `docker compose start redis`, not a fresh one —
#                       any stream data XADD-ed before the stop survives on
#                       the container's own volume-less in-memory state IF
#                       the container itself wasn't removed; `down` between
#                       runs does remove it, `stop`/`start` does not).
#   kill-hub             SIGKILL the hub container (simulates a hard crash,
#                       not a graceful shutdown).
#   start-hub            `docker compose start hub` after kill-hub.
#   status               One-line status line per flatsat container.
#   flood-stream [n]     XADD n (default 10500) synthetic minimal records to
#                       the shared stream with MAXLEN ~ 10000, so Redis
#                       actually trims — the stream-at-cap scenario's setup
#                       step. Independent of the main seed (seed.mjs stays a
#                       realistic few-thousand-record dataset; this is an
#                       opt-in flood a scenario asks for explicitly).
set -euo pipefail
cd "$(dirname "$0")"

sub="${1:-status}"

case "$sub" in
  pause-peer)
    docker pause flatsat-peer
    ;;
  unpause-peer)
    docker unpause flatsat-peer
    ;;
  stop-redis)
    docker compose -p darkmux-flatsat stop redis
    ;;
  start-redis)
    docker compose -p darkmux-flatsat start redis
    ;;
  kill-hub)
    docker kill flatsat-hub
    ;;
  start-hub)
    docker compose -p darkmux-flatsat start hub
    ;;
  status)
    for c in flatsat-hub flatsat-peer flatsat-redis; do
      state=$(docker inspect -f '{{.State.Status}}' "$c" 2>/dev/null || echo "absent")
      echo "$c: $state"
    done
    ;;
  flood-stream)
    n="${2:-10500}"
    bun run lib/flood-stream.mjs "$n"
    ;;
  *)
    echo "unknown subcommand: $sub" >&2
    echo "usage: ./inject.sh <pause-peer|unpause-peer|stop-redis|start-redis|kill-hub|start-hub|status|flood-stream [n]>" >&2
    exit 1
    ;;
esac

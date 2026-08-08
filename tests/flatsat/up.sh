#!/usr/bin/env bash
# Build (once) + seed + bring the flatsat fleet up, in an order that never
# races a daemon reading state while the seed script is still writing it:
#
#   1. docker build the darkmux-flatsat:local image (ONE build; both hub
#      and peer reference the same tag — see Dockerfile's module doc for
#      why this isn't a host-built bind-mounted binary).
#   2. Start ONLY redis, wait for its healthcheck.
#   3. Seed BOTH machines' .state/ dirs + XADD the shared stream — daemons
#      aren't running yet, so there's nothing to race.
#   4. Start hub + peer, wait for their /health.
set -euo pipefail
cd "$(dirname "$0")"

echo "==> [1/4] building darkmux-flatsat:local (one image, shared by hub+peer)"
docker build -f Dockerfile -t darkmux-flatsat:local ../..

echo "==> [2/4] starting redis"
docker compose -p darkmux-flatsat up -d --wait redis

echo "==> [3/4] seeding .state/ (hub: full parity corpus, peer: hand-authored fixture)"
bun run seed/seed.mjs

echo "==> [4/4] starting hub + peer"
docker compose -p darkmux-flatsat up -d --wait hub peer

echo "==> flatsat is up:"
echo "    hub  -> http://127.0.0.1:18765"
echo "    peer -> http://127.0.0.1:18766"
echo "    redis -> 127.0.0.1:16379"

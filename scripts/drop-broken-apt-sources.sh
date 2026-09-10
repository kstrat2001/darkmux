#!/usr/bin/env bash
#
# Drop the Google Chrome apt source that ships in the GitHub Actions Ubuntu
# runner image itself — darkmux does not add it, and no darkmux job installs
# google-chrome-stable via apt anywhere in this repo (Playwright's `install
# --with-deps` fetches its own Chromium binary; it does not need this repo).
#
# When that mirror serves a package index whose hash doesn't match its
# release file — an ordinary mid-publish window on Google's end — `apt-get
# update` fails at exit 100 for the WHOLE runner, so every subsequent
# `apt-get install` on that runner fails too, regardless of what it was
# installing (#2601). Two jobs installing unrelated things (a browser, and
# separately redis) both went red on a pull request whose only change was a
# comment.
#
# Fix direction chosen: drop the source outright rather than pin apt to
# tolerate a failing third-party index. Tolerating a failure is the more
# dishonest of the two shapes here — it would also mask a REAL problem with
# a source a job actually needs. Dropping a source this repo never uses has
# no such downside: it removes the failure mode entirely instead of hiding
# it. See #2601 for the two shapes considered and the reasoning.
#
# Run this ONCE, before the first `apt-get update` in any job that touches
# apt (including implicitly, e.g. `playwright install --with-deps`) — never
# duplicated per job, so a job that adds its own install later inherits the
# fix instead of the flake.
#
# Matched by CONTENT (any source file naming Chrome's apt host), not by a
# hardcoded filename, so a future runner-image rename doesn't silently stop
# this from firing. Covers both the classic one-line `.list` format and the
# newer deb822 `.sources` format. Matches on the bare host (`dl.google.com`)
# rather than also requiring the `/linux/chrome` path segment immediately
# after it, since a URI can legally carry a port or other components between
# host and path — `dl.google.com` alone is specific enough here: darkmux
# itself never names that host in any source it adds.
set -euo pipefail

shopt -s nullglob
dropped=0
for f in /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
  if grep -q 'dl\.google\.com' "$f" 2>/dev/null; then
    echo "Dropping runner-shipped apt source: $f"
    sudo rm -f "$f"
    dropped=$((dropped + 1))
  fi
done

if [ "$dropped" -eq 0 ]; then
  echo "No Google Chrome apt source found on this runner — nothing to drop."
fi

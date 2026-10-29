#!/usr/bin/env bash
# brew-smoke.sh — install darkmux the way a brew user does and check what they
# get (#3423). Three brew-only breakages shipped because nothing ran this path:
# no release commit in brew builds (#3409), a brew service with no PATH to
# `docker` (#3418), a first-dispatch pull hung on a credential helper (#3419).
#
# Usage (CI, or a throwaway Mac): scripts/brew-smoke.sh [formula.rb]
#   default formula: packaging/homebrew/darkmux.rb
#
# It installs the formula from a scratch tap (building the tarball its `url`
# names, exactly what `brew install` gives a user), then checks:
#   1. `darkmux --version` is `<version> (release <DARKMUX_COMMIT[:7]>)`;
#   2. `brew services start` brings the daemon up and `/health` answers;
#   3. the daemon's own process PATH reaches Homebrew's bin and Docker's dirs.
# Everything it installs is removed on exit.
#
# Refuses to run outside CI unless BREW_SMOKE_ANYWAY=1: on a machine that runs
# darkmux from ~/.cargo/bin, a brew-installed darkmux would shadow it.
set -euo pipefail

FORMULA=${1:-packaging/homebrew/darkmux.rb}
TAP=darkmux-ci/smoke
PORT=8765

fail() { echo "brew-smoke: FAIL: $*" >&2; exit 1; }
ok() { echo "brew-smoke: ok: $*"; }

if [[ "${CI:-}" != "true" && "${BREW_SMOKE_ANYWAY:-}" != "1" ]]; then
  fail "not in CI; set BREW_SMOKE_ANYWAY=1 on a throwaway Mac (it installs and removes brew darkmux)"
fi
[[ -f "$FORMULA" ]] || fail "no formula at $FORMULA"

version=$(sed -n 's|.*/archive/refs/tags/v\([0-9][0-9.]*\)\.tar\.gz".*|\1|p' "$FORMULA" | head -1)
commit=$(sed -n 's/.*ENV\["DARKMUX_COMMIT"\] = "\([0-9a-f]\{40\}\)".*/\1/p' "$FORMULA" | head -1)
[[ -n "$version" ]] || fail "could not read the release version from the formula's url"
[[ -n "$commit" ]] || fail "could not read a 40-hex DARKMUX_COMMIT from the formula"

cleanup() {
  brew services stop "$TAP/darkmux" >/dev/null 2>&1 || true
  brew uninstall --force "$TAP/darkmux" >/dev/null 2>&1 || true
  brew untap "$TAP" >/dev/null 2>&1 || true
}
trap cleanup EXIT

export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1
brew tap-new --no-git "$TAP" >/dev/null
cp "$FORMULA" "$(brew --repo "$TAP")/Formula/darkmux.rb"
brew install --build-from-source "$TAP/darkmux"

# 1. The release commit is stamped (#3409).
bin="$(brew --prefix)/bin/darkmux"
got=$("$bin" --version)
want="darkmux $version (release ${commit:0:7})"
[[ "$got" == "$want" ]] || fail "--version is '$got', expected '$want'"
ok "$got"

# 2. The service starts and the daemon answers.
brew services start "$TAP/darkmux"
for _ in $(seq 1 60); do
  curl -fsS -m 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS -m 2 "http://127.0.0.1:$PORT/health" >/dev/null \
  || fail "daemon not answering /health on :$PORT after 60s; serve.err: $(tail -5 "$(brew --prefix)/var/log/darkmux/serve.err" 2>/dev/null)"
ok "brew services started the daemon; /health answers on :$PORT"

# 3. The daemon's own PATH reaches Homebrew's bin and Docker's dirs (#3418).
pid=$(lsof -t -nP -iTCP:"$PORT" -sTCP:LISTEN | head -1)
[[ -n "$pid" ]] || fail "no process listening on :$PORT"
daemon_path=$(ps eww -o command= -p "$pid" | tr ' ' '\n' | sed -n 's/^PATH=//p' | head -1)
[[ -n "$daemon_path" ]] || fail "could not read the daemon's PATH (pid $pid)"
for dir in "$(brew --prefix)/bin" /usr/local/bin "$HOME/.docker/bin"; do
  [[ ":$daemon_path:" == *":$dir:"* ]] || fail "daemon PATH lacks $dir: $daemon_path"
done
ok "daemon PATH reaches $(brew --prefix)/bin, /usr/local/bin and ~/.docker/bin"

echo "brew-smoke: PASS"

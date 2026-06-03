# Homebrew packaging

This directory holds darkmux's Homebrew formula and its companion wrapper
script. They live here in the main repo as the **source of truth** — the
actual operator-facing tap (`kstrat2001/homebrew-darkmux`) is a thin repo
that pulls these files in.

Tracking: [#618](https://github.com/kstrat2001/darkmux/issues/618).

## Files

- **`darkmux.rb`** — the formula. Currently head-only (no v0.5.0 tag yet
  per [#618](https://github.com/kstrat2001/darkmux/issues/618) item 4).
  Audit-clean (`brew audit --strict` against a local tap).
- **`darkmux-serve-wrapped`** — keychain-aware wrapper for
  `brew services start darkmux`. Reads `DARKMUX_REDIS_URL` from macOS
  Keychain at process-start so the password never lives in the launchd
  plist. Falls back to single-machine mode (no Redis URL) if the keychain
  item doesn't exist.

## Operator install path (once the tap repo exists)

```bash
brew tap kstrat2001/darkmux
brew install --HEAD darkmux           # head-only until v0.5.0
brew services start darkmux           # optional — runs serve under launchd
```

See the [always-on hub guide](../../docs/guide/always-on-hub.html) for the
full hub-posture setup (Redis hardening, audit substrate, log rotation,
daily integrity check) that composes on top of the brew-managed service.

## Setting up the tap repo (one-time, when ready to ship)

The tap is a standalone GitHub repo named `homebrew-<name>` under the user's
namespace. For darkmux: `kstrat2001/homebrew-darkmux`.

Steps:

1. **Create the tap repo on GitHub** as a public repo named
   `kstrat2001/homebrew-darkmux`. Minimal README + MIT license.
2. **Clone it locally and seed the formula directory:**
   ```bash
   git clone git@github.com:kstrat2001/homebrew-darkmux
   cd homebrew-darkmux
   mkdir -p Formula
   cp /path/to/darkmux-public/packaging/homebrew/darkmux.rb Formula/
   git add Formula/darkmux.rb
   git commit -m "feat: initial formula (head-only, refs #618)"
   git push
   ```
3. **Test from another machine:**
   ```bash
   brew tap kstrat2001/darkmux
   brew install --HEAD darkmux
   ```
4. **Set up CI for bottling per release** — a GitHub Actions workflow that
   builds bottles for `arm64_monterey`, `ventura`, `sonoma`, `sequoia` and
   uploads them to the tap's releases. See
   [Homebrew's bottle-building guide](https://docs.brew.sh/Bottles) for the
   actions setup; the `Homebrew/actions/setup-homebrew` action is the
   standard starting point.

## Keeping the tap in sync with the main repo

Two approaches:

**A. Manual copy on each release** (simplest, fine for early days):
After a tagged release in `kstrat2001/darkmux`, copy
`packaging/homebrew/darkmux.rb` from the main repo into the tap's
`Formula/darkmux.rb`, update the `url` and `sha256` lines for the new tag,
and push.

**B. Automated sync via GitHub Actions** (when manual gets old): a workflow
in the main repo that, on a new tag, computes the new tarball SHA256 and
opens a PR in the tap repo with the updated formula. Track this as a
follow-up to #618 once the manual process feels heavy.

## Updating the formula

When the formula needs changes (new dependencies, service block tweaks,
caveat updates, etc.):

1. Edit `darkmux.rb` in this directory.
2. Test locally:
   ```bash
   # Create a local tap if you don't already have one
   brew tap-new --no-git kstrat2001/darkmux
   TAP_DIR=$(brew --repository)/Library/Taps/kstrat2001/homebrew-darkmux
   mkdir -p "$TAP_DIR/Formula"

   # Copy your edits into the tap
   cp packaging/homebrew/darkmux.rb "$TAP_DIR/Formula/"

   # Audit
   brew audit --strict kstrat2001/darkmux/darkmux

   # Install + verify
   brew uninstall darkmux 2>/dev/null || true
   brew install --HEAD --build-from-source kstrat2001/darkmux/darkmux
   brew services info darkmux         # confirm plist generated correctly
   /opt/homebrew/bin/darkmux --version
   ```
3. Once verified, commit + push to the main repo. Then propagate to the tap
   repo per the sync approach (manual or automated) chosen above.

## What this DOESN'T solve

The hub guide [always-on-hub.html](../../docs/guide/always-on-hub.html)
spells these out in detail — repeated here so the formula's scope is
explicit:

- **`DARKMUX_AUDIT_DIR`** — opt-in compliance posture; operator runs the
  `/darkmux-enable-audit` skill.
- **`DARKMUX_ORCHESTRATOR`** — frontier-specific; operator's call.
- **Log rotation** (newsyslog) — formula sets log paths under
  `var/log/darkmux/` but rotation policy is operator preference.
- **Daily integrity-check launchd plist** — too specific to the audit
  substrate posture to live in a public formula.
- **Redis hardening** (AOF, memory cap, bind decision, requirepass) — the
  formula's caveats point at the guide; the operator runs the hardening.

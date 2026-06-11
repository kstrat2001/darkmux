# Homebrew packaging

This directory holds darkmux's Homebrew formula and its companion wrapper
script. They live here in the main repo as the **source of truth** — the
actual operator-facing tap (`kstrat2001/homebrew-darkmux`) is a thin repo
that pulls these files in.

Tracking: [#618](https://github.com/kstrat2001/darkmux/issues/618).

## Files

- **`darkmux.rb`** — the formula. Pinned to stable **v0.9.0** (url + sha256)
  with a `head` block for `--HEAD` installs from main. `brew style`-clean.
- **`darkmux-serve-wrapped`** — keychain-aware wrapper for
  `brew services start darkmux`. Reads `DARKMUX_REDIS_URL` from macOS
  Keychain at process-start so the password never lives in the launchd
  plist. Falls back to single-machine mode (no Redis URL) if the keychain
  item doesn't exist.

## Operator install path

The tap exists at [kstrat2001/homebrew-darkmux](https://github.com/kstrat2001/homebrew-darkmux)
(created 2026-06-04):

```bash
brew tap kstrat2001/darkmux
brew install darkmux                  # stable release (or --HEAD for latest main)
brew services start darkmux           # optional — runs serve under launchd
```

See the [always-on hub guide](../../docs/guide/always-on-hub.html) for the
full hub-posture setup (Redis hardening, audit substrate, log rotation,
daily integrity check) that composes on top of the brew-managed service.

The tap-side README, LICENSE, and BOOTSTRAP docs (source-of-truth for what
gets pushed into the tap repo) live at `tap-bootstrap/` in this directory.

## Keeping the tap in sync with the main repo

The source-of-truth formula is here at `packaging/homebrew/darkmux.rb`; the
tap is downstream. Two paths:

**A. Automated sync via `.github/workflows/sync-homebrew-tap.yml`**
(default; runs on every push to main that touches the formula):
The workflow opens a PR in the tap repo with the updated formula. Requires
a repository secret named `HOMEBREW_TAP_TOKEN`:

1. Create a fine-grained personal access token at
   https://github.com/settings/personal-access-tokens — Resource owner:
   your user; Repository access: `kstrat2001/homebrew-darkmux` only;
   Permissions: `Contents: Read and write` + `Pull requests: Read and write`.
2. Add it to this repo's secrets at
   `Settings → Secrets and variables → Actions → New repository secret`
   with name `HOMEBREW_TAP_TOKEN`.
3. The workflow checks the secret on every run; if missing it logs a notice
   and skips the sync. This means the workflow lands safely before the
   secret is configured.

Once configured, every change to `packaging/homebrew/darkmux.rb` on main
auto-opens a PR in the tap repo. Review + merge there; operators pick up
the new formula on their next `brew upgrade --HEAD darkmux`.

**B. Manual fallback** (when CI is down or the workflow is being edited):
```bash
cd /path/to/homebrew-darkmux           # or wherever you've cloned the tap
cp /path/to/darkmux-public/packaging/homebrew/darkmux.rb Formula/darkmux.rb
git diff Formula/darkmux.rb            # sanity check
git add Formula/darkmux.rb
git commit -m "sync: formula pulled from darkmux@<sha>"
git push
```

## Future: bottling per release

Track in [#618](https://github.com/kstrat2001/darkmux/issues/618) item 3.
The bottling workflow lives IN THE TAP REPO (not here) per Homebrew
convention — once Cargo.toml ships a real semver tag and the tap gains a
release-tag workflow that builds bottles for `arm64_monterey`, `ventura`,
`sonoma`, `sequoia` and uploads to the tap's GitHub releases, operators
can drop `--HEAD` and install pre-compiled binaries in ~5 seconds. See
[Homebrew's bottle-building guide](https://docs.brew.sh/Bottles) for the
template; the `Homebrew/actions/setup-homebrew` action is the starting
point.

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

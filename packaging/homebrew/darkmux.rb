# This is the source-of-truth Homebrew formula for darkmux. When the custom tap
# repo (kstrat2001/homebrew-darkmux per #618) is created, this file is copied
# into the tap as Formula/darkmux.rb. Editing it here keeps the formula
# version-controlled alongside the source it formulates.
#
# Operator-facing install path:
#   brew tap kstrat2001/darkmux
#   brew install darkmux                # stable release (v1.13.1)
#   brew install --HEAD darkmux         # build from main instead
#
# For local development / smoke testing:
#   brew install --build-from-source ./packaging/homebrew/darkmux.rb
#
# Tracking: #618 (this formula), #178 (skill it pairs with), #176 (sibling
# skill), #280 (related fleet primitive that simplifies post-formula).

class Darkmux < Formula
  desc "Profile multiplexer + lab for local LLM stacks (LMStudio, Ollama)"
  homepage "https://darkmux.com"
  # Stable release: v1.13.1 (stability patch from a review-swarm audit — a
  # compaction tool-boundary orphan that 400-ed long dispatches (#1158), a
  # dispatch panic that silently killed the fleet runner (#1159), lost jobs
  # published before a runner existed (#1160), /diff blocking + over-allocation
  # (#1161), a viewer per-session-id leak (#1162), + two message cleanups
  # (#1163). No schema change (FLOW_SCHEMA 1.14.0, CONFIG_SCHEMA 1.1) — drop-in
  # over v1.13.0).
  # `brew install darkmux` builds from this source tarball; `brew install
  # --HEAD darkmux` builds from main instead. The sha256 is of the
  # GitHub-generated source tarball for the tag (`shasum -a 256`).
  url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v1.13.1.tar.gz"
  sha256 "51907b513fce882e0188a3043827318faa18382583d6070cf88aa5564a40403c"
  license "MIT"
  head "https://github.com/kstrat2001/darkmux.git", branch: "main"

  depends_on "rust" => :build

  # Redis is OPTIONAL — operator decides single-machine vs hub:
  #   brew install darkmux                         # CLI only (single-machine)
  #   brew install darkmux && brew install redis   # hub posture
  # Documented in caveats below, not as a hard depends_on. A formula that
  # always pulls redis would force every CLI user to install a daemon they
  # don't run.

  def install
    # (#1129) Stamp a stable (tarball) build as a release so `darkmux --version`,
    # `darkmux doctor`, and the viewer header read `<version> (release)`. The
    # tarball has no `.git`, so without this stamp the build would be
    # indistinguishable from a bare source build. A `--HEAD` build has a git
    # checkout and bakes its short SHA instead, so leave it unstamped.
    ENV["DARKMUX_RELEASE"] = "1" unless build.head?

    # Workspace-aware install. The root [[bin]] target in Cargo.toml is the
    # only thing that needs to land in bin/. The std_cargo_args helper sets
    # --locked + --root + --path so the result lands at #{bin}/darkmux.
    system "cargo", "install", *std_cargo_args(path: ".")

    # Wrapper script for `brew services start darkmux`. The wrapper reads
    # DARKMUX_REDIS_URL from macOS Keychain at process-start so the password
    # never lives in the launchd plist file. Operator stores the password
    # once via `security add-generic-password -a $USER -s darkmux-redis -w`
    # (see caveats); the wrapper does the read + export + exec.
    libexec.install "packaging/homebrew/darkmux-serve-wrapped"
    chmod 0755, libexec/"darkmux-serve-wrapped"
  end

  service do
    run [opt_libexec/"darkmux-serve-wrapped"]
    keep_alive true
    log_path var/"log/darkmux/serve.out"
    error_log_path var/"log/darkmux/serve.err"
    working_dir Dir.home
    # NOTE: env vars baked in at plist-generation time (NOT process-start).
    # Anything that needs to be resolved live (Redis URL from Keychain,
    # operator-named DARKMUX_MACHINE_ID) is handled by the wrapper script
    # above, which reads them at exec time. The static defaults below are
    # the safe-to-bake values.
    environment_variables(
      DARKMUX_REDIS_STREAM: "darkmux:flow",
      DARKMUX_REDIS_MAXLEN: "10000",
    )
  end

  def caveats
    <<~EOS
      darkmux is installed but the `serve` daemon is NOT started by default.

      Scope: this formula ships the `darkmux` CLI + `serve` daemon + the
      keychain wrapper + bundled skills. The `darkmux-runtime` Docker image
      that `darkmux crew dispatch` and `darkmux lab run` use is not bundled,
      but you don't build it by hand: on the first dispatch with no local
      image, darkmux pulls the version-pinned image from GHCR on demand
      (ghcr.io/kstrat2001/darkmux-runtime:<version>, #759) — you just need
      Docker running. (`docker build -t darkmux-runtime:latest runtime/` from
      a source checkout is the offline/dev alternative.) So brew is a complete
      install end to end: `swap` / `status` / `profiles` / `fleet` / `flow` /
      `serve` / `doctor`, the hub coordinator role, AND local dispatches.

      For a single-machine CLI install (no daemon needed):
        # Already done. Run `darkmux --help` to explore.

      For a hub machine (Redis-backed, multi-machine fleet coordinator):
        1. brew install redis
           brew services start redis

        2. Set Redis password and store in Keychain. The wrapper script reads
           it at process-start (never lands in any file on disk).
             security add-generic-password -a "$USER" -s darkmux-redis -w
             # paste the password when prompted

           Then add `requirepass` to Redis matching that password:
             DARKMUX_REDIS_PASS=$(security find-generic-password \\
                                  -a "$USER" -s darkmux-redis -w)
             echo "requirepass \\"$DARKMUX_REDIS_PASS\\"" | \\
                  sudo tee -a #{HOMEBREW_PREFIX}/etc/redis.conf > /dev/null
             brew services restart redis

        3. In your shell rc (~/.zshrc), set per-machine identity:
             export DARKMUX_MACHINE_ID=studio     # operator-named; not hostname
             export DARKMUX_ORCHESTRATOR=claude-code

           (The wrapper script picks up DARKMUX_MACHINE_ID from your env at
           launchd-load time. Re-run `brew services restart darkmux` after
           any change.)

        4. brew services start darkmux

      For the production-grade hub posture — Redis AOF persistence, audit
      substrate (BLAKE3 hash-chained JSONL; any post-hoc edit is surfaced by
      `darkmux flow integrity-check`), log rotation, and daily integrity
      checks — see the guide:

        https://darkmux.com/guide/always-on-hub.html

      The Phase 2 launchd-plist section there is the under-the-hood version
      of what `brew services start darkmux` does for you; the other phases
      (Redis hardening, audit, log rotation, integrity check) remain
      operator-driven steps that compose on top of the brew-managed service.
    EOS
  end

  test do
    assert_match "darkmux", shell_output("#{bin}/darkmux --version")
    assert_match "fleet",   shell_output("#{bin}/darkmux fleet --help")
    assert_match "doctor",  shell_output("#{bin}/darkmux --help")
    # Doctor should run end-to-end without panic; non-zero exit is fine
    # (it'll warn about most checks in a fresh install with no profile + no
    # Redis + no LMStudio).
    system bin/"darkmux", "doctor"
  end
end

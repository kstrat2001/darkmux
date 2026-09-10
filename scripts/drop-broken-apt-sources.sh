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
# Root cause of why the source is even there to break: the browser package's
# own install script now writes the newer deb822 `.sources` format, but the
# runner image's own cleanup step still only removes the older one-line
# `.list` filename. The mismatch is what lets the source survive into the
# image at all — once the runner image's cleanup matches the newer filename,
# this script becomes a no-op safety net rather than the primary fix, and can
# eventually be dropped.
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
# Matched by CONTENT (any source that DECLARES dl.google.com as one of its
# URIs), not by a hardcoded filename, so a future runner-image rename doesn't
# silently stop this from firing. Covers both the classic one-line `.list`
# format (one `deb`/`deb-src` line per declared source) and the newer deb822
# `.sources` format, where a single `URIs:` field can itself hold more than
# one space-separated URI and can fold onto continuation lines (any line
# starting with whitespace continues the field above it) — each individual
# URI token is its own declared source, at the same granularity as one
# `deb`/`deb-src` line in the classic format.
#
# The match is precise on purpose, because a naive whole-file grep for the
# hostname is a source of real damage, not just false positives:
#   - it fires on the host appearing in a COMMENT, not a real source line
#   - it fires on the host appearing inside a Signed-By KEYRING PATH
#     (e.g. .../keyrings/dl.google.com.gpg), which names no such source
#   - it fires on the host as a SUBSTRING of an unrelated, longer hostname
#     (e.g. notdl.google.com.example.org)
#   - worst case: a file can legally carry more than one declared source
#     (e.g. the distribution archive's own file, with this browser's source
#     appended to it). Deleting the WHOLE FILE on any partial match would
#     delete the distribution archive itself — the index update would then
#     exit 0 with nothing left to fetch, and every later install would fail
#     with a misleading "unable to locate package", which is a worse, silent
#     failure than the flake this script exists to fix.
#
# So: only lines/fields that actually DECLARE a source are considered (a
# `deb`/`deb-src` line for .list; a `URIs:` field, folded across
# continuation lines, for .sources), only the declared URI VALUE is
# inspected — for .list, that means positionally locating it (past the
# `deb`/`deb-src` keyword and past an optional bracketed options block like
# `[arch=amd64 signed-by=...]`), never a hostname-shaped substring found
# anywhere else on the line, so a URL living INSIDE the options block (e.g.
# a signing key fetched over https) is never mistaken for the source's own
# URI. A URI's userinfo (`user:pass@`) is stripped before the host is read
# off, cutting at the LAST `@` — this reveals the real host when the real
# host is preceded by real credentials, and it refuses to let a URI use the
# target hostname AS fake userinfo to disguise an unrelated real host.
#
# A URI counts as a declared source once it is scheme-bearing (has a
# `scheme:` prefix), regardless of WHICH scheme — a mirror using `file:`,
# `cdrom:`, `copy:`, `ftp:`, or `tor+http(s):`, none of which carry a
# comparable host, still has to be counted, or a mixed file whose OTHER
# source uses one of those schemes looks like 100% of its declared sources
# match and gets deleted whole.
#
# The host must match dl.google.com EXACTLY as a hostname, not a substring.
# A file is removed only when EVERY declared source in it — every
# `deb`/`deb-src` line, every whitespace-separated URI token in every
# `URIs:` field — names that host. A file with a mix of sources is left
# entirely in place and a warning is printed instead — this script never
# edits a file down to a subset of its lines/URIs, only removes a file
# outright when 100% of what it declares is the one host we're after. (The
# browser's own install script does the more surgical per-line thing for
# the older format — commenting out only its own line when other sources
# share the file — but whole-file removal is simpler and safe here because
# it's the ONLY case a file is removed at all.) Splitting a multi-URI field
# uses `read -a` rather than an unquoted word list, because this script
# also turns on `nullglob`: an unquoted split would silently DROP any URI
# token containing a glob metacharacter (e.g. a `?query=` string) that
# happens to match no file in the working directory, undercounting the
# file's declared sources and letting a mixed file look like a full match.
#
# Reads and removals escalate to root only when needed: run directly if
# already root (a plain container job with no `sudo` binary at all), else
# via `sudo` if available. A file that can't be read, or a source that
# can't be removed because escalation isn't available or fails, is WARNED
# about on stderr AND as a GitHub Actions `::warning::` annotation (so the
# finding reaches the run summary, not just a buried log line inside a
# collapsed, exit-0 step) — never silently treated as "nothing to drop" the
# way a suppressed permission error would. A read failure reports the
# ORIGINAL (unprivileged) error even when a subsequent escalated read is
# also attempted and fails without output of its own (e.g. no `sudo`
# binary exists at all) — the first error is the informative one and is
# never allowed to be silently overwritten by a blank one.
#
# Exit status: this script exits non-zero if it found a source it could NOT
# remove (escalation unavailable or failed) — that leaves the exact flake
# this script exists to prevent still armed, and `apt-get update` is
# expected to fail right afterward anyway, so failing loudly here is more
# honest than reporting success. It exits 0 when nothing was found, when a
# mixed file was deliberately left in place (a judgment call, not a
# failure of this script), and on a normal successful drop.
set -euo pipefail

# Run "$@" as root: directly if we already are root (common in a plain
# container job that has no `sudo` binary at all), via `sudo` if we're not
# root but `sudo` is available, or fail loudly (exit 127, no command run) if
# neither applies — never blindly assume `sudo` exists and let a "command
# not found" abort the whole script mid-loop after we've already announced
# what we were about to do.
as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  elif command -v sudo >/dev/null 2>&1; then
    sudo "$@"
  else
    return 127
  fi
}

# Extract the hostname from a URI: strip the scheme, drop any userinfo
# (user[:pass]@) by cutting at the LAST '@' — never the first, so a
# credential value can't be used to disguise the real host on either side
# of the '@' — then cut at the first of / : ? # (whichever comes first) so
# a port, path, or query string never leaks into the comparison.
host_of() {
  local uri="$1" rest
  rest="${uri#*://}"
  case "$rest" in
    *@*) rest="${rest##*@}" ;;
  esac
  rest="${rest%%[/:?#]*}"
  printf '%s' "$rest"
}

# Exact hostname match — never a substring match, so
# notdl.google.com.example.org or dl.google.com.evil.example never counts.
host_matches() {
  local h="${1,,}"
  [ "$h" = "dl.google.com" ]
}

shopt -s nullglob
dropped=0
warned=0
remove_failed=0

for f in /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
  # Try an unprivileged read first — these files are normally world-readable
  # and the common case (no root, no sudo, ordinary permissions) should
  # never need escalation just to look. Only escalate if the plain read
  # fails, and only warn if BOTH fail — never swallow a permission error
  # into a silent "nothing to drop", and never lose the FIRST (usually more
  # informative) error behind a second attempt that produced no output of
  # its own.
  content=""
  first_err=""
  if ! content=$(cat "$f" 2>&1); then
    first_err="$content"
    if ! content=$(as_root cat "$f" 2>&1); then
      reason="$first_err"
      if [ -n "$content" ]; then
        reason="$reason; escalated read also failed: $content"
      fi
      echo "WARNING: could not read $f ($reason) — a Google Chrome apt source here, if any, was NOT checked" >&2
      warned=1
      continue
    fi
  fi

  total=0
  matching=0

  if [[ "$f" == *.sources ]]; then
    # deb822 stanza format: a stanza's URIs: field can hold more than one
    # space-separated URI, and its value can fold onto continuation lines
    # (any line starting with whitespace continues the field above it).
    # Every individual URI token — not the stanza as a whole — is its own
    # declared source, counted into the same file-wide total/matching
    # tally the classic-format branch below uses, so the file-level "every
    # declared source must match" rule applies at one consistent
    # granularity instead of contradicting an "any URI in this field"
    # stanza-level shortcut.
    stanza=""
    process_stanza() {
      local s="$1" line lc collecting=0 out="" uris=() u h
      while IFS= read -r line; do
        if [[ "$line" == [[:space:]]* ]]; then
          [ "$collecting" -eq 1 ] && out+=" $line"
          continue
        fi
        lc="${line,,}"
        if [[ "$lc" == uris:* ]]; then
          collecting=1
          out="${line#*:}"
        else
          collecting=0
        fi
      done <<<"$s"
      [ -z "${out//[[:space:]]/}" ] && return 0
      # read -a splits on whitespace WITHOUT pathname expansion — see the
      # header note on why an unquoted `for u in $out` word list is unsafe
      # here now that the rule is "all must match" rather than "any".
      read -ra uris <<<"$out"
      for u in "${uris[@]}"; do
        total=$((total + 1))
        h=$(host_of "$u")
        if host_matches "$h"; then
          matching=$((matching + 1))
        fi
      done
    }
    while IFS= read -r line; do
      if [ -z "$(printf '%s' "$line" | tr -d '[:space:]')" ]; then
        process_stanza "$stanza"
        stanza=""
      else
        stanza+="$line"$'\n'
      fi
    done <<<"$content"
    process_stanza "$stanza"
  else
    # Classic one-line format: one declared source per `deb`/`deb-src` line.
    # Comments and blank lines declare nothing; an inline trailing comment
    # after the fields is stripped before extracting the URI.
    while IFS= read -r line; do
      trimmed="$(printf '%s' "$line" | sed -E 's/^[[:space:]]+//')"
      [ -z "$trimmed" ] && continue
      [[ "$trimmed" == \#* ]] && continue
      [[ "$trimmed" =~ ^(deb|deb-src)[[:space:]] ]] || continue
      nocomment="${trimmed%%#*}"

      # Positionally locate the URI: strip the deb/deb-src keyword, then an
      # optional bracketed options block, then take the next
      # whitespace-separated token. This is the ONLY place the URI comes
      # from — never a hostname-shaped substring matched anywhere else on
      # the line, which previously let a URL living INSIDE the options
      # block (e.g. a signed-by= fetched over https) get mistaken for the
      # source's own URI.
      rest="${nocomment#deb-src}"
      [ "$rest" = "$nocomment" ] && rest="${nocomment#deb}"
      rest="$(printf '%s' "$rest" | sed -E 's/^[[:space:]]+//')"
      if [[ "$rest" == \[* ]]; then
        rest="${rest#*]}"
        rest="$(printf '%s' "$rest" | sed -E 's/^[[:space:]]+//')"
      fi
      uri="${rest%%[[:space:]]*}"
      [ -z "$uri" ] && continue

      # Count it as a declared source whenever it's scheme-bearing (has a
      # "scheme:" prefix), regardless of WHICH scheme. See the header note
      # on non-web schemes for why this can't be narrowed to http(s) only.
      if [[ "$uri" =~ ^[A-Za-z][A-Za-z0-9+.-]*: ]]; then
        total=$((total + 1))
        host=$(host_of "$uri")
        if host_matches "$host"; then
          matching=$((matching + 1))
        fi
      fi
    done <<<"$content"
  fi

  if [ "$total" -eq 0 ]; then
    continue
  fi

  if [ "$matching" -eq "$total" ]; then
    if as_root rm -f "$f"; then
      echo "Dropped runner-shipped apt source: $f"
      dropped=$((dropped + 1))
    else
      msg="found a Google Chrome apt source at $f but could not remove it (privilege escalation unavailable or failed) — apt-get update may still fail because of it"
      echo "WARNING: $msg" >&2
      echo "::warning::$msg"
      warned=1
      remove_failed=1
    fi
  elif [ "$matching" -gt 0 ]; then
    msg="$f declares a Google Chrome apt source (dl.google.com) alongside at least one other source — leaving the whole file in place rather than risk dropping something a job needs. Edit it by hand if it is causing apt-get update to fail."
    echo "WARNING: $msg" >&2
    echo "::warning::$msg"
    warned=1
  fi
done

if [ "$dropped" -eq 0 ] && [ "$warned" -eq 0 ]; then
  echo "No Google Chrome apt source found on this runner — nothing to drop."
fi

if [ "$remove_failed" -eq 1 ]; then
  exit 1
fi

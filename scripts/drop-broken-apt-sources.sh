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
# URI. The URI's AUTHORITY is isolated first — cutting off the path, query,
# and fragment before anything else touches the string — and only THEN is a
# userinfo (`user:pass@`) stripped from it, cutting at its LAST `@`. Doing
# the authority cut first matters: a path segment can itself contain an `@`,
# and cutting at the last `@` across the WHOLE URI (path included) mistakes
# that segment for userinfo, in both directions (verified against Python's
# own URL parser and a real connection with curl) — reading the wrong host
# out of a path that merely mentions the target hostname, and reading the
# wrong host out of the real target URI when ITS path happens to contain an
# `@`. Isolating the authority first means the userinfo cut can only ever
# see an `@` that is actually part of the authority — real credentials
# still reveal the real host, and a URI still can't use the target hostname
# as fake userinfo to disguise an unrelated real host.
#
# COUNTING a declared source and MATCHING it against the target host are two
# separate questions, and only the second one is allowed to be picky. Every
# `deb`/`deb-src` line counts, unconditionally, the moment it's recognized as
# one — same as every whitespace-separated `URIs:` token counts in the
# deb822 branch, no matter what it looks like. A line only gets to COUNT AS
# A MATCH once its URI is scheme-bearing (has a `scheme:` prefix, regardless
# of WHICH scheme — a mirror using `file:`, `cdrom:`, `copy:`, `ftp:`, or
# `tor+http(s):`, none of which carry a comparable host, still has to be
# counted as a non-matching source) and its host is dl.google.com exactly.
#
# The classic-format branch used to gate the COUNT itself on being able to
# cleanly extract a URI at all — which meant a `deb`/`deb-src` line this
# script's positional extraction didn't fully understand (verified against
# apt's own parser: a quoted URI, or an option value containing `#`, are
# real, apt-accepted shapes this extraction mishandles) simply vanished from
# the tally instead of costing a match. In a mixed file, that made an
# untouched OTHER source disappear from the total, leaving the browser
# source alone looking like 100% of what the file declares — and deleting
# the whole file, the other source included. That is the exact failure mode
# the paragraph above calls out as worse than the flake this script exists
# to fix, reached by a different door. The deb822 branch never had this
# hole, because it always counted every token it split out regardless of
# what the token looked like; the fix brings the classic branch's counting
# rule into line with it, so a line this script can't fully parse can only
# ever cost a match, in both formats alike, never quietly remove one from
# the denominator.
#
# That guarantee covers the denominator, not the numerator — a line the
# script COULD extract *a* URI from, but the wrong one, could still create a
# false match instead of merely costing one. It did: an option value glued
# directly to the URI that follows it with no separating space (e.g.
# `[signed-by=...]https://dl.google.com/x] https://example.org/repo`) used
# to have its options block cut at the FIRST `]` — which landed inside the
# glued browser-looking text rather than at the option's own close — so a
# third-party source got read as `https://dl.google.com/x]`, matched, and
# deleted, while the real declared source (`example.org`) was destroyed
# alongside it with no warning. The options-block walk below now stops only
# at a `]` actually followed by whitespace (or end of line), matching apt's
# own cutpoint exactly, so this shape resolves to the real URI instead of a
# false match — closing that hole rather than merely documenting it.
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
# A file this script COULD read but couldn't fully parse (a quoted URI is
# the known real-world shape) gets the same "never silently claim nothing
# was found" treatment. This script never GUESSES such a line into a match
# — it is always left in place, which is the safe direction — but it also
# can't prove the line ISN'T a Google Chrome source apt would still fetch,
# so it's WARNED about too, and that warning is what keeps the closing
# "nothing to drop" line from asserting something this script never
# actually checked.
#
# Exit status: this script exits non-zero if it found a source it could NOT
# remove (escalation unavailable or failed) — that leaves the exact flake
# this script exists to prevent still armed, and `apt-get update` is
# expected to fail right afterward anyway, so failing loudly here is more
# honest than reporting success. It exits 0 when nothing was found, when a
# mixed file was deliberately left in place (a judgment call, not a
# failure of this script), and on a normal successful drop.
#
# New assumption this creates, named explicitly: a hard exit here now makes
# passwordless privilege escalation (root, or `sudo` with no password
# prompt) a DEPENDENCY of every job that calls this script, not just an
# eventual one. Not live today — every call site in this repo runs on
# GitHub's own hosted `ubuntu-latest` runner, where the default user already
# has passwordless `sudo`. But before this exit-status change, a job whose
# `sudo` broke would only have failed later, wherever it next needed
# escalation itself: the redis job already depends on it explicitly a few
# lines after this script runs (`sudo apt-get install redis-server`); the
# two Playwright call sites depend on it only implicitly, inside
# `--with-deps`'s own internal apt-get. For those two, this script's own
# exit code is now the FIRST thing that would fail — the same dependency,
# arriving one step earlier than before.
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

# Extract the hostname from a URI: strip the scheme, cut off everything from
# the first of / ? # onward to isolate the AUTHORITY first (userinfo@host:port
# — never the path/query/fragment), THEN drop any userinfo (user[:pass]@)
# within that authority by cutting at its LAST '@', THEN drop a port by
# cutting at the first remaining ':' — EXCEPT when the host itself is a
# bracketed IPv6 literal (`[::1]`, optionally followed by `:port`), where the
# host is everything between the brackets and a naive first-':' cut would
# instead chop the literal apart at its own internal colons. Neither of
# darkmux's targets (dl.google.com, a hostname) is ever an IPv6 literal, so
# this branch can never itself produce a match either way — it only keeps
# the extracted "host" honest for a shape this script otherwise mangles.
#
# The authority must be isolated BEFORE the userinfo cut, not folded into one
# cut across the whole remainder — a path segment can itself contain an '@'
# (e.g. https://example.com/@dl.google.com/repo), and cutting at the LAST '@'
# across the whole string mistakes that path segment for userinfo, reporting
# the wrong host in both directions: an unrelated URI whose PATH merely
# mentions the target host reads as a match (a false positive that can delete
# an unrelated file), and the real target host followed by a path containing
# its own '@' reads as some other host entirely (a false negative that lets
# the real source silently survive). Isolating the authority first means the
# only '@' ever considered is one that is actually part of the authority.
host_of() {
  local uri="$1" rest authority
  rest="${uri#*://}"
  authority="${rest%%[/?#]*}"
  case "$authority" in
    *@*) authority="${authority##*@}" ;;
  esac
  case "$authority" in
    \[*)
      # IPv6 literal: the host is everything between the brackets; an
      # optional port, if present, follows immediately after the closing
      # bracket as ":port" and is dropped along with the bracket itself.
      authority="${authority#\[}"
      authority="${authority%%]*}"
      ;;
    *)
      authority="${authority%%:*}"
      ;;
  esac
  printf '%s' "$authority"
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
      msg="could not read $f ($reason) — a Google Chrome apt source here, if any, was NOT checked"
      echo "WARNING: $msg" >&2
      echo "::warning::$msg"
      warned=1
      continue
    fi
  fi

  total=0
  matching=0
  file_unresolved=0

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
        # apt ends the options block at the FIRST ']' that is followed by
        # whitespace (or end of line) — not simply the first ']' anywhere.
        # Verified against apt's own parser both ways: a ']' glued directly
        # to what follows it (no separating space) does NOT end the block,
        # and a ']' that genuinely is followed by whitespace still ends it
        # exactly where expected. Cutting at the first ']' unconditionally
        # (the prior behavior) let an option value glued straight to a
        # following URI — e.g. `[signed-by=...]https://dl.google.com/x]
        # https://example.org/repo` — misread as if the option's own
        # trailing text were the declared source's URI, treating a
        # third-party source as a Google Chrome one and deleting it. Walking
        # bracket-by-bracket instead of stopping at the first one reproduces
        # apt's real cutpoint, so the URI extracted here is the one apt
        # itself would use.
        walk="$rest"
        closed=0
        while [[ "$walk" == *']'* ]]; do
          after="${walk#*]}"
          if [ -z "$after" ] || [[ "$after" == [[:space:]]* ]]; then
            rest="$after"
            closed=1
            break
          fi
          walk="$after"
        done
        # An options block with no ']' that is ever followed by whitespace
        # (or end of line) is malformed in a way this script can't resolve
        # into a clean URI — leave `rest` empty so the unconditional count
        # below still tallies the line, but the unparsed-line accounting
        # further down treats it the same as any other line this script
        # can't finish extracting a URI from, rather than guessing.
        [ "$closed" -eq 0 ] && rest=""
        rest="$(printf '%s' "$rest" | sed -E 's/^[[:space:]]+//')"
      fi
      uri="${rest%%[[:space:]]*}"

      # A `deb`/`deb-src` line ALWAYS declares a source, whether or not this
      # script managed to extract a clean URI from it — so it is counted
      # unconditionally, here, before any test of what the URI looks like.
      # This is the one thing that makes the deb822 branch above and this
      # branch agree: over there, every whitespace-separated URIs: token is
      # counted no matter what it looks like, so a token this script doesn't
      # understand only ever COSTS a match (protects the file); here, before
      # this fix, a line whose URI this script couldn't cleanly extract was
      # dropped from the tally entirely instead — the exact inversion of that
      # rule, and the more dangerous one: it let an unparsed OTHER source
      # vanish from a mixed file's total, making the file look like a 100%
      # match on the browser source alone and deleting the whole thing,
      # including the source this script never even looked at. Real apt
      # accepts a quoted URI, and an option value containing '#' (both
      # verified against apt's own parser), neither of which this script's
      # positional extraction fully understands — so those shapes still fail
      # to yield a clean match below, but they can no longer make a mixed
      # file's total look smaller than it is. (An option value containing
      # ']' USED to be a third such shape, but glued straight to a following
      # URI with no separating space it didn't just fail to match — it could
      # extract the WRONG URI and falsely match on it, per the options-block
      # walk fixed above; it's not counted alongside these two because it's
      # not merely safe-but-lossy the way they are.)
      total=$((total + 1))
      if [ -z "$uri" ]; then
        # Recognized as a real declared source, but this script could not
        # extract ANY URI from it (an options block whose ']' never lands
        # before whitespace or end of line, i.e. the malformed-brackets case
        # above). Never counts as a match — see the residual-honesty note
        # near the closing summary for why this still has to be surfaced.
        file_unresolved=$((file_unresolved + 1))
        continue
      fi

      # Only a scheme-bearing URI can possibly MATCH — gate the match test,
      # never the count above, on this. See the header note on non-web
      # schemes for why this can't be narrowed to http(s) only.
      if [[ "$uri" =~ ^[A-Za-z][A-Za-z0-9+.-]*: ]]; then
        host=$(host_of "$uri")
        if host_matches "$host"; then
          matching=$((matching + 1))
        fi
      else
        # Recognized as a real declared source, and this script DID extract
        # something — but it isn't a URI apt would accept as one (a quoted
        # URI is the known real-world example: the extracted token still
        # carries its literal quote characters, so it can never look
        # scheme-bearing). Same residual-honesty accounting as the
        # empty-`uri` case above.
        file_unresolved=$((file_unresolved + 1))
      fi
    done <<<"$content"
  fi

  if [ "$file_unresolved" -gt 0 ]; then
    # This file counts as declaring $file_unresolved source line(s) this
    # script could not turn into a clean, comparable URI — apt itself may
    # still parse and fetch what's there (a quoted URI is a real,
    # apt-accepted shape; see the mkcorpus note this pairs with), so the
    # script genuinely does not know whether one of them names
    # dl.google.com. It never matches these lines either way (safe: nothing
    # gets deleted on a guess) — but staying silent about the gap would let
    # the closing summary below claim "no Google Chrome apt source found"
    # when the honest answer is "this script couldn't tell." Reusing the
    # `warned` flag (rather than a separate silent counter) is what keeps
    # that summary honest: it suppresses the false-negative claim the same
    # way any other warning already does.
    msg="$f declares $file_unresolved source line(s) this script could not fully parse into a comparable URI — it cannot rule out one of them being a Google Chrome apt source that apt itself would still fetch. Left in place; inspect by hand if apt-get update continues to fail on this runner because of a source here."
    echo "WARNING: $msg" >&2
    echo "::warning::$msg"
    warned=1
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

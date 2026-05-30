#!/usr/bin/env bash
# integrations/openclaw/oc-scaffold.sh — emit darkmux's validated
# openclaw agent scaffolds (a `systemPromptOverride` body + recommended
# profile + tool subset) as a paste-ready `agents.list[]` snippet.
#
# This is a STANDALONE utility for openclaw users, NOT a darkmux CLI verb
# (it replaced `darkmux agent template`, #538). It needs no darkmux binary
# — only `jq` and this repo's `agent-scaffolds/*.json`. That keeps the
# openclaw integration maintainable on its own: edit a scaffold JSON and
# re-run; no `cargo install --force` to pick up the change. The doctrine
# is "the engine doesn't compile openclaw-schema knowledge into its verb
# surface" — see DESIGN.md "Relationship to openclaw".
#
# Usage:
#   integrations/openclaw/oc-scaffold.sh list             # list available scaffolds
#   integrations/openclaw/oc-scaffold.sh template <role>  # emit agents.list[] snippet
#   integrations/openclaw/oc-scaffold.sh -h, --help       # this help
#
# The snippet goes to stdout (pipe/redirect it); guidance goes to stderr.
# Paste the emitted object into the `agents.list` array of your openclaw
# config (e.g. ~/.openclaw/openclaw.json). darkmux never auto-edits it —
# the operator owns their agent definitions.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCAFFOLDS_DIR="$SCRIPT_DIR/agent-scaffolds"
SELF="integrations/openclaw/oc-scaffold.sh"

print_help() {
    cat <<EOH
$SELF — emit darkmux's validated openclaw agent scaffolds as a
paste-ready agents.list[] snippet (replaces the old \`darkmux agent\` verb).

Usage:
  $SELF list             # list the role scaffolds this repo ships
  $SELF template <role>  # emit the agents.list[] snippet for <role>
  $SELF -h, --help       # this help

Snippet → stdout; paste it into the \`agents.list\` array of your
openclaw config (e.g. ~/.openclaw/openclaw.json). Needs \`jq\`.
EOH
}

if ! command -v jq >/dev/null 2>&1; then
    echo "oc-scaffold: \`jq\` not found on PATH — install jq to use this script." >&2
    exit 1
fi

if [ ! -d "$SCAFFOLDS_DIR" ]; then
    echo "oc-scaffold: scaffolds dir not found: $SCAFFOLDS_DIR" >&2
    exit 1
fi

# Collect available role ids from the scaffold JSON filenames.
role_ids=()
for f in "$SCAFFOLDS_DIR"/*.json; do
    [ -e "$f" ] || continue
    role_ids+=("$(basename "$f" .json)")
done

available_csv() { local out; out="$(printf '%s, ' "${role_ids[@]}")"; echo "${out%, }"; }

cmd_list() {
    echo "darkmux ships ${#role_ids[@]} openclaw role scaffold(s):"
    echo
    for f in "$SCAFFOLDS_DIR"/*.json; do
        [ -e "$f" ] || continue
        jq -r '
            "• \(.role) (\(.runtime))",
            "    \(.description)",
            "    pairs with profile: \(.recommended_profile), tools: \(.recommended_tools | join(", "))",
            ""
        ' "$f"
    done
    echo "Generate a snippet:  $SELF template <role>"
}

cmd_template() {
    local role="${1:-}"
    if [ -z "$role" ]; then
        echo "oc-scaffold: template requires a <role> (available: $(available_csv))" >&2
        exit 2
    fi
    local file="$SCAFFOLDS_DIR/$role.json"
    if [ ! -f "$file" ]; then
        echo "oc-scaffold: agent role '$role' not found. Available: $(available_csv)" >&2
        exit 2
    fi

    # Mirror the agents.list[] shape the old `darkmux agent template`
    # emitted (openclaw 2026.5+ schema: tools is an object with `allow`).
    jq '{
        "_notes": [
            "Auto-drafted by `oc-scaffold.sh template \(.role)` (runtime=\(.runtime)).",
            "Pair with the `\(.recommended_profile)` profile for best fit. Adjust tools/skills to taste."
        ],
        id: .role,
        systemPromptOverride: .override_text,
        tools: { allow: .recommended_tools }
    }' "$file"

    # Paste guidance to stderr so stdout stays a clean JSON object.
    local profile
    profile="$(jq -r '.recommended_profile' "$file")"
    {
        echo
        echo "// Paste the above object into the \`agents.list\` array of your openclaw"
        echo "// config (e.g. ~/.openclaw/openclaw.json). Recommended profile: \`$profile\`."
        echo "// Adjust \`tools\` to taste; the override text is the validated scaffold —"
        echo "// tune the task-specific framing for your codebase, but keep the structural"
        echo "// blocks (Tool Call Style, Execution Bias) — they're the load-bearing parts."
    } >&2
}

case "${1:-}" in
    list)            cmd_list ;;
    template)        shift; cmd_template "${1:-}" ;;
    -h|--help|"")    print_help ;;
    *)
        echo "oc-scaffold: unknown command: $1" >&2
        echo "Try: $SELF --help" >&2
        exit 2
        ;;
esac

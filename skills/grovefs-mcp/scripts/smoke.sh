#!/usr/bin/env bash
# Connection + workflow self-test for the `grovefs mcp` entrance.
#
# Builds a THROWAWAY store in a temp dir (never touches a real one), drives the
# whole golden workflow through the MCP framing via mcp_call.py, and asserts the
# answers. Use it after wiring an MCP client, after a rebuild, or when a call
# behaves unexpectedly — it isolates "is my client/framing wrong" from "is the
# server wrong".
#
#   bash scripts/smoke.sh
#
# Exit 0 = every check passed; non-zero = the failing checks are printed.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
CALL="$HERE/mcp_call.py"
BIN=${GROVEFS_BIN:-}
[ -n "$BIN" ] || { command -v grovefs >/dev/null 2>&1 && BIN=grovefs; }
if [ -z "$BIN" ]; then
    # The skill lives at <repo>/skills/grovefs-mcp, but accept any nesting depth.
    for cand in ../../target/release/grovefs ../../../target/release/grovefs ./target/release/grovefs; do
        [ -x "$cand" ] && { BIN="$cand"; break; }
    done
fi
[ -n "$BIN" ] || { echo "no grovefs binary (set GROVEFS_BIN)"; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "python3 required"; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STORE="$TMP/store"
SEEDS="$TMP/seeds"
mkdir -p "$STORE" "$SEEDS/docs/2026" "$SEEDS/photos/private"
echo "team a"     > "$SEEDS/docs/a.md"
echo "team b"     > "$SEEDS/docs/2026/b.md"
echo "team private" > "$SEEDS/photos/private/x.jpg"
# A symlink too, so the `size` self-description can be checked per kind.
ln -s a.md "$SEEDS/docs/latest"

pass=0
fail=0
note() { printf '%s\n' "$*"; }
# ok <desc> <haystack> <needle>
ok() { # ok DESC TEXT NEEDLE_SUBSTRING
    if printf '%s' "$2" | grep -qF -- "$3"; then pass=$((pass+1)); note "  OK:   $1";
    else fail=$((fail+1)); note "  FAIL: $1 (want '$3' in: $(printf '%s' "$2" | head -c 200))"; fi
}
no() { # no DESC TEXT NEEDLE_MUST_BE_ABSENT
    if printf '%s' "$2" | grep -qF -- "$3"; then fail=$((fail+1)); note "  FAIL: $1 ('$3' present)";
    else pass=$((pass+1)); note "  OK:   $1"; fi
}
code() { # code DESC EXPECTED_EXIT CMD...
    local desc="$1" want="$2"; shift 2
    "$@" >/dev/null 2>&1; local got=$?
    if [ "$got" = "$want" ]; then pass=$((pass+1)); note "  OK:   $desc (exit $got)";
    else fail=$((fail+1)); note "  FAIL: $desc (want exit $want, got $got)"; fi
}
mcp() { python3 "$CALL" --bin "$BIN" --quiet --timeout 20 --db-path "$STORE" "$@" 2>/dev/null; }
# jpick TEXT SUFFIX — the first rendered display path ending in SUFFIX.
# Rules are written against paths the store actually shows, never guessed ones.
jpick() { printf '%s' "$1" | python3 -c '
import json, sys
txt = sys.stdin.read()
suffix = sys.argv[1]
d = json.loads(txt[txt.index("{"):])
print(next(e["path"] for e in d["entries"] if e["path"].endswith(suffix)))' "$2"; }
# jsize TEXT SUFFIX — the `size` reported for the entry ending in SUFFIX.
jsize() { printf '%s' "$1" | python3 -c '
import json, sys
txt = sys.stdin.read()
suffix = sys.argv[1]
d = json.loads(txt[txt.index("{"):])
print(next(e["size"] for e in d["entries"] if e["path"].endswith(suffix)))' "$2"; }

# ---------------------------------------------------------------- seed a base
note "[0] seed a physical view (CLI only — the MCP entrance never builds base trees)"
note "$( $BIN --home "$STORE" import --view team --source "$SEEDS" | tail -1 )"

note "[1] handshake + tool surface"
TOOLS="$(mcp list)"
ok "nine tools advertised"          "$TOOLS" "(9 tools)"
ok "shape preview tool present"     "$TOOLS" "list_view_tree"
ok "pre-publish dry run present"    "$TOOLS" "validate_definition"

note "[2] discovery"
VS="$(mcp call list_views)"
ok "physical view listed"           "$VS" '"team"'
TREE="$(mcp call list_view_tree '{"view":"team"}')"
ok "tree renders real display path" "$TREE" "/photos/private/x.jpg"
# `size` is the METADATA field, not a byte count, and the tool says so — pin both
# halves so the docs cannot drift away from the render again.
ok "the tool admits size is not bytes" "$(mcp --json list)" "NOT a byte count"
SIZE_A="$(jsize "$TREE" "$(jpick "$TREE" "/docs/a.md")")"
SIZE_DOCS="$(jsize "$TREE" "$(jpick "$TREE" "/docs")")"
[ "$SIZE_A" = "0" ] \
  && { pass=$((pass+1)); note "  OK:   a file renders size 0 (metadata only)"; } \
  || { fail=$((fail+1)); note "  FAIL: a file renders size 0 (got $SIZE_A)"; }
[ "$SIZE_DOCS" = "4096" ] \
  && { pass=$((pass+1)); note "  OK:   a dir renders the 4096 stat placeholder"; } \
  || { fail=$((fail+1)); note "  FAIL: a dir renders 4096 (got $SIZE_DOCS)"; }
# Read the real paths out of that listing (import nests the source under /seeds,
# so hardcoding "/photos/private" would hide nothing).
PRIV="$(jpick "$TREE" "/photos/private")"
DOC="$(jpick "$TREE" "/docs/a.md")"
RULES="hide $PRIV"

note "[3] draft -> dry-run -> publish -> verify (never guess a path)"
BAD="$(mcp call preview_rules '{"rules":"bog /x"}')"
ok "bad verb refused with line no." "$BAD" "line 1: unknown layout verb 'bog'"
VAL="$(mcp call validate_definition "{\"name\":\"release\",\"base\":\"team\",\"rules\":\"$RULES\"}")"
ok "dry run accepted, writes nothing" "$VAL" '"accepted": true'
DEF="$(mcp call define_derived_view "{\"name\":\"release\",\"base\":\"team\",\"rules\":\"$RULES\"}")"
ok "derived view published"         "$DEF" "defined derived view 'release' over base 'team': 1 hide"
NEW="$(mcp call list_view_tree '{"view":"release"}')"
ok "lens renders the base tree"     "$NEW" "$DOC"
no "pruned subtree is gone"         "$NEW" "$PRIV"
GV="$(mcp call get_view '{"name":"release"}')"
ok "verbatim rules read back"       "$GV" "hide $PRIV"

note "[4] governance guards answer with the CLI's own words"
SAME="$(mcp call validate_definition "{\"name\":\"team\",\"base\":\"team\",\"rules\":\"hide $DOC\"}")"
ok "name == base refused"           "$SAME" "derived view name must differ from its base"
GHOST="$(mcp call list_view_tree '{"view":"ghost"}')"
ok "unknown view refused honestly"  "$GHOST" "no local metadata tree for view 'ghost'"
GONE="$(mcp call remove_view '{"name":"team"}')"
ok "a physical base is not a definition slot" "$GONE" "no view definition 'team'"

note "[5] alias (public writable name) and gapless rotation"
AL="$(mcp call alias_view '{"name":"team-public","real":"team"}')"
ok "alias created, real name governed" "$AL" "alias 'team-public' -> real view 'team'"
DUP="$(mcp call alias_view '{"name":"team-2","real":"team"}')"
ok "one public name per real view"  "$DUP" "already governed by alias 'team-public'"
ROT="$(mcp call alias_view '{"name":"team-public2","real":"team","retire":"team-public"}')"
ok "rotation retires the old name"  "$ROT" "team-public"
code "rotation refuses a non-alias retire" 3 \
    python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" call alias_view \
    '{"name":"team-public3","real":"team","retire":"release"}'

note "[6] fork (independent tree, shared bytes) — local SQLite base only"
FK="$(mcp call fork_view '{"name":"lab","base":"team"}')"
ok "fork seeded from the base"      "$FK" "forked view 'lab' from base 'team'"
NOF="$(mcp call fork_view '{"name":"lab2","base":"release"}')"
ok "a governed base cannot be forked" "$NOF" "release"
code "get_view on a fork reports its own tree" 0 \
    python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" call get_view '{"name":"lab"}'
ok "fork owns meta/lab.db"          "$( $BIN --home "$STORE" view list )" "lab"

note "[7] the same governance as resources / prompts / completion"
RES="$(mcp resources)"
ok "view tree exposed as resource"  "$RES" "grovefs://view/team/tree"
RD="$(mcp read grovefs://view/release/tree)"
ok "resource bytes match the tool"  "$RD" '"contents"'
no "pruned path absent in resource" "$RD" "$PRIV"
code "unknown resource URI is a protocol error" 2 \
    python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" read grovefs://view/ghost/tree
PR="$(mcp prompt reshape-view '{"base":"team"}')"
ok "reshape playbook instantiated"  "$PR" "validate_definition"
CP="$(mcp complete base 'r')"
ok "completion offers real view names" "$CP" "release"

note "[8] logging: setLevel warning -> a refused call pushes notifications/message (a successful one does not)"
python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" setlevel warning call list_view_tree '{"view":"ghost"}' > /dev/null 2>"$TMP/notes"
ok "a warning push was received"    "$(cat "$TMP/notes")" "[push] notifications/message"
ok "pushed severity is warning"     "$(cat "$TMP/notes")" '"level": "warning"'
python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" setlevel warning call list_views '{}' > /dev/null 2>"$TMP/notes2"
no "a successful call stays quiet"  "$(cat "$TMP/notes2")" "[push] notifications/message"

note "[9] cleanup (removing a fork deletes its own tree, bytes stay held)"
RMF="$(mcp call remove_view '{"name":"lab"}')"
ok "fork removed with its tree"     "$RMF" "removed fork view 'lab'"
RMD="$(mcp call remove_view '{"name":"release"}')"
ok "definition removed, data intact" "$RMD" "removed view definition 'release'"
RMA="$(mcp call remove_view '{"name":"team-public2"}')"
ok "alias removed"                  "$RMA" "removed view definition 'team-public2'"
FINAL="$(mcp call list_views)"
no "no definitions left"            "$FINAL" '"kind": "derived"'
ok "base view survives"             "$FINAL" '"team"'

note "[10] the API-key gate (--require-api-key)"
KEY="$($BIN apikey --db-path "$STORE" create --label smoke --for mcp | grep -oE '[0-9a-zA-Z_-]+:[0-9a-fA-F]+' | head -1)"
if [ -z "$KEY" ]; then
    fail=$((fail+1)); note "  FAIL: could not mint an api key"
else
    gated=(--server-arg --require-api-key)
    code "tools/list without a key is refused (-32001)" 2 \
        python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" "${gated[@]}" list
    code "tools/list with a key is admitted" 0 \
        python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" "${gated[@]}" --api-key "$KEY" list
    code "wrong key is refused" 2 \
        python3 "$CALL" --bin "$BIN" --quiet --db-path "$STORE" "${gated[@]}" --api-key "nope:deadbeef" list
    AUDIT="$($BIN apikey --db-path "$STORE" calls)"
    ok "refused calls land in the audit log" "$AUDIT" "tools/list"
fi

note ""
note "SUMMARY: pass=$pass fail=$fail"
[ "$fail" = 0 ] && note "ALL MCP CHECKS PASSED" || note "see FAIL lines above"
[ "$fail" = 0 ]

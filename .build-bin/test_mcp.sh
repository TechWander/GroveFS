#!/usr/bin/env bash
# MCP integration e2e (`grovefs mcp`): an AI agent's entrance onto view
# governance. Drives the stdio JSON-RPC server the way an MCP client does —
# initialize -> tools/list -> tools/call — and asserts the AGENT-SIDE effects
# that matter: a data listing's shape is customized with inline rule TEXT (no
# rules file), live mounts hot-reload it, and every answer is byte-identical
# to what the `grovefs view` CLI prints for the same operation (one contract,
# two entrances). Also checks the protocol plumbing (notifications never
# answered, malformed frames -> -32700, tool errors inside results) and the
# broader MCP method surface an agent's client uses: resources/* (a view as
# attachable context), prompts/* (governance playbooks), completion/complete
# (view-name fill), and logging/setLevel -> notifications/message streaming.
# Assertion-based like the other suites; emits a SUMMARY line for run_all.sh.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs
BASE=$PWD/.mcptest/home
fusermount -u .mcptest/mnt .mcptest/mnt_al 2>/dev/null   # drop last run's mounts
fusermount -uz .mcptest/mnt .mcptest/mnt_al 2>/dev/null  # lazy fallback, then rm
rm -rf .mcptest
mkdir -p .mcptest/home .mcptest/mnt .mcptest/mnt_al
export GROVEFS_HOME=$BASE

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
T(){ timeout 5 "$@"; }

# Seed a physical view with a real tree the lens will reshape.
cat > .mcptest/team.manifest <<'EOF'
mkdir docs
mkdir docs/2026
mkdir photos
mkdir photos/private
touch docs/a.md
touch docs/2026/b.md
touch photos/private/x.jpg
EOF
$Y init --view team --manifest .mcptest/team.manifest > .mcptest/init.log 2>&1 \
  && ok "seeded physical view 'team'" || { bad "init failed: $(cat .mcptest/init.log)"; exit 1; }

# ------------------------------------------------------------------ handshake
echo "===== 1) MCP handshake + tool discovery ====="
MCPOUT=.mcptest/stdio.out
mkfifo .mcptest/in
$Y mcp --db-path "$BASE" < .mcptest/in > "$MCPOUT" 2> .mcptest/mcp.err &
MCPPID=$!
exec 3>.mcptest/in            # held-open writer: the server stays alive
say(){ printf '%s\n' "$1" >&3; }
# wait-for-id: block until the server's answer frame for request id $1 lands
resp(){ local i=0; while [ $i -lt 50 ]; do
    local r; r=$(jq -c --argjson id "$1" 'select(.id==$id)' "$MCPOUT" 2>/dev/null | tail -1)
    [ -n "$r" ] && { echo "$r"; return 0; }; sleep 0.1; i=$((i+1))
  done; return 1; }
have(){ resp "$1" | jq -e "$2" >/dev/null 2>&1; }

say '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"suite","version":"0"}}}'
if have 1 '.result.protocolVersion=="2025-06-18" and .result.serverInfo.name=="grovefs" and (.result.instructions|length>0)'; then
  ok "initialize: version echoed, serverInfo + agent instructions present"
else bad "initialize answer wrong: $(resp 1 || echo NO-RESPONSE)"; fi
if have 1 '.result.capabilities|(has("tools") and has("resources") and has("prompts") and has("completions") and has("logging"))'; then
  ok "initialize advertises the full capability surface (tools/resources/prompts/completions/logging)"
else bad "capabilities: $(resp 1 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","method":"notifications/initialized"}'
say '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
if have 2 '[.result.tools[].name]|sort==["alias_view","define_derived_view","fork_view","get_view","list_view_tree","list_views","preview_rules","remove_view","validate_definition"]'; then
  ok "tools/list advertises exactly the nine governance + introspection tools"
else bad "tools/list: $(resp 2 || echo NO-RESPONSE)"; fi
# The notification must NOT have produced a response frame (id-less = no answer).
if jq -e 'select(has("id")|not)' "$MCPOUT" >/dev/null 2>&1; then
  bad "notification was answered (protocol violation)"
else ok "notifications are never answered"; fi

# ------------------------------------------------------------------ the agent path
echo "===== 2) agent customizes a data listing (inline rules, no files) ====="
say '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"preview_rules","arguments":{"rules":"bogber /x"}}}'
if have 3 '.result.isError==true and (.result.content[0].text|test("bad layout rules"))'; then
  ok "preview_rules rejects a bad verb before anything lands"
else bad "preview bad rules: $(resp 3 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"define_derived_view","arguments":{"name":"release","base":"team","rules":"# public release: docs + public photos only\nonly /docs\nonly /photos/public"}}}'
if have 4 '.result.isError==null and (.result.content[0].text|test("defined derived view .release. over base .team.: 0 hide, 2 only, 0 reloc"))'; then
  ok "define_derived_view published a read-only lens from inline rule text"
else bad "define: $(resp 4 || echo NO-RESPONSE)"; fi
[ -f "$BASE/meta/views/release.layout" ] && ok "definition landed at meta/views/release.layout" || bad "no definition file"

# A live LOCAL mount of that name serves the agent-chosen shape, read-only.
$Y --home "$BASE" mount --view release -m .mcptest/mnt --foreground > .mcptest/mnt.log 2>&1 &
BPID=$!
for i in $(seq 40); do [ -f .mcptest/mnt/docs/a.md ] && break; sleep 0.5; done
# `only` keeps the governed subtree and synthesizes its display parents, so
# the listing shows /docs in full and /photos down to /photos/public — while
# the private sibling is gone from the agent's cut entirely.
if [ -d .mcptest/mnt/docs/2026 ] \
   && [ ! -e .mcptest/mnt/photos/private ] && [ ! -e .mcptest/mnt/docs/b.md ]; then
  ok "mount of the agent-defined view shows only the governed subtree (private pruned, /docs intact)"
else bad "lens shape wrong: $(ls .mcptest/mnt 2>&1)"; fi
if ! T sh -c 'echo x > .mcptest/mnt/docs/new.md' 2>/dev/null; then
  ok "mount came up read-only (writes belong to base view 'team')"
else bad "derived mount was writable"; fi

# The agent BROADENS its own lens (hide only the private subtree). A local
# mount resolves the definition once at mount time (hot-reload-into-live-mount
# is the meta-server entrance, covered by the layout suite); here the agent's
# new shape must simply govern the NEXT mount.
say '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"define_derived_view","arguments":{"name":"release","base":"team","rules":"hide /photos/private"}}}'
if have 5 '.result.content[0].text|test("already defined — replacing|defined derived view .release.")'; then
  ok "agent redefined the lens (replacement accepted)"
else bad "redefine: $(resp 5 || echo NO-RESPONSE)"; fi
fusermount -u .mcptest/mnt 2>/dev/null; kill $BPID 2>/dev/null
for i in $(seq 20); do ! mountpoint -q .mcptest/mnt 2>/dev/null && break; sleep 0.3; done
$Y --home "$BASE" mount --view release -m .mcptest/mnt --foreground > .mcptest/mnt2.log 2>&1 &
BPID=$!
for i in $(seq 40); do [ -f .mcptest/mnt/docs/a.md ] && break; sleep 0.5; done
if [ -d .mcptest/mnt/docs ] && [ -d .mcptest/mnt/photos ] && [ ! -e .mcptest/mnt/photos/private ]; then
  ok "agent's new shape served on the next mount (docs kept, /photos/private pruned)"
else bad "redefined shape wrong: $(ls -R .mcptest/mnt 2>&1 | head -5)"; fi

# Discovery + preview: the agent can SEE the real listing (paths to reuse in
# rules) and the shape a lens yields, all read-only from metadata — no mount.
echo "===== 2b) agent discovers paths + previews shapes (introspection) ====="
say '{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"view":"team","depth":20}}}'
TT=$(resp 20 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '.kind=="physical" and ([.entries[].path]|index("/photos/private/x.jpg")) and ([.entries[].path]|index("/docs/a.md"))' 2>/dev/null)
[ "$TT" = "true" ] && ok "list_view_tree shows the whole physical tree (exact paths to reuse in rules)" || bad "physical tree: $(resp 20 || echo NO-RESP)"
# THROUGH a live derived lens: /photos/private is pruned exactly as the mount shows it.
say '{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"view":"release","depth":20}}}'
TT=$(resp 21 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '.kind=="derived" and .rendered_from=="team" and ([.entries[].path]|index("/docs")) and ([.entries[].path]|index("/photos")) and (([.entries[].path]|index("/photos/private"))==null)' 2>/dev/null)
[ "$TT" = "true" ] && ok "list_view_tree renders a derived view THROUGH its lens (private pruned, same shape as the mount)" || bad "derived tree: $(resp 21 || echo NO-RESP)"
# SIMULATE an uncommitted lens: preview the effect before publishing, writes nothing.
say '{"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"base":"team","rules":"only /docs","depth":20}}}'
TT=$(resp 22 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '.kind=="simulated" and ([.entries[].path]|index("/docs/2026/b.md")) and (([.entries[].path]|index("/photos"))==null)' 2>/dev/null)
[ "$TT" = "true" ] && ok "list_view_tree previews a base+rules lens WITHOUT committing it" || bad "simulate: $(resp 22 || echo NO-RESP)"
[ ! -f "$BASE/meta/views/onlydocs.layout" ] && ok "shape preview wrote no definition" || bad "preview leaked a definition file"
# get_view echoes the exact rules for read-modify-write editing.
say '{"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"get_view","arguments":{"name":"release"}}}'
TT=$(resp 23 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '.kind=="derived" and .base=="team" and (.definition|test("hide /photos/private"))' 2>/dev/null)
[ "$TT" = "true" ] && ok "get_view returns the lens's verbatim rules + counts" || bad "get_view: $(resp 23 || echo NO-RESP)"

# validate_definition: dry-run a WHOLE publish (name+base+rules) against every
# governance guard WITHOUT writing — the last pre-publish safety net beyond
# preview_rules' verb-syntax check (it catches name==base, shadowing a physical
# db, replacing a pure alias — the things that would make a define_derived_view fail).
say '{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"validate_definition","arguments":{"name":"team","base":"team","rules":"only /docs"}}}'
if have 30 '.result.isError==true and (.result.content[0].text|test("must differ from its base"))'; then
  ok "validate_definition refuses name==base with the exact CLI wording (a real publish would too)"
else bad "validate reject: $(resp 30 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"validate_definition","arguments":{"name":"brandnew","base":"team","rules":"hide /photos/private"}}}'
VD=$(resp 31 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '.accepted==true and .dry_run==true and .hide==1 and .would_replace_existing_definition==false' 2>/dev/null)
[ "$VD" = "true" ] && ok "validate_definition accepts a good trio as a JSON verdict" || bad "validate accept: $(resp 31 || echo NO-RESP)"
[ ! -f "$BASE/meta/views/brandnew.layout" ] && ok "...and the dry run writes no definition" || bad "validate leaked a definition file"

# The same governance, re-exposed through the OTHER common MCP method families
# — resources (attach a view as context), prompts (a reusable playbook), and
# completion (auto-fill a view-name argument) — so an agent's client can act
# without a tools/call. Same renderers as the tools, so results must agree.
echo "===== 2c) resources + prompts + completion surfaces ====="
say '{"jsonrpc":"2.0","id":40,"method":"resources/list"}'
RL=$(resp 40 | jq -e '[.result.resources[].uri]|(index("grovefs://view/release/definition")) and (index("grovefs://view/team/tree")) and (index("grovefs://view/release/tree"))' 2>/dev/null)
[ "$RL" = "true" ] && ok "resources/list enumerates the governed views as resources" || bad "resources/list: $(resp 40 || echo NO-RESP)"
say '{"jsonrpc":"2.0","id":41,"method":"resources/templates/list"}'
RT=$(resp 41 | jq -e '(.result.resourceTemplates|length)==2' 2>/dev/null)
[ "$RT" = "true" ] && ok "resources/templates/list advertises the two uri templates" || bad "templates: $(resp 41 || echo NO-RESP)"
say '{"jsonrpc":"2.0","id":42,"method":"resources/read","params":{"uri":"grovefs://view/release/definition"}}'
RD=$(resp 42 | jq -r '.result.contents[0].text' 2>/dev/null | jq -e '.kind=="derived" and .base=="team" and (.definition|test("hide /photos/private"))' 2>/dev/null)
[ "$RD" = "true" ] && ok "resources/read returns the view definition (agrees with get_view)" || bad "resources/read definition: $(resp 42 || echo NO-RESP)"
say '{"jsonrpc":"2.0","id":43,"method":"resources/read","params":{"uri":"grovefs://view/release/tree"}}'
RT2=$(resp 43 | jq -r '.result.contents[0].text' 2>/dev/null | jq -e '.kind=="derived" and ([.entries[].path]|index("/docs")) and (([.entries[].path]|index("/photos/private"))==null)' 2>/dev/null)
[ "$RT2" = "true" ] && ok "resources/read renders the lens tree (same shape the mount shows)" || bad "resources/read tree: $(resp 43 || echo NO-RESP)"
say '{"jsonrpc":"2.0","id":44,"method":"resources/read","params":{"uri":"grovefs://view/team/bogus"}}'
if have 44 '.error.code==-32002'; then ok "unknown resource facet -> protocol error -32002 (not a crash)"; else bad "bad facet: $(resp 44 || echo NO-RESP)"; fi
say '{"jsonrpc":"2.0","id":45,"method":"prompts/list"}'
if have 45 '[.result.prompts[].name]|sort==["reshape-view","rotate-alias"]'; then ok "prompts/list advertises the two governance playbooks"; else bad "prompts/list: $(resp 45 || echo NO-RESP)"; fi
say '{"jsonrpc":"2.0","id":46,"method":"prompts/get","params":{"name":"reshape-view","arguments":{"base":"team","goal":"hide private shots"}}}'
PGT=$(resp 46 | jq -r '.result.messages[0].content.text' 2>/dev/null)
echo "$PGT" | grep -q "validate_definition" && echo "$PGT" | grep -q "hide private shots" \
  && ok "prompts/get instantiates a playbook with the given arguments" || bad "prompt: $(resp 46 || echo NO-RESP)"
say '{"jsonrpc":"2.0","id":47,"method":"completion/complete","params":{"ref":{"type":"ref/resource","value":"grovefs://view/{name}/tree"},"argument":{"name":"name","value":"re"}}}'
CC=$(resp 47 | jq -e '[.result.completion.values[]]|any(.=="release")' 2>/dev/null)
[ "$CC" = "true" ] && ok "completion/complete fills a view-name argument from the store" || bad "completion: $(resp 47 || echo NO-RESP)"

# Name governance through the agent: alias, gapless rotation, real-name refusal.
echo "===== 3) agent rotates public names (alias governance) ====="
say '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"alias_view","arguments":{"name":"teamA","real":"team"}}}'
if have 6 '.result.content[0].text|test("defined pure alias .teamA. -> real view .team.")'; then
  ok "alias_view defined public name teamA"
else bad "alias: $(resp 6 || echo NO-RESPONSE)"; fi
if ! $Y --home "$BASE" mount --view team -m .mcptest/nonexistent --foreground 2>.mcptest/refused.log; then
  grep -q "governed by an alias" .mcptest/refused.log \
    && ok "real name 'team' refused as mount entry while governed (same contract as CLI)" \
    || bad "refused for the wrong reason: $(cat .mcptest/refused.log)"
else bad "governed real name still mountable"; fusermount -u .mcptest/nonexistent 2>/dev/null; fi
$Y --home "$BASE" mount --view teamA -m .mcptest/mnt_al --foreground > .mcptest/mnt_al.log 2>&1 &
APID=$!
for i in $(seq 40); do [ -f .mcptest/mnt_al/docs/a.md ] && break; sleep 0.5; done
if [ -d .mcptest/mnt_al/photos ] && T sh -c 'echo w > .mcptest/mnt_al/docs/w.txt' 2>/dev/null; then
  ok "alias mount shows the FULL tree and is writable"
else bad "alias mount wrong: $(cat .mcptest/mnt_al.log)"; fi
say '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"alias_view","arguments":{"name":"teamB","real":"team","retire":"teamA"}}}'
if have 7 '.result.content[0].text|test("rotated public name for real view .team.: .teamA. -> .teamB.")'; then
  ok "gapless rotation through the agent (write-before-delete, old name gone)"
else bad "rotation: $(resp 7 || echo NO-RESPONSE)"; fi
[ ! -f "$BASE/meta/views/teamA.layout" ] && [ -f "$BASE/meta/views/teamB.layout" ] \
  && ok "retired slot removed, new slot in place" || bad "rotation file state wrong"

# list_views answers as structured JSON an agent can act on (the tool result
# TEXT itself parses as JSON — unwrap it, then assert on the payload).
say '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"list_views","arguments":{}}}'
LV=$(resp 8 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '(.physical|index("team")) and (.defined|map(.name)|sort==["release","teamB"]) and (.defined|map(select(.kind=="derived"))[0].only==0)' 2>/dev/null)
if [ "$LV" = "true" ]; then
  ok "list_views reports physical trees + governed definitions (post-reload kinds)"
else bad "list_views: $(resp 8 || echo NO-RESPONSE)"; fi

# A FORK: an independent metadata tree seeded from the physical base — the
# agent asks for organization rights without upload rights, and can drop it
# again without ever touching 'team'. Same ops the CLI drives.
say '{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"fork_view","arguments":{"name":"lab","base":"team"}}}'
if have 60 '.result.isError==null and (.result.content[0].text|test("forked view .lab. from base .team."))'; then
  ok "fork_view seeded an independent tree (shared bytes, no copies)"
else bad "fork_view: $(resp 60 || echo NO-RESPONSE)"; fi
[ -f "$BASE/meta/lab.db" ] && ok "fork owns meta/lab.db beside the base tree" || bad "fork tree missing"
say '{"jsonrpc":"2.0","id":61,"method":"tools/call","params":{"name":"list_views","arguments":{}}}'
if resp 61 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '(.physical|index("lab")) and (.defined|map(select(.name=="lab" and .kind=="fork" and .real_view=="team"))|length==1)' >/dev/null 2>&1; then
  ok "list_views reports the fork as kind=fork of its base"
else bad "list_views fork entry: $(resp 61 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","id":62,"method":"tools/call","params":{"name":"get_view","arguments":{"name":"lab"}}}'
if resp 62 | jq -r '.result.content[0].text' 2>/dev/null | jq -e '.kind=="fork" and .own_tree=="meta/lab.db" and (.mount|test("content read-only"))' >/dev/null 2>&1; then
  ok "get_view spells out the fork's mount contract + own tree"
else bad "get_view fork: $(resp 62 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","id":63,"method":"tools/call","params":{"name":"alias_view","arguments":{"name":"labpub","real":"lab"}}}'
if have 63 '.result.isError==true and (.result.content[0].text|test("is a fork"))'; then
  ok "alias_view refuses a fork target (tree identity, not a public name)"
else bad "alias-to-fork: $(resp 63 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","id":64,"method":"tools/call","params":{"name":"remove_view","arguments":{"name":"lab"}}}'
if have 64 '.result.content[0].text|test("removed fork view .lab. of base .team.")'; then
  [ ! -f "$BASE/meta/lab.db" ] && [ -f "$BASE/meta/team.db" ] \
    && ok "remove_view deleted the fork tree only (base tree intact)" || bad "fork rm left the wrong files"
else bad "fork rm: $(resp 64 || echo NO-RESPONSE)"; fi

# One contract, two entrances: the CLI sees the agent's state verbatim.
$Y view --db-path "$BASE" list > .mcptest/cli_list.log 2>&1
grep -q "view 'release' -> base 'team': 1 hide, 0 only, 0 reloc rule(s) (read-only)" .mcptest/cli_list.log \
  && ok "CLI 'grovefs view list' shows the agent-defined lens verbatim" || bad "CLI view list: $(cat .mcptest/cli_list.log)"

# ------------------------------------------------------------------ error paths
echo "===== 4) protocol + tool error paths ====="
say '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"define_derived_view","arguments":{"name":"x"}}}'
if have 9 '.result.isError==true and (.result.content[0].text|test("missing string argument"))'; then
  ok "missing argument -> isError inside result (agent-visible, no crash)"
else bad "missing-arg path: $(resp 9 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}'
if have 10 '.result.isError==true and (.result.content[0].text|test("unknown tool"))'; then
  ok "unknown tool -> isError with a readable message"
else bad "unknown-tool path: $(resp 10 || echo NO-RESPONSE)"; fi
say '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"define_derived_view","arguments":{"name":"team","base":"team","rules":"only /docs"}}}'
if have 11 '.result.isError==true and (.result.content[0].text|test("must differ from its base"))'; then
  ok "governance refusals match the CLI strings exactly"
else bad "name==base path: $(resp 11 || echo NO-RESPONSE)"; fi
say 'not json at all'
say '{"jsonrpc":"2.0","id":12,"method":"ping"}'
if have 12 '.result!=null'; then
  # the garbage frame must have produced exactly one extra answer (-32700)
  if jq -s 'map(select(has("error") and .error.code== -32700))|length==1' "$MCPOUT" | grep -q true; then
    ok "malformed frame answered with parse error -32700, stream recovered"
  else bad "no -32700 for malformed frame"; fi
else bad "server did not recover after malformed frame"; fi

# Server-initiated logging: once a client sets a threshold, the server pushes
# notifications/message frames (no id) alongside responses — an info on an
# accepted call, a warning on a refusal.
echo "===== 5) logging/setLevel -> notifications/message stream ====="
say '{"jsonrpc":"2.0","id":50,"method":"logging/setLevel","params":{"level":"info"}}'
if have 50 '.result!=null'; then ok "logging/setLevel acknowledged (threshold stored per session)"; else bad "setLevel: $(resp 50 || echo NO-RESP)"; fi
say '{"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"view":"ghost"}}}'
if have 51 '.result.isError==true'; then
  for i in $(seq 30); do jq -s 'map(select(.method=="notifications/message" and .params.level=="warning"))|length>=1' "$MCPOUT" | grep -q true && break; sleep 0.1; done
  if jq -s 'map(select(.method=="notifications/message" and .params.logger=="grove-mcp"))|length>=1' "$MCPOUT" | grep -q true; then
    ok "a refused call streams a notifications/message frame (warning) on the server side"
  else bad "no notification frame emitted after the refused call"; fi
else bad "ghost call not answered: $(resp 51 || echo NO-RESP)"; fi

# Cleanup: agent removes its definitions; underlying data stays.
say '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"remove_view","arguments":{"name":"release"}}}'
say '{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"remove_view","arguments":{"name":"teamB"}}}'
if have 14 '.result.content[0].text|test("removed view definition")'; then
  [ -f "$BASE/meta/team.db" ] && ok "definitions removed, physical view data untouched" || bad "data vanished"
else bad "remove path: $(resp 14 || echo NO-RESPONSE)"; fi

exec 3>&-          # close stdin -> server exits at EOF
# Unmount FIRST (fusermount waits for FUSE_DEV_IOC_BACKING_CLOSE), then reap
# the mount daemons — killing before unmounting leaves a zombie mount that
# blocks the NEXT run's fusermount with EACCES.
fusermount -u .mcptest/mnt .mcptest/mnt_al 2>/dev/null
fusermount -uz .mcptest/mnt .mcptest/mnt_al 2>/dev/null
kill $BPID $APID $MCPPID 2>/dev/null
wait 2>/dev/null
rm -f .mcptest/in
echo "SUMMARY: pass=$pass fail=$fail"
[ "$fail" -eq 0 ]

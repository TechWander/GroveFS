#!/bin/bash
# 集群功能实测 第三部分: apikey(scope/视图绑定/只读原地切换/撤销封停已建挂载/审计)
# 与 MCP 全方法面.
# 门禁测试用一套独立的隔离栈: meta-server(:50173) + data-server(:50111) 都开
# --require-api-key 并共用 /tmp/gft_gate/meta/api_keys.db 这一份注册表, 这样
# M_OPEN 与 OP_OPEN 两侧都被钥匙管住(生产 :50053 用的是静态 token, 不审 apikey).
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
GV="grovefs view --meta-dir $HOME/meta"
ENV=/etc/grovefs/cluster.env
G=/tmp/gft_gate
AK="grovefs apikey --meta-dir $G"
KEYENV=/tmp/gft_key.env
mk_keyenv(){ printf 'GROVEFS_API_KEY=%s\n' "$1" | sudo tee $KEYENV >/dev/null; sudo chmod 600 $KEYENV; }
gmnt(){ # gmnt <view> <mntpoint> <unit> [额外 flag]  —— 走门禁栈
  sudo mkdir -p "$2" && sudo chown hanjun:hanjun "$2"
  sudo systemd-run --unit="gft-$3" --collect --quiet \
    -p EnvironmentFile=$ENV -p EnvironmentFile=$KEYENV -p User=hanjun \
    /usr/local/bin/grovefs mount --view "$1" -m "$2" \
    --meta-server 127.0.0.1:50173 --data-servers 127.0.0.1:50111 ${4:-} --foreground
  for i in $(seq 1 40); do mountpoint -q "$2" && break; sleep 0.25; done
  echo "    [$1 -> $2: $(mountpoint -q "$2" && echo 已挂载 || echo 未挂载)]"
}
glog(){ sudo journalctl -u "gft-$1" -b --no-pager 2>/dev/null | grep -v "^--" | tail -${2:-3} | tr '\n' '|'; }
tkill(){ sudo systemctl stop "gft-$1" 2>/dev/null; sudo rm -rf "/tmp/gft_$1"; }
kid(){ echo "$1" | sed -n 's/^\([0-9a-f]\{32\}\):.*/\1/p' | head -1; }

echo "===== D0 隔离栈: 门禁 meta-server + 门禁 data-server ====="
sudo rm -rf $G /tmp/gft_keystore; mkdir -p $G
grovefs view --meta-dir $G create --view keyed 2>&1 | head -1 | sed 's/^/  建视图 keyed: /'
grovefs view --meta-dir $G create --view other 2>&1 | head -1 | sed 's/^/  建视图 other: /'
sudo systemd-run --unit=gft-gatem --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs meta-server --meta-dir $G --listen 127.0.0.1:50173 --require-api-key
sudo systemd-run --unit=gft-keyds --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs data-server --data-dir /tmp/gft_keystore --listen 127.0.0.1:50111 \
  --meta-dir $G --require-api-key
for i in $(seq 1 30); do ss -ltn | grep -q :50173 && ss -ltn | grep -q :50111 && break; sleep 0.2; done
echo "  监听 50173/50111: $(ss -ltn | grep -cE ':5017[3]|:50111') 个"
echo "  argv 里没有密钥: $(tr '\0' ' ' < /proc/$(pgrep -f 'listen 127.0.0.1:50111' | head -1)/cmdline)"
echo "  注册表位置: $(ls -l $G/meta/api_keys.db 2>&1)"

echo "===== D1 apikey create: 明文一次性 + scope ====="
K_ANY=$($AK create --label d-any 2>&1 | grep -oE '[0-9a-f]{32}:[0-9a-f]{64}' | head -1)
$AK create --label d-sample 2>&1 | sed 's/^/  /'
K_MNT=$($AK create --label d-mount --for mount 2>&1 | grep -oE '[0-9a-f]{32}:[0-9a-f]{64}' | head -1)
K_MCP=$($AK create --label d-mcp --for mcp 2>&1 | grep -oE '[0-9a-f]{32}:[0-9a-f]{64}' | head -1)
K_RO=$($AK create --label d-ro --view keyed --ro 2>&1 | grep -oE '[0-9a-f]{32}:[0-9a-f]{64}' | head -1)
K_RW=$($AK create --label d-rw --view keyed 2>&1 | grep -oE '[0-9a-f]{32}:[0-9a-f]{64}' | head -1)
echo "  明文只在 create 那次出现: list 中含 secret 的行数 -> $($AK list 2>&1 | grep -cE ':[0-9a-f]{64}') 处"
$AK list 2>&1 | head -7 | sed 's/^/  /'
$AK list --view keyed 2>&1 | sed 's/^/  /'
echo "  --for 非法值的拒绝原文: $($AK create --label d-bad --for both 2>&1 | tail -2 | tr '\n' '|')"

echo "===== D2 挂载侧: 无密钥 / scope 错配 / 视图绑定 ====="
echo "  (a) 不带密钥挂载 keyed:"
sudo rm -f $KEYENV
sudo systemd-run --unit=gft-nokey --collect --quiet -p EnvironmentFile=$ENV -p User=hanjun \
  /usr/local/bin/grovefs mount --view keyed -m /tmp/gft_nokey \
  --meta-server 127.0.0.1:50173 --data-servers 127.0.0.1:50111 --foreground
for i in $(seq 1 20); do mountpoint -q /tmp/gft_nokey && break; sleep 0.25; done
echo "    挂载状态: $(mountpoint -q /tmp/gft_nokey && echo '已挂载(FAIL)' || echo '未挂载(OK)')"
echo "    拒绝原文: $(glog nokey)"
sudo systemctl stop gft-nokey 2>/dev/null; sudo rm -rf /tmp/gft_nokey
echo "  (b) mcp-scope 密钥用于挂载:"
mk_keyenv "$K_MCP"
gmnt keyed /tmp/gft_scope scope
echo "    挂载状态: $(mountpoint -q /tmp/gft_scope && echo '已挂载(FAIL)' || echo '未挂载(OK)')"
echo "    拒绝原文: $(glog scope)"
tkill scope
echo "  (c) mount-scope 密钥用于 MCP(应由 MCP 侧拒):"
mk_keyenv "$K_MNT"
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"},"apiKey":"%s"}}\n{"jsonrpc":"2.0","method":"notifications/initialized"}\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n' "$K_MNT" > /tmp/gft_scopein
timeout 15 grovefs mcp --meta-dir $G --require-api-key < /tmp/gft_scopein 2>/tmp/gft_scopeerr >/tmp/gft_scopeout
echo "    应答: $(grep -oE '"code":-?[0-9]+,"message":"[^"]*"' /tmp/gft_scopeout | head -2 | tr '\n' '|')"
echo "    stderr: $(head -2 /tmp/gft_scopeerr | tr '\n' '|')"
echo "  (d) 绑定 keyed 的密钥去挂 other:"
mk_keyenv "$K_RO"
gmnt other /tmp/gft_wrong wrong
echo "    挂载状态: $(mountpoint -q /tmp/gft_wrong && echo '已挂载(视图绑定未生效)' || echo '未挂载(绑定生效)')"
echo "    拒绝原文: $(glog wrong)"
tkill wrong

echo "===== D3 只读密钥与 set-ro 原地切换(不重连) ====="
mk_keyenv "$K_RW"
gmnt keyed /tmp/gft_rw rw
python3 - <<'PY'
import os
P = "/tmp/gft_rw/f.txt"
fd = os.open(P, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"first\n"); os.fsync(fd); os.close(fd)
print("  rw 密钥写入: OK 现网回读:", repr(open(P).read()))
PY
ID=$(kid "$K_RW")
$AK set-ro --id "$ID" 2>&1 | sed 's/^/  /'
sleep 1.5
python3 - <<'PY'
import os
P = "/tmp/gft_rw/f.txt"
for tag, fl in (("O_WRONLY|O_APPEND", os.O_WRONLY | os.O_APPEND), ("O_WRONLY|O_TRUNC", os.O_WRONLY | os.O_TRUNC)):
    try:
        fd = os.open(P, fl); os.write(fd, b"second\n"); os.fsync(fd); os.close(fd)
        print(f"  set-ro 后 {tag}: 写入成功(未拦截)")
    except OSError as e:
        print(f"  set-ro 后 {tag}: errno={e.errno} ({e.strerror})")
print("  set-ro 后读仍通:", repr(open(P).read()))
PY
echo "    (shell 追加的假成功) rc=$(bash -c "printf 'z' >> /tmp/gft_rw/f.txt" 2>/tmp/roerr; echo $?) 报错=[$(cat /tmp/roerr)] 文件行数=$(wc -l < /tmp/gft_rw/f.txt)"
echo "  挂载未被拆除: $(mountpoint -q /tmp/gft_rw && echo 是 || echo 否)"
$AK set-ro --id "$ID" --rw 2>&1 | sed 's/^/  /'
sleep 1.5
python3 - <<'PY'
import os
try:
    fd = os.open("/tmp/gft_rw/f.txt", os.O_WRONLY | os.O_APPEND); os.write(fd, b"third\n"); os.fsync(fd); os.close(fd)
    print("  恢复 rw 后追加: 成功 内容=", repr(open("/tmp/gft_rw/f.txt").read()))
except OSError as e:
    print(f"  恢复 rw 后追加: errno={e.errno} ({e.strerror})")
PY

echo "===== D4 revoke 封停已建挂载 ====="
gmnt keyed /tmp/gft_rv rv
printf 'live\n' > /tmp/gft_rv/g.txt && echo "  挂载内写入 OK, 现有内容: $(ls /tmp/gft_rv | tr '\n' ' ')"
$AK revoke --id "$ID" 2>&1 | sed 's/^/  /'
for i in $(seq 1 25); do mountpoint -q /tmp/gft_rv || break; sleep 0.4; done
echo "  等待 $((i))*0.4s 后挂载点状态: $(mountpoint -q /tmp/gft_rv && echo '仍在(未封停)' || echo '已自行卸载(封停生效)')"
echo "  挂载侧打印的原因: $(glog rv 5)"
echo "  单元结局: $(systemctl is-active gft-rv 2>&1)"
echo "  撤销后再挂载必须失败:"
gmnt keyed /tmp/gft_rv2 rv2
echo "    拒绝原文: $(glog rv2)"
tkill rv2
$AK list 2>&1 | grep -E "d-rw|^#" | sed 's/^/  撤销条目仍在: /'
$AK set-ro --id "$ID" 2>&1 | sed 's/^/  对已撤销 id 再 set-ro: /'
$AK revoke --id "$ID" 2>&1 | sed 's/^/  重复 revoke: /'
tkill rw; tkill rv

echo "===== D5 MCP 全方法面(stdio 文件式) ====="
R=/tmp/gft_mcp; sudo rm -rf $R; rm -f $R.in*; mkdir -p $R
mk_keyenv "$K_ANY"
cat > $R.in <<'JSON'
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_views","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_view","arguments":{"name":"keyed"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"view":"keyed","path":"/","depth":2}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_view_tree","arguments":{"base":"keyed","rules":"hide /f.txt","path":"/","depth":2}}}
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"preview_rules","arguments":{"rules":"bogus /x"}}}
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"validate_definition","arguments":{"name":"mcplens","base":"keyed","rules":"hide /f.txt"}}}
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"define_derived_view","arguments":{"name":"mcplens","base":"keyed","rules":"hide /f.txt"}}}
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"alias_view","arguments":{"name":"mcpub","real":"keyed"}}}
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"fork_view","arguments":{"name":"mcpfork","base":"keyed","seed":false}}}
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"graft_view","arguments":{"name":"mcpfork","src":"keyed","map":"/f.txt"}}}
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"remove_view","arguments":{"name":"mcpfork"}}}
{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"remove_view","arguments":{"name":"mcplens"}}}
{"jsonrpc":"2.0","id":15,"method":"resources/list"}
{"jsonrpc":"2.0","id":16,"method":"resources/templates/list"}
{"jsonrpc":"2.0","id":17,"method":"resources/read","params":{"uri":"grovefs://views"}}
{"jsonrpc":"2.0","id":18,"method":"resources/read","params":{"uri":"grovefs://guide"}}
{"jsonrpc":"2.0","id":19,"method":"resources/read","params":{"uri":"grovefs://view/keyed/definition"}}
{"jsonrpc":"2.0","id":20,"method":"resources/read","params":{"uri":"grovefs://view/keyed/tree?path=/&depth=2&max=50"}}
{"jsonrpc":"2.0","id":21,"method":"prompts/list"}
{"jsonrpc":"2.0","id":22,"method":"prompts/get","params":{"name":"reshape-view","arguments":{"view":"keyed"}}}
{"jsonrpc":"2.0","id":23,"method":"completion/complete","params":{"ref":{"type":"ref/prompt","name":"reshape-view"},"argument":{"name":"view","value":"ke"}}}
{"jsonrpc":"2.0","id":24,"method":"logging/setLevel","params":{"level":"debug"}}
{"jsonrpc":"2.0","id":25,"method":"ping"}
{"jsonrpc":"2.0","id":26,"method":"no/such/method"}
{"jsonrpc":"2.0","id":27,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}
{"jsonrpc":"2.0","id":28,"method":"tools/call","params":{"name":"get_view","arguments":{}}}
{"jsonrpc":"2.0","id":29,"method":"resources/read","params":{"uri":"grovefs://view/nosuch/definition"}}
{"jsonrpc":"1.0","id":30,"method":"ping"}
{"jsonrpc":"2.0","id":31,"method":"
JSON
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"},"apiKey":"%s"}}\n' "$K_ANY" > $R.auth
cat $R.auth $R.in > $R.in2
echo "  未鉴权(不带 apiKey, 服务器 --require-api-key)的拒绝原文:"
timeout 20 grovefs mcp --meta-dir $G --require-api-key < $R.in 2>$R.err >$R.out
echo "    stderr: $(head -2 $R.err | tr '\n' '|')"
echo "    被门禁的方法应答: $(grep -oE '"id":[0-9]+,"jsonrpc":"2.0","error":\{[^}]*\}' $R.out | head -3 | tr '\n' '|')"
echo "    未被门禁的方法( initialize/ping 之外): $(jq -c 'select(.id==25)' $R.out 2>/dev/null | head -c 120)"
echo "  鉴权后全量应答:"
timeout 30 grovefs mcp --meta-dir $G --require-api-key < $R.in2 2>$R.err2 >$R.out2
jq -c 'select(.id) | if .error then {id, err_code:.error.code, err:(.error.message|tostring|.[0:90])} else {id, keys:(.result|keys)} end' $R.out2 2>&1 | sed 's/^/    /'
echo "  工具清单:"; jq -r 'select(.id==2) | (.result.tools | length) as $n | "    \($n) 个: " + ([.result.tools[].name] | join(", "))' $R.out2 2>&1
echo "  工具注解(readOnlyHint 等)样例:"; jq -c 'select(.id==2) | .result.tools[0].annotations' $R.out2 2>&1
echo "  initialize 能力面:"; jq -c 'select(.id==1) | .result.capabilities' $R.out 2>/dev/null; jq -c 'select(.id==1) | .result | {protocolVersion, serverInfo, has_instructions: (.instructions|length)}' $R.out2 2>&1
echo "  resources/list:"; jq -r 'select(.id==15) | .result.resources[].uri' $R.out2 2>&1 | sed 's/^/    /'
echo "  templates/list:"; jq -r 'select(.id==16) | .result.resourceTemplates[] | [.uriTemplate, .name] | join("  ")' $R.out2 2>&1 | sed 's/^/    /'
echo "  prompts/list:"; jq -r 'select(.id==21) | .result.prompts[] | [.name, (.arguments|length)] | join(" args=")' $R.out2 2>&1 | sed 's/^/    /'
echo "  completion 结果:"; jq -c 'select(.id==23) | .result' $R.out2 2>&1 | head -c 300; echo
echo "  服务端通知行(logging): $(grep -c '"method":"notifications/message"' $R.out2) 条"
echo "  协议错误路径逐条:"; jq -c 'select(.id==26 or .id==27 or .id==28 or .id==29 or .id==30 or .id==31) | {id, error}' $R.out2 2>&1 | sed 's/^/    /'
echo "  governance 拒绝走 isError 而非 JSON-RPC 错误层:"; jq -c 'select(.id==10 or .id==12) | {id, isError:.result.isError, text:(.result.content[0].text|.[0:120])}' $R.out2 2>&1 | sed 's/^/    /'
echo "  structuredContent 双载:"; jq -c 'select(.id==3) | {has_structured: (.result | has("structuredContent"))}' $R.out2 2>&1 | sed 's/^/    /'
echo "  调用审计(apikey calls):"; $AK calls --limit 10 2>&1 | sed 's/^/    /'

echo "===== D6 清理 ====="
$AK list 2>&1 | grep -oE '^[0-9]+ +[0-9a-f]{32}' | awk '{print $2}' | while read -r k; do $AK revoke --id "$k" >/dev/null 2>&1; done
echo "  本轮密钥已全部 revoke(条目保留以属审计):"
$AK list 2>&1 | head -4 | sed 's/^/    /'
sudo systemctl stop gft-gatem gft-keyds 2>/dev/null
sudo rm -rf $G /tmp/gft_keystore $R $R.* /tmp/gft_scopein /tmp/gft_scopeout /tmp/gft_scopeerr /tmp/gft_key.env /tmp/roerr
echo "  隔离栈已停: 残留挂载 $(mount | grep -c grovefs) 个(应为 1=部署挂载), 监听 $(ss -ltn | grep -cE ':5017[3]|:50111') 个"
REMOTE

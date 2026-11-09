#!/bin/bash
# 集群功能实测 第二部分: 透镜 link/fork 内容写精确性/import/du 两条路径/
# view export-import 跨机/gc/refs rebuild/view rm --base.
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
M=$HOME/mnt
GV="grovefs view --meta-dir $HOME/meta"
ENV=/etc/grovefs/cluster.env
S=$HOME/gftstore          # 本地共置存储根: meta/ + blocks/ 都在此
as_root_env(){ sudo sh -c "set -a; . $ENV; set +a; $1"; }
tmnt(){
  sudo mkdir -p "$2" && sudo chown hanjun:hanjun "$2"
  sudo systemd-run --unit="gft-$3" --collect --quiet \
    -p EnvironmentFile=$ENV -p User=hanjun \
    /usr/local/bin/grovefs mount --view "$1" -m "$2" --foreground
  for i in $(seq 1 40); do mountpoint -q "$2" && break; sleep 0.25; done
  echo "    [挂载 $1 -> $2: $(mountpoint -q "$2" && echo 已挂载 || echo 未挂载)]"
}
tkill(){ sudo systemctl stop "gft-$1" 2>/dev/null; sudo rm -rf "/tmp/gft_$1"; }
# 两机均无 sqlite3 CLI, 统一走 python3 内置模块查元数据库.
q(){ python3 -c "import sqlite3,sys;c=sqlite3.connect(sys.argv[1]);print(' ; '.join(','.join(str(x) for x in r) for r in c.execute(sys.argv[2])))" "$1" "$2" 2>&1; }

echo "===== C0 本轮前置(承接 battery_1 的 P0 清盘) ====="
for n in $(ls "$S/meta"/*.db 2>/dev/null | xargs -r -n1 basename | sed 's/\.db$//' | grep -vE '^(api_keys|refs)$'); do grovefs view --meta-dir "$S" rm --name "$n" --base --data-dir "$S" >/dev/null 2>&1; done
rm -rf "$S"; mkdir -p "$S"
printf 'link /vis.txt /govdoc\n' > /tmp/c1.rules
$GV list 2>&1 | sed 's/^/  当前定义: /'

echo "===== C1 link 动词单独验证(keep_src: 目的地与源同时可见) ====="
cd "$M"; printf 'visible\n' > vis.txt
printf 'link /vis.txt /govdoc\n' > /tmp/c1.rules
$GV define --name c1 --base alice --rules /tmp/c1.rules 2>&1 | sed 's/^/  define: /'
tmnt c1 /tmp/gft_c1 c1
echo "  /govdoc      = $(cat /tmp/gft_c1/govdoc 2>&1)"
echo "  /vis.txt     $(test -e /tmp/gft_c1/vis.txt && echo '仍可见(keep_src OK)' || echo '丢失(FAIL)')"
echo "  同一对象两侧 md5: $(md5sum /tmp/gft_c1/govdoc /tmp/gft_c1/vis.txt $M/vis.txt 2>/dev/null | awk '{print $1}' | sort -u | tr '\n' ' ')"
tkill c1; $GV rm --name c1 2>&1 | sed 's/^/  撤: /'

echo "===== C2 fork 内容写语义精确验证(每种 open 形态单独看 errno) ====="
printf 'forkbase\n' > $M/vis.txt; sync $M/vis.txt; sync
$GV fork --name c2-fork --base alice 2>&1 | sed 's/^/  fork: /' | head -1
tmnt c2-fork /tmp/gft_c2 c2
python3 - <<'PY'
import os
P = "/tmp/gft_c2/vis.txt"
def probe(tag, flags):
    try:
        fd = os.open(P, flags)
    except OSError as e:
        print(f"  {tag}: open 即被拒 errno={e.errno} ({e.strerror})")
        return
    try:
        n = os.write(fd, b"X")
        try:
            os.fsync(fd); fin = "fsync 成功"
        except OSError as e2:
            fin = f"fsync errno={e2.errno} ({e2.strerror})"
        print(f"  {tag}: open+write({n}B) 成功 -> {fin}")
    except OSError as e:
        print(f"  {tag}: write errno={e.errno} ({e.strerror})")
    finally:
        try: os.close(fd)
        except OSError as e: print(f"    close errno={e.errno} ({e.strerror})")
probe("O_WRONLY        ", os.O_WRONLY)
probe("O_WRONLY|O_APPEND", os.O_WRONLY | os.O_APPEND)
probe("O_RDWR|O_TRUNC  ", os.O_RDWR | os.O_TRUNC)
probe("O_RDWR          ", os.O_RDWR)
try:
    os.truncate(P, 0); print("  os.truncate     : 成功")
except OSError as e:
    print(f"  os.truncate     : errno={e.errno} ({e.strerror})")
print("  fork 侧内容回读  :", repr(open(P, "rb").read()))
print("  基视图内容回读   :", repr(open(os.path.expanduser("~/mnt/vis.txt"), "rb").read()))
PY
tkill c2; $GV rm --name c2-fork 2>&1 | sed 's/^/  撤: /'

echo "===== C3 import 零拷贝 + 重跑增量(本地共置存储根) ====="
rm -rf /tmp/gft_src; mkdir -p /tmp/gft_src/docs /tmp/gft_src/photos
printf 'aaa\n' > /tmp/gft_src/docs/a.md; printf 'bbb\n' > /tmp/gft_src/photos/b.jpg
dd if=/dev/urandom of=/tmp/gft_src/bigblob bs=1M count=8 2>/dev/null
before_obj=$(find "$S" -path '*/blocks/*' -type f | wc -l)
echo "  导入前 blocks/ 对象数: $before_obj"
grovefs import --meta-dir "$S" --view imp --source /tmp/gft_src --dry-run 2>&1 | sed 's/^/  dry-run: /'
grovefs import --meta-dir "$S" --view imp --source /tmp/gft_src 2>&1 | sed 's/^/  import: /'
after_obj=$(find "$S" -path '*/blocks/*' -type f | wc -l)
echo "  导入后 blocks/ 对象数: $after_obj"
echo "  磁盘增量(字节): $(du -sb $S/blocks | cut -f1)  vs 源目录字节和 $(du -sb /tmp/gft_src | cut -f1)"
echo "  视图树内 data_ref 样例: $(q "$S/meta/imp.db" "select name||'='||data_ref from node where data_ref is not null limit 3")"
echo "  零拷贝的真实含义(同一 inode, 不新增数据块):"
for f in docs/a.md photos/b.jpg bigblob; do
  ino_src=$(stat -c '%i' /tmp/gft_src/$f)
  blk=$(find "$S/blocks" -type f -inum "$ino_src" 2>/dev/null | head -1)
  echo "    源 $f: ino=$ino_src links=$(stat -c '%h' /tmp/gft_src/$f) -> blocks 内同 inode 条目: ${blk:-未找到}"
done
echo "    blocks 文件 inode 去重前/后: $(find $S/blocks -type f -printf '%i\n' | sort | wc -l) / $(find $S/blocks -type f -printf '%i\n' | sort -u | wc -l)"
grovefs import --meta-dir "$S" --view imp --source /tmp/gft_src 2>&1 | sed 's/^/  重跑: /'
printf 'ccc\n' > /tmp/gft_src/docs/c.md
grovefs import --meta-dir "$S" --view imp --source /tmp/gft_src 2>&1 | sed 's/^/  新增一个文件后重跑: /'
echo "  两次导入同一源到两个视图(共享对象):"
grovefs import --meta-dir "$S" --view imp2 --source /tmp/gft_src 2>&1 | sed 's/^/    imp2: /'
echo "    blocks/ 路径数: $(find "$S" -path '*/blocks/*' -type f | wc -l) (每视图各有自己的对象名)"
echo "    磁盘实际占用: $(du -sb $S/blocks | cut -f1) 字节 ; 源目录: $(du -sb /tmp/gft_src | cut -f1) 字节 (去重后不翻倍)"
echo "    distinct inode 数: $(find $S/blocks -type f -printf '%i\n' | sort -u | wc -l)"
echo "    holders 台账: $(q "$S/meta/refs.db" 'select count(*), sum(holders) from holders')"

echo "===== C4 du 本地路径(--meta-dir / --home, 自含数据根) ====="
grovefs du --meta-dir "$S" --view imp --total 2>&1 | sed 's/^/  --meta-dir --total: /'
grovefs du --home "$S" --view imp --depth 1 2>&1 | sed 's/^/  --home --depth 1: /'
exp=$(find /tmp/gft_src -type f -printf '%s\n' | awk '{s+=$1} END{print s}')
got=$(grovefs du --meta-dir "$S" --view imp -b --total 2>/dev/null | awk '$2=="/"{print $1}')
echo "  与源目录字节和核对: 期望=$exp 实测=$got $([ "$exp" = "$got" ] && echo '(OK)' || echo '(FAIL)')"
grovefs du --meta-dir "$S" --view imp --path /gft_src/docs 2>&1 | sed 's|^|  --path /gft_src/docs: |'
grovefs du --meta-dir "$S" --view imp --path docs 2>&1 | sed 's/^/  --path docs(裸相对名): /'
grovefs du --meta-dir "$S" --view imp --path nosuchdir 2>&1 | sed 's/^/  不存在路径: /'

echo "===== C5 du 远程路径(--meta-server + --data-servers) 与三种拒绝 ====="
as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total' 2>&1 | sed 's/^/  双地址 --total: /'
as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 -b --depth 1' 2>&1 | sed 's/^/  字节 --depth 1: /'
mntsum=$(find $M -type f -printf '%s\n' 2>/dev/null | awk '{s+=$1} END{print s+0}')
clusum=$(as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 -b --total' 2>/dev/null | awk '$2=="/"{print $1}')
echo "  挂载点文件字节和=$mntsum  du 报=$clusum $([ "$mntsum" = "$clusum" ] && echo '(OK 一致)' || echo '(不一致, 见结论)')"
echo "  条带对象单地址测量必须拒绝(不得静默 0):"
as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100 --total' 2>&1 | sed 's/^/    /'
echo "  完全没有数据源必须拒绝(只清除数据源, 保留 token):"
sudo sh -c "set -a; . $ENV; set +a; unset GROVEFS_DATA_SERVERS; grovefs du --view alice --meta-server 127.0.0.1:50053 --total" 2>&1 | sed 's/^/    /'
echo "  本地数据根无 blocks/ 必须拒绝:"
grovefs du --view imp --meta-dir "$S" --data-dir /tmp 2>&1 | sed 's/^/    /'
echo "  丢对象必须出声(挑 64MiB 条带文件的一个非空分块挑走):"
cand=$(q "$HOME/meta/meta/alice.db" "select data_ref from node where name='big.bin' and data_ref like 's%'")
echo "    选中 data_ref: $cand  (格式 s<服务器数>:<条带字节>:<uuid>)"
uuid=$(echo "$cand" | awk -F: '{print $3}' | cut -d'#' -f1)
host=""; lost=""
for h in gfs1 gfs2; do
  f=$(ssh -n $h "sudo find /data/gfs*/grove/blocks -name '${uuid}*' -type f -size +0 2>/dev/null | head -1")
  if [ -n "$f" ]; then host=$h; lost=$f; break; fi
done
echo "    挑中分块: $host:$lost"
if [ -n "$lost" ]; then
  ssh -n $host "sudo stat -c '    挑走前 %n %s 字节' \"$lost\""
  ssh -n $host "sudo mv $lost /tmp/hidden.obj" && echo "    已把该分块从数据根挑走(模拟丢对象)"
fi
echo "    挑走后 du --total:"; as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total' 2>&1 | sed 's/^/      /'
echo "    挑走后 du -b --path /datasets: $(as_root_env "grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 -b --path /datasets" 2>&1 | tr '\n' '|')"
echo "    挂载点内读受影响文件: $(cat 2>/tmp/caterr $M/datasets/big.bin >/dev/null; echo "rc=$? 报错=[$(cat /tmp/caterr)]")"
ssh -n $host "sudo mv /tmp/hidden.obj $lost" && echo "    已复原该分块"
echo "  复原后重测:"
as_root_env 'grovefs du --view alice --meta-server 127.0.0.1:50053 --data-servers gfs1:50100,gfs2:50100 --total' 2>&1 | sed 's/^/    /'

echo "===== C6 view export/import 跨机迁移 + --as-fork ====="
$GV export --view alice --out /tmp/alice.export.jsonl 2>&1 | sed 's/^/  export: /'
echo "  导出文件行数: $(wc -l < /tmp/alice.export.jsonl) 头部: $(head -c 200 /tmp/alice.export.jsonl)"
echo "  导出虚拟名必须被拒:"
$GV define --name c6lens --base alice --rules /tmp/c1.rules >/dev/null 2>&1
$GV export --view c6lens --out /tmp/x.jsonl 2>&1 | sed 's/^/    /'
$GV rm --name c6lens 2>/dev/null
echo "  跨机迁移(共置存储根 $S/imp -> gfs1:/tmp/gft_recv):"
SR="$S"
rm -f /tmp/imp.export.jsonl
grovefs view --meta-dir "$SR" export --view imp --out /tmp/imp.export.jsonl 2>&1 | sed 's/^/    export: /'
echo "    导出行数: $(wc -l < /tmp/imp.export.jsonl) 头部: $(head -c 160 /tmp/imp.export.jsonl)"
key=$(q "$SR/meta/imp.db" "select data_ref from node where data_ref is not null and data_ref not like 's%' limit 1" | cut -d';' -f1 | tr -d ' ')
# 分桶目录是 key 的哈希(不是前两位), 故按名字找而不是猜路径
kpath=$(find "$SR/blocks" -name "$key" -type f | head -1)
src_md5=$(md5sum "$kpath" 2>/dev/null | cut -d' ' -f1)
echo "    源对象 $key 落点 $kpath" 
echo "    源对象 $key md5=$src_md5"
ssh -n gfs1 'sudo rm -rf /tmp/gft_recv; sudo mkdir -p /tmp/gft_recv; sudo chown -R hanjun /tmp/gft_recv'
scp -q -o BatchMode=yes /tmp/imp.export.jsonl gfs1:/tmp/
# tar 走管道时 ssh 不能加 -n(-n 会把 stdin 换成 /dev/null)
tar -C "$SR" -cf - blocks 2>/dev/null | ssh gfs1 'tar -C /tmp/gft_recv -xf -'
echo "    已拷 blocks/: gfs1 上对象数 $(ssh -n gfs1 'find /tmp/gft_recv/blocks -type f | wc -l')"
ssh -n gfs1 "grovefs view --meta-dir /tmp/gft_recv import --view moved --in /tmp/imp.export.jsonl" 2>&1 | sed 's/^/    gfs1 import: /'
ssh -n gfs1 "grovefs view --meta-dir /tmp/gft_recv list; echo '    导入后 du(数掂必须指向目标存根): '; grovefs du --meta-dir /tmp/gft_recv --view moved -b --total --data-dir /tmp/gft_recv" 2>&1 | sed 's/^/  /'
got_md5=$(ssh -n gfs1 "find /tmp/gft_recv/blocks -name '$key' -type f -exec md5sum {} + 2>/dev/null | cut -d' ' -f1")
echo "    同一 data_ref 在 gfs1 上的 md5=$got_md5  $([ "$src_md5" = "$got_md5" ] && echo '(字节一致 OK)' || echo '(不一致 FAIL)')"
local_bytes=$(grovefs du --meta-dir "$SR" --view imp -b --total 2>/dev/null | awk '$2=="/"{print $1}')
remote_bytes=$(ssh -n gfs1 "grovefs du --meta-dir /tmp/gft_recv --view moved -b --total --data-dir /tmp/gft_recv" 2>/dev/null | awk '$2=="/"{print $1}')
echo "    迁移前后字节: 源=$local_bytes 目标=$remote_bytes $([ "$local_bytes" = "$remote_bytes" ] && echo '(OK)' || echo '(FAIL)')"
echo "  重复导入未加 --replace 必须被拒:"
ssh -n gfs1 'grovefs view --meta-dir /tmp/gft_recv import --view moved --in /tmp/imp.export.jsonl' 2>&1 | sed 's/^/    /'
echo "  --replace 覆盖(离线迁移专用):"
ssh -n gfs1 'grovefs view --meta-dir /tmp/gft_recv import --view moved --in /tmp/imp.export.jsonl --replace' 2>&1 | sed 's/^/    /'
echo "  --as-fork 记录为 fork:"
ssh -n gfs1 'grovefs view --meta-dir /tmp/gft_recv import --view asfk --in /tmp/imp.export.jsonl --as-fork moved' 2>&1 | sed 's/^/    /'
ssh -n gfs1 'grovefs view --meta-dir /tmp/gft_recv list' 2>&1 | sed 's/^/    /'
echo "  迁移目标侧收尾: $(ssh -n gfs1 'sudo rm -rf /tmp/gft_recv /tmp/imp.export.jsonl; ls -d /tmp/gft_recv 2>&1')"

echo "===== C7 gc(本地孤儿审计/回收) 与 refs rebuild ====="
echo "  造孤儿: 直接在 blocks/ 放一个无人引用的对象"
mkdir -p "$S/blocks/zz"
dd if=/dev/urandom of="$S/blocks/zz/orphan.bin" bs=1k count=4 2>/dev/null
ls "$S/blocks" | head -3 | sed 's/^/    blocks 顶层条目: /'
grovefs gc --meta-dir "$S" --data-dir "$S" --older-than 0 2>&1 | sed 's/^/  gc 干跑: /'
grovefs gc --meta-dir "$S" --data-dir "$S" --older-than 0 --apply 2>&1 | sed 's/^/  gc --apply: /'
echo "  回收后对象是否真消失: $(find "$S/blocks" -name 'orphan.bin' | wc -l) 个残留"
echo "  gc 在带挂载/远程数据根上的拒绝原文:"
grovefs gc --meta-dir "$HOME/meta" --data-dir "$HOME/meta" --older-than 0 2>&1 | sed 's/^/    /'
echo "  refs rebuild:"
q "$S/meta/refs.db" 'delete from holders' >/dev/null 2>&1
echo "    人为清空后台账行数: $(q "$S/meta/refs.db" 'select count(*) from holders')"
grovefs refs --meta-dir "$S" rebuild 2>&1 | sed 's/^/    rebuild: /'
echo "    重建后: $(q "$S/meta/refs.db" 'select count(*), sum(holders) from holders') 行/持有数"
echo "    重建前 du 仍可测: $(grovefs du --meta-dir "$S" --view imp -b --total 2>&1 | tr '\n' ' ')"
echo "    台账被清空未 rebuild 时的删除保护(手工比对): $(q "$S/meta/refs.db" 'select count(*) from holders where holders > 1') 个对象多持有者"

echo "===== C8 view rm --base 的守卫与字节回收 ====="
$GV create --view c8base 2>&1 | head -1 | sed 's/^/  建基树 c8base: /'
echo "  守卫1: 视图正被挂载时"
tmnt c8base /tmp/gft_c8 c8
$GV rm --name c8base --base 2>&1 | sed 's/^/    /'
tkill c8
echo "  守卫2: 仍有定义指向它"
$GV alias --name c8pub --real c8base 2>&1 | head -1 | sed 's/^/    建别名: /'
$GV rm --name c8base --base 2>&1 | sed 's/^/    /'
$GV rm --name c8pub 2>&1 | sed 's/^/    撤别名: /'
echo "  守卫3: 未挂载且无定义 -> 真正删树并回收字节"
$GV rm --name c8base --base 2>&1 | sed 's/^/    /'
echo "    剩余树: $(ls "$HOME/meta/meta"/*.db 2>/dev/null | xargs -n1 basename | tr '\n' ' ')"
echo "  本地存储根的 --base 回收(blocks/ 在旁):"
mkdir -p "$S/blocks"
grovefs view --meta-dir "$S" rm --name imp2 --base --data-dir "$S" 2>&1 | sed 's/^/    rm imp2 --base: /'
grovefs view --meta-dir "$S" rm --name imp --base --data-dir "$S" 2>&1 | sed 's/^/    rm imp --base: /'
echo "    剩余 blocks 对象数: $(find "$S/blocks" -type f | wc -l)  ; 剩余树: $(ls "$S/meta" 2>/dev/null | tr '\n' ' ')"
REMOTE

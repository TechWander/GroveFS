#!/bin/bash
# 集群功能实测 第一部分: 挂载与 POSIX 语义 / 视图四态.
# 远端脚本经 `ssh gfs-client bash -s` 原样投递(不在宿主做二次引号展开).
# 输出即为测试报告要照抄的回显, 失败的原样保留.
set -uo pipefail
export LC_ALL=C
ssh -o BatchMode=yes gfs-client 'bash -s' <<'REMOTE'
# A6 的阻塞锁探针可能留下 SIGTERM 杀不掉的子进程(D 状态在 FUSE 等待里), 它会
# 一直占着 ssh 的 stdout 管道 -> 整个会话不返回. 因此本脚本把所有输出先落到远端
# 文件, 结束时再一次性吐回管道: 卡住的子进程只持有文件 fd, 不阻塞会话结束.
OUT=/tmp/vm_b1_evidence.out
rm -f /tmp/vm_b1_probe.out /tmp/vm_b1_probe.out.rc
exec 3>&1 4>&2
exec >"$OUT" 2>&1
M=$HOME/mnt
# 治理面: --meta-dir 是子命令组级选项(`grovefs view --meta-dir D list`),
# 写在 `grovefs` 后面会被 clap 拒绝, 故统一放在组名之后.
GV="grovefs view --meta-dir $HOME/meta"
ENV=/etc/grovefs/cluster.env
tmnt(){ # tmnt <view> <mntpoint> <unit>
  sudo mkdir -p "$2" && sudo chown hanjun:hanjun "$2"
  sudo systemd-run --unit="gft-$3" --collect --quiet \
    -p EnvironmentFile=$ENV -p User=hanjun \
    /usr/local/bin/grovefs mount --view "$1" -m "$2" --foreground
  for i in $(seq 1 40); do mountpoint -q "$2" && break; sleep 0.25; done
  echo "    [挂载 $1 -> $2: $(mountpoint -q "$2" && echo 已挂载 || echo 未挂载)]"
}
tkill(){ sudo systemctl stop "gft-$1" 2>/dev/null; sudo rm -rf "/tmp/gft_$1"; }

echo "===== P0 清盘: 回到 systemd 部署态, 不带上一轮残留 ====="
systemctl is-active grovefs-meta-server grovefs-mount-alice | sed 's/^/  单元 active: /'
for u in $(systemctl list-units --no-legend 'gft-*' | awk '{print $1}'); do sudo systemctl stop "$u"; done
for d in /tmp/gft_*/; do mountpoint -q "$d" && sudo fusermount -u "$d" 2>/dev/null; done
sudo rm -rf /tmp/gft_*; sudo rm -f /tmp/c1.rules /tmp/e2.rules /tmp/b2.rules /tmp/alice.export.jsonl /tmp/x.jsonl /tmp/roerr /tmp/e1 /tmp/e1b /tmp/e2 /tmp/e3 /tmp/e4 /tmp/werr
echo "  清掉 transient 挂载单元与测试目录: 剩余挂载 $(mount | grep -c grovefs) 个"
names=$($GV list 2>/dev/null | grep -oE "(alias|fork|derived view|lens) '[a-zA-Z0-9_-]+'" | sed "s/.*'\(.*\)'/\1/" | sort -u)
for n in $names; do $GV rm --name "$n" >/dev/null 2>&1 && echo "  撤定义: $n"; done
for b in $(ls "$HOME/meta/meta"/*.db 2>/dev/null | xargs -r -n1 basename | sed 's/\.db$//' | grep -vE '^(alice|api_keys|refs)$'); do
  $GV rm --name "$b" --base 2>&1 | sed "s/^/  撤残留树 $b: /"
done
cd "$M"
for f in *; do rm -rf -- "$f"; done
# 上一轮可能把结构写落到了基树里(fork 未建成时), 这里显式清干
rm -rf newdir b4-renamed.txt only-here lockf 2>/dev/null
mkdir -p datasets private
printf 'payload-A\n' > a1b.txt; printf 'visible\n' > vis.txt; printf 'onlyme\n' > onlyme.txt
printf 'secret\n' > private/secret.txt; printf 't1\n' > datasets/t1.txt
dd if=/dev/urandom of=datasets/big.bin bs=1M count=64 2>/dev/null; sync
rm -f "$HOME/meta/meta/pv.db" 2>/dev/null
echo "  alice 基线内容: [$(ls | tr '\n' ' ')] datasets/[$(ls datasets | tr '\n' ' ')]"
echo "  grovefs 进程数(本机): $(pgrep -x grovefs | wc -l)"
echo "  存储根对象数 gfs1/gfs2: $(ssh -n gfs1 'sudo find /data/gfs*/grove/blocks -type f | wc -l') / $(ssh -n gfs2 'sudo find /data/gfs*/grove/blocks -type f | wc -l')"

echo "===== A1 常规读写/mkdir/rename/unlink ====="
cd "$M"
echo "payload-A" > a1.txt; mkdir -p d1/d2; echo deep > d1/d2/deep.txt
echo "  回读: $(cat a1.txt) / $(cat d1/d2/deep.txt)"
mv a1.txt a1b.txt
echo "  rename 后旧名: $(test -e a1.txt && echo '仍存在(FAIL)' || echo '已消失(OK)')"
echo "  rename 后新名: $(cat a1b.txt)"
cp a1b.txt copy.txt; rm copy.txt
echo "  cp/rm: $(test -e copy.txt && echo '残留(FAIL)' || echo '已删除(OK)')"
echo "  递归删除: $(rm -rf d1; test -e d1 && echo '残留(FAIL)' || echo '已清除(OK)')"

echo "===== A2 符号链接(含悬空) ====="
ln -s a1b.txt link_to_a1
echo "  readlink: $(readlink link_to_a1)"
echo "  透过软链读: $(cat link_to_a1)"
ln -s missing.txt broken_link
ls -l broken_link | sed 's|.*hanjun hanjun|  条目: |'
cat broken_link 2>&1 | sed 's|^|  读悬空软链: |'
rm -f link_to_a1 broken_link

echo "===== A3 硬链接(同 inode, 共享字节) ====="
printf 'shared-bytes' > h_src; ln h_src h_alt
stat -c '  src: ino=%i links=%h size=%s' h_src
stat -c '  alt: ino=%i links=%h size=%s' h_alt
rm h_src
echo "  删一名后另一名仍可读: $(cat h_alt)"
stat -c '  剩余 links=%h ino=%i' h_alt
rm -f h_alt

echo "===== A4 权限位与时间戳 ====="
printf 'x' > perm.txt; chmod 600 perm.txt; stat -c '  chmod 600 -> 模式=%a' perm.txt
chmod 444 perm.txt; stat -c '  chmod 444 -> 模式=%a uid=%u' perm.txt
cat perm.txt >/dev/null 2>&1 && echo "  只读文件仍可读(OK)" || echo "  只读文件不可读(FAIL)"
printf 'y' > perm.txt 2>/tmp/werr; echo "  写只读文件: rc=$? 报错=[$(cat /tmp/werr)] 写后内容=$(cat perm.txt)"
id | sed 's/^/  当前身份: /'
touch -d '2021-03-04 05:06:07' perm.txt; stat -c '  mtime 显式设置 -> %y' perm.txt
rm -f perm.txt

echo "===== A5 truncate / 稀疏洞 / 追加 ====="
python3 - <<'PY'
import os
fd = os.open("sparse.bin", os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"A" * 10)
os.truncate(fd, 1 << 20)
os.pwrite(fd, b"Z", (1 << 20) - 1)
os.fsync(fd)
st = os.fstat(fd)
print(f"  st_size={st.st_size}  st_blocks*512={st.st_blocks*512}")
print(f"  洞内读回 16B 全零={os.pread(fd, 16, 512) == bytes(16)}")
os.close(fd)
with open("sparse.bin", "ab") as f:
    f.write(b"tail"); f.flush(); os.fsync(f.fileno())
print("  追加后尾部:", open("sparse.bin","rb").read()[-4:])
print("  重开 size:", os.path.getsize("sparse.bin"))
os.remove("sparse.bin")
PY

echo "===== A6 POSIX 建议锁(flock / fcntl), 每步 12s 超时 ====="
# 探针必须与 ssh 的管道完全脱钩: 阻塞式 flock 不收敛时, 进程会停在 D 状态
# (wchan=request_wait_answer), SIGTERM/SIGKILL 都无法在 FUSE 应答前投递;
# 若它还持有 ssh 的 stdin/stdout, 整个取证会话会被它卡死.
TL(){ local of=/tmp/vm_b1_probe.out rc t=${2:-12}; timeout -s KILL "$t" python3 -c "$1" </dev/null >"$of" 2>&1; rc=$?; cat "$of"; rm -f "$of"; [ $rc = 124 ] && echo "    [$t}s 超时 -> 该步挂起(进程停在 D 状态, 信号不生效)]"; [ $rc != 0 ] && [ $rc != 124 ] && echo "    [rc=$rc]"; }
TL "
import fcntl,os
fd=os.open('lockf',os.O_RDWR|os.O_CREAT|os.O_TRUNC,0o644)
fcntl.flock(fd,fcntl.LOCK_EX); print('  (1) flock(LOCK_EX) 取得: OK')
fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB); print('  (2) 同 fd 重入 LOCK_EX|LOCK_NB: 成功(POSIX 语义)')
os.close(fd); print('  (3) close 释放: OK')
"
echo "  (4) 另一进程非阻塞 flock(应冲突):"
TL "
import fcntl,os,subprocess,sys
fd=os.open('lockf',os.O_RDWR)
fcntl.flock(fd,fcntl.LOCK_EX)
p=subprocess.run([sys.executable,'-c','import fcntl,os,sys;\nfd=os.open(\"lockf\",os.O_RDWR)\ntry:\n fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB); rc=0\nexcept OSError as e:\n rc=1000+e.errno\nsys.exit(rc)'],capture_output=True,text=True)
print('    子进程退出码=%s (1013=EACCES 冲突; 0=未互斥=FAIL)' % p.returncode)
print('    子进程 stderr:', (p.stderr.strip() or '(空)')[:120])
os.close(fd)
"
echo "  (5) flock 持有下 fcntl(F_SETLK) 整文件写锁(同一冲突域):"
TL "
import fcntl,os,struct,subprocess,sys
fd=os.open('lockf',os.O_RDWR)
fcntl.flock(fd,fcntl.LOCK_EX)
p=subprocess.run([sys.executable,'-c','import fcntl,os,struct,sys;\nfd=os.open(\"lockf\",os.O_RDWR)\ntry:\n fcntl.fcntl(fd,fcntl.F_SETLK,struct.pack(\"hhqqi\",fcntl.F_WRLCK,os.SEEK_SET,0,0,0)); rc=0\nexcept OSError as e:\n rc=1000+e.errno\nsys.exit(rc)'],capture_output=True,text=True)
print('    子进程退出码=%s (1013=EACCES; 0=两锁不互斥=FAIL)' % p.returncode)
os.close(fd)
"
echo "  (6) 释放后 fcntl(F_SETLK)/F_GETLK 登记与查询:"
TL "
import fcntl,os,struct
fd=os.open('lockf',os.O_RDWR)
fl=struct.pack('hhqqi',fcntl.F_WRLCK,os.SEEK_SET,0,0,0)
fcntl.fcntl(fd,fcntl.F_SETLK,fl); print('    F_SETLK 登记: OK')
q=fcntl.fcntl(fd,fcntl.F_GETLK,struct.pack('hhqqi',fcntl.F_RDLCK,os.SEEK_SET,0,0,0))
print('    F_GETLK 回显 type=%d (F_UNLCK=3 表示查询到自身已释放/或返回自身)' % struct.unpack('hhqqi',q)[0])
fcntl.fcntl(fd,fcntl.F_SETLK,struct.pack('hhqqi',fcntl.F_UNLCK,os.SEEK_SET,0,0,0)); print('    F_UNLCK 释放: OK')
os.close(fd); os.remove('lockf')
"
echo "  (7) 阻塞式 flock 等对方释放(父子同进程组, 1.5s 后父释放):"
TL "
import fcntl,os,time
fd=os.open('lockf',os.O_RDWR|os.O_CREAT|os.O_TRUNC,0o644)
fcntl.flock(fd,fcntl.LOCK_EX)
r,w=os.pipe()
pid=os.fork()
if pid==0:
    os.close(r); time.sleep(0.2)
    fd2=os.open('lockf',os.O_RDWR)
    t0=time.time(); fcntl.flock(fd2,fcntl.LOCK_EX)
    os.write(w,('子阻塞 %.2fs 后取得锁' % (time.time()-t0)).encode()); os.close(w); os._exit(0)
os.close(w); time.sleep(1.5); os.close(fd)
print('  父进程已 close 释放;', os.read(r,120).decode())
" 15
echo "  A6 结束时挂载仍可用: $(ls lockf 2>/dev/null; test -d . && echo 可读)"

echo "===== A7 xattr / O_DIRECT(当前版本边界) ====="
python3 - <<'PY'
import os
open("xa.txt","w").write("v")
try:
    os.setxattr("xa.txt", "user.note", b"v1")
    print("  setxattr 成功:", os.getxattr("xa.txt", "user.note"))
except OSError as e:
    print(f"  setxattr: errno={e.errno} ({e.strerror})")
try:
    print("  listxattr:", os.listxattr("xa.txt"))
except OSError as e:
    print(f"  listxattr: errno={e.errno} ({e.strerror})")
os.remove("xa.txt")
PY
dd if=/dev/zero of=odirect.bin bs=1M count=1 oflag=direct 2>&1 | tail -1 | sed 's/^/  O_DIRECT: /'
rm -f odirect.bin

echo "===== B1 基视图: view list 与物理树 ====="
$GV list 2>&1 | sed 's/^/  /'
echo "  meta/ 目录: $(ls -1 "$HOME/meta/meta" | tr '\n' ' ')"
echo "  views/ 目录: $(ls -1 "$HOME/meta/meta/views" 2>&1 | tr '\n' ' ')"

echo "===== B2 派生透镜: hide/only/map/link 四动词 ====="
mkdir -p private datasets
printf 'secret-doc\n' > private/hidden.txt
printf 'ds-1\n' > datasets/t1.txt
printf 'visible\n' > vis.txt
cat > /tmp/lab.rules <<'RULES'
# 三个改写动词各一条(参数顺序统一为 <src> <dst>)
hide /private
map /datasets /data
link /vis.txt /govdoc
RULES
cat > /tmp/only.rules <<'RULES'
only /onlyme.txt
RULES
printf 'onlyme\n' > onlyme.txt
$GV define --name lab --base alice --rules /tmp/lab.rules 2>&1 | sed 's/^/  define: /'
tmnt lab /tmp/gft_lab lab
echo "  透镜根目录: [$(ls -1 /tmp/gft_lab 2>&1 | tr '\n' ' ')]"
echo "  hide: /private $(test -e /tmp/gft_lab/private && echo '仍可见(FAIL)' || echo '不可见(OK)')"
echo "  map:  /data=[$(ls /tmp/gft_lab/data 2>&1 | tr '\n' ' ')] 原 /datasets $(test -e /tmp/gft_lab/datasets && echo '仍在' || echo '已消失(改名生效)')"
echo "  link: /govdoc=$(cat /tmp/gft_lab/govdoc 2>&1) 源 /vis.txt $(test -e /tmp/gft_lab/vis.txt && echo '保留(keep_src OK)' || echo '丢失(FAIL)')"
echo "  基视图侧原路径仍在: $(ls -1 $M | tr '\n' ' ')"
printf 'x' 2>/tmp/roerr > /tmp/gft_lab/vis.txt; echo "  透镜写内容: rc=$? 报错=[$(cat /tmp/roerr)]"
mkdir /tmp/gft_lab/newdir 2>/tmp/roerr2; echo "  透镜 mkdir: rc=$? 报错=[$(cat /tmp/roerr2)]"
tkill lab
echo "  only 单独一个透镜(白名单语义, 与改写动词互不干扰):"
$GV define --name lab2 --base alice --rules /tmp/only.rules 2>&1 | sed 's/^/  define: /'
tmnt lab2 /tmp/gft_lab2 lab2
echo "    根目录=[$(ls -1 /tmp/gft_lab2 2>&1 | tr '\n' ' ')] 白名单 /onlyme.txt $(test -e /tmp/gft_lab2/onlyme.txt && echo 可见 || echo 不可见); 非白名单 /vis.txt $(test -e /tmp/gft_lab2/vis.txt && echo 仍可见 || echo 已隐藏)"
tkill lab2
$GV rm --name lab2 2>/dev/null
echo "  未知动词拒绝原文:"
printf 'bogus /x\n' > /tmp/bad.rules
$GV define --name badv --base alice --rules /tmp/bad.rules 2>&1 | sed 's/^/    /'
echo "  目标冲突拒绝原文(同一目的地两条规则):"
printf 'map /a /z\nmap /b /z\n' > /tmp/dup.rules
$GV define --name badv2 --base alice --rules /tmp/dup.rules 2>&1 | sed 's/^/    /'
echo "  规则文件内写 base 行的拒绝原文:"
printf 'base alice\nhide /private\n' > /tmp/withbase.rules
$GV define --name badv3 --base alice --rules /tmp/withbase.rules 2>&1 | sed 's/^/    /'
$GV rm --name badv 2>/dev/null; $GV rm --name badv2 2>/dev/null; $GV rm --name badv3 2>/dev/null

echo "===== B3 纯别名 + --retire 无空档轮换 ====="
$GV create --view pv 2>&1 | sed 's/^/  create pv: /'
tmnt pv /tmp/gft_pv pv
printf 'pub-content\n' > /tmp/gft_pv/pubfile.txt
tkill pv
$GV alias --name pub-a --real pv 2>&1 | sed 's/^/  alias pub-a: /'
tmnt pub-a /tmp/gft_pva puba
echo "  别名挂载可见真实视图内容: [$(ls /tmp/gft_pva | tr '\n' ' ')]"
printf 'during-rotation\n' > /tmp/gft_pva/rot.txt
tkill puba
$GV alias --name pub-b --real pv --retire pub-a 2>&1 | sed 's/^/  alias pub-b --retire pub-a: /'
echo "  轮换后老别名挂载:"
tmnt pub-a /tmp/gft_pva2 pva2
echo "    结果: $(mountpoint -q /tmp/gft_pva2 && echo '仍可挂载(FAIL)' || echo '不可挂载(OK)')"
echo "    日志尾部: $(sudo journalctl -u gft-pva2 -b --no-pager 2>/dev/null | tail -3 | tr '\n' '|')"
sudo rm -rf /tmp/gft_pva2
echo "  轮换后新别名挂载:"
tmnt pub-b /tmp/gft_pvb pvb
echo "    内容: [$(ls /tmp/gft_pvb 2>&1 | tr '\n' ' ')] rot.txt=$(cat /tmp/gft_pvb/rot.txt 2>&1)"
printf 'writable\n' > /tmp/gft_pvb/w.txt && echo "    别名可写(OK)"
echo "  别名存续期间真实名 pv 挂载:"
tmnt pv /tmp/gft_pvreal pvreal
echo "    结果: $(mountpoint -q /tmp/gft_pvreal && echo '挂载成功(FAIL)' || echo '被拒(OK)')"
echo "    日志尾部: $(sudo journalctl -u gft-pvreal -b --no-pager 2>/dev/null | tail -3 | tr '\n' '|')"
tkill pvreal; tkill pvb
$GV alias --name pub-c --real pub-a 2>&1 | sed 's/^/  链式别名拒绝: /'
$GV rm --name pub-b 2>&1 | sed 's/^/  撤别名 pub-b: /'
echo "  撤别名后真实名恢复可挂载:"
tmnt pv /tmp/gft_pv3 pv3
echo "    结果: $(mountpoint -q /tmp/gft_pv3 && echo '成功(OK)' || echo '失败(FAIL)')"
echo "    轮换留下的内容仍在: [$(ls /tmp/gft_pv3 | tr '\n' ' ')]"
tkill pv3

echo "===== B4 fork(结构可写/内容只读) 与 --empty ====="
# 自建本节点要用的名字, 不依赖上一节留下的状态
$GV rm --name alice-fork 2>/dev/null; $GV rm --name empty-fork 2>/dev/null
rm -rf $M/newdir $M/b4-renamed.txt
printf 'forkme\n' > $M/b4.txt; printf 'todelete\n' > $M/b4del.txt; sync $M/b4.txt $M/b4del.txt; sync
$GV fork --name alice-fork --base alice 2>&1 | sed 's/^/  fork: /'
$GV fork --name empty-fork --base alice --empty 2>&1 | sed 's/^/  fork --empty: /'
tmnt alice-fork /tmp/gft_fk fk
echo "  fork 根内容: [$(ls /tmp/gft_fk | tr '\n' ' ')]"
mkdir /tmp/gft_fk/newdir 2>/tmp/e1; echo "  结构写 mkdir: rc=$? 报错=[$(cat /tmp/e1)] 结果目录=$(ls -d /tmp/gft_fk/newdir 2>/dev/null && echo 存在)"
mv /tmp/gft_fk/b4.txt /tmp/gft_fk/b4-renamed.txt 2>/tmp/e1b; echo "  结构写 rename: rc=$? 报错=[$(cat /tmp/e1b)] 基视图 b4.txt=$(cat $M/b4.txt 2>&1)"
python3 - <<'PY2'
import os
P = "/tmp/gft_fk/b4-renamed.txt"
for tag, fl in (("append(O_WRONLY|O_APPEND)", os.O_WRONLY | os.O_APPEND),
                ("truncate(O_RDWR|O_TRUNC)", os.O_RDWR | os.O_TRUNC),
                ("write-only(O_WRONLY)", os.O_WRONLY)):
    try:
        fd = os.open(P, fl)
    except OSError as e:
        print(f"  内容写 {tag}: open 即被拒 errno={e.errno} ({e.strerror})")
        continue
    try:
        os.write(fd, b"mutate"); print(f"  内容写 {tag}: write 未报错")
    except OSError as e:
        print(f"  内容写 {tag}: write errno={e.errno} ({e.strerror})")
    try:
        os.fsync(fd); print(f"    {tag} fsync 成功")
    except OSError as e:
        print(f"    {tag} fsync errno={e.errno} ({e.strerror})")
    os.close(fd)
print("  fork 侧回读内容:", repr(open(P, "rb").read()))
print("  基视图侧 b4.txt:", repr(open(os.path.expanduser("~/mnt/b4.txt"), "rb").read()))
PY2
printf 'mutate' 2>/tmp/e2 >> /tmp/gft_fk/b4-renamed.txt; echo "  shell 追加 >> : rc=$? 报错=[$(cat /tmp/e2)] (bash 吞 EROFS 则 rc=0, 见逐 open 探针)"
: 2>/tmp/e3 > /tmp/gft_fk/b4-renamed.txt; echo "  shell 截断 > : rc=$? 报错=[$(cat /tmp/e3)] 截断后大小=$(stat -c %s /tmp/gft_fk/b4-renamed.txt 2>/dev/null) 基视图大小=$(stat -c %s $M/b4.txt)"
echo "  fork 内删除不影响基视图: $(rm /tmp/gft_fk/b4del.txt 2>/tmp/e4 && echo '已删(fork 名)' || echo "失败:$(cat /tmp/e4)") ; 基视图 b4del.txt $(test -e $M/b4del.txt && echo '仍在(OK)' || echo '丢失(FAIL)')"
tkill fk
tmnt empty-fork /tmp/gft_efk efk
echo "  --empty fork 挂载内容: [$(ls -A /tmp/gft_efk | tr '\n' ' ')]"
mkdir /tmp/gft_efk/only-here 2>/dev/null && echo "  空 fork 结构可写(OK): [$(ls /tmp/gft_efk)]"
tkill efk
echo "  从 fork 再 fork 的拒绝原文:"
$GV fork --name bad2 --base alice-fork 2>&1 | tail -4 | sed 's/^/    /'
$GV view 2>/dev/null; $GV list 2>&1 | sed 's/^/  当前视图: /'

echo "===== B5 view graft(选择性接枝) 与拒绝原文 ====="
$GV fork --name gft-fork --base alice --empty 2>&1 | sed 's/^/  建空 fork: /'
printf '/a1b.txt\n/datasets\timported/\n' | $GV graft --name gft-fork --src alice --map - 2>&1 | sed 's/^/  graft: /'
tmnt gft-fork /tmp/gft_gr gr
echo "  接枝结果: 根=[$(ls /tmp/gft_gr | tr '\n' ' ')] imported/=[$(ls /tmp/gft_gr/imported 2>&1 | tr '\n' ' ')]"
echo "  字节共享: $(md5sum /tmp/gft_gr/a1b.txt $M/a1b.txt 2>/dev/null | awk '{print $1}' | sort -u | wc -l) 个不同 md5 (1=共享同一对象)"
printf '/a1b.txt\n' | $GV graft --name gft-fork --src alice --map - 2>&1 | tail -3 | sed 's/^/  重名拒绝: /'
printf '/a1b.txt\n' | $GV graft --name gft-fork --src alice --map - --skip-existing 2>&1 | tail -3 | sed 's/^/  --skip-existing: /'
printf '/vis.txt\n' | $GV graft --name gft-fork --src nosuchview --map - 2>&1 | tail -3 | sed 's/^/  未知源视图拒绝: /'
printf '/vis.txt\n' | $GV graft --name lab --src alice --map - 2>&1 | tail -3 | sed 's/^/  接枝进非 fork 拒绝: /'
printf '/nosuch.txt\n' | $GV graft --name gft-fork --src alice --map - 2>&1 | tail -3 | sed 's/^/  不存在路径拒绝: /'
echo "  基视图删除对 fork 的影响: $(rm -f $M/a1b.txt; echo 已删基视图名)"
echo "    fork 侧 a1b.txt: $(cat /tmp/gft_gr/a1b.txt 2>&1)"
echo "    字节未回收(fork 仍持名): $(ls -1 $HOME/meta/meta | tr '\n' ' ')"
tkill gr
$GV rm --name gft-fork 2>&1 | sed 's/^/  撤 graft fork: /'
$GV rm --name alice-fork 2>&1 | sed 's/^/  撤 alice-fork: /'
$GV rm --name empty-fork 2>&1 | sed 's/^/  撤 empty-fork: /'

echo "===== B6 四态并立: 同一路径的四种呈现 ====="
printf 'hide /private\nonly /onlyme.txt\nmap /datasets /data\nlink /govdoc /vis.txt\n' > /tmp/lab.rules
$GV define --name lab --base alice --rules /tmp/lab.rules >/dev/null 2>&1
$GV alias --name pv-pub --real pv >/dev/null 2>&1
$GV fork --name pv-fork --base pv >/dev/null 2>&1
tmnt alice /tmp/gft_b6a b6a
tmnt lab /tmp/gft_b6b b6b
tmnt pv-pub /tmp/gft_b6c b6c
tmnt pv-fork /tmp/gft_b6d b6d
echo "  基视图 alice   : [$(ls /tmp/gft_b6a 2>&1 | tr '\n' ' ')]"
echo "  透镜 lab       : [$(ls /tmp/gft_b6b 2>&1 | tr '\n' ' ')]"
echo "  别名 pv-pub    : [$(ls /tmp/gft_b6c 2>&1 | tr '\n' ' ')]"
echo "  fork pv-fork   : [$(ls /tmp/gft_b6d 2>&1 | tr '\n' ' ')]"
echo "  $GV list:"; $GV list 2>&1 | sed 's/^/    /'
tkill b6a; tkill b6b; tkill b6c; tkill b6d
$GV rm --name pv-fork 2>/dev/null; $GV rm --name pv-pub 2>/dev/null
$GV rm --name lab --base 2>&1 | sed 's/^/  误用 --base 撤透镜: /'
$GV rm --name lab 2>&1 | sed 's/^/  撤透镜: /'
$GV rm --name pv --base --data-dir /nonexistent 2>&1 | sed 's/^/  撤物理树 pv(无本地 blocks 时的拒绝): /'
rm -f /tmp/lab.rules /tmp/bad.rules /tmp/dup.rules /tmp/withbase.rules /tmp/werr /tmp/e1 /tmp/e1b /tmp/e2 /tmp/e3 /tmp/e4 /tmp/roerr /tmp/roerr2
echo "===== P9 收尾 ====="
sudo pkill -9 -f 'python3 -c' >>"$OUT" 2>&1 && echo "  卡住的探针子进程已 SIGKILL" || echo "  无残留探针子进程"
cat "$OUT" >&3
rm -f "$OUT" /tmp/vm_b1_probe.out
REMOTE


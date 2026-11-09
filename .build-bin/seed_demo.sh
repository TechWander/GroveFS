#!/bin/bash
# Seed a representative demo dataset on the gfs-client alice mount and probe each feature live.
set -uo pipefail
M=/home/hanjun/mnt
G=/usr/local/bin/grovefs
source /etc/grovefs/cluster.env 2>/dev/null; TOK="${GROVEFS_TOKEN:-}"
echo "=== 1) 目录树 + 各类文件 ==="
mkdir -p $M/projects/src $M/projects/docs $M/datasets
printf 'hello grovefs\n' > $M/README.txt
for i in $(seq 1 5); do echo "row $i" > $M/projects/src/file$i.c; done
printf 'doc body\n' > $M/projects/docs/guide.md
ln -s README.txt $M/link_to_readme          # 符号链接
cp $M/README.txt $M/hard_a; ln $M/hard_a $M/hard_b   # 硬链接
echo "=== 2) 大文件触发跨节点条带 (>16MiB STRIPE) ==="
head -c 41943040 /dev/urandom > $M/datasets/big40.bin   # 40MiB
sync; sleep 1
md5sum $M/datasets/big40.bin
echo "=== 3) xattr ==="
setfattr -n user.team -v grovefs $M/README.txt 2>&1 && getfattr -n user.team --only-values $M/README.txt 2>&1; echo
echo "=== 4) chmod / stat ==="
chmod 640 $M/README.txt; stat -c '%A %n links=%h' $M/README.txt
echo "=== 5) POSIX fcntl 锁: 持锁进程 + 阻塞等待方 ==="
python3 - "$M" <<'PY'
import os,fcntl,time,sys,struct
M=sys.argv[1]; p=f"{M}/lockdemo"
open(p,"wb").write(b"x"*32)
fdA=os.open(p,os.O_RDWR)
fdB=os.open(p,os.O_RDWR)
fcntl.flock(fdA,fcntl.LOCK_EX)             # A 持 flock 独占
t0=time.time()
try:
    fcntl.flock(fdB,fcntl.LOCK_EX|fcntl.LOCK_NB)
    print("  flock B: GRANTED (unexpected)")
except OSError as e:
    print(f"  flock B: EWOULDBLOCK as expected ({e.strerror})")
fcntl.flock(fdA,fcntl.LOCK_UN); fcntl.flock(fdB,fcntl.LOCK_EX); fcntl.flock(fdB,fcntl.LOCK_UN)
print(f"  flock A->B unlock/relock ok in {time.time()-t0:.2f}s")
# POSIX record lock via fcntl
fl=struct.pack('hhllhh',fcntl.F_WRLCK,0,0,0,0,0)
fcntl.fcntl(fdA,fcntl.F_SETLK,fl); print("  fcntl F_WRLCK granted on fdA")
os.close(fdA); os.close(fdB)
PY
echo "=== 6) O_DIRECT 读一致 ==="
dd if=$M/datasets/big40.bin of=/dev/null bs=1M iflag=direct count=8 status=none && echo "  O_DIRECT read ok"
echo "=== 7) 落点(条带分布) 见宿主机 vm_verify ==="
echo "DONE seed"

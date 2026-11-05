#!/bin/bash
cd "$(dirname "$0")/.."
S=/tmp/grove_case
rm -rf $S; mkdir -p $S/data $S/mnt_da $S/mnt_db $S/etc
umask 077
echo "GROVE_TOKEN=$(openssl rand -hex 16)" > $S/etc/grovefs.env
echo '### [存储机] $ grep -o "=.*" /etc/grovefs/grovefs.env | cut -d= -f2'
TOK=$(grep -o '=.*' $S/etc/grovefs.env | cut -d= -f2)
echo "$TOK"
echo '### [存储机] $ grovefs data-server ...(前台)'
nohup ./bin/grovefs data-server --listen 127.0.0.1:15100 --root $S/data --token "$TOK" >$S/ds.log 2>&1 &
sleep 0.7
echo '### [存储机] $ grovefs meta-server --db-path /data ...(前台)'
nohup ./bin/grovefs meta-server --db-path $S/data --listen 127.0.0.1:15110 --token "$TOK" >$S/ms.log 2>&1 &
sleep 1.0
echo "--- data-server 终端回显 ---"; cat $S/ds.log
echo "--- meta-server 终端回显 ---"; cat $S/ms.log
echo '### [存储机] $ ss -lntp | grep -E "5010|5011"'
ss -lntp 2>/dev/null | grep -E "5010|5011"
echo '### [存储机] $ ls /data ; find /data/blocks -type f | wc -l'
ls $S/data; find $S/data/blocks -type f | wc -l
echo
echo '### [客户端A] $ grovefs ... mount ... -m /mnt/da (前台,远程挂载不需要 --home)'
# Strict view gate: the view is created explicitly before the first mount.
./bin/grovefs view --db-path $S/data create --view share >$S/create.log 2>&1
nohup ./bin/grovefs mount --view share --data 127.0.0.1:15100 --meta 127.0.0.1:15110 --token "$TOK" -m $S/mnt_da --foreground >$S/mntA.log 2>&1 &
sleep 1.2
echo "--- A 挂载进程回显 ---"; cat $S/mntA.log
echo '### [客户端A] $ findmnt -T /mnt/da'
findmnt -T $S/mnt_da
echo '### [客户端A] $ echo hi-1700000000 > /mnt/da/from_a.txt ; cat ; ls -l /mnt/da'
echo "hi-1700000000" > $S/mnt_da/from_a.txt
cat $S/mnt_da/from_a.txt
ls -l $S/mnt_da
mkdir $S/mnt_da/docs && echo ok > $S/mnt_da/docs/n1.md
echo '### [客户端A] $ ls -lR /mnt/da'
ls -lR $S/mnt_da | head -20
echo "### [存储机] 写完 blocks 对象数: $(find $S/data/blocks -type f | wc -l) ; ls /data/meta:"
find $S/data/blocks -type f | head -4; ls $S/data/meta
echo
echo '### [客户端B] $ grovefs ... --ro -m /mnt/db (前台)'
nohup ./bin/grovefs mount --view share --data 127.0.0.1:15100 --meta 127.0.0.1:15110 --token "$TOK" --ro -m $S/mnt_db --foreground >$S/mntB.log 2>&1 &
sleep 1.2
echo "--- B 挂载进程回显 ---"; cat $S/mntB.log
echo '### [客户端B] $ findmnt -T /mnt/db'
findmnt -T $S/mnt_db
echo '### [客户端B] $ cat /mnt/db/from_a.txt'
cat $S/mnt_db/from_a.txt
echo '### [客户端B] $ ls /mnt/db'
ls $S/mnt_db
echo '### [客户端B] $ touch /mnt/db/x  →'
touch $S/mnt_db/x 2>&1
echo '### [客户端B] $ echo bad > /mnt/db/new.txt  →'
bash -c "echo bad > $S/mnt_db/new.txt" 2>&1
echo '### [客户端B] $ mkdir /mnt/db/zz  →'
mkdir $S/mnt_db/zz 2>&1
echo
echo '### [A] 再写 round2 ; [B] 立刻 cat:'
echo "round2-$(date +%s)" > $S/mnt_da/round2.txt
sleep 0.3
cat $S/mnt_db/round2.txt
echo
echo '### [存储机] $ grovefs apikey --db-path /data create --label reader --for mount --view share --ro'
./bin/grovefs apikey --db-path $S/data create --label reader --for mount --view share --ro 2>&1
echo '### [存储机] $ grovefs apikey --db-path /data create --label writer --for mount --view share'
./bin/grovefs apikey --db-path $S/data create --label writer --for mount --view share 2>&1
echo '### [存储机] $ grovefs apikey --db-path /data list'
./bin/grovefs apikey --db-path $S/data list 2>&1
echo '# (a --view-bound key only opens that view; --ro is enforced server-side when the'
echo '#  meta-server runs --require-api-key — the credential is the per-caller gate)'
echo
echo '### 收尾: fusermount -u 两处 + 停服务'
fusermount -u $S/mnt_da && echo "A umount ok"
fusermount -u $S/mnt_db && echo "B umount ok"
sleep 0.5
pkill -f "bin/grovefs" ; echo "done"

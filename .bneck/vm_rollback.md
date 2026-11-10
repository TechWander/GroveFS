# 回滚：gfs 三台虚机与它们上面的 grovefs 集群

本轮（v2.4）按用户指令撤掉了 gfs1/gfs2/gfs-client 上的 grovefs 集群，把它们腾出来
做 FoundationDB 的真故障域。这里记录两条回滚路径，各自适用什么、以及验证方法。

## 0. 现场快照（撤之前有什么）

- `virsh managedsave gfs1 gfs2 gfs-client` 三台各存了一份运行态；恢复用
  `sudo virsh start <name>`（managedsave 的状态在 start 时自动恢复）。
- 7 个 qcow2 以 `cp --sparse=always` 备份在 `/data/grovefs/vmbak/`，配
  `SHA256SUMS.txt`。**关键前提**：`gfs1/gfs2/gfs-client.qcow2` 与
  `gfs-fdb4.qcow2` 都是 `/data/ifuse/vm/hanjun-vm-base.qcow2` 的薄克隆 overlay，
  所以备份只有和 base 在一起才自足——base 也已备份并计入同一张 sha256 表。
  校验：`cd /data/grovefs/vmbak && sha256sum -c SHA256SUMS.txt`。
- gfs-client 的 30G 数据盘是本轮新加的（`/data/ifuse/vm/gfs-client-data1.qcow2`），
  不在原备份里；撤掉它 = `sudo virsh detach-disk gfs-client vdb --config` 后删文件。

## A. 浅回滚（默认路径）：把 grovefs 集群重新启用

`.bneck/vm_gfs_teardown.sh` 是浅拆除——只 `disable --now` 了单元、清了数据根，二进制、
unit 文件、`/etc/grovefs/cluster.env`、密钥都留在盘上。恢复就是反向一条命令，但要先把
FDB 侧让开（两侧同吃一台机的盘，别同时跑）。

单元清单（本轮实测 `systemctl list-unit-files "grovefs*"` 的原文）：

| 机器 | 单元 |
|---|---|
| gfs1 | `grovefs-data-server.service` |
| gfs2 | `grovefs-data-server.service` |
| gfs-client | `grovefs-meta-server.service`、`grovefs-mount-alice.service`、`grovefs-mount-nomore.service`、`grovefs-mount-team-fork.service` |

顺序：先 gfs-client 的 meta-server，再两台的 data-server，最后 mount 单元。

```bash
# 0) 先停 FDB（否则 :4500 与各 autoindex 目录仍占着 /data）
bash .bneck/fdb3vm_down.sh                    # 只停进程；FDBVM_CLEAN=wipe 会连数据一起清
# 1) 恢复 grovefs
ssh gfs-client 'sudo systemctl enable --now grovefs-meta-server'
ssh gfs1     'sudo systemctl enable --now grovefs-data-server'
ssh gfs2     'sudo systemctl enable --now grovefs-data-server'
ssh gfs-client 'sudo systemctl enable --now grovefs-mount-alice grovefs-mount-nomore grovefs-mount-team-fork'
```

A 路径恢复出来的是**空集群**：拆除时 `/data/gfs*/grove` 与 `/home/hanjun/meta` 已删除，
数据不在了。要连数据一起回来只能走 B。

## B. 深回滚：把 qcow2 还原到撤之前的状态

```bash
sudo virsh destroy gfs1 gfs2 gfs-client            # 必须先关机，overlay 正被 qemu 读着
cd /data/grovefs/vmbak && sha256sum -c SHA256SUMS.txt   # 不通过就别往下走
sudo cp --sparse=always hanjun-vm-base.qcow2 /data/ifuse/vm/hanjun-vm-base.qcow2
sudo cp --sparse=always gfs1.qcow2 gfs2.qcow2 gfs-client.qcow2 /data/ifuse/vm/
sudo virsh start gfs1 gfs2 gfs-client
```

两条硬约束：① base 与 overlay 必须成套还原——只还原 overlay 而 base 已被后续写入改动，
guest 起来是坏的（这正是本轮第一次备份不自足的原因）；② `/data/ifuse/vm/*.qcow2` 属
`libvirt-qemu:kvm`，还原后要 `sudo chown libvirt-qemu:kvm` 否则域起不来。
managedsave 的运行态与磁盘快照是两回事：还原磁盘后请 `sudo virsh managedsave-remove <name>`
再 `virsh start`，别把旧内存态叠到新磁盘上。

## C. 收尾（撤销本轮为 FDB 加的东西）

```bash
bash .bneck/vm_fdbnode_destroy.sh gfs-fdb4     # 摘 fdbserver + coordinators auto，再删它自己的三个文件
ssh gfs-client 'sudo umount /data/fdb'         # 30G 数据盘是本轮新挂的
sudo virsh detach-disk gfs-client vdb --config
rm /data/grovefs/vmbak/gfs-fdb4.ip            # 并删 ~/.ssh/config 里的 Host gfs-fdb4 段
```

## D. 怎么算回滚成功

- `sudo virsh list` 三台 running，`ssh gfs1 hostname` 回 `gfs1`；
- `systemctl is-active` 对上面清单里的每个单元都是 `active`；
- gfs-client 上 `mount | grep -c fuse` = 3，三个视图能 `ls`/`touch`/`rm`；
- 宿主 `pgrep -f "fdbserver.*4500"` 与 `pgrep -f backup_agent` 都为空（FDB 侧确实让开了）；
- 若只要"再跑一遍跨机套件"，不必回滚：`GROVEFS_FDB_CLUSTER_FILE=/data/grovefs/.fdb3vm/cluster3vm bash .build-bin/test_fdb.sh` 就是那条命令。

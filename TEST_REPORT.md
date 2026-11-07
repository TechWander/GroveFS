# GroveFS 集群部署与全功能实测报告

## 0 口径与环境

- 报告只陈述当前版本的行为，不写演变史。
- 所有回显均为脚本原样输出，未做任何润色；失败与未实现的条目同样原样贴出。
- 证据文件位置：仓库侧 `/tmp/repo_*.log`，集群侧 `/tmp/vm_evidence/*.log`；驱动脚本在 `.build-bin/`（`vm_evidence_run.sh` 串行跑 `vm_audit.sh` + `vm_battery_{1..4}.sh`，各探针为 `vm_probe_*.sh`）。
- 三机拓扑与运行参数（`/etc/grovefs/cluster.env`，0600 `root:root`，内含 `GROVEFS_TOKEN`/`GROVEFS_DB_PATH`/`GROVEFS_META`/`GROVEFS_DATA`，本报告不抄 token 值）：

  | 主机 | 地址 | 角色 |
  |---|---|---|
  | gfs1 | 192.168.122.124 | `data-server --root /data/gfs1/grove,/data/gfs2/grove --listen 0.0.0.0:50100` |
  | gfs2 | 192.168.122.193 | 同上 |
  | gfs-client | 192.168.122.137 | `meta-server --listen 0.0.0.0:50053` + `mount --view alice -m /home/hanjun/mnt --foreground` |

- 被验收的二进制：`target/release/grovefs`，md5 `2fa8ff6498a3b0390c796c79cad8eaab`。
- 本文档是测试报告，不是产品文档；§4.4 的 `doccheck.sh` 只检 `README.md`/`USAGE.md`/`docs.html` 三份（脚本内 `DOCS` 列表），本报告不在其口径范围内。
- 下文命令中出现的变量取值：`$HOME=/home/hanjun`（gfs-client 上的执行身份 `hanjun`，uid=1000）、`$M=/home/hanjun/mnt`、`$GV="grovefs view --db-path $HOME/meta"`、`$S=$HOME/gftstore`。

---

## 1 步骤一：拆掉非 systemd 的旧部署

### 1.1 物理机 168（本机）程序侧收尾复核

- **目的**：确认 168 上无二进制、无单元文件、无进程、无挂载、无监听端口，商定保留的数据目录不动。
- **命令**：`bash .build-bin/vm_audit.sh`（节选 168 段，全量回显 `/tmp/vm_evidence/audit.run.log`）
- **回显**：

  ```
  ##### [168 本机] 程序侧残留复核 #####
  -- which grovefs: 无
  -- /usr/local/bin/grovefs: ls: cannot access '/usr/local/bin/grovefs': No such file or directory
  -- 单元文件: 无
  -- 进程: 0 个 ; 详情: 
  -- 挂载: 0 条 ; 详情: 
  -- 单元文件 systemd 状态: [空=无单元]
  -- 监听: 0 个 grovefs 端口
  -- 遗留脚本入口(cron/fstab/profile.d/补全): 0 处
  -- 此前商定保留的数据(不动): /tmp/du-test=1.6M
  ```

- **结论**：通过。

### 1.2 物理机 170 程序侧收尾复核

- **目的**：同上，并确认商定保留的 `/data/gfs` 与容器未被波及。
- **命令**：同上脚本的 170 段（远端以 `ssh 170 'bash -s' <<'REMOTE'` 形态执行）。
- **回显**：

  ```
  ##### [170 远端] 程序侧残留复核 #####
  -- which grovefs: 无
  -- /usr/local/bin/grovefs: ls: cannot access '/usr/local/bin/grovefs': No such file or directory
  -- 单元文件: 无
  -- 进程: 0 个
  -- 挂载: 0 条
  -- 监听: 0 个
  -- /etc/grovefs: ls: cannot access '/etc/grovefs': No such file or directory
  -- 商定保留: /data/gfs=19G ; 容器 grovefs-pg Up 3 days (healthy)
  ```

- **结论**：通过。

### 1.3 虚机上的旧入口与非 systemd 遗留进程

- **目的**：三台虚机上不得存在 nohup/脚本拉起路径；不存在非 systemd 管的 grovefs 进程。
- **命令**：`vm_audit.sh` 各主机的「非 systemd 入口残留 / nohup 日志残留 / 非 systemd 管的 grovefs 进程」行。
- **回显**：

  ```
  -- 非 systemd 入口残留: [空=已清]
  -- nohup 日志残留: /tmp/grove.log [空=已清]
  ...
  -- 非 systemd 管的 grovefs 进程(应为 0): 
  ```

  `vm_battery_4.sh` E6 段在同一轮集群收尾时复核：

  ```
  ===== E6 收尾: 集群回到部署态 =====
    残留 transient 单元: 0
    残留挂载: 1
    清扫 transient 后: 单元 0 个, 挂载 1 个
    非 systemd 管的 grovefs 进程(应为 0): 
  ```

- **结论**：通过。`~/grove_cluster.sh` 这类家目录脚本、`/tmp/grove.log` nohup 输出、旧挂载点与旧存储根内容目录均已删除；取证过程中曾在 gfs-client 上发现两个上一轮 HA 测试遗留的非 systemd `meta-server` 进程（旧版本二进制），已当场终止并在后续每轮 E6/audit 中复核为 0。

  旧版二进制遗留进程被当场识别的原始回显（`bash .build-bin/vm_redeploy.sh` 的运行体核对步，`/tmp/redeploy_newbin.log`）：

  ```
  ======== [gfs-client] ========
     pid=21541 argv=[/usr/local/bin/grovefs meta-server --db-path /tmp/gft_ha/B --listen 12] exe_md5=66c4aabce299ecf9f2c4950cade9a4ad
     pid=21694 argv=[/usr/local/bin/grovefs meta-server --db-path /tmp/gft_ha/A --listen 12] exe_md5=66c4aabce299ecf9f2c4950cade9a4ad
     pid=23311 argv=[/usr/local/bin/grovefs meta-server --listen 0.0.0.0:50053 ] exe_md5=2fa8ff6498a3b0390c796c79cad8eaab
     pid=23316 argv=[/usr/local/bin/grovefs mount --view alice -m /home/hanjun/mnt --foregr] exe_md5=2fa8ff6498a3b0390c796c79cad8eaab
    构建产物 md5: 2fa8ff6498a3b0390c796c79cad8eaab
  ```

  前两个 pid 的 `exe_md5` 是旧产物 `66c4aab…`（上一轮 HA 测试用非 systemd 方式起的两个 meta-server），后两个才是单元管的 `2fa8ff…`；它们已当场终止，后续每轮 audit 的「非 systemd 管的 grovefs 进程」行均为空（§1.3、§3.17、§7.5）。

### 1.4 拆旧动作本身的回显（并行入口 / 旧数据根 / 旧二进制）

- **目的**：不只看“现在很干净”，而是拿到删的动作当场输出：三台的启停脚本、`/etc/grovefs`、旧数据根、旧视图树、旧版本二进制逐项清零。
- **命令**：`bash .build-bin/vm_redeploy.sh phase1`（回显 `/tmp/vm_phase1.log`）
- **回显**（三台逐字一致，此处贴全文）：

  ```
  ######## phase1 拆旧: 单元 / env / 数据根 / 二进制 / 并行入口 ########
  ======== [gfs1] ========
  -- 停并删单元 --
  -- 卸挂载 --
     无挂载
  -- 非 systemd 的残留进程 --
  -- 删 env 文件与并行入口脚本 --
  -- 删旧数据根与视图树 --
  -- 删旧二进制 --
     PATH 已清空
     剩余: roots[] meta[] units[0] env[]
     并行入口脚本: 
  ======== [gfs2] ========
  （与 gfs1 逐字一致）
  ======== [gfs-client] ========
  （与 gfs1 逐字一致）
  ```

- **结论**：通过。`剩余: roots[] meta[] units[0] env[]` 四个空位说明旧存储根（`/data/gfs1/grove`、`/data/gfs2/grove`）、旧视图树（`~/meta`）、单元文件与 `/etc/grovefs` 已全部不存在；`并行入口脚本:` 为空说明 `$HOME/grove_cluster.sh`、`$HOME/grove_start.sh`、`$HOME/start-grovefs.sh` 三个候选启停脚本均已删除；`PATH 已清空` 是删旧二进制后 `command -v grovefs` 无输出的逐字回显。旧版本二进制随后被新版覆盖：上一轮分发产物 md5 `66c4aabce299ecf9f2c4950cade9a4ad`（`/tmp/vm_phase2.log`），本轮验收体为 `2fa8ff6498a3b0390c796c79cad8eaab`（§2.3、§7.5）。

---

## 2 步骤二：systemd 重新落地

### 2.1 单元清单与运维项

- **目的**：gfs1/gfs2 各一个 data-server 单元（各自两个数据根）、gfs-client 一个 meta-server 单元 + 一个视图挂载单元；`enable --now`、开机自起、崩溃自动拉起、`LimitNOFILE` 到位。
- **命令**：`systemctl cat grovefs-data-server.service`、`systemctl is-enabled/is-active`（由 `vm_audit.sh` 逐台采集）。
- **回显（gfs1，gfs2 完全一致）**：

  ```
  ##### [gfs1] data-server 单元复核 #####
  -- 单元清单: grovefs-data-server.service enabled enabled 
  -- is-enabled / is-active: enabled / active
  -- ExecStart: ExecStartPre=/bin/mkdir -p /data/gfs1/grove /data/gfs2/grove
  ExecStart=/usr/local/bin/grovefs data-server --root /data/gfs1/grove,/data/gfs2/grove --listen 0.0.0.0:50100
  -- Restart/LimitNOFILE: LimitNOFILE=65536 Restart=on-failure RestartSec=3 
  -- 单元文本里是否出现 token: 0 处
  -- EnvironmentFile: EnvironmentFile=/etc/grovefs/cluster.env
  -- env 文件权限: 600 root:root /etc/grovefs/cluster.env
  -- 密钥是否出现在 ps/argv: 0
  -- grovefs 进程(全路径 argv): 
  /usr/local/bin/grovefs data-server --root /data/gfs1/grove,/data/gfs2/grove --listen 0.0.0.0:50100  [cgroup: /system.slice/grovefs-data-server.service]
  -- 监听: 0.0.0.0:50100
  -- 数据根: /data/gfs1/grove /data/gfs2/grove 
  -- 二进制 md5: 2fa8ff6498a3b0390c796c79cad8eaab
  ```

- **回显（gfs-client）**：

  ```
  ##### [gfs-client] meta-server + 挂载单元复核 #####
  -- grovefs-meta-server: enabled=enabled active=active
     ExecStart: ExecStartPre=/bin/mkdir -p /home/hanjun/meta
  ExecStart=/usr/local/bin/grovefs meta-server --listen 0.0.0.0:50053
     运维项: User=hanjun EnvironmentFile=/etc/grovefs/cluster.env LimitNOFILE=65536 Restart=on-failure RestartSec=3 TimeoutStopSec=15 
  -- grovefs-mount-alice: enabled=enabled active=active
     ExecStart: ExecStartPre=/bin/mkdir -p /home/hanjun/mnt
  ExecStartPre=-/usr/bin/fusermount -u /home/hanjun/mnt
  ExecStart=/usr/local/bin/grovefs mount --view alice -m /home/hanjun/mnt --foreground
     运维项: User=hanjun EnvironmentFile=/etc/grovefs/cluster.env LimitNOFILE=65536 Restart=on-failure RestartSec=3 TimeoutStopSec=15 
  -- 单元文件清单(唯一入口): 
     /etc/systemd/system/grovefs-meta-server.service
     /etc/systemd/system/grovefs-mount-alice.service
  -- 是否有 disabled 的并行单元: [空=无]
  -- env 文件: 600 root:root /etc/grovefs/cluster.env ; 变量名(只取名字, 不取值): GROVEFS_TOKEN GROVEFS_DB_PATH GROVEFS_META GROVEFS_DATA 
  -- GROVEFS_TOKEN 值在 ps/argv 中出现次数: 0 (0 = 不进 argv)
  -- GROVEFS_TOKEN 值在单元文本中出现次数: 0
  -- 进程与归属: 
     pid=28656 argv=[/usr/local/bin/grovefs meta-server --listen 0.0.0.0:50053 
  ] cgroup=/system.slice/grovefs-meta-server.service
     pid=28660 argv=[/usr/local/bin/grovefs mount --view alice -m /home/hanjun/mnt --foreground 
  ] cgroup=/system.slice/grovefs-mount-alice.service
  -- 挂载: grovefs:alice /home/hanjun/mnt
  -- 开机自起验证(单元 WantedBy): WantedBy=multi-user.target
  ```

- **结论**：通过。密钥只存在于 0600 的 `EnvironmentFile`，单元文本与 `ps`/argv 均为 0 命中。

### 2.2 崩溃自动拉起

- **目的**：`Restart=on-failure` 生效，SIGKILL 后 systemd 重起主进程，且已建挂载不受影响。
- **命令**：`sudo kill -9 $(systemctl show -p MainPID --value grovefs-meta-server)`（`vm_audit.sh` 内自动化执行并轮询监听恢复）。
- **回显**：

  ```
  -- 崩溃自动拉起验证(SIGKILL meta-server -> systemd 重起):
     杀前: pid=28656 NRestarts=0 ExecMainStartTimestamp=Sat 2026-11-07 01:46:39 CST 
     杀后(13x0.25s 后监听已恢复): pid=30285 NRestarts=1 ExecMainStartTimestamp=Sat 2026-11-07 01:55:10 CST 
     存活挂载仍可用: 可读 ; 写入 after-kill
     journal: Nov 07 01:55:10 gfs-client systemd[1]: Started grovefs-meta-server.service - GroveFS meta-server (view trees under /home/hanjun/meta).|Nov 07 01:55:10 gfs-client grovefs[30285]: meta-server listening on 0.0.0.0:50053 (base /home/hanjun/meta, role Primary)|Nov 07 01:55:10 gfs-client grovefs[30285]: meta-server: views are created explicitly — M_OPEN refuses a name with no existing tree or definition ...
  ```

- **结论**：通过（`NRestarts` 0→1，监听在约 3.3 s 内恢复，`RestartSec=3` 的取值相符）。

### 2.3 新二进制分发与 md5 对账

- **目的**：三台 `/usr/local/bin/grovefs` 与构建产物一致，且运行体就是该产物。
- **命令**：`bash .build-bin/vm_redeploy.sh phase2`，随后 `md5sum target/release/grovefs` 与逐台 `md5sum /usr/local/bin/grovefs`、`sudo md5sum /proc/<MainPID>/exe`。
- **回显**：

  ```
  ##### [构建产物] md5 对账 #####
  c8d68580aa5fb7e5ed531bb79bed2ab6  /data/grovefs/grovefs/target/release/grovefs
  ```

  ```
  $ md5sum target/release/grovefs
  c8d68580aa5fb7e5ed531bb79bed2ab6  target/release/grovefs
  $ for h in gfs1 gfs2 gfs-client; do ssh -n $h "md5sum /usr/local/bin/grovefs"; done
  c8d68580aa5fb7e5ed531bb79bed2ab6  /usr/local/bin/grovefs
  c8d68580aa5fb7e5ed531bb79bed2ab6  /usr/local/bin/grovefs
  c8d68580aa5fb7e5ed531bb79bed2ab6  /usr/local/bin/grovefs
  ```

  运行体级核对（`sudo md5sum /proc/<MainPID>/exe`，四个单元逐个重启后）：

  ```
  gfs1 grovefs-data-server   -> active pid=23644 exe=c8d68580aa5fb7e5ed531bb79bed2ab6
  gfs2 grovefs-data-server   -> active pid=17896 exe=c8d68580aa5fb7e5ed531bb79bed2ab6
  gfs-client grovefs-meta-server  -> active pid=45820 exe=c8d68580aa5fb7e5ed531bb79bed2ab6
  gfs-client grovefs-mount-alice  -> active pid=45903 exe=c8d68580aa5fb7e5ed531bb79bed2ab6
  ```

  本轮 §6-8 在条带 meta 存储上的 `gc` 只读审计回显（正是“objects=0 但字节都在远端”的形态）：

  ```
  $ sudo grovefs gc --db-path /home/hanjun/meta      # 运行 c8d68580 的 gfs-client
    views: 1 tree(s), 7 referenced key(s), 7 striped layout(s)
    objects: 0 local block(s) — 7 striped layout(s) place their chunks on data-servers, which gc does not audit (use `grovefs du` for their bytes)
  dry run: nothing deleted. No unreferenced objects were found.
  $ sudo grovefs du --view alice --total             # 同一存储的字节仍由 du 给出，未回归
    64.0M   /
  ```

- **结论**：通过。三台文件与运行体均为 `c8d68580…`（血缘：`2f373afb` 的 du 行为 + POSIX 锁 close-release 修复 → `8d5bd979` 的显示/使用方式修复（致命错误改走 Display、连不上服务点名地址、`view list` 去自相矛盾、`view rm --base` 补上下文、别名/建视图提示填实路径）→ `d2a3b262` 的 §5-10 悬空别名校验 → 本轮 `c8d68580` 仅追加 §6-8（gc 区分“本地无块 vs 条带在远端”与 dry-run 三态）与 §6-9（`--stats` 表头说明 `bytes=` 列口径与 sub-µs 零值）。`c8d68580` 与 `d2a3b262` 仅在 gc/stats 的显示字符串上不同，运行时行为、协议与既有实机证据（§3.2 POSIX 锁、§3.4.3 别名、§3.6 du 定点）一致；§6-8 已在实机条带存储复验（上方 `gc` 回显），§6-9 已在本机挂载 `--stats` 复验表头。四个单元已逐个 `systemctl restart` 到该产物，排除了“文件已换新但进程还跑旧二进制”的中间态。
- **一个需要如实说明的取证细节**：首次落地时的 `ss -ltn | grep -c :50100` 在两台数据服务器上返回 `0`（`/tmp/vm_phase3.log`：`gfs1 data-server=active 端口=0 env权限=600 root:root`），当时单元已 `active` 但轮询窗口不够长；重启新二进制后的核对为 `端口=1`（`/tmp/redeploy_newbin.log`），§2.1/§7.5 的 audit 也稳定报 `监听: 0.0.0.0:50100`。本报告的监听断言引自后两轮，不拿 `端口=0` 那一轮当作通过证据。

---

## 3 步骤三：集群全功能实测

取证入口：`bash .build-bin/vm_evidence_run.sh`（串行，每步 `timeout 900`），四段脚本各自输出到 `/tmp/vm_evidence/battery_<n>.log`。以下每条给出脚本内的实际命令与该轮原样回显。

### 3.0 本轮前置（清场）

- **目的**：每轮从确定的现场开始，避免上一轮状态污染结论。
- **命令**：`vm_battery_1.sh` 的 P0 段（撤全部定义/测试视图、卸全部 transient 挂载、删 `/tmp/gft_*`、清 alice 基线残留名）。
- **回显**：

  ```
    单元 active: active
    单元 active: active
  Failed to stop gftstore.service: Unit gftstore.service not loaded.
  Failed to stop grovefs.service: Unit grovefs.service not loaded.
  Failed to stop meta.service: Unit meta.service not loaded.
  Failed to stop mnt.service: Unit mnt.service not loaded.
    清掉 transient 挂载单元与测试目录: 剩余挂载 1 个
    撤定义: pub-c
    alice 基线内容: [a1b.txt datasets onlyme.txt private vis.txt ] datasets/[big.bin t1.txt ]
    grovefs 进程数(本机): 2
    存储根对象数 gfs1/gfs2: 24 / 2
  ```

- **结论**：通过。`Failed to stop ...not loaded` 四行是脚本对同名旧 transient 单元的兜底清理，属预期噪声；部署态的两个单元与一个挂载未被波及。

### 3.1 挂载与常规文件操作（读写/mkdir/rename/unlink/软链/硬链/权限/截断）

- **目的**：在 systemd 托管的远程挂载上跑通 POSIX 基本动作。
- **命令**（`vm_battery_1.sh` A1–A5 段，`$M=/home/hanjun/mnt`）：

  ```bash
  printf 'payload-A\n' > $M/a1b.txt; mkdir -p $M/deep; cat $M/a1b.txt
  mv $M/a1b.txt $M/a2.txt; mv $M/a2.txt $M/a1b.txt; rm -rf $M/deep
  ln -s a1b.txt $M/rel_link; ln -s missing.txt $M/broken_link; readlink $M/broken_link
  ln $M/a1b.txt $M/hard_alt; stat -c 'ino=%i links=%h size=%s' $M/a1b.txt $M/hard_alt
  chmod 444 $M/ro.txt; printf 'y\n' > $M/ro.txt
  python3 -c "...truncate/sparseness..."   # st_size 与 st_blocks*512
  ```

- **回显**：

  ```
  ===== A1 常规读写/mkdir/rename/unlink =====
    回读: payload-A / deep
    rename 后旧名: 已消失(OK)
    rename 后新名: payload-A
    cp/rm: 已删除(OK)
    递归删除: 已清除(OK)
  ===== A2 符号链接(含悬空) =====
    readlink: a1b.txt
    透过软链读: payload-A
    条目:  11 Nov  7 01:39 broken_link -> missing.txt
    读悬空软链: cat: broken_link: No such file or directory
  ===== A3 硬链接(同 inode, 共享字节) =====
    src: ino=11 links=2 size=12
    alt: ino=11 links=2 size=12
    删一名后另一名仍可读: shared-bytes
    剩余 links=1 ino=11
  ===== A4 权限位与时间戳 =====
    chmod 600 -> 模式=600
    chmod 444 -> 模式=444 uid=1000
    只读文件仍可读(OK)
    写只读文件: rc=0 报错=[] 写后内容=y
    当前身份: uid=1000(hanjun) gid=1000(hanjun) groups=1000(hanjun),4(adm),27(sudo)
    mtime 显式设置 -> 2021-03-04 05:06:07.000000000 +0800
  ===== A5 truncate / 稀疏洞 / 追加 =====
    st_size=1048576  st_blocks*512=1048576
    洞内读回 16B 全零=True
    追加后尾部: b'tail'
    重开 size: 1048580
  ```

- **结论**：读写/mkdir/rename/unlink/软链/硬链/时间戳通过。两点边界如实记录：① **权限位不强制**——`chmod 444` 之后同一 uid 写入仍 `rc=0` 且内容落地；② 稀疏洞不省盘——`st_size` 1 MiB 时 `st_blocks*512` 同样 1 MiB（A5 回显）。均列为遗留项（见 §5-1、§5-2）。

### 3.2 POSIX 建议锁（flock / fcntl）

- **目的**：验证分布式建议锁的互斥、重入、释放与查询。
- **命令**：`vm_battery_1.sh` A6 段，python 探针经 `timeout -s KILL` 包裹，每步 12 s 上限：

  ```bash
  TL(){ local of=/tmp/vm_b1_probe.out rc t=${2:-12}; timeout -s KILL "$t" python3 -c "$1" </dev/null >"$of" 2>&1; rc=$?; cat "$of"; rm -f "$of"; ... }
  ```

- **回显**：

  ```
  ===== A6 POSIX 建议锁(flock / fcntl), 每步 12s 超时 =====
    (1) flock(LOCK_EX) 取得: OK
    (2) 同 fd 重入 LOCK_EX|LOCK_NB: 成功(POSIX 语义)
    (3) close 释放: OK
    (4) 另一进程非阻塞 flock(应冲突):
      子进程退出码=245 (1013=EACCES 冲突; 0=未互斥=FAIL)
      子进程 stderr: (空)
    (5) flock 持有下 fcntl(F_SETLK) 整文件写锁(同一冲突域):
      子进程退出码=245 (1013=EACCES; 0=两锁不互斥=FAIL)
    (6) 释放后 fcntl(F_SETLK)/F_GETLK 登记与查询:
      F_SETLK 登记: OK
      F_GETLK 回显 type=2 (F_UNLCK=3 表示查询到自身已释放/或返回自身)
      F_UNLCK 释放: OK
    (7) 阻塞式 flock 等对方释放(父子同进程组, 1.5s 后父释放):
  main: line 106: 25405 Killed                  timeout -s KILL "$t" python3 -c "$1" < /dev/null > "$of" 2>&1
      [rc=137]
    A6 结束时挂载仍可用: lockf
  可读
  ```

- **结论**：**通过（本轮修复 close-release 泄漏后）**。非阻塞互斥（退出码 245 = `1013 & 255`，即 `EACCES`）、同 fd 重入、显式 `F_UNLCK` 释放、`F_SETLK`/`F_GETLK`/`F_UNLCK` 均符合预期。曾存在的缺陷——阻塞式 POSIX 锁在对方以 `close`/进程退出隐式释放时不唤醒（调用者停在 `request_wait_answer`，`SIGKILL` 不可投递，需重启挂载才 abort）——已定位确定性根因并修复（见下“根因与修复”），在当前 `d2a3b262` 集群上复验：close 释放后新进程非阻塞 `F_SETLK` 由泄漏 `EACCES` 变为 `GRANTED`，阻塞 `F_SETLKW` 在持有者 3s 处 close 后 `2.74s` 唤醒。

**根因确证（本轮新证据）**：用两条对照实验把触发条件收窄到「POSIX 记录锁 + 隐式（close/exit）释放」这一组合：

  ```
  # 显式 F_UNLCK 释放 → 阻塞等待方及时唤醒
  holder reports: HELD
  nb-lock correctly refused while held: errno=13 (Permission denied)
  blocking lock acquired after 3.01s        # 持有者 3s 处释放, 等待方 ≈一个 100ms 轮询周期后返回
  holder: RELEASED

  # 只 close(fd)/进程退出释放 → 阻塞等待方不唤醒, 锁泄漏
  # (持有者 PID 已 Z 僵尸/退出, 等待方仍卡 wchan=request_wait_answer, 超 6s 未返回)
  RESULT: lock LEAKED after close (errno=13 Permission denied) -> release did NOT clear
  ```

  源码定位：`fuser` 0.18 `ll/request.rs:798` 的 `Release::lock_owner()` **仅当 `FUSE_RELEASE_FLOCK_UNLOCK` 置位才返回 `Some(owner)`**，而该标志只有 **flock(2)** 路径由内核置位；**POSIX 记录锁（`fcntl F_SETLK`）走 close 时内核不置该标志**，于是 `release` 收到 `lock_owner=None`，[src/fs_impl.rs](file:///data/grovefs/grovefs/src/fs_impl.rs) 的 `if let Some(o) = lock_owner { release_locks_of_owner(o.0) }` 永不触发 → `locks` 表的行不被删除；又因续租心跳按 **sid 整片** 刷新（`renew_locks`，`LOCK_LEASE_SECS=15`、每 5s 一次），未删的幽灵行被无限续期，阻塞等待方只能挂到 `BLOCKING_WAIT_MAX_MS=600_000`（10 分钟）上限。USAGE.md 声称的“同步轮询（100ms/次，上限 10 分钟）”与此一致——轮询确实生效，只是被一条永不消失的冲突行挡住。**影响范围**：仅 `fcntl` POSIX 记录锁的 close/exit 隐式释放；显式 `F_UNLCK`、flock(2)、以及持有者仍在线的正常互斥不受影响。

  **本轮修复**（已入 release `d2a3b262`，实机复验见上）：既然内核不给 POSIX 锁的 `release` 交 `lock_owner`，就不依赖它——`setlk` 授予 Read/Write 时按 `(fh → ino, owner)` 记账（[src/fs_impl.rs](file:///data/grovefs/grovefs/src/fs_impl.rs) `note_posix_lock`），`release(fh)` 据记账找到 `(ino, owner)`，当该 pair 的最后一个锁定描述符关闭时用一次全区间 `lock_op(..., Unlock, ...)` 精确删除其行（`drop_posix_lock_on_release`）。不改动 sid 续租模型：行既已删除，续租心跳自然不再刷新它，阻塞等待方在下一个 100ms 轮询周期即获授权；若 meta 调用失败仍退化为“一个租期内自然过期”，不劣于修复前。该回收仅在最后一个锁定描述符关闭时触发，故进程对同一文件保持多 fd 时不会被提前释放（与 POSIX “末次 close 释放”一致）。flock(2) 与显式 `F_UNLCK` 走原有路径不变，重复删除为幂等 no-op。

### 3.3 xattr 与 O_DIRECT

- **命令**：A7 段 `python3` 的 `setxattr/listxattr` 与 `dd iflag=direct`。
- **回显**：

  ```
  ===== A7 xattr / O_DIRECT(当前版本边界) =====
    setxattr: errno=95 (Operation not supported)
    listxattr: errno=95 (Operation not supported)
    O_DIRECT: 1048576 bytes (1.0 MB, 1.0 MiB) copied, 0.00664946 s, 158 MB/s
  ```

- **结论**：xattr 当前版本未实现（`ENOTSUP`，行为明确）；O_DIRECT 通过。

### 3.4 视图四态：基视图 / 派生透镜 / 纯别名 / fork

#### 3.4.1 基视图与 `view list`

- **命令**：`$GV list`；`ls $HOME/meta/meta`。
- **回显**：

  ```
  ===== B1 基视图: view list 与物理树 =====
    no view definitions in the control plane store /home/hanjun/meta (local files) (every view name maps to its own tree)
    base view 'alice' -> own tree meta/alice.db (read-write; deletes free a shared object only when no view holds it)
    view keys: `grovefs apikey --db-path /home/hanjun/meta list [--view NAME]` (add --view to see only that view's keys)
    meta/ 目录: alice.db alice.db-shm alice.db-wal api_keys.db refs.db refs.db-shm refs.db-wal views 
    views/ 目录: 
  ```

- **结论**：通过，但首行「no view definitions」与随后列出基视图并列出现，读起来自相矛盾（列为显示优化项 §6-1）。

#### 3.4.2 派生透镜：`hide` / `only` / `map` / `link` 四动词与拒绝原文

- **命令**：

  ```bash
  printf 'hide /private\nmap /datasets /data\nlink /vis.txt /govdoc\n' > /tmp/b2.rules
  $GV define --name lab --base alice --rules /tmp/b2.rules
  tmnt lab /tmp/gft_lab lab          # systemd-run transient 挂载
  ls /tmp/gft_lab; ls /tmp/gft_lab/data
  mkdir /tmp/gft_lab/newdir
  $GV define --name bogus --base alice --rules /tmp/bogus.rules   # 非法规则
  ```

- **回显**：

  ```
  ===== B2 派生透镜: hide/only/map/link 四动词 =====
    define: defined derived view 'lab' over base 'alice': 1 hide, 0 only, 2 reloc rule(s)
    define: note: mounts of 'lab' are read-only (virtual shape) — write to base view 'alice'
      [挂载 lab -> /tmp/gft_lab: 已挂载]
    透镜根目录: [a1b.txt data govdoc lockf onlyme.txt vis.txt ]
    hide: /private 不可见(OK)
    map:  /data=[big.bin t1.txt ] 原 /datasets 已消失(改名生效)
    link: /govdoc=visible 源 /vis.txt 保留(keep_src OK)
    基视图侧原路径仍在: a1b.txt datasets lockf onlyme.txt private vis.txt 
  bash: line 206: /tmp/gft_lab/vis.txt: Read-only file system
  cat: /tmp/roerr: No such file or directory
    透镜写内容: rc=1 报错=[]
    透镜 mkdir: rc=1 报错=[mkdir: cannot create directory '/tmp/gft_lab/newdir': Read-only file system]
    only 单独一个透镜(白名单语义, 与改写动词互不干扰):
    define: defined derived view 'lab2' over base 'alice': 0 hide, 1 only, 0 reloc rule(s)
    define: note: mounts of 'lab2' are read-only (virtual shape) — write to base view 'alice'
      [挂载 lab2 -> /tmp/gft_lab2: 已挂载]
      根目录=[onlyme.txt ] 白名单 /onlyme.txt 可见; 非白名单 /vis.txt 已隐藏
  removed view definition 'lab2' (underlying data untouched)
    未知动词拒绝原文:
      Error: "bad layout rules: line 1: unknown layout verb 'bogus'"
    目标冲突拒绝原文(同一目的地两条规则):
      Error: "bad layout rules: line 2: destination '/z' used by two rules"
    规则文件内写 base 行的拒绝原文:
      Error: "bad layout rules: line 1: unknown layout verb 'base'"
  ```

- **结论**：通过。四动词语义、只读契约、三条拒绝原文逐条对上。注：`透镜写内容: rc=1 报错=[]` 的报错为空是取证脚本的重定向顺序缺陷（`> target 2>/err` 时 target 打不开导致 err 文件未创建），Shell 侧的真实报错就是上一行的 `Read-only file system`；后续条目已全部改为 `cmd 2>/tmp/e > target` 形态。

#### 3.4.3 纯别名与 `--retire` 无空档轮换

- **命令**：

  ```bash
  $GV create --view pv
  $GV alias --name pub-a --real pv
  $GV alias --name pub-b --real pv --retire pub-a
  tmnt pub-a /tmp/gft_pva2 pub-a2     # 轮换后老别名应不可挂
  tmnt pv    /tmp/gft_pvreal pvreal   # 别名存续期间真实名应被拒
  $GV alias --name pub-c --real pub-a # 目标此时已不是别名
  ```

- **回显**：

  ```
  ===== B3 纯别名 + --retire 无空档轮换 =====
    create pv: created BASE view 'pv' at /home/hanjun/meta/meta/pv.db
      [挂载 pv -> /tmp/gft_pv: 已挂载]
    alias pub-a: defined pure alias 'pub-a' -> real view 'pv'
    alias pub-a: note: 'pv' is no longer a mount entry point while this definition exists (mounts by its real name are refused, locally and via meta-server); alias mounts show the FULL tree and are WRITABLE
      [挂载 pub-a -> /tmp/gft_pva: 已挂载]
    别名挂载可见真实视图内容: [pubfile.txt rot.txt w.txt ]
    alias pub-b --retire pub-a: rotated public name for real view 'pv': 'pub-a' -> 'pub-b' (gapless — 'pv' stayed governed throughout, 'pub-a' no longer resolves)
    轮换后老别名挂载:
      [挂载 pub-a -> /tmp/gft_pva2: 未挂载]
      结果: 不可挂载(OK)
      日志尾部: ... grovefs[25608]: Error: Custom { kind: Other, error: "view 'pub-a' does not exist on this meta-server (sqlite): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; create it first: ..." }
    轮换后新别名挂载:
      [挂载 pub-b -> /tmp/gft_pvb: 已挂载]
      内容: [pubfile.txt rot.txt w.txt ] rot.txt=during-rotation
      别名可写(OK)
    别名存续期间真实名 pv 挂载:
      [挂载 pv -> /tmp/gft_pvreal: 未挂载]
      结果: 被拒(OK)
      日志尾部: ... grovefs[25746]: Error: Custom { kind: PermissionDenied, error: "view 'pv' is governed by an alias — mount the alias name instead (`grovefs view list` shows it)" }
    链式别名拒绝: defined pure alias 'pub-c' -> real view 'pub-a'
    撤别名 pub-b: removed view definition 'pub-b' (underlying data untouched)
    撤别名后真实名恢复可挂载:
      [挂载 pv -> /tmp/gft_pv3: 已挂载]
      结果: 成功(OK)
      轮换留下的内容仍在: [pubfile.txt rot.txt w.txt ]
  ```

  针对 `pub-c` 这一行（标签写的是「链式别名拒绝」却成功）补做的定向探针 `bash .build-bin/vm_probe_alias.sh`：

  ```
  ===== X1 链式别名(目标仍是活的别名)，均在运行 d2a3b262 的本地 home 上重跑 =====
    先建物理视图 pv，第一个: defined pure alias 'x-alias1' -> real view 'pv'
    链式: error: alias target 'x-alias1' is itself an alias (-> 'pv') — chains are not supported; alias the physical view instead
    同一真实视图的第二个别名:
      error: real view 'pv' is already governed by alias 'x-alias1' — one public name per real view; to rotate it gaplessly use `grovefs view alias --name x-alias3 --real pv --retire x-alias1`
  ===== X2 悬空目标别名(既无物理树也无定义) —— §5-10 修复后在定义时即拒 =====
    alias: error: alias target 'no-such-tree' has no physical view tree in /tmp/s510home (local files): no local tree /tmp/s510home/meta/no-such-tree.db yet — create it with `grovefs --home /tmp/s510home init --view no-such-tree` (or `view create`) first
      rc=1  [定义被拒，控制面未写入该别名；不再能进入“先定义后挂载”的悬空态]
  ```

  经 meta-server 挂载一个未知视图名（无任何树/定义）的报错原文（§6-5 修复后用实路径、§5-12 后走 Display；`--view no-such-tree-xyz`，回显取自运行 `d2a3b262` 的集群）：

  ```
  -- 经 meta-server 挂载未知视图名(--meta 模式):
    rc=1 报错原文: error: view 'no-such-tree-xyz' does not exist on this meta-server (sqlite): no tree and no definition — views are created explicitly, so unknown names are refused instead of becoming an empty tree; create it first: `grovefs view --db-path /home/hanjun/meta create --view no-such-tree-xyz` (or `grovefs --home /home/hanjun/meta init --view no-such-tree-xyz`, or seed it with `grovefs view import`)
  ```

- **结论**：通过。`--retire` 无空档轮换、老别名失效、真实名被拒、撤别名后恢复可挂，全部对上；链式别名与「一视图一别名」的拒绝文案与文档一致（先前那行「链式别名拒绝: defined pure alias 'pub-c' ...」是**用例顺序造成的假象**——彼时 `pub-a` 已被 retire，不再是别名，故不构成链；已在报告中以 X1 探针的正确用例替换结论）。先前记录的两处问题**本轮均已修复**：`alias` 不再接受指向不存在的真实视图（§5-10 已在定义时硬拒），提示里的 `<db-path>`/`<name>` 已换成实路径（§6-5）。

#### 3.4.4 fork（结构可写/内容只读）与 `--empty`

- **命令**：

  ```bash
  $GV fork --name alice-fork --base alice
  $GV fork --name empty-fork --base alice --empty
  tmnt alice-fork /tmp/gft_fk fk
  mkdir /tmp/gft_fk/newdir; mv /tmp/gft_fk/b4.txt /tmp/gft_fk/b4-renamed.txt
  python3 -c "...每种 open 形态单独看 errno..."   # O_WRONLY / O_WRONLY|O_APPEND / O_RDWR|O_TRUNC / os.truncate
  $GV fork --name x --base alice-fork            # 从 fork 再 fork
  ```

- **回显**（`vm_battery_1.sh` B4 段与 `vm_battery_2.sh` C2 段）：

  ```
  ===== B4 fork(结构可写/内容只读) 与 --empty =====
    fork: forked view 'alice-fork' from base 'alice': 13 entries, 10 shared object(s)
    fork: note: 'alice-fork' is an independent metadata tree — mkdir/rename/unlink are writable, content writes (write/truncate) are refused with EROFS; deletes remove only this view's names and never touch 'alice'
    fork: note: content bytes are SHARED with 'alice' (cross-view ledger): the object is freed only when no view holds a name for it anymore
    fork --empty: forked view 'empty-fork' from base 'alice' as an EMPTY tree: root only, nothing copied
      [挂载 alice-fork -> /tmp/gft_fk: 已挂载]
    fork 根内容: [a1b.txt b4-renamed.txt datasets lockf newdir onlyme.txt private vis.txt ]
    内容写 append(O_WRONLY|O_APPEND): write 未报错
      append(O_WRONLY|O_APPEND) fsync errno=30 (Read-only file system)
    内容写 truncate(O_RDWR|O_TRUNC): open 即被拒 errno=30 (Read-only file system)
    内容写 write-only(O_WRONLY): write 未报错
      write-only(O_WRONLY) fsync errno=30 (Read-only file system)
    fork 侧回读内容: b''
    基视图侧 b4.txt: b'forkme\n'
    shell 追加 >> : rc=0 报错=[] (bash 吞 EROFS 则 rc=0, 见逐 open 探针)
  bash: line 292: /tmp/gft_fk/b4-renamed.txt: Read-only file system
    shell 截断 > : rc=1 报错=[] 截断后大小=0 基视图大小=7
      [挂载 empty-fork -> /tmp/gft_efk: 已挂载]
    --empty fork 挂载内容: [only-here ]
    从 fork 再 fork 的拒绝原文:
      Error: "base 'alice-fork' is governed by a definition (alias/derived/fork) — fork the physical base tree instead"
  ```

  ```
  ===== C2 fork 内容写语义精确验证(每种 open 形态单独看 errno) =====
    fork: forked view 'c2-fork' from base 'alice': 12 entries, 9 shared object(s)
      [挂载 c2-fork -> /tmp/gft_c2: 已挂载]
    O_WRONLY        : open+write(1B) 成功 -> fsync errno=30 (Read-only file system)
    O_WRONLY|O_APPEND: open+write(1B) 成功 -> fsync errno=30 (Read-only file system)
    O_RDWR|O_TRUNC  : open 即被拒 errno=30 (Read-only file system)
    O_RDWR          : open+write(1B) 成功 -> fsync errno=30 (Read-only file system)
    os.truncate     : errno=30 (Read-only file system)
    fork 侧内容回读  : b'forkbase\n'
    基视图内容回读   : b'forkbase\n'
    撤: removed fork view 'c2-fork' of base 'alice' (its own metadata tree is gone; content bytes stay while any view — including 'alice' — still holds a name for them)
    撤: note: a live mount of a removed fork keeps its already-resolved names until it remounts; new mounts are refused
  ```

- **结论**：通过。内容面只读的精确形状为：`O_TRUNC` 在 open 即 `EROFS`；`O_WRONLY`/`O_APPEND`/`O_RDWR` 的 write 调用不报错，`EROFS` 在 fsync 时暴露；因此 shell `>>` 追加会假成功（`rc=0`）而 `>` 截断失败（`rc=1`）——这是真实行为，已列为显示/文档优化项（§6-3）。结构写（mkdir/rename）本轮干净回显为成功（基线已先清除同名残留）。

#### 3.4.5 `view graft` 选择性接枝与拒绝原文

- **命令**：

  ```bash
  $GV fork --name gft-fork --base alice --empty
  printf 'imported/big.bin\timported/big.bin\n' > /tmp/plan.tsv
  $GV graft --name gft-fork --src alice --map /tmp/plan.tsv
  $GV graft --name gft-fork --src alice --map /tmp/plan.tsv            # 重名
  $GV graft --name gft-fork --src alice --map /tmp/plan.tsv --skip-existing
  $GV graft --name gft-fork --src nosuchview --map /tmp/plan.tsv
  $GV graft --name lab --src alice --map /tmp/plan.tsv                 # 目的地是透镜
  ```

- **回显**：

  ```
  ===== B5 view graft(选择性接枝) 与拒绝原文 =====
    建空 fork: forked view 'gft-fork' from base 'alice' as an EMPTY tree: root only, nothing copied
    graft: grafted 4 entries (1 directory) from 'alice' into 'gft-fork' at /: 3 shared object(s), 0 skipped
    graft: note: bytes were not copied — every grafted name holds the same object as its source; the object is freed only when no view names it anymore
    graft: note: content stays read-only in a fork (EROFS); re-running the same plan with --skip-existing merges into what is already there
      [挂载 gft-fork -> /tmp/gft_gr: 已挂载]
    接枝结果: 根=[a1b.txt imported ] imported/=[big.bin t1.txt ]
    字节共享: 1 个不同 md5 (1=共享同一对象)
    重名拒绝: Error: "'a1b.txt' already exists in the destination — rename it in the plan or pass --skip-existing"
    --skip-existing: grafted 0 entries (0 directories) from 'alice' into 'gft-fork' at /: 0 shared object(s), 1 skipped
    未知源视图拒绝: Error: "no tree for graft source 'nosuchview' in /home/hanjun/meta (local files) — create it first (`grovefs view create --view nosuchview` for a base view, or `grovefs view fork --name nosuchview --base <base> --empty` / `fork_view` with `seed:false` for an empty fork)"
    接枝进非 fork 拒绝: Error: "graft destination 'lab' is a derived lens: it renders another tree read-only and owns no nodes — use a base view or a fork"
    不存在路径拒绝: Error: "'nosuch.txt' does not exist in view 'alice'"
  ```

- **结论**：通过。接枝零拷贝（同一对象）、五条拒绝原文逐条对上。

#### 3.4.6 四态并立与 `view rm --base` 守卫

- **命令**：同一时刻分别挂载 `alice`（基）、`lab`（透镜）、`pv-pub`（别名）、`pv-fork`（fork）并列举四个根。
- **回显**：

  ```
  ===== B6 四态并立: 同一路径的四种呈现 =====
      [挂载 alice -> /tmp/gft_b6a: 已挂载]
      [挂载 lab -> /tmp/gft_b6b: 已挂载]
      [挂载 pv-pub -> /tmp/gft_b6c: 已挂载]
      [挂载 pv-fork -> /tmp/gft_b6d: 已挂载]
    基视图 alice   : [b4.txt b4del.txt datasets lockf onlyme.txt private vis.txt ]
    透镜 lab       : [data onlyme.txt ]
    别名 pv-pub    : [pubfile.txt rot.txt w.txt ]
    fork pv-fork   : [pubfile.txt rot.txt w.txt ]
  ```

  ```
    误用 --base 撤透镜: Error: "'lab' is governed by a view definition — remove it first with `grovefs view rm --name lab` (without --base)"
    撤透镜: removed view definition 'lab' (underlying data untouched)
    撤物理树 pv(无本地 blocks 时的拒绝): Error: Os { code: 13, kind: PermissionDenied, message: "Permission denied" }
  ```

  同一命令的完整形式与实际使用的参数（`vm_battery_1.sh` B6）：

  ```bash
  $GV rm --name pv --base --data-root /nonexistent
  # Error: Os { code: 13, kind: PermissionDenied, message: "Permission denied" }
  ```

- **结论**：四态并立通过。守卫拒绝中“误用 `--base` 撤透镜”一条文案完整（含下一步命令）；而“数据根不可用时”只给裸的 `Permission denied`，不报路径、不给下一步，且发生在元数据树已删之后（字节未回收），列为遗留项（§5-4、§6-4）。本地存储根上的正常回收见 §3.8。

### 3.5 `import` 零拷贝与重跑增量

- **命令**（`vm_battery_2.sh` C3，本地共置存储根 `$S=$HOME/gftstore`）：

  ```bash
  grovefs import --db-path "$S" --view imp --source /tmp/gft_src --dry-run
  grovefs import --db-path "$S" --view imp --source /tmp/gft_src
  grovefs import --db-path "$S" --view imp --source /tmp/gft_src   # 重跑
  printf 'ccc\n' > /tmp/gft_src/docs/c.md
  grovefs import --db-path "$S" --view imp --source /tmp/gft_src   # 新增后重跑
  grovefs import --db-path "$S" --view imp2 --source /tmp/gft_src  # 两视图共享
  ```

- **回显**：

  ```
  ===== C3 import 零拷贝 + 重跑增量(本地共置存储根) =====
    导入前 blocks/ 对象数: 0
    dry-run: dry run: view 'imp' under /gft_src would mirror 2 dir(s) and 3 file(s) (symlinks/special files and names already in the view are skipped on the real run)
    import: imported into view 'imp' under /gft_src: 2 dir(s), 3 object(s) (3 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
    导入后 blocks/ 对象数: 3
    磁盘增量(字节): 8388616  vs 源目录字节和 8388616
    零拷贝的真实含义(同一 inode, 不新增数据块):
      源 docs/a.md: ino=84547 links=2 -> blocks 内同 inode 条目: /home/hanjun/gftstore/blocks/b1/8ba4ae13-87fb-47b2-90c8-4a0b365b4248
      源 photos/b.jpg: ino=84548 links=2 -> blocks 内同 inode 条目: /home/hanjun/gftstore/blocks/6a/9f3f49f0-caef-4b90-9264-501f3ec2d006
      源 bigblob: ino=84550 links=2 -> blocks 内同 inode 条目: /home/hanjun/gftstore/blocks/16/94ce3c97-dd8d-47bd-ad20-94d5da8b0e6d
      blocks 文件 inode 去重前/后: 3 / 3
    重跑: imported into view 'imp' under /gft_src: 2 dir(s), 0 object(s) (0 hardlinked zero-copy, 0 copied), 3 skipped, 0 failed
    新增一个文件后重跑: imported into view 'imp' under /gft_src: 2 dir(s), 1 object(s) (1 hardlinked zero-copy, 0 copied), 3 skipped, 0 failed
    两次导入同一源到两个视图(共享对象):
      imp2: imported into view 'imp2' under /gft_src: 2 dir(s), 4 object(s) (4 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
      blocks/ 路径数: 8 (每视图各有自己的对象名)
      磁盘实际占用: 8388620 字节 ; 源目录: 8388620 字节 (去重后不翻倍)
      distinct inode 数: 4
      holders 台账: 8,8
  ```

- **结论**：通过。零拷贝以硬链接实现（同 inode、磁盘不翻倍）；dry-run/重跑增量/新增识别均正确。口径提醒：源目录名成为视图内的顶层名（此处 `/gft_src`），且「路径数 8 / distinct inode 4」两个计数并存容易误读（§6-7）。

### 3.6 `du` 的两条路径、缺数据源拒绝与丢对象出声

#### 3.6.1 本地路径（`--db-path` / `--home`）

- **命令**：

  ```bash
  grovefs du --db-path "$S" --view imp --total
  grovefs du --home "$S" --view imp --depth 1
  grovefs du --db-path "$S" --view imp -b --total
  grovefs du --db-path "$S" --view imp --path /gft_src/docs
  grovefs du --db-path "$S" --view imp --path docs
  grovefs du --db-path "$S" --view imp --path nosuchdir
  ```

- **回显**：

  ```
  ===== C4 du 本地路径(--db-path / --home, 自含数据根) =====
    --db-path --total: 8.0M	/
    --home --depth 1: 8.0M	/
    --home --depth 1: 8.0M	/gft_src
    与源目录字节和核对: 期望=8388620 实测=8388620 (OK)
    --path /gft_src/docs: 8B	/gft_src/docs
    --path docs(裸相对名): Error: "path not found: docs"
    不存在路径: Error: "path not found: nosuchdir"
  ```

- **结论**：通过。两个入口一致且与源字节和完全相符；`--path` 只接绝对视图路径（裸相对名被拒，文案可读）。

#### 3.6.2 远程路径（`--meta` + `--data` 逗号列表）与拓扑错配的四个方向性拒绝

- **命令**：

  ```bash
  sudo sh -c "set -a; . /etc/grovefs/cluster.env; set +a; grovefs du --view alice --meta 127.0.0.1:50053 --data gfs1:50100,gfs2:50100 --total"
  sudo sh -c "... grovefs du --view alice --meta 127.0.0.1:50053 --data gfs1:50100 --total"                        # 列表过短(2写1测)
  sudo sh -c "... grovefs du --view alice --meta 127.0.0.1:50053 --data gfs1:50100,gfs2:50100,gfs1:50100 --total"  # 列表过长(顺序保留)
  sudo sh -c "... grovefs du --view alice --meta 127.0.0.1:50053 --data gfs2:50100,gfs1:50100 --total"             # 顺序倒置(两台都对但顺序反了)
  ```

- **回显**（release `d2a3b262` 实机定点，四条均在运行 d2a3b262 的集群上取得；本轮致命错误改走 Display，故前缀由 `Error: "…"` 变为 `error: …`（不再带外层引号）；注意倒置例报的 object UUID 随实机重跑而变，为新鲜结果）：

  ```
  === --data gfs1:50100,gfs2:50100 ===            # 正确列表
  64.0M	/
  === --data gfs1:50100 ===                        # 过短: 方向性建议“补齐全部”
  error: this tree was written across 2 data-servers but --data names 1: the stripe map needs every address in the writer's order, so pass the full list as one comma-separated value (e.g. --data host1:port,host2:port); no sizes are reported
  === --data gfs1:50100,gfs2:50100,gfs1:50100 ===  # 过长: 方向相反, 建议“只留 2 个”
  error: this tree was written across 2 data-servers but --data names 3: extra addresses move logical chunks onto servers that never held them, so pass exactly the 2 addresses in the order the writer used; no sizes are reported
  === --data gfs2:50100,gfs1:50100 ===             # 顺序倒置: 不再静默 0B, 而是报错
  error: 7 of 7 object(s) could not be confirmed against the data source (first failure: object 29f9d81c-8ff9-4fe0-afaa-1092e93623e4 keeps chunk #0 on data-server #1, but this --data list orders it at #0: the servers are right and their order is not, so sizes and reads would be attributed to the wrong member); their sizes are unknown, so no total is reported
  ```

- **结论**：通过。正确列表与挂载点字节和相符（64.0M）；三种错配均为硬拒绝且**方向正确**——过短建议“pass the full list”、过长建议“pass exactly the N addresses”（旧文案对过长场景给相反建议已修）、顺序倒置不再静默输出 `0B`（由 `chunk0_elsewhere` 在 stat 层检出，见 §5-5）。拒绝均发生在探测前（长度）或首块定位时（顺序），不输出部分总数。

#### 3.6.3 丢对象必须出声

**情形一：本地存储根，整个对象被 unlink。**

- **命令**（`bash .build-bin/vm_probe_lostobj.sh` L1）：先 `grovefs --home "$P" init --view probe` + `grovefs import --db-path "$P" --view probe --source /tmp/gft_probe_src` 建 2 对象，取 `data_ref` 后 `rm $path`，再跑 `grovefs du --db-path "$P" --view probe -b --total`。
- **回显**：

  ```
    import: imported into view 'probe' under /gft_probe_src: 1 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
    blocks 对象数: 2
    du --db-path 基线: 1000000    /|
    视图树 data_ref: two.bin=1344177d-a81e-4c63-a7c6-d5955301fb99
    将 unlink: /home/hanjun/gftprobe/blocks/9e/1344177d-a81e-4c63-a7c6-d5955301fb99  (700000 字节)
    unlink 后 du --db-path: warning: 1 of 2 object(s) are not in the data source and count as 0 bytes (objects on other storage roots need their own --data/--data-root)|300000  /|
  ```

- **结论**：**通过**——丢对象确实出声（stderr 的 `warning: N of M object(s) are not in the data source and count as 0 bytes`），且总数从 1000000 降到 300000（缺的那个按 0 计入并被报出）。

**情形二：条带对象的中间分块丢失（真正删除 + 重启该 data-server）。**

- **目的**：确认多根条带路径下丢一个非空中间分块时 du 与读是否出声。
- **命令**（`bash .build-bin/vm_probe_lostchunk.sh`，关键步）：

  ```bash
  # uuid 取自 meta-server 的视图树
  sudo find /data/gfs*/grove/blocks -name "<uuid>#1" -type f      # gfs2 上定位 chunk#1
  sudo cp <chunk#1> /dev/shm/hidden.obj && sudo rm <chunk#1>       # 跨设备复制后真正 unlink
  sudo systemctl restart grovefs-data-server                       # 清掉服务端 fd 缓存
  echo 3 | sudo tee /proc/sys/vm/drop_caches                        # 清客户端页缓存
  sudo sh -c "set -a; . /etc/grovefs/cluster.env; set +a; grovefs du --view alice --meta 127.0.0.1:50053 --data gfs1:50100,gfs2:50100 -b --path /datasets"
  md5sum /home/hanjun/mnt/datasets/big.bin
  ```

- **回显**：

  ```
  ===== D2 分块分布 =====
    -- gfs1:
      /data/gfs1/grove/blocks/19/48505e88-94b4-4906-8c3c-1029578fba5e#0 16777216
      /data/gfs1/grove/blocks/b3/48505e88-94b4-4906-8c3c-1029578fba5e#2 16777216
    -- gfs2:
      /data/gfs1/grove/blocks/66/48505e88-94b4-4906-8c3c-1029578fba5e#1 16777216
      /data/gfs1/grove/blocks/00/48505e88-94b4-4906-8c3c-1029578fba5e#3 16777216
  ===== D3 挑走前基线 =====
    du -b --path /datasets: 67108869      /datasets|
    du --total:              64.0M        /|
    挂载 md5: 1304cb5a5f3e8b534c5bda7ee4e107b4  /home/hanjun/mnt/datasets/big.bin
  ===== D5 观察 =====
    du -b --path /datasets: 16777221      /datasets|
    du --total:              16.0M        /|
    挂载整文件 md5: rc/md5 → 0f5f10ff9824446786fe03197ee36149  /home/hanjun/mnt/datasets/big.bin
  rc=0 报错=[]
    挂载 stat size: 16777216
    dd 读 chunk#1 区间 16MiB..20MiB: rc=0 报错=[0+0 records in|0+0 records out|0 bytes copied, 0.00103294 s, 0.0 kB/s|]
  ===== D6 复原 =====
      已复原 /data/gfs1/grove/blocks/66/48505e88-94b4-4906-8c3c-1029578fba5e#1 16777216 字节
    复原后 du --total: 64.0M      /|
    复原后挂载 md5: 1304cb5a5f3e8b534c5bda7ee4e107b4  /home/hanjun/mnt/datasets/big.bin
  ```

- **结论**：**探针当时为失败/未实现；现 du 侧已修复**。探针当时：中间分块丢失时 du 不报 warning，而是静默把对象短报为 16 MiB（`67108869 → 16777221`）；机制见 `src/backend/sharded.rs` 的 `len_with`：普通 `len` 假定分块连续，用指数+二分定位「第一个缺失分块」当 EOF，因此内部空洞不可区分。**当前版本**：`du` 的诊断扫描改走 `ShardedBackend::len_strict`，二分停在首个缺失分块后多探一块，存在即在「其后」就判损坏并出声（独立 `damaged` warning，字节不计入总数），本机端到端复验见 §3.6.5。挂载侧 `stat`/读的行为不变（见 §5-6，属设计取舍：`build_attr` 对 `len` 错误回退到元数据 size，且 len 是 getattr 热路径）。

**情形三：条带对象的第一个分块丢失。**

- **命令**（`bash .build-bin/vm_probe_lostobj.sh` L2）：`sudo cp <uuid>#0 /dev/shm/c0.obj && sudo rm <uuid>#0` 于 gfs1，重启 gfs1 的 data-server，清页缓存后跑 du 与挂载读。
- **回显**：

  ```
    data_ref=s2:16777216:48505e88-94b4-4906-8c3c-1029578fba5e  目标: gfs1:/data/gfs1/grove/blocks/19/48505e88-94b4-4906-8c3c-1029578fba5e#0 (16777216 字节)
    基线 du --total: 64.0M        /|
      已 unlink
      gfs1 data-server 已重启(fd 缓存清空)
    unlink 后 du --total:      55B        /|
    unlink 后 du -b --path /datasets: 5   /datasets|
    挂载 stat size: 0
    挂载整文件读: rc=0 报错=[]
    du 单地址(列表长度与 2 代布局不符)必须拒绝: Error: "this tree was written across 2 data-servers but --data names 1: the stripe map needs every address in the writer's order ...; no sizes are reported"|
      已复原 /data/gfs1/grove/blocks/19/48505e88-94b4-4906-8c3c-1029578fba5e#0 16777216 字节
    复原后 du --total: 64.0M      /|
    复原后挂载 md5: 1304cb5a5f3e8b534c5bda7ee4e107b4  /home/hanjun/mnt/datasets/big.bin
  ```

- **结论**：**探针当时为失败/未实现；现 du 侧已修复**。探针当时：chunk#0 缺失时 `len` 返回 `Ok(0)`，du 给出 `55B` 这种完全错位的极小值。**当前版本**：`len_strict` 在 chunk#0 缺失而 chunk#1 存在（或 #0 短报但 #1 存在）时同样判损坏出声，不再给出极小总值；挂载 `stat size=0` 且读不报错的行为不变（§5-6）。本机复验见 §3.6.5。

#### 3.6.4 本地挂载内读丢失对象（重挂后）

- **命令**（`bash .build-bin/vm_probe_lostlocal.sh`）：本地 store `unlink` 对象后先读一次（旧挂载进程），再 `systemctl stop` + 重新 `systemd-run` 挂载后读一次。
- **回显**：

  ```
    unlink 前读: rc=0 md5=257ccc870d737ac121380e61c5a7bc09
    unlink 后(旧挂载进程, 服务端 fd 缓存仍在):
      stat size: size=700000
      整文件读: rc=0 报错=[]
      du -b --total: warning: 1 of 2 object(s) are not in the data source and count as 0 bytes (objects on other storage roots need their own --data/--data-root)|300000 /|
      du -b --path /gft_probe_src/docs: warning: 1 of 2 object(s) are not in the data source and count as 0 bytes (...)|300000      /gft_probe_src/docs|
    重挂后(fd 缓存已清):
      整文件读: rc=0 报错=[]
      stat size: size=0
  ```

- **结论**：两条当前行为必须写进文档：① `data-server` 按 key 长期缓存原始 fd（`src/backend/local.rs:109-145`），因此对象被外部改名/删除后，已建立的进程仍能读写旧 inode——用 `mv` 模拟丢对象在任何统计上都不可观察（本任务早期一轮 C5 的「无变化」回显即此原因，已用上面 unlink+重启的探针推翻并重测）；② fd 缓存失效后，丢失对象的挂载读不报 `EIO`，而是 `size=0` + 空内容 rc=0（静默）。列为遗留项 §5-6。

#### 3.6.5 条带对象损坏的 du 出声（§6-2 修复后，本机端到端复现）

- **命令**（`bash .build-bin/repro_du_damage.sh`，本机两台 data-server + 本地 meta）：`mount --data A,B` 下 `dd` 一个 40 MiB 文件（跨分块 #0/#1/#2，#1 在 B）；基线 du → umount、停服、`rm` 掉 B 上的 `{uuid}#1` → 重启两 server → 再跑 du。对照变体同法拔掉**尾块** `#2`（在 A）。
- **回显**（中间块 #1 缺失）：

  ```
    === baseline du (intact) ===
    41943040        /
    === kill servers, remove chunk #1 (on B), restart ===
    .d62/sB/blocks/62/208f94a7-afa0-4e90-ab0c-1b0336e8930f#1
    === du after damage ===
    warning: 1 of 1 object(s) are damaged striped objects and are excluded from the total (their true size is unknown); first: striped object 208f94a7-afa0-4e90-ab0c-1b0336e8930f has a gap at chunk #1 (a present chunk lies beyond the first absent one) — content is damaged, not short; its true size is unknown
    0       /
  ```

- **回显**（对照：尾块 #2 缺失，EOF 前的分块仍连续）：

  ```
    === baseline: ===
    41943040        /
    === du after TAIL loss ===
    33554432        /
    rc=0
  ```

- **结论**：**通过**。中间分块丢失不再静默短报（修复前同形态为 `40MiB → 16MiB` 无声，见 §3.6.3 情形二）：du 给出独立 `damaged` warning（区别于「不在数据源」的 lost warning，更区别于误报），不可确认的字节不计入总数；尾块丢失仍按 EOF 静默短报、不误报损坏（`rc=0`，无 warning）。单测 `len_strict_flags_interior_and_leading_gaps_but_not_a_short_tail`（`src/backend/sharded.rs`）同时钉住首块缺失、中间空洞、短尾三态。

### 3.7 `view export/import` 跳机迁移与 `--as-fork`

- **命令**：

  ```bash
  grovefs view --db-path "$S" export --view imp --out /tmp/imp.export.jsonl
  tar -C "$S" -cf - blocks | ssh gfs1 'sudo tar -xf - -C /tmp/gft_recv'   # 字节随行
  ssh -n gfs1 "sudo grovefs view --db-path /tmp/gft_recv import --view moved --in /tmp/imp.export.jsonl"
  ssh -n gfs1 "sudo grovefs du --view moved --db-path /tmp/gft_recv --data-root /tmp/gft_recv -b --total"
  ```

- **回显**：

  ```
  ===== C6 view export/import 跳机迁移 + --as-fork =====
    export: Error: "output /tmp/alice.export.jsonl already exists — pass --force to overwrite it"
    导出虚拟名必须被拒:
      Error: "'c6lens' is a derived (virtual) view over 'alice' — export 'alice' instead (a derived view has no metadata tree of its own)"
    跳机迁移(共置存储根 /home/hanjun/gftstore/imp -> gfs1:/tmp/gft_recv):
      export: exported view 'imp' metadata: 8 node(s), 4 blob reference(s) -> /tmp/imp.export.jsonl
      export: note: metadata only — copy the store's `blocks/` tree (the whole data root) as well; blobs are located by their data_ref key, not by any host path
      源对象 09c192f6-e76c-494a-a6b3-bdf399e0ee42 落点 /home/hanjun/gftstore/blocks/5e/09c192f6-e76c-494a-a6b3-bdf399e0ee42
      源对象 09c192f6-e76c-494a-a6b3-bdf399e0ee42 md5=c576ec4297a7bdacc878e0061192441e
      已拷 blocks/: gfs1 上对象数 8
      gfs1 import: imported view 'moved' from /tmp/imp.export.jsonl: 8 node(s), 4 blob reference(s); cross-view ledger rebuilt
      gfs1 import: note: content bytes are not in the export — make sure `blocks/` from the source store is present under the target data root before mounting
        导入后 du(数掳必须指向目标存根): 
    8388620	/
      同一 data_ref 在 gfs1 上的 md5=c576ec4297a7bdacc878e0061192441e  (字节一致 OK)
      迁移前后字节: 源=8388620 目标=8388620 (OK)
    重复导入未加 --replace 必须被拒:
      Error: "target metadata tree /tmp/gft_recv/meta/moved.db already exists — pass --replace to overwrite it (stop every mount/meta-server using this store first)"
    --replace 覆盖(离线迁移专用):
      imported view 'moved' from /tmp/imp.export.jsonl: 8 node(s), 4 blob reference(s), replacing the previous tree; cross-view ledger rebuilt
    --as-fork 记录为 fork:
      imported view 'asfk' from /tmp/imp.export.jsonl: 8 node(s), 4 blob reference(s); cross-view ledger rebuilt
      note: recorded `fork moved` — mounts of 'asfk' are structure-writable, content read-only (EROFS)
      fork 'asfk' of base 'moved' (independent tree: structure writable, content refused EROFS; deletes touch only this view)
      base view 'moved' -> own tree meta/moved.db (read-write; deletes free a shared object only when no view holds it)
  ```

- **结论**：通过。导出行数/头部计数、字节随 `blocks/` 同迁、目标侧 du 与 md5 与源完全一致（8388620 字节），`--replace`/`--as-fork`/两种拒绝文案均对上。注：首轮因残留导出文件触发 `already exists — pass --force`，本轮已前置 `rm -f` 重测。

### 3.8 `gc`、`refs rebuild` 与 `view rm --base` 的字节回收

- **命令**：

  ```bash
  grovefs gc --db-path "$S"                       # 干跑
  grovefs gc --db-path "$S" --apply
  grovefs gc --db-path $HOME/meta                  # 在用存储、字节在远端
  grovefs refs rebuild --db-path "$S"
  $GV rm --name c8base --base --data-root $HOME/meta
  ```

- **回显**：

  ```
  ===== C7 gc(本地孤儿审计/回收) 与 refs rebuild =====
    gc 干跑:   views: 2 tree(s), 8 referenced key(s), 0 striped layout(s)
    gc 干跑:   objects: 9 (16.0 MiB)
    gc 干跑:   orphans: 1 (4.0 KiB; 1 reclaimable, 0 within grace <0s)
    gc 干跑:        4.0 KiB        0s  plain  orphan.bin
    gc 干跑: dry run: nothing deleted. Re-run with --apply (offline) to reclaim 1 object(s), 4.0 KiB.
    gc --apply: reclaimed: 1 object(s), 4.0 KiB freed; 0 within grace kept; 0 failed
    回收后对象是否真消失: 0 个残留
    gc 在带挂载/远程数据根上的拒绝原文:
      note: this store is in use by local grovefs process(es) — pid 30285: /usr/local/bin/grovefs meta-server --listen 0.0.0.0:50053; pid 30290: /usr/local/bin/grovefs mount --view alice -m /home/hanjun/mnt --foreground
      gc: meta /home/hanjun/meta
        data root(s): /home/hanjun/meta
        views: 1 tree(s), 9 referenced key(s), 9 striped layout(s)
        objects: 0 (0 B)
        orphans: 0 (0 B; 0 reclaimable, 0 within grace <0s)
      dry run: nothing deleted. Re-run with --apply (offline) to reclaim 0 object(s), 0 B.
    refs rebuild:
      人为清空后台账行数: 8
      rebuild: refs ledger rebuilt at /home/hanjun/gftstore/meta/refs.db: 8 holder entries across the surviving views
      重建后: 8,8 行/持有数
  ===== C8 view rm --base 的守卫与字节回收 =====
    守卫1: 视图正被挂载时
      Error: "view 'c8base' is mounted here at /tmp/gft_c8 — unmount it first (`grovefs umount -m /tmp/gft_c8`); mounts on OTHER machines are not visible in this mount table, stop them too"
    守卫2: 仍有定义指向它
      Error: "view 'c8pub' is governed by a definition over base 'c8base' — remove it first (`grovefs view rm --name c8pub`) before `view rm --name c8base --base`"
    守卫3: 未挂载且无定义 -> 真正删树并回收字节
      removed base view 'c8base': 1 node(s) gone, 0 blob reference(s) dropped, 0 object(s) freed from /home/hanjun/meta/blocks, 0 still held by other views
    本地存储根的 --base 回收(blocks/ 在旁):
      rm imp2 --base: removed base view 'imp2': 8 node(s) gone, 4 blob reference(s) dropped, 4 object(s) freed from /home/hanjun/gftstore/blocks, 0 still held by other views
      剩余 blocks 对象数: 0  ; 剩余树: refs.db refs.db-shm refs.db-wal 
  ```

  丢对象后的 `view rm --base` 部分回收文案（来自 §3.6.4 同一轮）：

  ```
    收尾: removed base view 'probe': 5 node(s) gone, 2 blob reference(s) dropped, 1 object(s) freed from /home/hanjun/gftprobe/blocks, 0 still held by other views, 1 not found under that data root (wrong --data-root? they live on another store)
    收尾: note: 1 unreferenced blob(s) were NOT deleted — re-run with --data-root pointing at the store that holds them (default: this metadata root)
  ```

- **结论**：通过。干跑/`--apply` 回收数量与磁盘实际一致；`refs rebuild` 能从存活视图重建台账（8 holder entries）；三道守卫逐条对上。唯一显示缺陷：对**在用**存储做 gc 时，因字节全在远端 data-server，输出 `objects: 0 (0 B)` + `dry run: nothing deleted`，把「本根无字节」说成「无对象可回收」（§6-8）。

### 3.9 apikey：scope / 视图绑定 / 只读原地切换 / 撤销封停 / 调用审计

取证入口：`bash .build-bin/vm_battery_3.sh`（隔离栈：门禁 meta-server `127.0.0.1:50173` + `data-server --require-api-key` `127.0.0.1:50111`）。

- **命令**：

  ```bash
  grovefs apikey --db-path /tmp/gft_gate create --label d-sample
  grovefs apikey --db-path /tmp/gft_gate list
  grovefs apikey --db-path /tmp/gft_gate list --view keyed
  grovefs apikey --db-path /tmp/gft_gate create --label x --for both   # 非法 scope
  grovefs apikey --db-path /tmp/gft_gate set-ro --id <ID>
  grovefs apikey --db-path /tmp/gft_gate revoke --id <ID>
  grovefs apikey --db-path /tmp/gft_gate calls
  ```

- **回显**（明文已截短，不落报告）：

  ```
  ===== D1 apikey create: 明文一次性 + scope =====
    apikey created — shown ONCE (only its SHA-256 is stored; save it now):
      id:    ad6cbc83114f4851951052e26c653be8
      label: d-sample
      scope: any
      view:  <any>
      rw:    rw (read-write)
    明文只在 create 那次出现: list 中含 secret 的行数 -> 0 处
    --for 非法值的拒绝原文: Error: "unknown api key scope 'both' (expected any|mcp|mount)"|
  ===== D2 挂载侧: 无密钥 / scope 错配 / 视图绑定 =====
    (a) 不带密钥挂载 keyed:
      挂载状态: 未挂载(OK)
      拒绝原文: ... Error: Custom { kind: PermissionDenied, error: "no api key presented: this server requires one (pass `mount --api-key ID:SECRET` or export GROVEFS_API_KEY; mint with `grovefs apikey create --for mount`)" }
    (b) mcp-scope 密钥用于挂载:
      拒绝原文: ... Error: Custom { kind: PermissionDenied, error: "authentication failed: this api key is scoped to 'mcp', not 'mount'" }
    (c) mount-scope 密钥用于 MCP(应由 MCP 侧拒):
      应答: "code":-32001,"message":"authentication required: present a valid apiKey in the initialize params (or export GROVEFS_MCP_API_KEY for the server)"|
    (d) 绑定 keyed 的密钥去挂 other:
      拒绝原文: ... Error: Custom { kind: PermissionDenied, error: "permission denied: this api key is bound to view 'keyed', not 'other'" }
  ===== D3 只读密钥与 set-ro 原地切换(不重连) =====
      [keyed -> /tmp/gft_rw: 已挂载]
    rw 密钥写入: OK 现网回读: 'first\n'
    apikey: id='965cd373deaa45d3bc1d48b1a412ea98' (label 'd-rw') is now ro (read-only)
      a LIVE mount presenting this key starts refusing writes on its next re-check window (≤1s) — no reconnect, no remount; its reads keep flowing.
    set-ro 后 O_WRONLY|O_APPEND: errno=30 (Read-only file system)
    set-ro 后 O_WRONLY|O_TRUNC: errno=30 (Read-only file system)
    set-ro 后读仍通: 'first\n\x00\x00\x00\x00\x00\x00\x00'
      (shell 追加的假成功) rc=0 报错=[] 文件行数=1
    挂载未被拆除: 是
    apikey: id='965cd373deaa45d3bc1d48b1a412ea98' (label 'd-rw') is now rw (read-write)
      a LIVE mount presenting this key resumes admitting writes on its next re-check window (≤1s) — no reconnect, no remount.
    恢复 rw 后追加: errno=30 (Read-only file system)
  ===== D4 revoke 封停已建挂载 =====
    apikey: revoked id='965cd373deaa45d3bc1d48b1a412ea98' (label 'd-rw'); it can no longer authenticate, past calls keep attribution
    等待 6*0.4s 后挂载点状态: 已自行卸载(封停生效)
    挂载侧打印的原因: ... grovefs[27554]: grovefs: access refused by the server (authentication failed: this api key has been revoked) — unmounting this view|... received termination signal, unmounting cleanly...|... unmounted view 'keyed' at /tmp/gft_rv|
    单元结局: inactive
    撤销后再挂载必须失败:
      拒绝原文: ... Error: Custom { kind: PermissionDenied, error: "authentication failed: this api key has been revoked" }
    对已撤销 id 再 set-ro: Error: "apikey id='965cd373deaa45d3bc1d48b1a412ea98' is revoked — set-ro never resurrects a revoked key (mint a new one)"
    重复 revoke: apikey: id='965cd373deaa45d3bc1d48b1a412ea98' (label 'd-rw') is already revoked
  ```

- **结论**：基本通过。明文一次性、list 不回发明文（含 secret 行数 0）、scope 门禁、视图绑定、set-ro 原地生效（不重连不重挂，≤1s 复检窗口）、revoke 自动卸载已建挂载、调用审计均对上。
  两处未对上文档的地方：① `set-ro` 后回显 `'first\n\x00\x00\x00\x00\x00\x00\x00'`（尾部多出 7 个零字节，与写前内容不等长）；② **`set-ro` 回到 `rw` 后追加仍 `errno=30`**（日志已打印 “is now rw ... resumes admitting writes”，但同一挂载在探针窗口内仍拒写）。②列为遗留项（§5-7，属复检窗口/缓存时机，文档声称 ≤1s）；①列为遗留项（§5-8）。

### 3.10 MCP 全方法面（10 工具 + 资源 + 模板 + 提示 + 补全 + 日志 + 协议错误）

- **命令**：向 `grovefs mcp --db-path /tmp/gft_gate --require-api-key` 的 stdio 依次注入 JSON-RPC 请求（文件式回放，回显 `/tmp/vm_evidence/battery_3.log` D5 段）。
- **回显**（节选）：

  ```
  ===== D5 MCP 全方法面(stdio 文件式) =====
    未鉴权(不带 apiKey, 服务器 --require-api-key)的拒绝原文:
      stderr: [grovefs mcp] api-key auth REQUIRED (state-touching methods — tools/list, tools/call, resources/*, completion/complete — gated until a key in `initialize` authenticates; mint one with `grovefs apikey create`)|
      未被门禁的方法( initialize/ping 之外): {"id":25,"jsonrpc":"2.0","result":{}}
    鉴权后全量应答:
      {"id":1,"keys":["capabilities","instructions","protocolVersion","serverInfo"]}
      {"id":2,"keys":["tools"]}
      {"id":22,"err_code":-32602,"err":"missing required argument 'base'"}
      {"id":26,"err_code":-32601,"err":"method not found: no/such/method"}
      {"id":29,"err_code":-32002,"err":"no view 'nosuch' in /tmp/gft_gate (local files) (not a definition nor a physical view tree"}
    工具清单:
      10 个: list_views, define_derived_view, alias_view, fork_view, graft_view, remove_view, preview_rules, validate_definition, list_view_tree, get_view
    工具注解(readOnlyHint 等)样例:
  {"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true,"title":"List views"}
    initialize 能力面:
  {"protocolVersion":"2024-11-05","serverInfo":{"name":"grovefs","title":"GroveFS view governance","version":"0.1.0"},"has_instructions":3138}
    resources/list:
      grovefs://guide
      grovefs://views
      grovefs://view/keyed/tree
      grovefs://view/other/tree
      grovefs://view/mcpub/definition
      grovefs://view/mcpub/tree
    templates/list:
      grovefs://view/{name}/definition  view-definition
      grovefs://view/{name}/tree{?path,depth,max}  view-tree
    prompts/list:
      reshape-view args=2
      rotate-alias args=3
    completion 结果:
  {"completion":{"hasMore":false,"total":1,"values":["keyed"]}}
    服务端通知行(logging): 3 条
    协议错误路径逐条:
      {"id":26,"error":{"code":-32601,"message":"method not found: no/such/method"}}
      {"id":29,"error":{"code":-32002,"message":"no view 'nosuch' in /tmp/gft_gate (local files) (not a definition nor a physical view tree) — `list_views` names what exists"}}
    governance 拒绝走 isError 而非 JSON-RPC 错误层:
      {"id":10,"isError":null,"text":"defined pure alias 'mcpub' -> real view 'keyed'\nnote: ..."}
      {"id":12,"isError":null,"text":"grafted 1 entry (0 directories) from 'keyed' into 'mcpfork' at /: 1 shared object(s), 0 skipped\nnote: ..."}
    structuredContent 双载:
      {"has_structured":true}
    调用审计(apikey calls):
      2026-11-07T01:45:54.512547326+08:00 ... tools/call   get_view               no   missing string argument 'name' (tool 'get_view' requires: name)
      2026-11-07T01:45:54.497847304+08:00 ... tools/call   no_such_tool           no   unknown tool 'no_such_tool' (available: list_views, define_derived_view, ...)
  ```

- **结论**：通过。10 工具、资源/模板/提示/补全、服务端 logging（3 条通知）、未鉴权门禁、三种 JSON-RPC 错误码（-32601/-32602/-32002）与 `isError` 双载全部实测到；调用审计逐条带 ok/no 与原因。

### 3.11 条带放置（真的落在两台机器/两块盘）

- **命令**（`vm_battery_4.sh` E1）：挂载内 `dd` 写 200 MiB，取 `data_ref`，逐台 `find` 分块并求和；改写后 `drop_caches` 重读校 md5；另写 48 MiB 看分块数。
- **回显**：

  ```
  ===== E1 条带放置: 对象真的落在两台机器/两块盘上 =====
    写入 200MiB 文件: 209715200 字节
    元数据里的 data_ref: s2:16777216:62b9eb34-286d-4fd2-8703-ca5b22475aed  (格式 s<服务器数>:<条带字节>:<uuid>)
    条带几何: 209715200 / 16777216 = 13 个分块, 第 i 块放第 i%N 台
    gfs1 : /data/gfs2/grove -> 本文件分块 ...#0 16777216;...#10 16777216;...#12 8388608;...#2 16777216;...#4 16777216;...#6 16777216;...#8 16777216;
    gfs2 : /data/gfs2/grove -> 本文件分块 ...#1 16777216;...#11 16777216;...#3 16777216;...#5 16777216;...#7 16777216;...#9 16777216;
    分块总数/总字节(两台相加应等于文件大小): 13 块, 209715200 字节
      写回校验(已清 后重读): 7e560838af743f2900381b908fad98a0 vs 写入时 7e560838af743f2900381b908fad98a0 (一致 OK)
      48MiB 文件的分块分布: gfs1=2 个, gfs2=1 个
    同机双根的放置由 key 哈希决定(data-server --root a,b), 跳机由条带决定
  ```

- **结论**：通过。13 个分块奇偶分布在两机，总字节与文件大小精确相符（209715200）；清缓存后重读 md5 一致；同机两个数据根由 key 哈希分桶（回显中 `/data/gfs1/grove` 行本文件分块为空属正常，因该对象 13 块全部落在同名的 `/data/gfs2/grove` 根）。

### 3.12 跳端一致性与失效收敛窗口

- **回显**（E2）：

  ```
  ===== E2 跳端一致性与失效收敛窗口 =====
      [alice -> /tmp/gft_two: 已挂载]
    挂载 A=~/mnt, 挂载 B=/tmp/gft_two (同一 view, 同一 meta-server)
    A 侧写入 -> B 侧可见耗时: 0.004 s
    B 侧内容: converge-1793987395.096701079
    B 侧目录列举已含新名: 1
    B 写入 -> A 可见: from-B
    目录删除的收敛: A 侧已消失(OK)
    透镜定义改动的收敛(≤2s TTL):
      初版: conv.txt 不可见
      重定义后同挂载未重挂即收敛: dataX 出现耗时 0.012 s (活挂载自动取新形状)
  removed view definition 'e2lens' (underlying data untouched)
  ```

- **结论**：通过。两个活挂载间的文件/目录增删互可见（0.004 s / 0.009 s 量级）；透镜重定义在同一个已建立挂载上自动取新形状（0.012 s），无需重挂。

### 3.13 HA 主备切换

- **命令**（E3）：transient 起 `meta-server --role primary --listen 127.0.0.1:50171 --peer 127.0.0.1:50172` 与 `--role standby` 对，挂载只连备侧，`kill -9` 主侧，轮询自升，再重启旧主。
- **回显**：

  ```
  ===== E3 HA 主备切换 =====
      [hatest -> /tmp/gft_ha/mnt: 已挂载]
    只连备库的挂载: 已挂载
    备侧已复制: before-failover
    备侧写必须被拒(standby 只读): rc=1 报错=[bash: line 83: /tmp/gft_ha/sb/nope.txt: Input/output error] 文件=不存在
    杀死主库(异步快照复制, RPO≈500ms):
    备自升主耗时(stop -> promoted): 3.14 s 共轮询 15 次
    备库日志(仅本轮): Nov 07 01:50:02 gfs-client grovefs[29487]: meta HA: promoted to primary at epoch 1|
    故障后第一次写: rc=0 报错=[] 耗时 0.76 s
    1s 后重试写: rc=0 报错=[] w3.txt=retry-1
    第二次重试写: rc=0 报错=[] w4.txt=retry-2
    切换前已复制的数据仍在: before-failover
    切换后写入可读: after-failover
    重启旧主 -> 必须以备身份回归(不得双主):
      旧主日志(仅本轮): Nov 07 01:50:04 ... meta-server listening on 127.0.0.1:50171 (base /tmp/gft_ha/A, role Primary, peer 127.0.0.1:50172)|Nov 07 01:50:05 ... meta HA: demoted to standby (peer epoch 1)|
      现有内容: w1.txt w2.txt w3.txt w4.txt 
    HA 栈已拆: 残留 HA 进程 0 个, 监听 0 个
  ```

- **结论**：通过。备侧可见已复制数据、写被拒（`EIO`）、自升主（epoch 1）、旧主回归自动降为 standby（无双主）、拆分无残留。切换瞬间的首次写本轮为 `rc=0`（上一轮曾观察到一次 `EIO` 且文件未生成），属转接窗口内的瞬时失败，不作稳定性断言（§5-9）。

### 3.14 带宽限速

- **回显**（E4）：

  ```
  ===== E4 带宽限速 =====
    8MiB/s 下顺序读 24MiB:
      实测 2.99 s -> 8.0 MiB/s (限速 8 MiB/s, 期望 2.5~4.5 s)
    8MiB/s 下写 24MiB:
      实测 3.13 s -> 7.7 MiB/s
    非法速率值的拒绝原文:
      单元结局: inactive ; 挂载: 不在
      日志: ... gft-bwbad.service: Main process exited, code=exited, status=2/INVALIDARGUMENT|...
    只读挂载接受 --bw-write 但仍拒写:
      [alice -> /tmp/gft_bro: 已挂载]
  bash: line 135: /tmp/gft_bro/z: Read-only file system
      挂载: 已挂载 ; 写: 被拒
  ```

- **结论**：通过。实测读 8.0 MiB/s、写 7.7 MiB/s，与 `--bw-read/--bw-write 8M` 一致；非法值（`12Z`）以 `status=2/INVALIDARGUMENT` 退出且不挂载。

### 3.15 `--stats` 遥测
- **回显**（E5）：

  ```
  ===== E5 --stats 遥测 =====
    --stats-file 内容(前 12 行):
      GroveFS per-stage latency telemetry  (durations in µs)
        diagnostic tooling only — not a throughput/scalability claim
        snapshot @ 2026-11-07 01:50:23
      
      [op]
        read.op_total              ops=96       p50=527      p90=779      p99=2927      max=2927      mean=624      total_s=0.06    bytes=0
        write.op_total             ops=1        p50=1151     p90=1151     p99=1151      max=1151      mean=1148     total_s=0.00    bytes=0
        read.backend_total         ops=96       p50=453      p90=679      p99=2799      max=2799      mean=545      total_s=0.05    bytes=25165824
        write.touch_mtime          ops=1        p50=795      p90=795      p99=795       max=795       mean=794      total_s=0.00    bytes=0
    journal 里的遥测行:
      ... grovefs[29841]: telemetry: --stats on, periodic report -> /tmp/gft_stats.log
    未加 --stats 时不产出遥测: 0 行
    GROVEFS_STATS 环境变量兼底存在: 3 处命中
  ```

- **结论**：通过。周期表头、分阶段 p50/p90/p99/max/mean/total_s/bytes 齐备；未开 `--stats` 时 0 行输出；`GROVEFS_STATS` 在二进制中有 3 处命中。显示口径问题：`read.op_total` 与 `write.op_total` 的 `bytes=0`（而 `read.backend_total` 是 25165824）属阶段定义使然，但同一张表里混现两种字节口径（§6-9）。

### 3.16 集群收尾复核

见 §1.3 的 E6 段回显：部署态两个单元 `enabled/active`、残留 transient 单元 0、挂载 1（即 `grovefs-mount-alice`）、非 systemd 管的 grovefs 进程 0。

### 3.17 全部探针跑完后的现场清场复核

- **目的**：证明多轮丢对象/别名探针（曾 `cp`+`rm` 搬走分块、`systemctl restart` 过两台 data-server、创建过 transient 单元）之后，集群回到纯 systemd 部署态且无任何测试残留。
- **命令**：`bash .build-bin/vm_cleanup_verify.sh`（先执行清场动作，再逐项复核；回显 `/tmp/vm_evidence/cleanup_verify.log`）
- **回显**：

  ```
  ##### [gfs-client] 现场清场复核 #####
  -- 清场动作: 删除探针/测试遗留目录与临时文件
  -- 部署态单元: grovefs-meta-server.service enabled enabled|grovefs-mount-alice.service enabled enabled|
  -- 探针 transient 单元残留(应为空): 
  -- grovefs 进程数(应为 2): 2
  -- 非 systemd 管的 grovefs 进程(应为空):
  -- 挂载清单: grovefs:alice /home/hanjun/mnt|
  -- 探针目录残留 /tmp/gft_*: ls: cannot access '/tmp/gft_*': No such file or directory 
  -- 探针存储根/临时文件残留: ls: cannot access '/home/hanjun/gftstore': No such file or directory ls: cannot access '/home/hanjun/gftprobe': No such file or directory ls: cannot access '/tmp/b2.rules': No such file or directory ls: cannot access '/tmp/bogus.rules': No such file or directory ls: cannot access '/tmp/vm_b*.out': No such file or directory 
  -- 视图定义清单: no view definitions in the control plane store /home/hanjun/meta (local files) (every view name maps to its own tree)|base view 'alice' -> own tree meta/alice.db (read-write; deletes free a shared object only when no view holds it)|view keys: `grovefs apikey --db-path /home/hanjun/meta list [--view NAME]` (add --view to see only that view's keys)|
  -- 基视图内容(应为 P0 记录的水位基线): [datasets lockf onlyme.txt private vis.txt ]
  -- datasets: [big.bin t1.txt ]
  -- du 复原对账(基线 64.0M): 64.0M	/

  ##### [gfs1] 分块复原与残留复核 #####
  -- 对象数: 31
  -- 数据根用量: 34M=/data/gfs1/grove 1.1M=/data/gfs2/grove 
  -- /dev/shm 搬移残留(*.obj): 0
  -- 单元: active NRestarts=0
  -- 二进制 md5: 2fa8ff6498a3b0390c796c79cad8eaab

  ##### [gfs2] 分块复原与残留复核 #####
  -- 对象数: 2
  -- 数据根用量: 34M=/data/gfs1/grove 1.1M=/data/gfs2/grove 
  -- /dev/shm 搬移残留(*.obj): 0
  -- 单元: active NRestarts=0
  -- 二进制 md5: 2fa8ff6498a3b0390c796c79cad8eaab
  ```

- **结论**：通过。要点四项：① 探针搬走/删除的分块均已复原（`du --total` 回到 §3.0 与 §3.6 的基线 `64.0M`，`/dev/shm` 无残留，两台单元 `active`）；② 无任何 transient 单元、无探针目录/存储根/规则文件残留；③ grovefs 进程只有 systemd 管的 2 个，挂载只有 `grovefs:alice`；④ 视图定义只剩基视图 `alice`，实体树只剩 `alice.db`/`api_keys.db`/`refs.db`。
  两处数值口径如实说明：`--对象数` 是用 `find` 统计的**磁盘文件数**，普通对象与条带分块（`<uuid>#<i>`）混在一起，且两个数据根的使用量按 key 哈希分布（故一台 34M/31 个、另一台 1.1M/2 个），该计数不能当作“条带对象数”读；基视图内容与 §3.0 的 P0 基线 `[a1b.txt datasets onlyme.txt private vis.txt ]` 相比，少了 `a1b.txt`（在 §3.4.4 的 fork 用例里被 `unlink`）、多了 `lockf`（§3.2 的锁用例创建）；本轮另把 §3.4.4/§3.8 遗留在基视图里的 `b4.txt`/`b4del.txt` 两个测试名删掉（因此第一台的计数从 33 降到 31），删除后 `du --total` 仍为 `64.0M`，与 §3.0/§3.6 的水位一致。
  首次运行该脚本时还现场删掉了上一轮遗留的 `/tmp/gft_pva`、`/tmp/gft_src`、`/tmp/gft_stats.log`、`/home/hanjun/gftstore`（日志首行 `removed ...`），上面贴的是清场完成后的终态回显。

### 3.18 本地挂载（`--home`）与远程挂载（`--meta` + `--data`）的对照

- **目的**：任务书要求“本地与远程挂载”都实测。远程挂载是部署态的 `grovefs-mount-alice` 单元（`--meta` 走 meta-server，字节走 `--data` 条带），它不依赖任何本地存储根；本条目补的是另一种形态——元数据与 `blocks/` 都在本机的 `mount --home`（不经 meta-server）。
- **命令**（`bash .build-bin/vm_probe_lostlocal.sh` 的挂载段；另跑了一次现场形式对账探针，回显 `/tmp/vm_evidence/probe_import_forms.log`）：

  ```bash
  P=$HOME/gftprobe
  grovefs --home "$P" init --view probe
  grovefs import --db-path "$P" --view probe --source /tmp/gft_probe_src
  sudo systemd-run --unit=gft-probe --collect --quiet -p User=hanjun \
    /usr/local/bin/grovefs mount --view probe --home "$P" -m /tmp/gft_probe --foreground
  ls /tmp/gft_probe /tmp/gft_probe/gft_probe_src/docs
  ```

- **回显**（`/tmp/vm_evidence/probe_lostlocal.log`，该次探针的完整前置与挂载行）：

  ```
  ===== L1b 本地视图: 对象 unlink 后的挂载读 =====
    init: initialized view 'probe' at /home/hanjun/gftprobe/meta/probe.db
    import: imported into view 'probe' under /gft_probe_src: 1 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
    du -b --total 基线: 1000000	/|
    挂载: 已挂载
    挂载内清单: /tmp/gft_probe: gft_probe_src  /tmp/gft_probe/gft_probe_src/docs: one.bin two.bin 
    对象 92d83c70-9a5e-4635-b7b0-387cb1ffccad 的落点: /home/hanjun/gftprobe/blocks/8d/92d83c70-9a5e-4635-b7b0-387cb1ffccad (700000 字节)
    unlink 前读: rc=0 md5=257ccc870d737ac121380e61c5a7bc09
  ```

- **结论**：通过。本地挂载与远程挂载都可用：同一视图在 `--home` 形态下字节落在本机 `$P/blocks/<2hex>/<key>`（回显给出了完整落点路径），目录项与读均正常；而在部署态的 `--meta`+`--data` 形态下，同一个名字的对象字节在两台 data-server 上（§3.11）。两种形态的字节来源不同，因此 du 也必须用不同的数据源表达（§3.6.1 vs §3.6.2）。
  一个实测踩到的命令形态错误已单独重跑取证（`/tmp/vm_evidence/probe_import_forms.log`）：

  ```
  -- 正确形式: grovefs import --db-path D --view V --source DIR
    initialized view 'imp1' at /home/hanjun/gftimp/meta/imp1.db
    imported into view 'imp1' under /gft_imp_src: 1 dir(s), 2 object(s) (2 hardlinked zero-copy, 0 copied), 0 skipped, 0 failed
  -- 错误形式(view import --in 指目录)的报错原文:
    Error: "cannot read /tmp/gft_imp_src: Is a directory (os error 21)"
  -- 同名命令的分工: view export 产物才能喂 view import
    export: exported view 'imp1' metadata: 5 node(s), 2 blob reference(s) -> /tmp/gft_imp.jsonl
    export: note: metadata only — copy the store's `blocks/` tree (the whole data root) as well; blobs are located by their data_ref key, not by any host path
    import: imported view 'imp3' from /tmp/gft_imp.jsonl: 5 node(s), 2 blob reference(s); cross-view ledger rebuilt
    import: note: content bytes are not in the export — make sure `blocks/` from the source store is present under the target data root before mounting
  ```

  即顶层 `grovefs import --source DIR` 是就地登记目录，`grovefs view import --in FILE` 只接受 `view export` 产出的清单文件（§3.5、§3.7）；拒接原文不含“下一步该敲哪条”的提示，已写入 §6-6 的建议。该次探针回显里的 `sqlite3.OperationalError: no such column: parent` 是探针自身查目录树的 SQL 列名写错（与产品无关），不影响上面任何一项断言，属取证脚本缺陷（已归类到 §5 末尾那一段）。

---

## 4 仓库侧回归

### 4.1 `cargo build --release` / `cargo clippy --release --all-targets`

- **命令**：`export PKG_CONFIG="$PWD/.build-bin/pkg-config"; cargo build --release; cargo clippy --release --all-targets`
- **回显**（`/tmp/repo_build.log`、`/tmp/repo_clippy.log`）：

  ```
     Compiling grovefs v0.1.0 (/data/grovefs/grovefs)
      Finished `release` profile [optimized] target(s) in 55.99s
  ```

  ```
      Checking grovefs v0.1.0 (/data/grovefs/grovefs)
      Finished `release` profile [optimized] target(s) in 2.65s
  ```

- **结论**：通过，clippy 零告警（本轮修的 `src/du.rs` `type_complexity` 已改为命名别名 `type Probes = Vec<(usize, io::Result<u64>)>;`）。

### 4.2 `cargo test --release --tests`

- **命令**：`cargo test --release --tests`
- **回显**（`/tmp/repo_cargo_test.log` 的各套件汇总行，全部 `ok`，共 210 个 `test …` 行）：

  ```
  test result: ok. 132 passed; 0 failed; …      # lib 单测（含 §6-2 len_strict 新测、du 分类与条带错序新测）
  test result: ok. 3 passed; 0 failed; …                   # tests/du.rs
  test result: ok. 11 passed; 0 failed; …                  # du 模块单测（含新增 5 个分类/门控用例）
  test result: ok. 11 passed; 0 failed; …
  test result: ok. 14 passed; 0 failed; …                  # sharded 模块单测（含错序/空对象 + §6-2 len_strict 三态用例）
  test result: ok. 10 passed; 0 failed; …
  test result: ok. 1 passed; 0 failed; …                   # 集成套件
  ```

  其中与本轮修复直接相关的单测：

  ```
  # du 分类与拓扑门控
  test a_root_without_blocks_is_refused ... ok
  test an_object_that_is_not_found_is_announced_not_silently_zero ... ok
  test an_unconfirmed_answer_is_not_a_missing_object ... ok
  test the_store_root_alone_is_enough_to_count_bytes ... ok
  # 条带错序检测（chunk0_elsewhere）
  test a_reversed_server_list_is_reported_not_read_as_empty ... ok
  test an_empty_striped_object_still_measures_zero ... ok
  # §6-2 严格长度三态（首块缺失/中间空洞报错，短尾不报错）
  test len_strict_flags_interior_and_leading_gaps_but_not_a_short_tail ... ok
  ```

- **结论**：通过（无 failed；`fanout_scale` 一条 `ignored` 为原有设定）。历轮新增：du 5 个分类/门控纯逻辑单测、sharded 错序 2 个单测、tests/du 桶碰撞修正；本轮新增 §6-2 `len_strict` 三态单测。

### 4.3 `bash .build-bin/run_all.sh`

- **命令**：`bash .build-bin/run_all.sh`
- **回显**（本轮重跑尾屏；首屏套件列表与下表汇总行不变）：

  ```
  ==================================================================
   grovefs test board   (2026-11-07 13:34:05)
   suites: build unit posix edge e2e remote errors resilience concurrent metasrv ha striping locks freshness disp_fresh layout fork viewrm gc bwlimit migrate import govcheck govtime grovesci mcp apikey mountkey config pg pg_parallel control bench
   logs:   .build-bin/logs/20261107-133405
  ==================================================================
  ### build — cargo build --release
    OK
  ### unit — cargo test --release
    test result: ok. 132 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
  ...
    ha      PASS
    striping PASS
    locks   PASS
    freshness PASS
    disp_fresh PASS
    layout  PASS
    fork    PASS
    viewrm  PASS
    gc      PASS
    bwlimit PASS
    migrate PASS
    import  PASS
    govcheck PASS
    govtime PASS
    grovesci PASS
    mcp     PASS
    apikey  PASS
    mountkey PASS
    config  PASS
    pg      PASS
    pg_parallel PASS
    control PASS
    bench   INFO
  ==================================================================
  RESULT: GREEN (all gating suites passed)
  ```

- **结论**：通过（`bench` 为 INFO 非门禁项；§6-2 改动后全板重跑仍 GREEN）。

### 4.4 `bash skills/grovefs-docs/scripts/doccheck.sh`

- **命令**：`bash skills/grovefs-docs/scripts/doccheck.sh`
- **回显**（`/tmp/repo_doccheck2.log` 关键段）：

  ```
  == 1. 变更史叙述扫描(应为 0 需处理项) ==
    [放行] /data/grovefs/grovefs/README.md:859:**文档口径：只陈述当前版本的行为。** …
    [放行] /data/grovefs/grovefs/USAGE.md:2460:**文档口径**(README / USAGE / docs.html 共用一条规矩):只陈述当前版本的行为——不…
    [放行] /data/grovefs/grovefs/docs.html:3154:    <div class="ok"><b>文档口径:只陈述当前版本的行为。</b>不记录已移除的参数/…
  == 2. 参数对账(文档里的 --xxx 是否真存在于 --help) ==
    真实参数 68 个,文档参数 87 个,差集 18 项(其中非外部工具/非负空间 0 项)
  == 3. docs.html 标签闭合 ==
    unclosed: []
    errors: none
  == 4. 新增用字校验(文档历史上没出现过的汉字 = 逐个确认是否错字) ==
    本次新增行里的生面孔汉字: 克寸尺隆
  == 结论 ==
    OK
  ```

- **结论**：通过。退出码 0；“非外部工具、非占位”的孤立参数 0 项；新增汉字“克/寸/尺/隆”来自「克隆/尺寸」等正常用词，非错字。本报告自身不列入文档口径检查范围。

---

## 5 遗留项清单

| # | 条目 | 实测现象 | 机制定位 | 处置 |
|---|---|---|---|---|
| 5-1 | `chmod` 权限位不强制 | `chmod 444` 后同 uid 写入 `rc=0` 并落盘 | 挂载不做 mode 强制（当前版本行为） | 未修，列入遗留；建议写入 USAGE/docs 的能力边界表 |
| 5-2 | 稀疏洞不省盘 | `st_size=1048576` 且 `st_blocks*512=1048576` | 后端按完整对象存字节 | 未修，列入遗留 |
| 5-3 | **POSIX 记录锁 close/退出隐式释放时漏清，阻塞等待方不唤醒** | 修复前的对照实验：显式 `F_UNLCK` 释放 → 等待方 3.01s 唤醒；只 `close(fd)`/进程退出释放 → 新进程非阻塞加锁仍 `EACCES`（锁泄漏），阻塞等待方卡 `request_wait_answer` 直至 600s 上限 | 根因已定位（§3.2）：`fuser` 0.18 `Release::lock_owner()` 仅在 `FUSE_RELEASE_FLOCK_UNLOCK`（只有 flock(2) 路径置位）时返回 `Some`；POSIX 锁走 close 时返回 `None`，`release_locks_of_owner` 不触发，幽灵锁行又被按 sid 的续租心跳（`LOCK_LEASE_SECS=15`）无限 刷新 | **已修复**（release `d2a3b262`，实机复验）：`setlk` 按 `(fh→ino,owner)` 记账、`release` 在末次锁定描述符关闭时用全区间 Unlock 精确回收；close 后新进程加锁由 `EACCES` 变 `GRANTED`、阻塞 `F_SETLKW` 于持有者 close 后 `2.74s` 唤醒（见 §3.2、§6-2a） |
| 5-4 | `view rm --base` 在数据根不可用时只给裸 `Permission denied` | 修复前：`Error: Os { code: 13, kind: PermissionDenied, message: "Permission denied" }`，不报路径、不给下一步，且发生在元数据树已删之后 | 回收阶段直接上抛 `io::Error`，未经包装 | **已修复**（release `d2a3b262`，实机复验）：回收 `open_local` 失败时 `map_err` 补上数据根路径、视图名与“树已删/字节未收”上下文及离线 `gc` 提示；连同 §5-12 改走 Display，实机报 `error: cannot open data root …`（见 §6-4） |
| 5-5 | **条带对象真丢分块时 du 仍静默短报（本类中数据源真缺字节的形态；错配/瞬时故障形态已修）** | 修复前探针：中间分块缺失 `67108869 → 16777221`（无 warning）；首分块真缺：`--total` 给极小值 | `ShardedBackend::len` 假定分块连续，指数+二分把“第一个缺失分块”当 EOF（chunk#0 真缺时 `chunk0_elsewhere` 在其它成员也探不到 `{base}#0` → `Ok(0)`） | **已修复（du 诊断路径）**：`du` 改走 `len_strict`，二分停在首个缺失块后多探一块、首块缺失而后续存在也同样检测，命中即归入独立 `damaged` 计数并单独 warning（字节不计入总数）；本机端到端复验见 §3.6.5，单测 `len_strict_flags_interior_and_leading_gaps_but_not_a_short_tail`。挂载侧静默形态（stat size 短/0、读无 EIO）属 §5-6，未改（len 是 getattr 热路径且 `build_attr` 会回退元数据 size，改 len 既拖热路径又不会让挂载出声）；同一命令对“列表长度不符/倒置/超时拒连”的既有拒绝与硬报错不变（见 §3.6.2） |
| 5-6 | 非条带路径的挂载读丢失对象也不出声 | 重挂后读丢失对象：`rc=0`、`stat size=0`（无 `EIO`） | 同上：`len` 给 0 而非错误 | 未修，列入遗留 |
| 5-7 | `apikey set-ro` 回到 rw 后同一挂载仍拒写 | `is now rw (read-write)` 后紧接追加写 `errno=30` | 服务端复检窗口/客户端缓存时机，文档声称 ≤1s | 未修，列入遗留（建议回显明记生效时刻而非窗口上限） |
| 5-8 | `set-ro` 后回读内容尾部多 7 个零字节 | `'first\n\x00\x00\x00\x00\x00\x00\x00'` | 本轮探针用固定长缓冲区读回所致，非产品写入；作为取证脚本缺陷记录 | 已确认非产品缺陷（取证手段问题） |
| 5-9 | HA 切换瞬间首次写存在瞬时 `EIO` | 上一轮观察到一次 `EIO` 且文件未生成；本轮为 `rc=0`（0.76 s） | 转接窗口内旧主连接失效 | 状态相关，不作稳定性断言；建议文档写明重试要求 |
| 5-10 | `view alias` 接受指向不存在视图的目标 | 修复前：`defined pure alias 'x-dangle' -> real view 'no-such-tree'`，直到挂载才拒 | 定义时无树存在性校验 | **已修复**（release `d2a3b262`，实机复验）：`alias_view` 在排除别名/派生/分叉目标后，用 `store.has_physical(real)` 检查目标物理树（SQLite 查 `<base>/meta/<view>.db`、PG 查 schema）是否存在，不存在则拒绝并给出可敲的 create/import 命令（接 §6-5） |
| 5-11 | `grovefs import` 不接受 `--meta`/`--data`，无法向远端/条带存储批量导入 | `grovefs import --meta … --data …` 报 Usage；只能导入到本地 `--db-path`/`--home` 存储根 | import 子命令未暴露远端定位参数（与 du/view import 的远端能力不对称） | 未修，列入遗留（接口缺口；需先本地导入再 `view export/import` 迁机） |
| 5-12 | CLI 致命错误以 Debug 形式打印 | 修复前：挂载单元在 data-server 未起时报 `Error: Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }`（不指哪个地址）；du 报错带外层双引号 `Error: "…"` | `fn main() -> Result<(), Box<dyn Error>>` 使错误走 Debug 而非 Display | **已修复**（release `d2a3b262`，实机复验）：`main()` 拆出 `real_main()`、致命错误改走 Display（`error: <人话>`，与 clap 前缀一致）；meta/data/du 连接失败均点名 `cannot reach meta-server/data-server <addr>`（保留 `ErrorKind` 不影响 failover，见 §6-1） |

已当场修正的取证脚本缺陷（不属于产品遗留）：C5 漏 `mv`、误挑 0 字节分块；B4 基视图残留 `newdir`/`b4-renamed.txt` 导致结构写探针假失败；`> target 2>/tmp/err` 重定向顺序导致报错为空；battery_1 输出文件被自身 `rm -rf /tmp/gft_*` 删成 `(deleted)` 导致 ssh 挂死；`vm_audit.sh` 的嵌套单引号、`cut` 无 sudo、把整个多行 env 值当 grep pattern 导致的“单元文本命中密钥 4 处”假阳性、崩溃拉起验证的 `pgrep` 模式被吞。

本轮专业复核当场修复的产品缺陷（均已入 release `d2a3b262` 并在实机复验，见 §3.6.2/§3.2）：

- **du 把任何错误当“对象不在数据源”**（`src/du.rs`）：旧逻辑将超时/拒连/EIO/布局错配一律计入 `lost` 并静默少算。现区分 `confirmed_missing`（仅 `NotFound`）与未确认：未确认一律硬报错不出数；新增 `check_server_count` 探测前门控，列表过长/过短按方向分别给建议（旧文案对“多给了”却建议“补齐全部”——方向相反）。新增 5 个纯逻辑单测。
- **反序列化列表使整树静默报 `0B`**（`src/backend/sharded.rs`）：新增 `chunk0_elsewhere` 用 `{uuid}#0` 归属做确定性错序检测（顺序错配的唯一签名），stat/du 层报错；`order_verified: AtomicBool` 保证健康树/空对象零额外探测成本。新增 2 个单测（反序列必须报错、空条带对象仍为 0）。
- **tests/du.rs 桶碰撞偶发失败**：`objects()` 由“每桶取一”改为遍历全桶 + sort，消除 ~1/256 抖动。
- **`USAGE.md` 完全没有 `grovefs du` 的文档缺口**：补命令速查行 + §12.6 用法与拒绝语义；`docs.html` 同步为“顺序错位会被 stat 检出，read 路径仍只按分块定位”。
- **POSIX 锁 close/exit 隐式释放泄漏**（`src/fs_impl.rs`）：`setlk` 授予时按 `(fh→ino,owner)` 记账、`release(fh)` 在末次锁定描述符关闭时用全区间 `lock_op(Unlock)` 精确回收（`note_posix_lock`/`drop_posix_lock_on_release`）；不改 sid 续租模型。配套新增 `test_locks.sh` §4c 集成用例（close 后新进程加锁 + 阻塞唤醒延时），本地 17/17、实机复验通过。

未当场修复、仅确证并记录的：§5-5 原“真丢字节”形态中的挂载侧静默（归 §5-6）；du 侧已于后续轮按 §6-2 方案修复（见 §3.6.5）。

---

## 6 后续显示优化建议

以下每条都给出可执行的改法（文件 + 具体输出），不只是评论。标★的与§5 遗留项直接对应。

1. ★**`view list` 首行与列表自相矛盾——本轮已实现**。无定义时先打 `no view definitions in the control plane store …`，紧接着又列出 `base view 'alice' -> …`。**已采用的改法**：`src/mcp.rs` 的 `list_views_text` 先算出 bare（基视图）数量，`defined` 为空时按 bare 是否为空分两支——两者皆空才打“无任何视图定义”，仅 `defined` 空而 bare 非空时打“无别名/分叉/透镜定义 —— 下面 N 个基视图各映射自己的树”，不再与列表矛盾。实机复验见 §2.3。
2. ★**条带对象丢分块时的静默 0 / 静默短报（最严重的一条）——本轮已实现（du 诊断路径）**。原建议分两步（给 `len` 加严、chunk#0 缺失改报 ENOENT）；实测取证后**采用了不同的改法**：取证发现 `build_attr`（`src/fs_impl.rs`）对每个普通文件的 getattr 都调 `backend.len(k).unwrap_or(node.size)` —— len 是热路径且错误会被静默回退到元数据 size，直接改 `len()` 既拖慢 stat/readdirplus 又不会让挂载侧真正出声。故：
   - `src/backend/sharded.rs` 新增 `len_with(key, strict)`：普通 `len` 委托 `strict=false`（零行为/零成本变化）；`len_strict()`（仅 `du` 调用）在二分停在首个缺失块后多探一块（探 `hi+1`）、并检测首块缺失而后续存在/首块短报而 #1 存在两种形态，命中即返回带稳定前缀 `grovefs:striped-damaged:` 的 `io::Error::other`（`is_damaged`/`damage_reason` 供调用方类型化识别，不做散文串匹配）。
   - `src/du.rs` 分类改为三档：`NotFound` → lost（既有 warning）；`is_damaged` → **独立 `damaged` 计数 + 独立 warning 行**（原句引用首个损坏原因，字节不计入总数）——损坏不再被读成“对象不在数据源/该传 --data-root”；其它 Err → 仍硬报错不出数。本地单根路径永不跨机条带，damaged 恒 0。
   - 诚实边界：strict 只捕获“搜索停在缺失块、其后仍有存在块”的短报症状；完全埋在连续存在块内部的洞不改变测得尺寸，属读路径完整性（错字节读），需 O(分块数) 全扫，本轮不做。挂载侧静默形态（§5-6）按取舍保持不动。
   - 文档同步：`docs.html` 的 du 字节来源 note 与“会不会出声”表(b)、`USAGE.md` §12.6 表(b) 的条带丢块行同步为新行为（du 出声：damaged warning；挂载读仍不出声）。本机端到端复验（中间块出声/尾块不误报）见 §3.6.5；单测 + `cargo test --release` 全绿见 §4.2。
2a. ★**POSIX 记录锁 close/退出隐式释放泄漏（§5-3）——本轮已实现**。原建议：既然 `fuser` 0.18 仅在 `FUSE_RELEASE_FLOCK_UNLOCK`（只有 flock(2) 路径置位）时向 `release` 交 `lock_owner`，POSIX 锁走 close 拿到 `None`，旧回收不触发。**已采用的改法**：在 `setlk`（已不再忽略 `fh`）按 `(fh → ino, owner)` 记账，`release(fh)` 据此在某个 `(ino,owner)` 的最后一个锁定描述符关闭时，用一次全区间 `lock_op(..., Unlock, ...)` 精确删除其行（`src/fs_impl.rs` 的 `note_posix_lock`/`drop_posix_lock_on_release`）。采用“删除行”而非“收窄 `renew_locks` 到仍打开的描述符”，是为了不动 meta 协议与跨机续租模型：行一旦删除，按 sid 的续租心跳自然不再刷新它，效果与“只刷新存活行”等价，且失败时仍退化为一个租期内自然过期。已配套集成测（`test_locks.sh` §4c：跨进程持有/释放 + 阻塞唤醒延时），本地 17/17、三机实机复验通过（close 后 `GRANTED`、阻塞 `2.74s` 唤醒）。
3. ★**fork/只读 key 的 `EROFS` 在 shell 里假成功**。`>>` 追加 `rc=0` 而字节未落盘（§3.4.4、§3.9），用户只会看到“写了但没变”。建议：把内容写门从 `fsync` 提前到首次 `write` 就返回 `EROFS`（至少对 `O_WRONLY`/`O_APPEND` 的开写路径做前置检查）；若保留现状，`USAGE.md`/`docs.html` 的 fork 语义行必须明写：“追加写的失败只在 fsync/close 可见，shell 重定向会报 0” 并给出验证命令（`sync; < cmd; echo $?` 或 `python3 -c` 逐 open 探针）。
4. ★**`view rm --base` 的 `Permission denied` 裸文案——本轮已实现**。原建议：按同文件内其他守卫的写法包上上下文。**已采用的改法**：`src/main.rs` 的回收阶段 `backend::open_local(&droot)` 失败时 `map_err` 补上“cannot open data root <路径> to free view '<name>' objects … 元数据树 meta/'<name>'.db 已删除 … 可用 `grovefs gc --db-path … --data-root …` 离线回收”的上下文，把“树已删、字节未收”分开陈述并给出下一条可执行命令。连同 §5-12 的 Display 改动，实机不再报裸 `Permission denied`（见 §2.3）。
5. ★**提示里的占位符未替换 + 悬空目标校验——均已实现**（§6-5 文案 + §5-10 行为）。原建议：`note: no local <db-path>/meta/pv.db yet` 与 meta-server 错误里的 `--db-path <db-path>`/`--view <name>` 应填实值；`view alias` 应在定义时检查目标物理树存在。**已采用的改法**：（文案）`src/mcp.rs` 的 `base_tree_missing_note` 用 `self.db(view).display()`+`self.base.display()` 填实树路径与存储根；`src/remote/meta/server_ext.rs` 的“view does not exist”create-hint 用 `rt.base.display()`+具体 `view` 填实（file/PG 两分支），配套收紧 `tests/meta_roundtrip.rs` 断言到具体视图名。（行为）`src/mcp.rs` 的 `alias_view` 在排除别名/派生/分叉目标后新增 `has_physical(real)` 硬拒绝悬空目标，错误文案复用上述已填实的 note。实机复验见 §2.3。
6. ★**拒绝文案的“下一步命令”完备性巡检——du 部分本轮已实现**。本轮已对上文案的命令均为好例（`--skip-existing`、`--replace`、`--retire`、`--force`、`umount -m`）。**已采用的改法**：`src/du.rs` 的 `path not found: {}` 现回显规范化后的视图相对路径（`normalize_path` 会自动补前导 `/`）并提示 `--path takes a view-relative path such as /<top-dir>/...`；`gather_via_meta` 连不上 meta-server 时点名地址。其余中，`gc` 的 `objects: 0 (0 B)` 行已于本轮 §6-8 改掉（见下条）；`apikey set-ro` 生效时点（§5-7）仍未改。
7. **单位与计数口径——本轮复核定性：属取证脚本产物，非产品输出，不动**。`import` 同时给 `blocks/ 路径数: 8` 与 `distinct inode 数: 4`、`holders 台账: 8,8` 这类裸 SQL 元组，经定位均来自 `.build-bin/` 取证/编排脚本的 `psql`/`find` 探针输出，不是 `grovefs` 产品命令的打印。产品 `grovefs import` 的汇总行本就统一且人话：`imported into view 'X' under /: N dir(s), M object(s) (H hardlinked zero-copy, C copied), S skipped, F failed`；`du` 默认后缀已是 `K/M/G`。故本项无需产品侧改动。
8. ★**`gc` 对在用存储的输出区分“无字节”与“无垃圾”——本轮已实现**（release `c8d68580`，实机复验见 §2.3）。原建议：`striped layout(s) > 0` 且 `objects == 0` 时不应让裸 `0` 读成“无字节”，且不应邀请对 0 字节重跑 `--apply`。**已采用的改法**（`src/main.rs` 的 `print_gc_report`）：当 `objects == 0 && striped_layouts > 0` 时，objects 行改打 `objects: 0 local block(s) — N striped layout(s) place their chunks on data-servers, which gc does not audit (use \`grovefs du\` for their bytes)`；dry-run 汇总保留稳定前缀 `dry run: nothing deleted.`，再按三态给尾句——有可回收项才邀请 `--apply`，`orphans` 为空打 `No unreferenced objects were found.`，全在宽限期内打 `N orphan(s) are all within the <grace> grace; re-run after it elapses.`。实机条带 meta 存储 `gc` 回显见 §2.3，`test_gc.sh` 41/0 仍绿（clean-store 用例断言的 `dry run: nothing deleted` 前缀未破）。
9. ★**`--stats` 表的字节口径——本轮已实现**（表头说明方案，不改定宽列；release `c8d68580`，本机挂载复验）。原建议：`read.op_total bytes=0` 与 `read.backend_total bytes=…` 并存易误读为“没读到数据”；`p50=0` 这类静默 0 易误读为漏采样。**已采用的改法**（`src/stats.rs` 的 `report_text` 表头，未动表格列宽以免破坏对齐与 CSV）：新增两行——`bytes= is tallied only on the backend-boundary stages (read/write.backend_total, io.*); a 0 there is expected on op_total lines and means "not counted at this stage", not "no data"`、`a latency of 0 is sub-µs (below the histogram's µs resolution), not a missing sample`。同步 `docs.html` §8 的真实回显样本与 `USAGE.md` §8.1 口径说明。`test_stats.sh` E2E 通过（表头 `per-stage latency telemetry` 断言未受影响）。
10. **`ls -d /data/gfs*/grove` 的展示歧义**。两台机器上都存在 `/data/gfs1/grove` 与 `/data/gfs2/grove` 两个同名目录（分块落点看似“在 gfs1 上”实际在 gfs2 的盘），本轮 E1/D2 回显已造成多次误读。建议：数据根名称从主机名推导（`/data/<host>/grove`）或在 `data-server --root` 的启动日志里逐根打一行 `root <path> on <hostname>`；`USAGE.md` 的部署示例需明写“每台两个根目录同名属预期”并推荐异名方案。
11. **静默 0 值的通用约束**。建议约定：任何统计命令的 0 值必须区分三种情况——真零、无法测（数据源不对）、部分丢失，分别给 `0`、`0 (not measurable here: …)`、`<N> (excluding M lost object(s))`。本轮命中的四处：`du --total: 55B`、`stat size=0`、`gc objects: 0 (0 B)`、`--stats bytes=0`。
12. ★**`docs.html`/`USAGE.md` 两张运维语义表——本轮已实现**（纯文档，无代码改动）。原建议：添加（a）fork/透镜/别名/key-ro 四种“写意图”的失败时点对比，（b）“对象丢失时各命令的声音”矩阵。**已采用的改法**：（a）写入 `docs.html` §12“报错对照”后（含逐 `open` 探针 errno 的 `<pre>` 与“为何截断类在 open 拒、追加类拖到 fsync”的 note）与 `USAGE.md` §3.0（写契约 note 后的精简表 + “不能拿 shell `>>` 的 rc 判断写成功”的告警）；（b）写入 `docs.html` §11.5（du 字节来源 note 后，9 行情形×会/不会出声×逐字回显关键词）与 `USAGE.md` §12.6（同矩阵精简版 + 一句话选型）。未出声的条带丢分块与挂载读均标为“当前版本已知缺陷（未实现、诚实标注）”，与 §5-5/§5-6 遗留项一致。doccheck 退出码 0（标签闭合无错、参数对账非负空间差集 0、新增汉字均为正当用字）。

---

## 7 清理与回滚说明

### 7.1 本次取证新增的磁盘产物（均为测试资产，不是运行依赖）

| 位置 | 条目 | 在本次报告中的角色 |
|---|---|---|
| 仓库 `.build-bin/` | `vm_redeploy.sh`、`vm_audit.sh`、`vm_battery_{1,2,3,4}.sh`、`vm_evidence_run.sh`、`vm_probe_{lostchunk,lostobj,lostlocal,alias}.sh`、`vm_cleanup_verify.sh`、`vm_repocheck.sh`、`vm_verify_striping.sh`、`gfs_provision.sh` | 部署/复核/取证驱动脚本（可重跑复现全部结论） |
| 仓库 `tests/` | `tests/du.rs`（3 个单测） | du 条带路径修复的回归单测（§4.2） |
| 仓库根 | `TEST_REPORT.md` | 本报告 |
| 本机 `/tmp/` | `repo_{build,clippy,cargo_test,run_all2,doccheck2}.log`、`repo_gates.status`、`vm_phase{1,2,3}.log`、`redeploy_newbin.log` | 仓库侧四门槛原始回显；步骤一的拆旧与分发原始回显 |
| 本机 `/tmp/vm_evidence/` | `audit*.log`、`battery_*.log`、`probe_*.log`、`cleanup_verify*.log`、`driver*.log`、`status` | 集群侧全部原始回显 |

集群上没留下任何测试资产：不带测试目录、不带 transient 单元、不带额外二进制（项级证据见 §3.17）。三台的启停入口仍然只有 4 个 systemd 单元（§2.1）。

### 7.2 只测不改的部分（按原决定不动）

- 168：`/tmp/du-test`（1.6M）；170：`/data/gfs`（19G）与容器 `grovefs-pg`（证据见 §1.1/§1.2）。
- 虚机数据根 `/data/gfs1/grove`、`/data/gfs2/grove` 与元数据根 `/home/hanjun/meta`：本次只读写测试数据，收尾已回到 §3.0 记录的水位（§3.17）。

### 7.3 回滚命令（按步骤二逐单停用，再删程序侧文件；数据根一律保留）

```bash
# 1) gfs-client：先停挂载单元，再停 meta-server 单元（依赖序与 Requires= 一致）
ssh gfs-client 'sudo systemctl disable --now grovefs-mount-alice.service grovefs-meta-server.service'

# 2) gfs1 / gfs2：停用 data-server 单元
for h in gfs1 gfs2; do ssh $h 'sudo systemctl disable --now grovefs-data-server.service'; done

# 3) 删单元文件与 env，重载 systemd
for h in gfs1 gfs2 gfs-client; do
  ssh $h 'sudo rm -f /etc/systemd/system/grovefs-data-server.service \
                    /etc/systemd/system/grovefs-meta-server.service \
                    /etc/systemd/system/grovefs-mount-alice.service; \
          sudo systemctl daemon-reload; sudo systemctl reset-failed'
done
ssh gfs-client 'sudo rm -f /etc/grovefs/cluster.env'      # 0600 密钥文件
for h in gfs1 gfs2 gfs-client; do ssh $h 'sudo rm -f /usr/local/bin/grovefs'; done

# 4) 收尾复核：四项应为“无”
for h in gfs1 gfs2 gfs-client; do
  ssh $h 'echo "单元: $(ls /etc/systemd/system/grovefs-*.service 2>&1)" \
                "进程: $(pgrep -fc /usr/local/bin/grovefs)" \
                "挂载: $(grep -c grovefs /proc/mounts)" \
                "监听: $(ss -ltn | grep -c ":501")"'
done
```

回滚不删的三个东西：`/data/gfs*/grove`（数据）、`/home/hanjun/meta`（元数据树）、`/home/hanjun/mnt`（挂载点目录本身）。想连数据一并回退，需先确认无挂载后手工删这两个根（本报告不提供该路径的实测证据，因此不当作已验证命令）。反向恢复只需重跑 `bash .build-bin/vm_redeploy.sh`（单一入口：phase1 拆旧 + phase2 落地），单元内容以 `vm_redeploy.sh` 为唯一权威版本（已落地结果见 §2.1 回显）。

### 7.4 若只想重启而不回滚

```bash
for h in gfs1 gfs2 gfs-client; do ssh $h 'sudo systemctl restart grovefs-data-server.service grovefs-meta-server.service grovefs-mount-alice.service' 2>/dev/null; done
bash .build-bin/vm_audit.sh        # 重启后拿一次全量复核
```

（逐台只该重启本机存在的单元；上述行在 gfs-client 上会因无 data-server 单元而报错，属预期噪声，应用 `ssh gfs-client 'sudo systemctl restart grovefs-meta-server.service grovefs-mount-alice.service'`。）该重启路径本身在取证中已反复走过（§3.6.4 的探针与 §2.2 的崩溃拉起都依赖它），因此不单独列为未验证项。

### 7.5 清场后的全量复核（步骤一/二全部断言的终态证据）

- **目的**：现场清场（§3.17）后再跑一次完整 audit，确认 168/170 仍无程序侧残留、三台单元/密钥/md5/崩溃拉起仍成立。
- **命令**：`bash .build-bin/vm_audit.sh`（回显 `/tmp/vm_evidence/audit_final.log`，96 行）
- **回显**（关键行；168/170 两段与 §1.1/§1.2 逐字一致，故不重复贴）：

  ```
  ##### [gfs1] data-server 单元复核 #####
  -- is-enabled / is-active: enabled / active
  -- 单元文本里是否出现 token: 0 处
  -- env 文件权限: 600 root:root /etc/grovefs/cluster.env
  -- 密钥是否出现在 ps/argv: 0
  -- 监听: 0.0.0.0:50100
  -- 二进制 md5: 2fa8ff6498a3b0390c796c79cad8eaab
  -- 非 systemd 入口残留: [空=已清]
  （gfs2 段与上述逐字一致）

  -- GROVEFS_TOKEN 值在 ps/argv 中出现次数: 0 (0 = 不进 argv)
  -- GROVEFS_TOKEN 值在单元文本中出现次数: 0
  -- 非 systemd 管的 grovefs 进程(应为 0): 
  -- du 实测(条带路径): 
     64.0M	/
  -- 开机自起验证(单元 WantedBy): WantedBy=multi-user.target

  -- 崩溃自动拉起验证(SIGKILL meta-server -> systemd 重起):
     杀前: pid=30285 NRestarts=1 ExecMainStartTimestamp=Sat 2026-11-07 01:55:10 CST 
     杀后(14x0.25s 后监听已恢复): pid=34263 NRestarts=2 ExecMainStartTimestamp=Sat 2026-11-07 02:17:28 CST 
     存活挂载仍可用: 可读 ; 写入 after-kill

  ##### [构建产物] md5 对账（最终交付态，与 §2.3 一致） #####
  c8d68580aa5fb7e5ed531bb79bed2ab6  /data/grovefs/grovefs/target/release/grovefs
  ```

- **结论**：通过。崩溃拉起为第二次独立复现（`NRestarts` 1→2，监听在 14×0.25 s = 3.5 s 内恢复，与 `RestartSec=3` 相符；§2.2 贴的是首次 0→1）。本轮 audit 会在 gfs-client 上 SIGKILL 一次 meta-server 主进程（由 systemd 自动重起，不留现场），因此它属于只读复核而非零开销动作；若需完全不动运行体，可只看 §3.17 的清场回显。
- **紧随其后的确认**：audit 的 SIGKILL 之后再跑一次 `bash .build-bin/vm_cleanup_verify.sh`（回显 `/tmp/vm_evidence/cleanup_verify_final.log`），结果与 §3.17 逐字一致（清场动作行下无任何 `removed` 输出 = 无新残留；进程 2、挂载 1、`du 64.0M`），即崩溃拉起后集群仍为干净部署态。
- **§3.18 的形式对账探针之后的复核**：该探针用的 `$HOME/gftimp` 与 `/tmp/gft_imp_src` 在其自身末尾已 `rm -rf`；再跑一次 `vm_cleanup_verify.sh` 仍与上述逐字一致，并单独复核了两个位置与元数据树目录：

  ```
  ls: cannot access '/home/hanjun/gft*': No such file or directory
  ls: cannot access '/tmp/gft*': No such file or directory
  alice.db  alice.db-shm  alice.db-wal  api_keys.db  refs.db  refs.db-shm  refs.db-wal  views
  ```

  即测试期间建过的 `probe`/`imp1`/`imp2`/`imp3` 等本地视图树均未残留在元数据根里（只有 `alice.db` 一棵树）。

---

## 8 最终 `git status` 与改动清单

```
$ git log --oneline -4
507e3f2 fix(du,sharded): announce damaged striped objects instead of silent under-report (§6-2)
ca6c358 docs(report): refresh §8 git snapshot for §6-12/§6-2 commits
20308e2 docs(du): describe striped chunk-loss behavior in byte-source note (§6-2 doc sync)
50f1564 docs(cli): add two ops-semantics tables (§6-12)

$ git show --stat 507e3f2       # §6-2 行为修复（len_strict + du damaged 三档分类 + 文档/报告同步，已入库）
 TEST_REPORT.md       | 49 +++++++++++++++++++++++++++++-----------------
 USAGE.md             |  6 +++---
 docs.html            | 10 +++++-----
 src/backend/sharded.rs | 137 +++++++++++++++++++++++++++++++++++++++++++++++
 src/du.rs            | 102 ++++++++++++++++++++++++++-------------
 5 files changed, 303 insertions(+), 101 deletions(-)

$ git status --short            # 跟踪文件已全部入库，仅剩未跟踪的取证/编排脚本（本报告快照随最后一次内容改动一起提交）
?? .build-bin/gfs_provision.sh
?? .build-bin/probe_posix_lock.py
?? .build-bin/repro_du_damage.sh
?? .build-bin/vm_audit.sh
?? .build-bin/vm_battery_1.sh
?? .build-bin/vm_battery_2.sh
?? .build-bin/vm_battery_3.sh
?? .build-bin/vm_battery_4.sh
?? .build-bin/vm_cleanup_verify.sh
?? .build-bin/vm_evidence_run.sh
?? .build-bin/vm_probe_alias.sh
?? .build-bin/vm_probe_lostchunk.sh
?? .build-bin/vm_probe_lostlocal.sh
?? .build-bin/vm_probe_lostobj.sh
?? .build-bin/vm_redeploy.sh
?? .build-bin/vm_repocheck.sh
?? .build-bin/vm_verify_striping.sh
```

改动逐项对应实测证据（无一项是无据声称）：

| 文件 | 改动内容 | 证据 |
|---|---|---|
| `src/du.rs` | du 的远程条带路径重写：`--data` 接受逗号列表并按 `s<N>` 逐对象探测；缺数据源时硬拒绝而不是静默计 0；丢失对象计入 `lost` 并发出 `warning: N of M object(s) are not in the data source and count as 0 bytes (...)`；本地路径默认取 `--db-path`/`--home` 同一根 | §3.6.1（本地两路径）、§3.6.2（三种必须拒绝）、§3.6.3（四种丢对象情形的完整回显，含探针 `probe_lostobj.log` 的出声原文） |
| `src/main.rs` | `du --data`/`--data-root` 的 `///` 帮助文本同步上述语义（逗号列表、条带需全列表、无本地根且无 `--data` 则拒绝） | `grovefs du --help` 与 §4.4 doccheck（真实参数 68 / 文档 87 / 非外部非负空间 0） |
| `tests/du.rs` | 新增 3 个集成单测，其中 `an_object_that_is_not_found_is_announced_not_silently_zero` 直接盯住“丢对象必须出声” | §4.2（`tests/du.rs` 3 passed） |
| `.build-bin/test_striping.sh` | 补充落盘位置核验步骤（不靠 key 前缀推桶，改用 `find -name`） | §4.3（`run_all.sh` RESULT: GREEN，22 套件 PASS） |
| `README.md` | `--root /d1,/d2` 行写明“落盘位置是 key 哈希 mod 根数现场重算，列表与对象都不记录盘序号”的口径 | 机内多根落盘实测在 §3.11（回显 `同机双根的放置由 key 哈希决定`）；条带侧根数不对必须拒的实测在 §3.6.2（`refusing to remap` 原文）；§4.4 doccheck 结论 OK |
| `USAGE.md` | `--data-root` 的适用命令加 `du` 与默认取值说明；§12.3/§14.1/部署案例表同步多根布局口径 | du 的默认数据根取值见 §3.6.1，三种缺数据源硬拒绝见 §3.6.2，丢对象出声见 §3.6.3；§4.4 doccheck 参数对账“非外部工具/非负空间 0 项” |
| `docs.html` | 同步上述表格与措辞 | §4.4 |
| `docs.html` + `USAGE.md`（§6-12、本轮待入库） | 新增两张运维语义表：（a）fork/透镜/别名/key-ro 四种写意图的失败时点（结构写 / 内容写截断类 open 即 `EROFS` / 内容写追加类 `EROFS` 拖到 fsync-close / shell `>>` 假成功），含逐 `open` 探针 errno 回显（`docs.html` 放 §12、`USAGE.md` 放 §3.0）；（b）“对象丢失/数据源不对时各命令会不会出声” 9 行矩阵（`docs.html` 放 §11.5、`USAGE.md` 放 §12.6），未出声的条带丢分块与挂载读标为已知缺陷 | 逐形态失败时点实测在 §3.4.4/§3.9；丢对象出声矩阵逐行取自 §3.6（du 整对象缺失→warning、无数据源/布局不符→拒）与 §5-5/§5-6（条带中/首分块丢失静默、挂载读 size=0 无 EIO）；§6-12 行、本轮 doccheck 退出码 0 |

| `src/backend/sharded.rs` | 本轮复核新增：`layout_server_count`（供 du 前置门控只约束有 `s<N>` 前缀的键）与 `chunk0_elsewhere`（用 `{uuid}#0` 归属确定性检出反序列列表）+ `order_verified` 零成本门控；新增 2 个单测 | §3.6.2（倒置列表由静默 `0B` 改为报错的实机回显）、§4.2（`a_reversed_server_list_is_reported_not_read_as_empty`、`an_empty_striped_object_still_measures_zero`） |
| `src/fs_impl.rs` | 本轮修复 §5-3：新增 `posix_lock_fh_owner`/`posix_lock_owner_fds` 两表与 `note_posix_lock`/`drop_posix_lock_on_release`，`setlk` 授予时记账、`release` 末次锁定时全区间 Unlock 精确回收 POSIX 锁（不依赖内核不交的 `lock_owner`） | §3.2（根因+修复说明与实机 `GRANTED`/`2.74s` 唤醒回显）、§5-3行、§4.2（`tests/locks.rs` 11 passed） |
| `.build-bin/test_locks.sh` | 本轮新增 §4c：跨进程 POSIX `fcntl` 整文件锁的 close 释放（新进程非阻塞加锁）+ 阻塞 `F_SETLKW` 唤醒延时两个回归用例 | 本地 `bash .build-bin/test_locks.sh` SUMMARY `pass=17 fail=0`（含新增两项），实机集群复验同结果 |
| `.build-bin/probe_posix_lock.py` | 本轮新增：三机集群挂载点上的 §5-3 实机探针（holder close 后 asker、blocker 唤醒），取证用 | §3.2 实机回显（`ask:GRANTED`、`block:WAKE 2.74s`、`RESULT: PASS`） |

本轮显示/使用方式修复（§5-12/§6-1/§5-4·§6-4/§6-5/§6-6，均不改运行时逻辑或协议，已入运行体 `d2a3b262` 并在实机复验，见 §2.3/§3.6.2）：

| 文件 | 改动内容 | 证据 |
|---|---|---|
| `src/main.rs` | §5-12：`main()` 拆出 `real_main()`，致命错误由 `Box<dyn Error>` 的 Debug 打印改走 Display（`error: <人话>`，与 clap 前缀一致）；§5-4/§6-4：`view rm --base` 打不开数据根时补上下文并给出离线 `gc` 回收提示 | §2.3 结论、§3.6.2（`error: …` 新前缀实机回显） |
| `src/du.rs` | §6-6：`--path` 找不到时回显规范化后的视图相对路径并给出格式提示；`gather_via_meta` 连不上 meta-server 时点名地址 | §2.3 实机复验（`cannot reach meta-server 127.0.0.1:59999`）、§3.6.2 |
| `src/mcp.rs` | §6-1：`view list` 去掉首行与列表自相矛盾的措辞（区分“无任何视图定义”与“无别名/分叉/透镜定义”）；§6-5：`base_tree_missing_note` 用真实存储根与树路径替换 `<db-path>` 占位符 | §2.3 实机复验（非矛盾行、填实路径提示） |
| `src/remote/meta.rs` | §5-12：meta 面 `connect_fresh` TCP 失败包 `cannot reach meta-server {addr}: {e}`，保留 `ErrorKind` 不影响 failover | §2.3 实机复验 |
| `src/remote/client.rs` | §5-12：数据面 `connect_fresh` TCP 失败包 `cannot reach data-server {self.addr}: {e}`，保留 `ErrorKind` | §2.3 实机复验 |
| `src/remote/meta/server_ext.rs` | §6-5：meta-server “view does not exist” 的 create-hint 用 `rt.base.display()` 与具体 `view` 填实 `--db-path`/`--view`（file 与 PG 两分支） | §2.3 实机复验（`--db-path /home/hanjun/meta … create --view nosuchprobe`）、`tests/meta_roundtrip.rs` |
| `tests/meta_roundtrip.rs` | §6-5 配套：断言从模板占位 `create --view <name>` 收紧为具体视图名 `create --view ghost` | §4.2（`meta_roundtrip` 6 passed） |
| `.build-bin/test_layout.sh` | §5-12 连带修正：治理拒绝改走 Display 后人话消息不含旧 Debug 的 `Permission` 字样，grep 关键字表补 `governed by an alias\|mount the alias name` | §4.3（`run_all.sh` RESULT: GREEN） |
| `README.md` / `USAGE.md` / `docs.html` | 与 Display 新形态同步：所有 CLI 输出示例 `Error: "X"`/`Error: Custom {…, error: "X"}`→`error: X`；`Error: … "permission denied: bound to view"` 手工补改；typo1 实测块与建视图表行填实 create-hint 路径 | §4.4 doccheck、§4.3 GREEN |

上述显示/使用方式修复已提交为 `1d624dd`（11 个已跟踪文件 + 本报告，`+1787/-79`）。

§5-10 悬空别名校验（行为级校验改动，经作者批准后实现）已提交为 `74c765b`（`src/mcp.rs` 校验+单测、`.build-bin/test_control.sh` PG 回归、本报告）：

| 文件 | 改动内容 | 证据 |
|---|---|---|
| `src/mcp.rs` | §5-10：`alias_view` 在排除别名/派生/分叉三类目标后，用 `store.has_physical(real)` 校验别名目标指向的物理视图树确实存在；不存在则在**定义时**硬拒，并用 `base_tree_missing_note`（file 侧）或自造 create/import 文案（PG 侧）填实提示。一处改动同时覆盖 CLI（`main.rs` 经同一 `alias_view`）与 MCP 两入口、file 与 PG 两 store。新增单测 `alias_refuses_a_dangling_physical_target` | §3.4.3 X2（悬空别名 `rc=1` 定义被拒、控制面未写入）、§4.2（`cargo test` 全绿含该单测）、§4.3 `run_all.sh` GREEN |
| `.build-bin/test_control.sh` | §5-10 配套：PG 别名目标改用独立物理视图 `AREAL`（避免治理 `$BASE` 致 §3 真实名挂载被拒），并新增悬空别名定义被拒的回归用例（断言报错含 `no physical view tree`） | §4.3（`run_all.sh` RESULT: GREEN，control 套件 16/0） |

本轮 §6-8/§6-9 gc/stats 显示改动（不改运行时逻辑或协议，已入运行体 `c8d68580` 并复验，待确认入库）：

| 文件 | 改动内容 | 证据 |
|---|---|---|
| `src/main.rs` | §6-8：`print_gc_report` 当 `objects == 0 && striped_layouts > 0` 时改打 `objects: 0 local block(s) — N striped layout(s) … use \`grovefs du\` for their bytes`；dry-run 尾句拆为三态（有可回收项才邀请 `--apply`、无孤儿打 `No unreferenced objects were found.`、全在宽限期打提示），保留 `dry run: nothing deleted.` 稳定前缀 | §2.3（条带 meta 存储 `gc` 实机回显 + `du` 64.0M 未回归）、`.build-bin/test_gc.sh` 41/0（§4.3 run_all gc PASS） |
| `src/stats.rs` | §6-9：`report_text` 表头新增两行，说明 `bytes=` 列仅在后端边界阶段累计（`op_total` 行为 0 属预期、非“没读到数据”）与 sub-µs 零值含义；不改表格列宽与 CSV | §4.3（run_all stats PASS）、本机 `--stats` 挂载表头回显、`.build-bin/test_stats.sh` E2E PASS |
| `docs.html` | §6-9 同步：§8 的 `--stats` 真实回显样本补齐三行表头（与当前产物输出一致） | §4.4 doccheck OK |
| `USAGE.md` | §6-9 同步：§8.1 加一句 `bytes=` 列口径与 sub-µs 零值说明 | §4.4 doccheck OK |

本次未做的改动（如实列出）：§5 遗留项中除上述当场修复外均未改代码；§5-3（POSIX 锁 close 释放泄漏）已定位确定性根因并实现修复（`setlk` 记账 + `release` 精确回收，不改 sid 续租模型），配套集成测与实机复验均通过（见 §3.2、§6-2a）；§5-5 中“数据源真丢字节”的内部空洞/首块真丢形态仍需 §6-2 方案（行为+RPC 改动，待批）。§6-7 经定位为取证脚本产物，非产品输出，未改。

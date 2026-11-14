# GroveFS × FoundationDB 实验档安装包 —— 归档分支

本分支是**孤儿分支**（孤立历史，父提交为空），只存放 FoundationDB 实验档的自足投放包，
不接 `master` 的历史。目的与普通 `dist/vN` 分支相同：普通 `git clone` 不会拉取这 60 MB
二进制，主历史体积不随它增长；要取证的人单独 fetch 本分支即可。

## 这一档现在的地位

FoundationDB 作为第三种元数据后端**已从主干移除**：`src/fdb_store.rs`、`fdb` feature、
`vendor/foundationdb-sys`、`tests/fdb_*.rs`、`docs/fdb-keyspace.md` 与全部 `fdb://` 命令
口径都已删除，主干只支持 **SQLite** 与 **PostgreSQL** 两档元数据后端。本分支留的是移除
**之前**编出来的那份实物，用于取证与复现实验，不构成任何支持声明。

- 包内 `BUILDINFO.txt` 里「本包仍不是发布版……也不推到 dist 分支」那一句，已被
  2026-11-14 的归档决定取代（见 [ARCHIVE-NOTE.txt](ARCHIVE-NOTE.txt)）；文件本身按原样封在
  tar 内，字节未被改写。
- 源码不在本分支。要 FDB 那套源码，取远端的 `fdb-test` 分支：
  gitee `fdb-test` = `f9abe19`，GitHub `fdb-test` = `f052a39`（GitHub 侧按既有约定剔除
  `paper/` 与点目录，故两边提交不同点）。

## 分支内容

```
grovefs-fdb-experimental-linux-x86_64.tar.gz          60,098,093 字节，唯一的实物载体
grovefs-fdb-experimental-linux-x86_64.tar.gz.sha256   上面这个 tar 的校验行
README.md                                              本文件，仅分支所有
ARCHIVE-NOTE.txt                                       归档口径与取代说明，仅分支所有
```

包内文档（`BUILDINFO.txt`、包自己的 `README.md`、`SHA256SUMS.txt`）只随 tar 走，
**不在分支根放副本**：`SHA256SUMS.txt` 校验的对象就是解包后的那几个同名文件，
根目录再摆一份会让人在错误的目录里 `sha256sum -c` 并得到一失败一通过的歧义结果。
要看构建信息，先解包。

## 取用方式

```bash
git fetch origin refs/heads/dist/fdb-experimental:refs/remotes/origin/dist/fdb-experimental
git show origin/dist/fdb-experimental:grovefs-fdb-experimental-linux-x86_64.tar.gz > pkg.tar.gz
git show origin/dist/fdb-experimental:grovefs-fdb-experimental-linux-x86_64.tar.gz.sha256 > pkg.sha256
# 校验行里写的是原文件名，落盘时按原名保存才能直接 -c：
mv pkg.tar.gz grovefs-fdb-experimental-linux-x86_64.tar.gz
sha256sum -c grovefs-fdb-experimental-linux-x86_64.tar.gz.sha256
tar xzf grovefs-fdb-experimental-linux-x86_64.tar.gz
cd grovefs-fdb-experimental-linux-x86_64 && sha256sum -c SHA256SUMS.txt
```

两段校验都过，才说明拿到的是当初那份实物：第一段量 tar 本身，第二段量包内每个文件。

## 解包后怎么用（实验用途，照包内 README.md 走）

包是自足的：`grovefs` 二进制靠 RUNPATH `$ORIGIN/lib:$ORIGIN/../lib` 解析到包内
`lib/libfdb_c.so`（空环境实测 `ldd` 落在包内那一份），`fdb/` 里带 `fdbserver`/`fdbcli`/`fdbmonitor`，
起单机集群与挂载的完整命令写在包内 `README.md`。
不要拿它对接主干编出来的二进制——主干版本已不认 `fdb://`，会按 URI scheme 直接拒绝。

RUNPATH 第三段 `/data/grovefs/.fdbbin/lib` 是构建机遗留的兜底路径，本机已随 FDB 本地件一并清除；
它排在 `$ORIGIN` 两段之后，找不到也不影响解析，见 [ARCHIVE-NOTE.txt](ARCHIVE-NOTE.txt)。

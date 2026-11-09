# GroveFS v1.0 发布产物分支

本分支是**孤儿分支**（孤立历史，父提交为空），只存放 v1.0 的编译产物与构建信息，
不接 `master` 的历史。这样做的目的：普通 `git clone` 不会拉取二进制，主历史体积
不随发版增长；要取产物的人单独 fetch 本分支即可。

发版坐标与产物清单见 [BUILDINFO.txt](BUILDINFO.txt)。

## 取用方式

只下载产物目录（不动当前工作区）：

```bash
git fetch origin refs/heads/dist/v1.0:refs/remotes/origin/dist/v1.0
git show origin/dist/v1.0:dist/grovefs > grovefs && chmod +x grovefs
```

或整分支检出到独立目录：

```bash
git worktree add ../grovefs-v1.0-dist dist/v1.0
```

直接跑打包件：

```bash
sha256sum -c SHA256SUMS.txt --ignore-missing
tar xzf dist/grovefs-v1.0-linux-x86_64.tar.gz
./grovefs --version
```

## 目录内容

| 路径 | 说明 |
|---|---|
| `dist/grovefs` | 主二进制，x86_64 Linux，glibc ≥ 2.31，已 strip |
| `dist/grovefs-v1.0-linux-x86_64` | 同一文件的带版本号副本 |
| `dist/grovefs-v1.0-linux-x86_64.tar.gz` | 打包件（内含 `grovefs`） |
| `SHA256SUMS.txt` | 全部产物的 SHA-256 |
| `BUILDINFO.txt` | 发版坐标、构建环境、复现方式、能力要点、验证记录 |

## 与标签 v1.0 的关系

- `refs/tags/v1.0` —— 附注标签，钉死发版内容，指向源码提交 `1e19dff`，**永不漂移**。
- `refs/heads/v1.0` —— 分支，当前与标签同点，后续可按需要推进或废弃。
- `refs/heads/dist/v1.0` —— 本分支，存产物，基点同样是 `1e19dff`。

以源码为准一律用标签 `v1.0`；本分支的产物可追溯到该提交（见 BUILDINFO.txt 的
"复现方式"）。

## 重新生成产物

```bash
git checkout 1e19dff
cargo build --release
strip -o /tmp/grovefs target/release/grovefs
```

工具链版本需与 BUILDINFO.txt 记录一致，才能得到字节级相同的产物；否则仅保证
功能等价。

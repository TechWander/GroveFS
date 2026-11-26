//! GroveFS 元数据的 **treedb** 后端 —— 实验 spike（treedb 仓的 `DESIGN.md` §10 阶段 B）。
//!
//! 整个模块只在 `--features treedb` 下存在，入口是
//! `meta-server --meta-store treedb://host:port`。默认后端路径（SQLite / PG）逐字节
//! 不变：本文件不被编译时，`StoreKind` 里也没有 `Treedb` 之外的 treedb 痕迹。
//!
//! 这里**不做 SQL 翻译**：每个 [`MetaProvider`] 方法映射成一条或几条 treedb 二进制
//! op（[`treedb::engine::model::Op`]）。语义边界按 treedb 仓 `DESIGN.md` §5 划分 ——
//! fd/open 状态、字节本体都留在 grovefs 侧，引擎只管名字空间与引用账本。
//!
//! ## 单位与形状上的三处翻译
//!
//! | grovefs | treedb | 处理 |
//! |---|---|---|
//! | `NodeInfo.{atime,mtime,ctime}` 是 epoch **秒** | `Node` 里是 **毫秒** | 进出各折一次（[`ms_to_s`] / [`s_to_ms`]） |
//! | `data_ref: Option<String>`（形如 `1:4096:dev0`） | `DataRef = Vec<u8>`，空串 = 没有内容 | 字节直通，空串 ↔ `None` |
//! | 富 errno 会被压进 `MetaError` 的 6 个变体 | 引擎回裸 POSIX errno | [`Fail::to_meta`] 一张显式映射表，未知码归 `Db` 并带上原文 |
//!
//! ## 这一层实际动用的引擎面
//!
//! 引擎的 op 面是 51 条（treedb 仓 `DESIGN.md` §4.2，三档 22/22/7），本适配层只调其中
//! **13 条**：视图面 `CreateView`/`DropView`/`ListViews`，树面 `Lookup`/`Getattr`/
//! `Setattr`/`Create`/`Mkdir`/`Symlink`/`Unlink`/`Rmdir`/`Rename`/`ReadDirPlus`。条数与上一版
//! 一样，但最后一项换过：目录列表走 `ReadDirPlus`，不再走 `ReadDir`。理由是 `ReadDir`
//! 一页只给名字，属性得逐条 `Getattr` 回头补 —— 一页 1000 条就是 1 次列表 + 16 次批量属性
//! （一批 64 条）= 17 次往返。实测两臂对照（同一份 `du` 走完一棵 3000 条的树，
//! 配方 `.build-bin/bench_treedb_readdir.sh`）：引擎应用的 op 数 **3006 → 6**；把引擎放到
//! 每段 2 ms 延迟的 TCP 代理后面，墙钟 **256 ms → 52 ms**；同机 loopback 上则量不出差别
//! （28 ms vs 24 ms，在噪声里）。这项收益的单位是往返，本机测试恰好把它藏起来，所以
//! 两个数都要写。
//!
//! 本层不调用的那些不是白做的：`Key*`/`Audit*`/`Def*`/`Place*`/`Table*`/`Row*` 是给控制面
//! 搬进来准备的（现在还在本地 SQLite，迁法见 treedb 仓 `DESIGN.md` §10 阶段 C 4）；`Lock*`
//! 只有整 inode 形状，给不了 POSIX 记录锁；`WalFetch`/`SnapFetch` 是异步副本，不是
//! 这里要的同步提升。
//!
//! ## 明确接不了的东西（spike 的诚实边界，见 treedb 仓 `DESIGN.md` §5.3 / §8）
//!
//! - **硬链接**：引擎没有 `Link` op，一个 inode 只有一条目录项路径。`link_node` 回
//!   `EPERM`，不是"假装成功"。这条缺口占多大**面积**已实测：`posix`+`e2e` 两套矩阵
//!   逐标签重放过来的是 36 条（基线共 37 条，唯一没搬的是要 PTY 的 tty 默认日志），
//!   加上硬链接正反两面与本夹具自检共 49 条 = 39 OK + 10 SKIP，那 10 条全部是
//!   硬链接，其余每一格都过（配方 `.build-bin/test_treedb_matrix.sh`，板上
//!   `FEATURES_TREEDB=1` 走 `treedbmatrix` 那一行；结论在 treedb 仓 `DESIGN.md` §10 阶段 C 1）。
//! - **引擎的认证面已接，入口是环境变量**：`GROVEFS_TREEDB_KEY_FILE=/path` 给密钥
//!   文件路径（与 `GROVEFS_JOIN_KEY_FILE` / `GROVEFS_ROSTER_KEY_FILE` 同规：密钥字节
//!   永远不进 argv），设了就对每条连接跑 v5 双向握手（`Client::connect_auth`），
//!   没设就不握手。读不动密钥时**每次 open 都失败**，不降级成不认证。为什么不走
//!   `treedb://` URI 参数：`serve()` 解析完就把 `StoreKind::Treedb` 的 payload 定成了
//!   裸 `host:port`，`server_ext.rs` 里有十处操作者可见的文案在插 `{addr}`；要带
//!   query 就得改那个文件——而它是另一个主题正在动的地方。
//! - **POSIX 记录锁**：引擎的锁 key 是 `(view, ino, owner)`，**没有字节区间**，也就
//!   没有合并/拆分和 `release-all-of-owner`。`lock_op` 一律回 `EPERM`（`flock` 在这
//!   个后端上可见地失败），`get_lock` 回 `None`。给错语义比给不了更糟。
//! - **字节数不变量**：grovefs 的 size 权威在 data plane（`backend.len(key)`），元数据
//!   行的 `size` 只在没有 data_ref 时当兜底，truncate 也不回写它。于是引擎 `refs`
//!   账本里的 `bytes` 恒为 0：按 count 的 GC 判定不受影响，但**别拿这个后端的字节数
//!   做容量规划**。
//! - **view 名 > 25 字节**：`layout::valid_view_name` 不卡长度，引擎卡（`VIEW_NAME_MAX`），
//!   超了在 `view create` 处报错。
//! - **HA / 快照 / GC 聚合**：`backup_to_bytes` 直接拒绝，`serve()` 里
//!   `--role standby / --peer / --witness` 与这个后端互斥（同 PG）。

#![cfg(feature = "treedb")]

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use treedb::engine::model::{
    errno, Kind, Node, Op, Reply, RENAME_NOREPLACE, SA_ATIME, SA_DATA_REF, SA_GID, SA_MODE,
    SA_MTIME, SA_UID,
};
use treedb::net::auth::Key;
use treedb::net::Client;

use crate::meta::{LockConflict, LockType, MetaError, MetaProvider, NodeInfo, NodeKind};

/// `--meta-store` 的这个 scheme。后面只接 `host:port`，不接 path/query。
pub const SCHEME: &str = "treedb://";
/// 没写地址时的落点，与 `treedb-server` 的 `--listen` 默认值一致。
pub const DEFAULT_ADDR: &str = "127.0.0.1:7777";
/// 每个 store 握几条长连接。op 之间没有事务关系，round-robin 出去就是并行；
/// 一条连接同时只能有一个在飞的请求（`Client::call` 是一问一答）。
const DEFAULT_CONNS: usize = 4;
/// 拨号超时。数据目录在本机，1 s 足够；超了说明服务端没起来，早报错比挂着好。
const CONNECT_TIMEOUT_MS: u64 = 3_000;
/// `ReadDir` 一页取多少条（引擎侧上限 4096，协议里 `limit=0` 表示 1000）。
const PAGE: u32 = 1000;
/// 一批 `Getattr` 塞进一次 pipeline 的条数。自从分页改走 `ReadDirPlus`，它
/// 只服务一件事：补回 `NodeAttr` 不带的那个字段（symlink 目标），见 [`TreedbStore::attrs`]。
const ATTR_BATCH: usize = 64;

/// 这个 `--meta-store` URI 是不是指向 treedb。
pub fn is_treedb_store(uri: &str) -> bool {
    uri.starts_with(SCHEME)
}

/// `treedb://host:port` → `host:port`。裸 `treedb://` 用默认地址。
/// 错误串里绝不回显 URI 之外的东西：这个地址没有凭据，但它会进 banner 与日志。
pub fn parse_addr(uri: &str) -> Result<String, String> {
    let rest = uri
        .strip_prefix(SCHEME)
        .ok_or_else(|| format!("not a {SCHEME} uri"))?;
    if rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(format!(
            "{SCHEME} takes HOST:PORT only (got '{rest}'); a treedb instance is one address, \
             multi-instance placement is not wired yet"
        ));
    }
    if rest.is_empty() {
        return Ok(DEFAULT_ADDR.to_string());
    }
    // 让 std 去判 host:port 的合法性，别在这儿写第二套解析器。
    rest.parse::<std::net::SocketAddr>()
        .map_err(|e| format!("{SCHEME}{rest}: {e}"))?;
    Ok(rest.to_string())
}

/// 引擎握手密钥的来源。这个环境变量给的是**路径**，密钥字节本身永远不进
/// argv —— 与 `GROVEFS_JOIN_KEY_FILE`、`GROVEFS_ROSTER_KEY_FILE` 同规。
pub const KEY_FILE_ENV: &str = "GROVEFS_TREEDB_KEY_FILE";

/// 进程内解析一次密钥。三档语义，第三档是这个函数存在的全部理由：
///
/// - 未设置 / 空串 → `None`：不握手。引擎没配 `--auth-key-file` 时就该这么搭档。
/// - 设了且读得动 → `Some`。
/// - 设了但读不动（文件缺失、长度不对、全零）→ **每次 open 都拿这个错误失败**。
///   这一档绝不能降级成 `None`：那会把「密钥配错」变成悄悄不认证，运维只能在
///   引擎那一边的 EADV 海里找原因。fail-closed 的方向是当场报错，不是装死。
fn auth_key() -> Result<Option<Arc<Key>>, String> {
    static KEY: OnceLock<Result<Option<Arc<Key>>, String>> = OnceLock::new();
    match KEY.get_or_init(|| {
        let path = match std::env::var(KEY_FILE_ENV) {
            Ok(p) if !p.trim().is_empty() => p,
            // 没设、或者设成空串：都算「这个部署不要认证」。
            _ => return Ok(None),
        };
        // 错误串里只给路径与引擎的原文，不给密钥字节。
        match Key::load(std::path::Path::new(&path)) {
            Ok(k) => Ok(Some(Arc::new(k))),
            Err(e) => Err(format!("{KEY_FILE_ENV}={path}: {e}")),
        }
    }) {
        Ok(k) => Ok(k.clone()),
        Err(e) => Err(e.clone()),
    }
}

fn conn_count() -> usize {
    std::env::var("GROVEFS_TREEDB_CONNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| (1..=64).contains(n))
        .unwrap_or(DEFAULT_CONNS)
}

fn ms_to_s(ms: i64) -> i64 {
    // 负值只可能是 0/未设置；向零取整，别让一个 -1ms 变成 -1s。
    if ms <= 0 {
        0
    } else {
        ms / 1000
    }
}

fn s_to_ms(s: i64) -> i64 {
    s.saturating_mul(1000)
}

fn key_to_str(k: &[u8]) -> Option<String> {
    if k.is_empty() {
        None
    } else {
        // 键是 grovefs 自己写进去的 UTF-8 字符串；不是就按 lossy 读回来，
        // 宁可让 GC 报一个对不上的键，也不要在读路径上 panic。
        Some(String::from_utf8_lossy(k).into_owned())
    }
}

fn kind_of(k: Kind) -> NodeKind {
    match k {
        Kind::Dir => NodeKind::Dir,
        Kind::Symlink => NodeKind::Symlink,
        Kind::File => NodeKind::Regular,
    }
}

/// 引擎那条 inode → FUSE 侧看到的 [`NodeInfo`]。
fn info_of(n: &Node) -> NodeInfo {
    NodeInfo {
        id: n.ino as i64,
        parent_id: n.parent as i64,
        name: String::from_utf8_lossy(&n.name).into_owned(),
        kind: kind_of(n.kind),
        data_ref: key_to_str(&n.data_ref),
        link_target: if n.kind == Kind::Symlink {
            key_to_str(&n.link)
        } else {
            None
        },
        mode: n.mode,
        uid: n.uid,
        gid: n.gid,
        mtime: ms_to_s(n.mtime),
        atime: ms_to_s(n.atime),
        ctime: ms_to_s(n.ctime),
        nlink: n.nlink as u64,
        size: n.size,
    }
}

/// 一次调用的失败面。两种失败**必须**分开：
///
/// - [`Fail::Code`]：引擎按 POSIX 语义答了一个 errno。provider 需要原始码来分流
///   （`EISDIR` → 改走 `Rmdir`，`EEXIST` → 改走 `ListViews`），所以不能一上来就
///   压成 [`MetaError`]。
/// - [`Fail::Db`]：传输/协议/协议外应答。到 API 边界就是 `MetaError::Db`，
///   上层看到的是 EIO。
struct Fail {
    code: Option<u16>,
    msg: String,
}

/// 一条「不该出现在这条路径上」的应答 → 操作者读得懂的串。
///
/// 为什么不直接 `{other:?}`：那样会把引擎的拒绝文案打成字节数组（真实样本：
/// `Err { code: 68, msg: [97, 117, 116, …] }`），而没人能在日志里反解那串数 ——
/// 「没配密钥就被引擎拒了」这条第一手线索就是这么丢掉的。
fn unexpected(what: &str, r: &Reply) -> String {
    match r {
        Reply::Err { code, msg } => format!(
            "{what}: engine refused with errno {code} ({})",
            String::from_utf8_lossy(msg)
        ),
        other => format!("{what}: unexpected reply {other:?}"),
    }
}

impl Fail {
    fn code(code: u16, msg: Vec<u8>) -> Fail {
        Fail {
            code: Some(code),
            msg: String::from_utf8_lossy(&msg).into_owned(),
        }
    }
    fn db(msg: impl Into<String>) -> Fail {
        Fail {
            code: None,
            msg: msg.into(),
        }
    }

    /// treedb 的富 errno → grovefs 那 6 个 `MetaError` 变体。这张表就是
    /// 「协议表达能力被压扁」的地方：ESTALE/ELOOP/EXDEV 这些到 `Db` 里，
    /// 上层统一给 EIO。要恢复语义得先扩 `MetaError`（见 treedb 仓 DESIGN.md §10）。
    fn to_meta(&self) -> MetaError {
        match self.code.unwrap_or(0) {
            errno::EEXIST => MetaError::Exists,
            errno::ENOENT => MetaError::NotFound,
            errno::ENOTEMPTY => MetaError::NotEmpty,
            errno::ENOTDIR | errno::EISDIR => MetaError::TypeMismatch,
            errno::EPERM | errno::EBUSY | errno::EACCES | errno::EROFS => MetaError::OpNotPermitted,
            errno::EINVAL => MetaError::BadParentCycle,
            0 => MetaError::Db(format!("treedb: {}", self.msg)),
            c => MetaError::Db(format!("treedb: errno {c} ({})", self.msg)),
        }
    }
}

impl From<Fail> for MetaError {
    fn from(f: Fail) -> MetaError {
        f.to_meta()
    }
}

impl From<Fail> for io::Error {
    fn from(f: Fail) -> io::Error {
        io::Error::other(format!("treedb: {}", f.msg))
    }
}

/// 列目录失败绝不能静默退化成「这个目录是空的」：`ls` 会少条目、`du` 会少
/// 用量、`gc check` 会把还在的树看成不存在。`MetaProvider` 的签名回的是
/// `Vec`，没有失败通道，所以唯一能做的事是把这件事打到 stderr 上。
///
/// `ENOENT`/`ENOTDIR` 除外 —— 那是并发 rename/rmdir 的正常竞态，天天都在发生，
/// 一并打出来只会把真信号淹掉。
fn log_list_failure(op: &str, parent: i64, f: &Fail) {
    if matches!(f.code, Some(errno::ENOENT) | Some(errno::ENOTDIR)) {
        return;
    }
    eprintln!(
        "treedb {op}: listing ino {parent} failed ({}) — the listing is INCOMPLETE, not empty",
        f.msg
    );
}

/// 一个 base view 在 treedb 里的句柄。`MetaStore::Treedb` 唯一持有的东西。
pub struct TreedbStore {
    addr: String,
    view: String,
    /// 引擎里的 view id，open 时解析好。id 从 1 起发、且 drop 后永不复用
    /// （`dropped_view_id_is_never_reused_across_reopen`），所以一个活的 store
    /// 句柄上的 id 不会漂。
    view_id: u32,
    /// round-robin 的长连接池。`Client` 内含 `TcpStream`，不是 `Sync`，所以每条
    /// 连接各自一把锁：一个 op 锁一条，`N` 条给 `N` 路并发。
    conns: Vec<Mutex<Client>>,
    /// 握手密钥，`None` = 不认证。重连路径要靠它重建同一条连接，所以存在句柄上，
    /// 而不是每次重读环境变量：进程中途改 env 不该改变一个已有 store 的语义。
    key: Option<Arc<Key>>,
    rr: AtomicUsize,
    readonly: bool,
    /// 见模块头的「HA」条目：这个后端拒绝 `--role standby/--peer/--witness`，
    /// epoch 因此没有消费者，留个进程内计数只为让 `get_epoch()` 有个诚实的值。
    epoch: Arc<AtomicI64>,
    /// 同 SQLite 本地档：数据面不被 fence 门控，恒 0。
    fence: Arc<AtomicU64>,
}

impl TreedbStore {
    /// 连接 + 把 view 对上号。`readonly` 档绝不在远端建树：只查，查不到就失败
    /// （standby 不该有创建权）。
    pub fn open(addr: &str, view: &str, readonly: bool) -> io::Result<Self> {
        // 先定密钥再拨号：配错了要在第一条连接之前就报清楚，别让「连上了
        // 但每个 op 都 EADV」变成现场症状。
        let key = auth_key().map_err(io::Error::other)?;
        let n = conn_count();
        let mut conns = Vec::with_capacity(n);
        for _ in 0..n {
            let c = Client::connect_auth(addr, CONNECT_TIMEOUT_MS, key.as_deref())
                .map_err(|e| io::Error::other(format!("treedb connect {addr}: {e}")))?;
            conns.push(Mutex::new(c));
        }
        let mut st = TreedbStore {
            addr: addr.to_string(),
            view: view.to_string(),
            view_id: 0,
            conns,
            key,
            rr: AtomicUsize::new(0),
            readonly,
            epoch: Arc::new(AtomicI64::new(0)),
            fence: Arc::new(AtomicU64::new(0)),
        };
        // 建在第一个 store 上的 view 由这里负责；`view create` 走的是同一个入口。
        st.view_id = if readonly {
            st.lookup_view_id()?.ok_or_else(|| {
                io::Error::other(format!("treedb: view '{view}' does not exist (readonly)"))
            })?
        } else {
            st.resolve_or_create_view()?
        };
        Ok(st)
    }

    /// 删掉一个 view 的整棵树。返回 `Ok(false)` = 本来就没有。
    pub fn drop_view(addr: &str, view: &str) -> io::Result<bool> {
        let key = auth_key().map_err(io::Error::other)?;
        let mut c = Client::connect_auth(addr, CONNECT_TIMEOUT_MS, key.as_deref())
            .map_err(|e| io::Error::other(format!("treedb connect {addr}: {e}")))?;
        match c.call(&Op::DropView {
            name: view.as_bytes().to_vec(),
        }) {
            Ok(Reply::Ok) => Ok(true),
            Ok(Reply::Err {
                code: errno::ENOENT,
                ..
            }) => Ok(false),
            Ok(Reply::Err { code, msg }) => Err(io::Error::other(format!(
                "treedb DropView '{view}': errno {code} ({})",
                String::from_utf8_lossy(&msg)
            ))),
            Ok(other) => Err(io::Error::other(unexpected("treedb DropView", &other))),
            Err(e) => Err(io::Error::other(format!("treedb DropView '{view}': {e}"))),
        }
    }

    /// 这个 treedb 实例里现存的 view 名（`view list` 在 treedb 档下的权威来源）。
    pub fn list_views(addr: &str) -> io::Result<Vec<String>> {
        let key = auth_key().map_err(io::Error::other)?;
        let mut c = Client::connect_auth(addr, CONNECT_TIMEOUT_MS, key.as_deref())
            .map_err(|e| io::Error::other(format!("treedb connect {addr}: {e}")))?;
        match c.call(&Op::ListViews) {
            Ok(Reply::Views(v)) => Ok(v
                .into_iter()
                .map(|(name, _, _)| String::from_utf8_lossy(&name).into_owned())
                .collect()),
            Ok(other) => Err(io::Error::other(unexpected("treedb ListViews", &other))),
            Err(e) => Err(io::Error::other(format!("treedb ListViews: {e}"))),
        }
    }

    /// URI → 地址；不是 treedb scheme 时给 `None`。
    pub fn addr_of(uri: &str) -> Result<Option<String>, String> {
        if is_treedb_store(uri) {
            parse_addr(uri).map(Some)
        } else {
            Ok(None)
        }
    }

    // ---------------------------------------------------------------- 传输层

    /// 取一条连接的索引。round-robin 不做负载判定：连接数是个位数，
    /// 真正决定并行度的是「同一刻有几条不在等应答」。
    fn slot(&self) -> usize {
        self.rr.fetch_add(1, Ordering::Relaxed) % self.conns.len()
    }

    /// 发一个 op、收它的应答。传输失败时**只对只读 op** 重连重试一次：
    /// 写 op 的失败可能发生在「引擎已提交、应答没读到」之后，重发就是二次生效
    /// （`Create` 变 `EEXIST`、`RefUp` 把计数加两遍）。写失败让调用方看到 EIO。
    fn xcall(&self, op: &Op) -> Result<Reply, Fail> {
        let i = self.slot();
        let mut g = self.conns[i].lock();
        match g.call(op) {
            Ok(r) => Ok(r),
            Err(e) if op.is_readonly() => {
                let again =
                    Client::connect_auth(&self.addr, CONNECT_TIMEOUT_MS, self.key.as_deref());
                let last = match again {
                    Ok(c) => {
                        *g = c;
                        match g.call(op) {
                            Ok(r) => return Ok(r),
                            Err(e2) => e2.to_string(),
                        }
                    }
                    Err(re) => format!("{e}; reconnect: {re}"),
                };
                Err(Fail::db(format!("treedb transport: {last}")))
            }
            Err(e) => Err(Fail::db(format!("treedb transport: {e}"))),
        }
    }

    /// 一次 pipeline：同一批回包按请求顺序返回，长度与入参一致。
    fn xpipe(&self, ops: &[Op]) -> Result<Vec<Reply>, Fail> {
        let i = self.slot();
        let mut g = self.conns[i].lock();
        // pipeline 里可能混着写（attrs 批取混着 Setattr 的场景本 spike 没有），
        // 所以一律不自动重试。
        g.pipeline(ops)
            .map_err(|e| Fail::db(format!("treedb pipeline: {e}")))
    }

    /// 把 [`Reply::Err`] 变成 [`Fail`]，其余原样交出。
    fn val(r: Reply) -> Result<Reply, Fail> {
        match r {
            Reply::Err { code, msg } => Err(Fail::code(code, msg)),
            other => Ok(other),
        }
    }

    fn want_node(r: Reply) -> Result<Node, Fail> {
        match Self::val(r)? {
            Reply::Node(n) => Ok(n),
            other => Err(Fail::db(unexpected("treedb: expected Node", &other))),
        }
    }

    fn want_freed(r: Reply) -> Result<Option<String>, Fail> {
        match Self::val(r)? {
            Reply::NodeRef(d) => Ok(key_to_str(&d)),
            other => Err(Fail::db(unexpected(
                "treedb: expected NodeRef (GC 线索)",
                &other,
            ))),
        }
    }

    fn want_ok(r: Reply) -> Result<(), Fail> {
        match Self::val(r)? {
            Reply::Ok | Reply::Pong => Ok(()),
            other => Err(Fail::db(unexpected("treedb: expected Ok", &other))),
        }
    }

    // ---------------------------------------------------------------- 视图

    fn lookup_view_id(&self) -> Result<Option<u32>, Fail> {
        let want = self.view.as_bytes();
        match self.xcall(&Op::ListViews) {
            Ok(Reply::Views(v)) => Ok(v
                .into_iter()
                .find(|(n, _, _)| n == want)
                .map(|(_, id, _)| id)),
            Ok(r) => Err(Fail::db(format!("treedb ListViews: {r:?}"))),
            Err(e) => Err(e),
        }
    }

    /// `CreateView`，名字已存在就退回查 id —— 这正是 grovefs「同一个 view 第二次
    /// open」的常见路径，不是错误。
    fn resolve_or_create_view(&self) -> Result<u32, Fail> {
        let op = Op::CreateView {
            name: self.view.as_bytes().to_vec(),
        };
        match self.xcall(&op) {
            Ok(Reply::View { id, root }) => {
                // 引擎的 root ino 恒 1，grovefs 的 `ROOT_INO` 也是 1。不是 1 就说明
                // 两边假设漂了，后面每条路径解析都会错，必须在这里拦住。
                if root != crate::meta::ROOT_INO as u64 {
                    return Err(Fail::db(format!(
                        "treedb: view root ino {root} != grovefs ROOT_INO"
                    )));
                }
                Ok(id)
            }
            Ok(Reply::Err {
                code: errno::EEXIST,
                ..
            }) => self
                .lookup_view_id()?
                .ok_or_else(|| Fail::db("treedb: CreateView said EEXIST but ListViews has no row")),
            Ok(other) => Err(Fail::db(unexpected("treedb CreateView", &other))),
            Err(e) => Err(e),
        }
    }

    fn vid(&self) -> u32 {
        self.view_id
    }

    /// 写路径的统一闸门：只读档不碰引擎。
    fn gate_write(&self) -> Result<(), Fail> {
        if self.readonly {
            Err(Fail::code(
                errno::EROFS,
                "readonly store".as_bytes().to_vec(),
            ))
        } else {
            Ok(())
        }
    }

    // ---------------------------------------------------------------- 名字空间

    /// 一页目录：一条 `ReadDirPlus` 同时拿回 ino/kind/name/属性四个平行数组。
    ///
    /// 以前这里是 `ReadDir` + ⌈N/64⌉ 批 `Getattr`（那一批走 pipeline，不是逐条
    /// 往返，但一页仍然要多个往返、多张应答帧）。引擎自协议 v3 就有带属性的那一
    /// 档，本层 pin 的版本里始终有 —— 换过来之后一整页只有**一次**往返。
    ///
    /// 要补的唯一例外是符号链接：`NodeAttr` 为了不让每条重付 ino/kind/name 三份
    /// 字节，索性不带 `link`（treedb 仓 `DESIGN.md` §4.2），所以这一页里的 symlink
    /// 还得各取一次属性。数据目录里 symlink 罕见，这个补丁通常整页为空。
    fn page(&self, parent: i64, offset: u64, limit: u32) -> Result<Vec<NodeInfo>, Fail> {
        let op = Op::ReadDirPlus {
            view: self.vid(),
            parent: parent as u64,
            offset,
            limit,
        };
        let (inos, kinds, names, attrs) = match Self::val(self.xcall(&op)?)? {
            Reply::DirentsPlus {
                inos,
                kinds,
                names,
                attrs,
                ..
            } => (inos, kinds, names, attrs),
            other => return Err(Fail::db(unexpected("treedb ReadDirPlus", &other))),
        };
        // 四条数组同长是引擎侧的承诺（少一项就是整页错位，那比一个零属性糟得多），
        // 但这是一份跨版本的线协议，宁可在这里当断言查一遍，也不要拿越界下标去索引。
        if kinds.len() != inos.len() || attrs.len() != inos.len() || names.len() != inos.len() {
            return Err(Fail::db(format!(
                "treedb DirentsPlus: parallel arrays disagree ({} ino / {} kind / {} attr / {} name)",
                inos.len(),
                kinds.len(),
                attrs.len(),
                names.len()
            )));
        }
        let mut out = Vec::with_capacity(inos.len());
        let mut links: Vec<u64> = Vec::new();
        for i in 0..inos.len() {
            // `nlink == 0` 只可能是引擎那档「dentry 比 inode 新」的兜底零值（真节点
            // 至少有一条名字）。按 POSIX「readdir 可以返回刚消失的名字」走宽松一侧：
            // 这一条整个不交，绝不回半个条目 —— 与 `ReadDir` + `Getattr` 那版的
            // `filter_map` 行为一致。
            if attrs[i].nlink == 0 {
                continue;
            }
            let kind = Kind::from_code(kinds[i]).map_err(|e| {
                Fail::db(format!(
                    "treedb DirentsPlus: kind byte {} at row {i}: {e}",
                    kinds[i]
                ))
            })?;
            let kind = kind_of(kind);
            if matches!(kind, NodeKind::Symlink) {
                links.push(inos[i]);
            }
            let a = &attrs[i];
            out.push(NodeInfo {
                id: inos[i] as i64,
                parent_id: parent,
                name: String::from_utf8_lossy(names.get(i)).into_owned(),
                kind,
                data_ref: key_to_str(&a.data_ref),
                link_target: None,
                mode: a.mode,
                uid: a.uid,
                gid: a.gid,
                mtime: ms_to_s(a.mtime),
                atime: ms_to_s(a.atime),
                ctime: ms_to_s(a.ctime),
                nlink: a.nlink as u64,
                size: a.size,
            });
        }
        if !links.is_empty() {
            let filled: HashMap<i64, Option<String>> = self
                .attrs(&links)?
                .into_iter()
                .map(|n| (n.id, n.link_target))
                .collect();
            for n in out.iter_mut() {
                if matches!(n.kind, NodeKind::Symlink) {
                    n.link_target = filled.get(&n.id).cloned().flatten();
                }
            }
        }
        Ok(out)
    }

    /// 一批 ino 的属性，按 [`ATTR_BATCH`] 分块走 pipeline。
    ///
    /// `page()` 改走 `ReadDirPlus` 之后，这是全文件唯一还在逐条取属性的地方，而且
    /// 只在「这一页里有符号链接」时才被调用 —— 为的是 `NodeAttr` 故意不带
    /// 的 `link` 字段。
    fn attrs(&self, inos: &[u64]) -> Result<Vec<NodeInfo>, Fail> {
        let mut out = Vec::with_capacity(inos.len());
        for chunk in inos.chunks(ATTR_BATCH) {
            let ops: Vec<Op> = chunk
                .iter()
                .map(|&ino| Op::Getattr {
                    view: self.vid(),
                    ino,
                })
                .collect();
            for r in self.xpipe(&ops)? {
                if let Ok(n) = Self::want_node(r) {
                    out.push(info_of(&n));
                }
                // 取不到属性的 ino 直接消失：目录项比 inode 新（并发 unlink）时
                // 这就是正确行为，别拿一个假属性去填 FUSE 回复。
            }
        }
        Ok(out)
    }

    fn page_all(&self, parent: i64) -> Result<Vec<NodeInfo>, Fail> {
        let mut acc = Vec::new();
        let mut off = 0u64;
        loop {
            let got = self.page(parent, off, PAGE)?;
            if got.is_empty() {
                break;
            }
            off += got.len() as u64;
            acc.extend(got);
            if off > u32::MAX as u64 {
                break; // 防御：没人该把单目录推到 40 亿条
            }
        }
        Ok(acc)
    }

    /// 建一条 inode。`Reply::Node` 已带全属性，所以 `*_with_attr` 不必二次读。
    fn create(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Node, Fail> {
        self.gate_write()?;
        let v = self.vid();
        let name = name.as_bytes().to_vec();
        let op = if is_dir {
            Op::Mkdir {
                view: v,
                parent: parent as u64,
                name,
                mode,
                uid,
                gid,
            }
        } else {
            Op::Create {
                view: v,
                parent: parent as u64,
                name,
                mode,
                uid,
                gid,
            }
        };
        Self::want_node(self.xcall(&op)?)
    }

    /// 把内容对象键挂到 inode 上（引擎侧记账，见 `SA_DATA_REF`）。
    fn attach(&self, ino: i64, data_ref: &str) -> Result<Node, Fail> {
        let op = Op::Setattr {
            view: self.vid(),
            ino: ino as u64,
            valid: SA_DATA_REF,
            mode: 0,
            uid: 0,
            gid: 0,
            size: 0,
            atime: 0,
            mtime: 0,
            data_ref: data_ref.as_bytes().to_vec(),
            ref_bytes: 0,
        };
        Self::want_node(self.xcall(&op)?)
    }

    /// grovefs 的一个 `unlink` covers 文件与目录，引擎里是 `Unlink`/`Rmdir` 两条
    /// op 且各自有 EISDIR/ENOTDIR 守卫。先按文件试，撞 EISDIR 再按目录试：
    /// 常见路径一次往返，代价是「删目录」两次往返。
    fn unlink_any(&self, parent: i64, name: &str) -> Result<Option<String>, Fail> {
        self.gate_write()?;
        let v = self.vid();
        let n = name.as_bytes().to_vec();
        match self.xcall(&Op::Unlink {
            view: v,
            parent: parent as u64,
            name: n.clone(),
        }) {
            Ok(r) => Self::want_freed(r),
            Err(f) if f.code == Some(errno::EISDIR) => {
                Self::want_freed(self.xcall(&Op::Rmdir {
                    view: v,
                    parent: parent as u64,
                    name: n,
                })?)
            }
            Err(f) => Err(f),
        }
    }

    /// 属性类写入的公共部分：`Setattr` 只带自己那一位 valid。
    /// 参数就是 FUSE setattr 掩码的形状（同 `engine.rs`/`pg_store.rs` 的对应方法），
    /// 不为过 clippy 的 arity 线而拆散它。
    #[allow(clippy::too_many_arguments)]
    fn setattr(&self, ino: i64, valid: u32, mode: u32, uid: u32, gid: u32, atime: i64, mtime: i64) {
        if self.readonly {
            return;
        }
        let op = Op::Setattr {
            view: self.vid(),
            ino: ino as u64,
            valid,
            mode,
            uid,
            gid,
            size: 0,
            atime,
            mtime,
            data_ref: Vec::new(),
            ref_bytes: 0,
        };
        // 与 SQLite/PG 档一致：这些方法在 trait 里就没有返回位，属性写失败
        // 只能靠上层下一次 getattr 自己发现。
        let _ = self.xcall(&op).and_then(Self::want_ok);
    }
}

impl MetaProvider for TreedbStore {
    fn lookup(&self, parent: i64, name: &str) -> Option<NodeInfo> {
        let op = Op::Lookup {
            view: self.vid(),
            parent: parent as u64,
            name: name.as_bytes().to_vec(),
        };
        self.xcall(&op)
            .ok()
            .and_then(|r| Self::want_node(r).ok())
            .map(|n| info_of(&n))
    }

    fn getattr(&self, ino: i64) -> Option<NodeInfo> {
        let op = Op::Getattr {
            view: self.vid(),
            ino: ino as u64,
        };
        self.xcall(&op)
            .ok()
            .and_then(|r| Self::want_node(r).ok())
            .map(|n| info_of(&n))
    }

    fn readdir(&self, parent: i64) -> Vec<NodeInfo> {
        match self.page_all(parent) {
            Ok(v) => v,
            Err(f) => {
                log_list_failure("readdir", parent, &f);
                Vec::new()
            }
        }
    }

    fn readdir_page(&self, parent: i64, offset: usize, limit: usize) -> Vec<NodeInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let lim = limit.min(u32::MAX as usize) as u32;
        match self.page(parent, offset as u64, lim) {
            Ok(v) => v,
            Err(f) => {
                log_list_failure("readdir_page", parent, &f);
                Vec::new()
            }
        }
    }

    /// 引擎的 `ReadDir` 就是 `offset/limit` 分页（offset = 已返回条目数），
    /// 与这里要的页语义同形，所以照 PG 档一样开。
    fn supports_paged_readdir(&self) -> bool {
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn create_node(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.create_node_with_attr(parent, name, is_dir, data_ref, mode, uid, gid)
            .map(|n| n.id)
    }

    #[allow(clippy::too_many_arguments)]
    fn create_node_with_attr(
        &self,
        parent: i64,
        name: &str,
        is_dir: bool,
        data_ref: Option<&str>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        let mut n = self.create(parent, name, is_dir, mode, uid, gid)?;
        if let Some(k) = data_ref.filter(|k| !k.is_empty()) {
            n = self.attach(n.ino as i64, k).map_err(|f| f.to_meta())?;
        }
        Ok(info_of(&n))
    }

    fn create_symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<i64, MetaError> {
        self.create_symlink_with_attr(parent, name, target, uid, gid)
            .map(|n| n.id)
    }

    fn create_symlink_with_attr(
        &self,
        parent: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<NodeInfo, MetaError> {
        self.gate_write().map_err(|f| f.to_meta())?;
        let op = Op::Symlink {
            view: self.vid(),
            parent: parent as u64,
            name: name.as_bytes().to_vec(),
            target: target.as_bytes().to_vec(),
            uid,
            gid,
        };
        let n = self
            .xcall(&op)
            .and_then(Self::want_node)
            .map_err(|f| f.to_meta())?;
        Ok(info_of(&n))
    }

    /// 没有 `Link` op：一个 inode 只有一条路径。回 EPERM，与 `ln` 在只读名字空间
    /// 上失败同样可见，绝不返回「成功了但其实又建了一条 dentry」。
    fn link_node(
        &self,
        _src_ino: i64,
        _new_parent: i64,
        _new_name: &str,
    ) -> Result<i64, MetaError> {
        Err(Fail::code(errno::EPERM, b"treedb has no hardlinks".to_vec()).to_meta())
    }

    fn unlink(&self, parent: i64, name: &str) -> Result<Option<String>, MetaError> {
        self.unlink_any(parent, name).map_err(|f| f.to_meta())
    }

    fn rename(
        &self,
        parent: i64,
        name: &str,
        new_parent: i64,
        new_name: &str,
        noreplace: bool,
    ) -> Result<Option<String>, MetaError> {
        self.gate_write().map_err(|f| f.to_meta())?;
        let op = Op::Rename {
            view: self.vid(),
            old_parent: parent as u64,
            old_name: name.as_bytes().to_vec(),
            new_parent: new_parent as u64,
            new_name: new_name.as_bytes().to_vec(),
            flags: if noreplace { RENAME_NOREPLACE } else { 0 },
        };
        // 应答里的 GC 线索与 SQLite/PG 档的 `Ok(Some(key))` 同位：调用方拿它去
        // `backend.remove(key)`。判定在引擎侧与删 dentry 同 op 完成，所以没有
        // 「先 unlink 再问引用数」那个窗口。
        self.xcall(&op)
            .and_then(Self::want_freed)
            .map_err(|f| f.to_meta())
    }

    fn set_mode(&self, ino: i64, mode: u32) {
        self.setattr(ino, SA_MODE, mode, 0, 0, 0, 0);
    }

    fn set_uid_gid(&self, ino: i64, uid: u32, gid: u32) {
        self.setattr(ino, SA_UID | SA_GID, 0, uid, gid, 0, 0);
    }

    fn set_times(&self, ino: i64, atime: Option<i64>, mtime: Option<i64>) {
        if atime.is_none() && mtime.is_none() {
            return;
        }
        // 引擎的 Setattr 是「valid 位说改哪个就改哪个」，一次调用带两位。
        let mut valid = 0u32;
        let (mut a, mut m) = (0i64, 0i64);
        if let Some(x) = atime {
            valid |= SA_ATIME;
            a = s_to_ms(x);
        }
        if let Some(x) = mtime {
            valid |= SA_MTIME;
            m = s_to_ms(x);
        }
        self.setattr(ino, valid, 0, 0, 0, a, m);
    }

    fn touch_mtime(&self, ino: i64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.set_times(ino, None, Some(now));
    }

    /// 锁：见模块头。这里**不给语义**，给一个诚实的失败。
    fn lock_op(
        &self,
        _ino: i64,
        _owner: u64,
        _start: u64,
        _end: u64,
        _ty: LockType,
        _pid: u32,
        _sid: u64,
    ) -> Result<Option<LockConflict>, MetaError> {
        Err(Fail::code(
            errno::EPERM,
            b"treedb locks are per-inode, not per byte-range; POSIX record locks are not served here".to_vec(),
        )
        .to_meta())
    }

    /// 与 `lock_op` 一致：既然从不授予锁，`F_GETLK` 也就没有阻塞者。
    fn get_lock(
        &self,
        _ino: i64,
        _owner: u64,
        _start: u64,
        _end: u64,
        _ty: LockType,
    ) -> Option<LockConflict> {
        None
    }

    fn release_locks_of_owner(&self, _owner: u64) {}

    fn renew_locks(&self, _sid: u64) {}

    fn fence_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.fence)
    }

    /// 整棵树走一遍（`du` / `gc check` 的服务侧数据源）。每目录一次 `ReadDir`
    /// 分页 + 批 `Getattr`，所以千级目录是几十次往返 —— 够用，但不是 PG 档那种
    /// 一条 SQL 的量级。
    fn dump_tree(&self) -> Vec<(i64, i64, String, bool, Option<String>)> {
        let mut out = Vec::new();
        let root = crate::meta::ROOT_INO;
        let mut queue = vec![root];
        // 迭代而不是递归：目录深度是调用方给的输入，栈不该由它决定。
        while let Some(dir) = queue.pop() {
            let entries = match self.page_all(dir) {
                Ok(v) => v,
                Err(f) => {
                    // `dump_tree` 没有失败通道（trait 签名只回 `Vec`），所以「少了
                    // 一条子树」至少要被听见：`du` 拿这份 dump 算用量，静默返回
                    // 局部结果就是少报。
                    eprintln!(
                        "treedb dump_tree: listing ino {dir} failed ({}) — the dump is INCOMPLETE",
                        f.msg
                    );
                    break;
                }
            };
            for e in entries {
                if e.kind == NodeKind::Dir {
                    queue.push(e.id);
                }
                let is_dir = e.is_dir();
                out.push((e.id, e.parent_id, e.name, is_dir, e.data_ref));
            }
        }
        out
    }
}

/// `MetaStore` 上那几个非 trait 的方法（HA/池/备份）在 treedb 档下的回答。
impl TreedbStore {
    pub fn get_epoch(&self) -> i64 {
        self.epoch.load(Ordering::Relaxed)
    }

    pub fn set_epoch(&self, e: i64) -> io::Result<()> {
        self.epoch.store(e, Ordering::Relaxed);
        self.fence.store(e.max(0) as u64, Ordering::Relaxed);
        Ok(())
    }

    /// 与 PG 档同一条消息：原生 HA 是 SQLite 的 `VACUUM INTO`，这里没有对应物。
    pub fn backup_to_bytes(&self) -> io::Result<Vec<u8>> {
        Err(io::Error::other(
            "GroveFS-native HA snapshot is SQLite-only; treedb has no snapshot shipping yet \
             (single instance, its own WAL + checkpoint)",
        ))
    }

    /// treedb 的连接是 open 时定死的 `GROVEFS_TREEDB_CONNS` 条，没有「burst 之后
    /// 缩回地板」这回事，所以 sweeper 问的时候答 0，但**别**据此关掉回收线程：
    /// 这个后端真正的资源回收单位是整个 store（`rt.evict()`）。
    pub fn shrink_pools(&self) -> usize {
        0
    }

    /// 诊断用：地址、view id、连接数。给 banner 与 `view info` 留一个可读出口。
    pub fn describe(&self) -> String {
        format!(
            "treedb {} view='{}' id={} conns={}",
            self.addr,
            self.view,
            self.view_id,
            self.conns.len()
        )
    }
}

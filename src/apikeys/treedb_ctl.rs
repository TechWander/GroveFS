//! 控制面的第三个后端：把 `api_keys` / `api_key_usage` / `mcp_calls` 搬进 treedb 的
//! **通用表**（treedb 仓 `DESIGN.md` §10 阶段 C 4 的 M1）。
//!
//! **为什么不是一个 `Backend` 变体**：`apikeys::Backend` 分派的是**两套 SQL 方言**
//! （`exec2(sqlite_sql, pg_sql)` / `query2(...)`），而 treedb 没有 SQL —— 所以第三臂
//! 只能接在**方法层**：每个动词先问一句「是不是引擎后端」，是就把语义交给这里，不走
//! 任何 SQL 拼装。`exec2`/`query2` 里那个 `Backend::Treedb(_) => Err(...)` 分支因此
//! 永远不该被走到，留成拒绝而不是 `unreachable!()`（同 PG 分支的理由）。
//!
//! **入口**：`--meta-store treedb://…` 即走这里 —— 没有开关（M3 删掉了
//! `GROVEFS_TREEDB_CONTROL`）。引擎里的控制面为空而本地旧文件还有行时，服务端启动会
//! 拒绝并点名 `grovefs control-migrate`，否则症状是「谁都登录不了」，不是「迁移没做」；
//! 迁移跑过之后，`control_migrate::settle` 把旧文件改名为 `*.pre-treedb`。
//!
//! **形状**（与 §10 C 4 逐条对应，改动时两处一起改）：
//! - 表名一律 `grovefs_control__` 前缀：引擎的 `tables` 是一张平表，没有命名空间也
//!   没有权限，前缀是唯一的隔离手段；代价是一个引擎实例只放一套 grovefs 控制面。
//! - 行主键 = 今天 SQL 的主键；列值一律 UTF-8 字节；引擎没有 NULL ⇒ **空字节串就是
//!   NULL**（`revoked_at`/`expires_at`/`view`/`note`/… 全靠这条）；布尔 1 字节
//!   `b"1"`/`b"0"`；时间戳保持今天的 RFC 3339 文本，迁移不改写值。
//! - `last_used_at` 单独一张表：每次鉴权都会写它，塞在凭据行里就会与
//!   rotate/revoke 抢同一行，而 `RowPut` 是整行覆盖 ⇒ 必须读-改-CAS-写回。
//! - `mcp_calls` 的主键是 `!ts_us || !seq`（两个 8 字节大端**取反**）：取反 ⇒ 主键序
//!   就是时间倒序，「最新 N 条」直接是第一页，不用全表扫再排。SQLite 的 `rowid` 与
//!   PG 的 `seq` 在这里换成「微秒 + 进程内自增」，同级桶用 `PUT_IF_ABSENT` 撞键重试。
//! - 三处「对不上」的绕法：`ORDER BY created_at DESC` 与 `WHERE ts < cutoff` 都是
//!   全表扫 + 客户端过滤（这两张表的量级是运维手工发的 key 与审计行，与今天 SQLite
//!   那条 `DELETE` 一条条删同阶）；`WHERE role=… AND disabled_at IS NULL` 那种跨列条件
//!   属于 webauth（M1b），这里不涉及。
//!
//! 与 SQL 档的语义必须逐条对齐，凡是这里自己决定的地方都写了理由；`apikeys.rs` 那边
//! 只负责「哪个后端」与参数归一化（[`super::note_and_deadline`] 是两边共用的一条）。

use std::sync::atomic::{AtomicU64, Ordering};

use treedb::engine::model::{errno, Op, Reply, TableDef, PUT_IF_ABSENT, ROW_PK};

use super::{expiry_passed, now_ms, CallLog, KeyInfo, StoredKey};
use crate::control_treedb::{
    col, dump, flag, is_set, opt, opt_str, put_fields, text, Fields, Rows, PAGE,
};

/// 引擎的 `tables` 没有命名空间，前缀是唯一的隔离手段。
const T_KEYS: &str = "grovefs_control__api_keys";
const T_USAGE: &str = "grovefs_control__api_key_usage";
const T_CALLS: &str = "grovefs_control__mcp_calls";

/// `api_keys` 的列序。**这一串数字就是存储格式**：改顺序 = 改磁盘格式，要么加迁移
/// 要么换个表名重来。CAS 用的条件列（`revoked_at`）也从这里取，免得两处各写一个 11。
const AK_LABEL: usize = 0;
const AK_HASH: usize = 1;
const AK_HINT: usize = 2;
const AK_SCOPE: usize = 3;
const AK_VIEW: usize = 4;
const AK_RO: usize = 5;
const AK_OWNER: usize = 6;
const AK_NOTE: usize = 7;
const AK_CREATED_AT: usize = 8;
const AK_CREATED_BY: usize = 9;
const AK_EXPIRES_AT: usize = 10;
const AK_REVOKED_AT: usize = 11;
/// 一行的列数（也是 `RowPut` 要求的 `cols.len()`）。
const AK_COLS: usize = 12;

/// `mcp_calls` 的列序，同上。
const MC_TS: usize = 0;
const MC_KEY_ID: usize = 1;
const MC_KEY_LABEL: usize = 2;
const MC_METHOD: usize = 3;
const MC_TOOL: usize = 4;
const MC_OK: usize = 5;
const MC_DETAIL: usize = 6;
const MC_COLS: usize = 7;

/// 翻页大小与 errno 都在 [`crate::control_treedb`]：两张表（凭据、账户）用的是同一套
/// 管道，分两份写只会漂。

pub(super) struct TreedbKeys {
    rows: Rows,
    /// `mcp_calls` 主键后半段的自增。每行一个值、撞键就重取，见 [`Self::log_call`]。
    seq: AtomicU64,
}

impl TreedbKeys {
    /// 打开（或首次创建）三张表。`EEXIST` 视为成功（[`Rows::create`]）。
    pub(super) fn open(addr: &str) -> Result<Self, String> {
        let rows = Rows::open(addr)?;
        for def in [keys_def(), usage_def(), calls_def()] {
            rows.create(def)?;
        }
        Ok(Self { rows, seq: AtomicU64::new(0) })
    }

    // ---- api_keys ----------------------------------------------------------

    /// 新行。`PUT_IF_ABSENT` 让「id 撞了」变成 `EEXIST` 而不是覆盖别人的凭据：
    /// id 是 64 bit 随机，撞了也该报错而不是悄悄接管。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn insert_key(
        &self,
        id: &str,
        label: &str,
        hash: &str,
        hint: &str,
        scope: &str,
        view: Option<&str>,
        ro: bool,
        owner: Option<&str>,
        note: Option<&str>,
        expires_at: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<(), String> {
        let mut cols = vec![Vec::new(); AK_COLS];
        cols[AK_LABEL] = label.as_bytes().to_vec();
        cols[AK_HASH] = hash.as_bytes().to_vec();
        cols[AK_HINT] = hint.as_bytes().to_vec();
        cols[AK_SCOPE] = scope.as_bytes().to_vec();
        cols[AK_VIEW] = opt(view);
        cols[AK_RO] = flag(ro);
        cols[AK_OWNER] = opt(owner);
        cols[AK_NOTE] = opt(note);
        cols[AK_CREATED_AT] = super::now_iso().into_bytes();
        cols[AK_CREATED_BY] = opt(created_by);
        cols[AK_EXPIRES_AT] = opt(expires_at);
        // revoked_at 留空 = 活着；`create_extended` 从不造已撤销的行。
        self.rows.put(T_KEYS, id.as_bytes(), cols, PUT_IF_ABSENT, None)
            .map_err(|e| e.to_string())
    }

    pub(super) fn fetch_key(&self, id: &str) -> Result<Option<StoredKey>, String> {
        Ok(self.rows.get(T_KEYS, id.as_bytes())?.map(|c| stored(&c)))
    }

    /// 记录一次鉴权。**无条件覆盖**：SQL 那句 `UPDATE … WHERE id = ?` 命中 0 行时也是
    /// 静默成功（`stamp_used` 的契约就是「可能整条丢」），所以这里照样不管 key 行在不在
    /// —— 孤儿 usage 行不会被读到，删 key 时顺手删它（见 `apikeys` 的注释）。
    pub(super) fn stamp_used(&self, id: &str) -> Result<(), String> {
        let cols = vec![super::now_iso().into_bytes()];
        self.rows.put(T_USAGE, id.as_bytes(), cols, 0, None)
            .map_err(|e| e.to_string())
    }

    /// 就地改一行活着的凭据。`edit` 拿到的 `cols` 就是整行；返回 `false` 表示没有
    /// 活着的行（已撤销/不存在），与 SQL 档「`WHERE … AND revoked_at IS NULL` 命中 0 行」
    /// 同义。CAS 条件也钉在 `revoked_at == 空`：读与写之间被人撤销了，这次编辑作废。
    fn edit_live(
        &self,
        id: &str,
        edit: impl FnOnce(&mut Vec<Vec<u8>>),
    ) -> Result<bool, String> {
        let Some(mut cols) = self.rows.get(T_KEYS, id.as_bytes())? else {
            return Ok(false);
        };
        if !cols[AK_REVOKED_AT].is_empty() {
            return Ok(false);
        }
        edit(&mut cols);
        match self.rows.put(
            T_KEYS,
            id.as_bytes(),
            cols,
            0,
            Some((AK_REVOKED_AT as u32, Vec::new())),
        ) {
            Ok(()) => Ok(true),
            // EAGAIN = 条件不命中 = 这中间被撤销了。那是正常竞态，不是错误。
            Err(e) if e.is(errno::EAGAIN) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }

    pub(super) fn set_ro(&self, id: &str, ro: bool) -> Result<bool, String> {
        self.edit_live(id, |cols| cols[AK_RO] = flag(ro))
    }

    pub(super) fn rotate(&self, id: &str, hash: &str, hint: &str) -> Result<bool, String> {
        self.edit_live(id, |cols| {
            cols[AK_HASH] = hash.as_bytes().to_vec();
            cols[AK_HINT] = hint.as_bytes().to_vec();
        })
    }

    pub(super) fn update_fields(
        &self,
        id: &str,
        note: Option<&str>,
        expires_at: Option<Option<&str>>,
    ) -> Result<bool, String> {
        let (note_text, deadline) = super::note_and_deadline(note, expires_at);
        self.edit_live(id, |cols| {
            if note.is_some() {
                cols[AK_NOTE] = opt(note_text);
            }
            if expires_at.is_some() {
                cols[AK_EXPIRES_AT] = opt(deadline);
            }
        })
    }

    pub(super) fn revoke(&self, id: &str) -> Result<bool, String> {
        let ts = super::now_iso();
        self.edit_live(id, |cols| cols[AK_REVOKED_AT] = ts.into_bytes())
    }

    /// 全部 key，按 `created_at` 倒序。SQL 档靠 `ORDER BY created_at DESC`（PG 再加
    /// `seq DESC` 拆同一时刻的平手）；这里主键序是 id，所以排序在客户端做，平手也用
    /// id 定序 —— 比 SQLite 的「同一时刻顺序未定义」更确定，不丢信息。
    ///
    /// `last_used_at` 在另一张表，所以要再扫一遍 usage（一行一个用过的 key，量级同 key
    /// 数），在这里拼回去。
    pub(super) fn list(&self) -> Result<Vec<KeyInfo>, String> {
        let usage: std::collections::HashMap<Vec<u8>, Option<String>> = self
            .rows
            .scan(T_USAGE)?
            .into_iter()
            .map(|(k, cols)| (k, opt_str(&cols[0])))
            .collect();
        let mut out = Vec::new();
        for (key, c) in self.rows.scan(T_KEYS)? {
            out.push(KeyInfo {
                id: String::from_utf8_lossy(&key).into_owned(),
                owner: opt_str(&c[AK_OWNER]),
                label: text(&c[AK_LABEL]),
                scope: text(&c[AK_SCOPE]),
                view: opt_str(&c[AK_VIEW]),
                ro: is_set(&c[AK_RO]),
                created_at: text(&c[AK_CREATED_AT]),
                last_used_at: usage.get(&key).cloned().flatten(),
                revoked_at: opt_str(&c[AK_REVOKED_AT]),
                note: opt_str(&c[AK_NOTE]),
                expires_at: opt_str(&c[AK_EXPIRES_AT]),
                created_by: opt_str(&c[AK_CREATED_BY]),
                secret_hint: opt_str(&c[AK_HINT]),
                expired: false,
            });
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        // `expired` 是**服务端**的判词，一次时钟读数管整批 —— 与 SQL 档同一条规矩。
        let now = now_ms();
        for k in &mut out {
            k.expired = expiry_passed(k.expires_at.as_deref(), now);
        }
        Ok(out)
    }

    // ---- mcp_calls ---------------------------------------------------------

    /// 追加一行审计。主键 `!ts_us || !seq` 让「最新 N 条」= 主键序前 N 条。
    ///
    /// 撞键只有两种可能：同一微秒内的并发（本进程用 `seq` 拆开）与另一个进程恰好
    /// 算出了同一个 `(ts_us, seq)`。后者用 `PUT_IF_ABSENT` 挡下来 → 取下一个 `seq`
    /// 重试。审计行**不能**因为撞键就丢，所以这里是重试而不是覆盖。
    pub(super) fn log_call(
        &self,
        key_id: Option<&str>,
        key_label: Option<&str>,
        method: &str,
        tool: &str,
        ok: bool,
        detail: &str,
    ) -> Result<(), String> {
        let ts = super::now_iso();
        let mut cols = vec![Vec::new(); MC_COLS];
        cols[MC_TS] = ts.into_bytes();
        cols[MC_KEY_ID] = opt(key_id);
        cols[MC_KEY_LABEL] = opt(key_label);
        cols[MC_METHOD] = method.as_bytes().to_vec();
        cols[MC_TOOL] = tool.as_bytes().to_vec();
        cols[MC_OK] = flag(ok);
        cols[MC_DETAIL] = detail.as_bytes().to_vec();
        let ts_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        for _ in 0..8 {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            match self.rows.put(T_CALLS, &call_key(ts_us, seq), cols.clone(), PUT_IF_ABSENT, None) {
                Ok(()) => return Ok(()),
                Err(e) if e.is(errno::EEXIST) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("treedb control: could not find a free audit-row key in 8 tries".to_string())
    }

    pub(super) fn recent_calls(&self, limit: usize) -> Result<Vec<CallLog>, String> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(limit.min(PAGE as usize));
        let mut from: Vec<u8> = Vec::new();
        while out.len() < limit {
            let want = (limit - out.len()).min(PAGE as usize) as u32;
            let r = self
                .rows
                .call(Op::RowList {
                    table: T_CALLS.as_bytes().to_vec(),
                    column: ROW_PK,
                    value: Vec::new(),
                    from: from.clone(),
                    limit: want,
                })
                .map_err(|e| e.to_string())?;
            match r {
                Reply::Rows { rows, end } => {
                    let last = rows.last().map(|r| r.key.clone());
                    out.extend(rows.into_iter().map(|r| call_log(&r.cols)));
                    match last {
                        Some(k) if !end && out.len() < limit => from = k,
                        _ => break,
                    }
                }
                other => return Err(crate::control_treedb::unexpected("RowList", &other)),
            }
        }
        Ok(out)
    }

    /// 严格早于 `cutoff` 的行数。SQL 档是一条 `count(*) WHERE ts < ?`；这里全表扫 +
    /// 同一条文本比较（`ts` 是定宽 RFC 3339，字符串序就是时间序）。这张表是审计日志，
    /// 量级与今天 SQLite 那条逐行 `DELETE` 同阶，索引换不来一个量级。
    pub(super) fn count_calls_older_than(&self, cutoff: &str) -> Result<usize, String> {
        Ok(self
            .rows
            .scan(T_CALLS)?
            .into_iter()
            .filter(|(_, c)| text(&c[MC_TS]).as_str() < cutoff)
            .count())
    }

    /// 删掉早于 `cutoff` 的审计行，返回删了几条。谓词就是它说的那个（不抽样、不封顶），
    /// 与 [`Self::count_calls_older_than`] 用同一条比较，所以 dry-run 与实删对得上。
    pub(super) fn purge_calls_older_than(&self, cutoff: &str) -> Result<usize, String> {
        let mut n = 0;
        for (key, c) in self.rows.scan(T_CALLS)? {
            if text(&c[MC_TS]).as_str() < cutoff {
                self.rows.del(T_CALLS, &key)?;
                n += 1;
            }
        }
        Ok(n)
    }
}

// ---- 编码 ----------------------------------------------------------------

/// `mcp_calls` 的主键：两个 8 字节大端取反，所以「新」排在「旧」前面。
///
/// `pub(crate)` 是给 M2 的：`control-migrate` 必须用**同一个**公式给本地审计行
/// 算键，否则迁移过去的行在 `recent_calls` 里的顺序与今天不同。
pub(crate) fn call_key(ts_us: i64, seq: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(16);
    k.extend_from_slice(&(!(ts_us as u64)).to_be_bytes());
    k.extend_from_slice(&(!seq).to_be_bytes());
    k
}

fn stored(c: &[Vec<u8>]) -> StoredKey {
    StoredKey {
        label: text(&c[AK_LABEL]),
        secret_hash: text(&c[AK_HASH]),
        scope: text(&c[AK_SCOPE]),
        revoked_at: opt_str(&c[AK_REVOKED_AT]),
        view: opt_str(&c[AK_VIEW]),
        ro: is_set(&c[AK_RO]),
        owner: opt_str(&c[AK_OWNER]),
        expires_at: opt_str(&c[AK_EXPIRES_AT]),
    }
}

fn call_log(c: &[Vec<u8>]) -> CallLog {
    CallLog {
        ts: text(&c[MC_TS]),
        key_id: opt_str(&c[MC_KEY_ID]),
        key_label: opt_str(&c[MC_KEY_LABEL]),
        method: text(&c[MC_METHOD]),
        tool: text(&c[MC_TOOL]),
        ok: is_set(&c[MC_OK]),
        detail: text(&c[MC_DETAIL]),
    }
}

// ---- 表形状 --------------------------------------------------------------

fn keys_def() -> TableDef {
    TableDef {
        name: T_KEYS.as_bytes().to_vec(),
        // 列序 = [`AK_*`] 常量。没有索引列：`list` 是全表扫 + 客户端排序（手工发的
        // key，量级在几百），而所有热路径（fetch/stamp/edit）都是按主键直取。
        columns: vec![
            col("label", 0),
            col("secret_hash", 0),
            col("secret_hint", 0),
            col("scope", 0),
            col("view", 0),
            col("ro", 0),
            col("owner", 0),
            col("note", 0),
            col("created_at", 0),
            col("created_by", 0),
            col("expires_at", 0),
            col("revoked_at", 0),
        ],
    }
}

fn usage_def() -> TableDef {
    TableDef {
        name: T_USAGE.as_bytes().to_vec(),
        // 主键 = key id。一行只有一个字段，正因为「每次鉴权都写」这件事不该碰凭据行。
        columns: vec![col("last_used_at", 0)],
    }
}

fn calls_def() -> TableDef {
    TableDef {
        name: T_CALLS.as_bytes().to_vec(),
        // 主键 = `!ts_us || !seq`（[`call_key`]），所以这里不从主键拆 ts_us 列 ；
        // `ts` 是给人看的那份文本，与 SQL 档逐字相同。
        columns: vec![
            col("ts", 0),
            col("key_id", 0),
            col("key_label", 0),
            col("method", 0),
            col("tool", 0),
            col("ok", 0),
            col("detail", 0),
        ],
    }
}

// ---- 迁移面（M2：`grovefs control-migrate --to engine`）--------------------
//
// 三张表的「引擎列序 ↔ 本地 SQLite 列名」与开表用的 DDL 交出去，迁移那边只按名
// 填值、按名比对，不抄任何列号。**列名与上面 `AK_*` 那一段必须同时改**：名字列
// 表是从那段常量复述出来的，顺序错了 `put_fields` 会把值写进隔壁列。

pub(crate) const KEY_COLS: [&str; AK_COLS] = [
    "label",
    "secret_hash",
    "secret_hint",
    "scope",
    "view",
    "ro",
    "owner",
    "note",
    "created_at",
    "created_by",
    "expires_at",
    "revoked_at",
];
/// 引擎里这张表的唯一一列，本地是 `api_keys.last_used_at`（M1 把它拆了出来，
/// 免得每次鉴权都去 CAS 凭据行）。
pub(crate) const USAGE_COLS: [&str; 1] = ["last_used_at"];
pub(crate) const CALL_COLS: [&str; MC_COLS] =
    ["ts", "key_id", "key_label", "method", "tool", "ok", "detail"];

pub(crate) fn tables() -> [TableDef; 3] {
    [keys_def(), usage_def(), calls_def()]
}

pub(crate) fn put_key(rows: &Rows, key: &[u8], f: &Fields) -> Result<(), String> {
    put_fields(rows, T_KEYS, key, &KEY_COLS, f)
}

pub(crate) fn put_usage(rows: &Rows, key: &[u8], f: &Fields) -> Result<(), String> {
    put_fields(rows, T_USAGE, key, &USAGE_COLS, f)
}

pub(crate) fn put_call(rows: &Rows, key: &[u8], f: &Fields) -> Result<(), String> {
    put_fields(rows, T_CALLS, key, &CALL_COLS, f)
}

pub(crate) fn dump_keys(rows: &Rows) -> Result<Vec<(Vec<u8>, Fields)>, String> {
    dump(rows, T_KEYS, &KEY_COLS)
}

pub(crate) fn dump_usage(rows: &Rows) -> Result<Vec<(Vec<u8>, Fields)>, String> {
    dump(rows, T_USAGE, &USAGE_COLS)
}

pub(crate) fn dump_calls(rows: &Rows) -> Result<Vec<(Vec<u8>, Fields)>, String> {
    dump(rows, T_CALLS, &CALL_COLS)
}

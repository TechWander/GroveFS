//! The PLUMBING every control-plane table in the `treedb` engine needs: one call
//! out, one reply in, one row read or written.
//!
//! Two very different schemas sit on top of this — [`crate::apikeys`]' credentials
//! and audit trail, and [`crate::webauth`]'s sign-in accounts and delegations
//! (treedb DESIGN.md §10 C 4, milestones M1a/M1b). Their *semantics* share
//! nothing; what they do share is the answer to "where does an engine errno come
//! from", and that answer must exist once. The refusals that matter are not
//! errors: `EEXIST` (a `PUT_IF_ABSENT` lost a race) and `EAGAIN` (a CAS condition
//! did not hold) are ordinary paths — folding them into a message loses the
//! distinction for good, which is why [`CtlErr`] keeps the code beside the text.
//!
//! Two shapes are deliberately absent here and live in the callers:
//! * **Which tables** exist and what their columns mean (a column order is a disk
//!   format, and each store documents its own).
//! * **Read → modify → CAS write**. `RowPut` replaces the whole row, so every
//!   in-place edit is that loop; the condition column differs per store
//!   (`revoked_at == ""` for a credential, a `rev` counter for an account), and
//!   that choice is exactly what the caller has to justify.
//!
//! Nothing here interprets a byte: the engine's columns are opaque, and "no NULL"
//! is encoded as the empty byte string by [`opt`]/[`opt_str`] — one pair of
//! functions so the two directions cannot drift.

use std::sync::Arc;

use treedb::engine::model::{
    errno, ColumnDef, Op, Reply, TableDef, COND_NONE, ROW_PK,
};

use crate::treedb_store::ConnPool;

/// Rows per page. `RowList`'s cursor is "the last key of the previous page", so a
/// page starts at a located position instead of walking from the top.
pub(crate) const PAGE: u32 = 512;

/// One failed call, with the engine's errno when it had one.
pub(crate) struct CtlErr {
    pub(crate) code: Option<u16>,
    pub(crate) msg: String,
}

impl CtlErr {
    pub(crate) fn is(&self, code: u16) -> bool {
        self.code == Some(code)
    }

    /// A reply that was well-formed but unexpected (a row where a count was due,
    /// …). No errno: nothing on the engine side rejected the call.
    pub(crate) fn other(msg: impl Into<String>) -> Self {
        CtlErr { code: None, msg: msg.into() }
    }
}

impl std::fmt::Display for CtlErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "treedb control: errno {c} ({})", self.msg),
            None => write!(f, "treedb control: {}", self.msg),
        }
    }
}

/// One connection pool to the engine, plus the row primitives. Cheap to share and
/// stateless: the pool is process-wide ([`ConnPool::for_addr`]), so several stores
/// opening the same address share its connections.
pub(crate) struct Rows {
    pool: Arc<ConnPool>,
}

impl Rows {
    pub(crate) fn open(addr: &str) -> Result<Self, String> {
        let pool =
            ConnPool::for_addr(addr).map_err(|e| format!("treedb control ({addr}): {e}"))?;
        Ok(Self { pool })
    }

    pub(crate) fn call(&self, op: Op) -> Result<Reply, CtlErr> {
        let r = self
            .pool
            .xcall(&op)
            .map_err(|f| CtlErr { code: f.code, msg: f.msg })?;
        // The engine's refusals are part of normal traffic, so recover the errno
        // from `Reply::Err` instead of flattening it into a sentence — callers
        // branch on `EEXIST`/`EAGAIN`.
        crate::treedb_store::reply_fail(r).map_err(|f| CtlErr { code: f.code, msg: f.msg })
    }

    /// `TableCreate`, with `EEXIST` as success — two processes starting at once
    /// both get a store, the same way `resolve_or_create_view` does it.
    pub(crate) fn create(&self, def: TableDef) -> Result<(), String> {
        let name = String::from_utf8_lossy(&def.name).into_owned();
        match self.call(Op::TableCreate { def }) {
            Ok(_) => Ok(()),
            Err(e) if e.is(errno::EEXIST) => Ok(()),
            Err(e) => Err(format!("creating table {name}: {e}")),
        }
    }

    pub(crate) fn get(&self, table: &str, key: &[u8]) -> Result<Option<Vec<Vec<u8>>>, String> {
        let r = self
            .call(Op::RowGet { table: table.as_bytes().to_vec(), key: key.to_vec() })
            .map_err(|e| e.to_string())?;
        match r {
            Reply::Row(row) => Ok(row.map(|r| r.cols)),
            other => Err(unexpected("RowGet", &other)),
        }
    }

    /// Write a row. `cond` is `(column, expected bytes)`: a miss answers `EAGAIN`,
    /// which is what `WHERE … AND col = ?` means on this engine. Updates that must
    /// not create a row pass [`PUT_IF_EXISTS`] (absent → `ENOENT`), inserts that
    /// must not overwrite pass `PUT_IF_ABSENT` (present → `EEXIST`).
    pub(crate) fn put(
        &self,
        table: &str,
        key: &[u8],
        cols: Vec<Vec<u8>>,
        flags: u32,
        cond: Option<(u32, Vec<u8>)>,
    ) -> Result<(), CtlErr> {
        let (cond_col, cond_val) = cond.unwrap_or((COND_NONE, Vec::new()));
        match self.call(Op::RowPut {
            table: table.as_bytes().to_vec(),
            key: key.to_vec(),
            cols,
            flags,
            cond_col,
            cond_val,
        })? {
            Reply::Row(_) => Ok(()),
            other => Err(CtlErr::other(unexpected("RowPut", &other))),
        }
    }

    /// Every row in primary-key order. `column = ROW_PK` means "the whole table".
    pub(crate) fn scan(&self, table: &str) -> Result<Vec<(Vec<u8>, Vec<Vec<u8>>)>, String> {
        self.pages(table, ROW_PK, Vec::new())
    }

    /// Every row whose `column` equals `value`, in primary-key order within that
    /// value (`column` must have been declared `COL_INDEX`).
    pub(crate) fn scan_eq(
        &self,
        table: &str,
        column: u32,
        value: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<Vec<u8>>)>, String> {
        self.pages(table, column, value.to_vec())
    }

    fn pages(
        &self,
        table: &str,
        column: u32,
        value: Vec<u8>,
    ) -> Result<Vec<(Vec<u8>, Vec<Vec<u8>>)>, String> {
        let mut out = Vec::new();
        let mut from: Vec<u8> = Vec::new();
        loop {
            let r = self
                .call(Op::RowList {
                    table: table.as_bytes().to_vec(),
                    column,
                    value: value.clone(),
                    from: from.clone(),
                    limit: PAGE,
                })
                .map_err(|e| e.to_string())?;
            match r {
                Reply::Rows { rows, end } => {
                    let last = rows.last().map(|r| r.key.clone());
                    out.extend(rows.into_iter().map(|r| (r.key, r.cols)));
                    match last {
                        Some(k) if !end => from = k,
                        _ => return Ok(out),
                    }
                }
                other => return Err(unexpected("RowList", &other)),
            }
        }
    }

    pub(crate) fn del(&self, table: &str, key: &[u8]) -> Result<(), String> {
        match self
            .call(Op::RowDelete {
                table: table.as_bytes().to_vec(),
                key: key.to_vec(),
                flags: 0,
                cond_col: COND_NONE,
                cond_val: Vec::new(),
            })
            .map_err(|e| e.to_string())?
        {
            Reply::Ok => Ok(()),
            other => Err(unexpected("RowDelete", &other)),
        }
    }

    /// `SELECT COUNT(*) WHERE column = value`; `ROW_PK` counts the whole table.
    pub(crate) fn count(&self, table: &str, column: u32, value: &[u8]) -> Result<u64, String> {
        match self
            .call(Op::RowCount {
                table: table.as_bytes().to_vec(),
                column,
                value: value.to_vec(),
            })
            .map_err(|e| e.to_string())?
        {
            Reply::Count(n) => Ok(n),
            other => Err(unexpected("RowCount", &other)),
        }
    }

    pub(crate) fn total(&self, table: &str) -> Result<u64, String> {
        self.count(table, ROW_PK, b"")
    }

    /// The engine's VIEW names. Not a control table — the engine's own catalogue —
    /// but it arrives over the same pooled connection and answers the one question
    /// the control plane cannot answer locally: under `--meta-store treedb://…` a
    /// base view has no `meta/<view>.db`, so "does this tree exist" has to be asked
    /// here (see `mcp::TreedbControlStore`).
    pub(crate) fn views(&self) -> Result<Vec<String>, String> {
        match self.call(Op::ListViews).map_err(|e| e.to_string())? {
            Reply::Views(v) => Ok(v
                .into_iter()
                .map(|(name, _, _)| String::from_utf8_lossy(&name).into_owned())
                .collect()),
            other => Err(unexpected("ListViews", &other)),
        }
    }
}

pub(crate) fn unexpected(what: &str, r: &Reply) -> String {
    match r {
        Reply::Err { code, msg } => format!(
            "treedb control: {what}: errno {code} ({})",
            String::from_utf8_lossy(msg)
        ),
        other => format!("treedb control: {what}: unexpected reply {other:?}"),
    }
}

// ---- encodings shared by every control table ------------------------------

/// Text as-is, `None` as the empty byte string (the engine has no NULL).
pub(crate) fn opt(v: Option<&str>) -> Vec<u8> {
    v.map(|s| s.as_bytes().to_vec()).unwrap_or_default()
}

/// The reverse of [`opt`]. **The two directions change together.**
pub(crate) fn opt_str(b: &[u8]) -> Option<String> {
    if b.is_empty() { None } else { Some(text(b)) }
}

pub(crate) fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

pub(crate) fn flag(b: bool) -> Vec<u8> {
    vec![if b { b'1' } else { b'0' }]
}

pub(crate) fn is_set(b: &[u8]) -> bool {
    b.first() == Some(&b'1')
}

// ---- named rows (M2: `control-migrate`) -----------------------------------

/// One row's values addressed by NAME, never by index.
///
/// Its one user is `control-migrate`, which builds a value per LOCAL SQLite row
/// and compares it with one dumped from the engine. Names make two things
/// possible that indices cannot: a mismatch can say *which column* differs, and
/// neither side has to learn the other's column order — an old local file that
/// lacks a later column simply answers the empty byte string (the engine's NULL)
/// instead of shifting every following value by one.
#[derive(Clone, Debug, Default)]
pub(crate) struct Fields(pub(crate) Vec<(String, Vec<u8>)>);

impl Fields {
    pub(crate) fn push(&mut self, name: &str, value: Vec<u8>) {
        self.0.push((name.to_string(), value));
    }

    /// The column's bytes; a name this row does not carry is empty, which is how
    /// every optional column in these tables is encoded anyway.
    pub(crate) fn get(&self, name: &str) -> &[u8] {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice()).unwrap_or(b"")
    }

    /// Fill a column the LOCAL row does not have (a field that only exists on the
    /// engine side). Replaces in place, so [`Self::get`] keeps answering with the
    /// new bytes instead of the first, empty entry.
    pub(crate) fn set(&mut self, name: &str, value: Vec<u8>) {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name.to_string(), value)),
        }
    }
}

/// Write one row from named fields. `names` is the ENGINE column order and each
/// entry names the local column that feeds that slot; the list lives beside the
/// column indices in each store's `treedb_ctl`, so the order is stated once.
/// Unconditional (`flags = 0`): M2's contract is "rerun overwrites", not
/// "rerun collides", and the write is followed by a full read-back comparison.
pub(crate) fn put_fields(
    rows: &Rows,
    table: &str,
    key: &[u8],
    names: &[&str],
    f: &Fields,
) -> Result<(), String> {
    let cols: Vec<Vec<u8>> = names.iter().map(|n| f.get(n).to_vec()).collect();
    rows.put(table, key, cols, 0, None).map_err(|e| e.to_string())
}

/// Read a whole table back as named fields — the migrate command's verify pass.
/// The keys come back exactly as the engine stored them, so the comparison covers
/// the primary key as well as every column.
pub(crate) fn dump(
    rows: &Rows,
    table: &str,
    names: &[&str],
) -> Result<Vec<(Vec<u8>, Fields)>, String> {
    Ok(rows
        .scan(table)?
        .into_iter()
        .map(|(k, cols)| {
            let mut f = Fields::default();
            for (i, n) in names.iter().enumerate() {
                // A short row (a table written by an older build) reads as empty
                // rather than panicking: the comparison then reports the column.
                f.push(n, cols.get(i).cloned().unwrap_or_default());
            }
            (k, f)
        })
        .collect())
}

pub(crate) fn col(name: &str, flags: u32) -> ColumnDef {
    ColumnDef { name: name.as_bytes().to_vec(), flags }
}

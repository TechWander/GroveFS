//! **Sign-in accounts and delegations in the `treedb` engine's generic tables**
//! (treedb DESIGN.md §10 C 4, milestone M1b) — the second half of "the control
//! plane moves into the engine", beside [`super::super::apikeys`]' credentials.
//!
//! Same switch, same routing rule, same row plumbing: the three entry points are
//! `GROVEFS_TREEDB_CONTROL=engine`, the `--meta-store treedb://…` URI, and
//! [`crate::control_treedb::Rows`]. The reason the switch exists is stated once in
//! [`super::WebAuthStore::open_treedb`]: until the tables are FILLED, routing here
//! by default would report every account as unknown, which reads as "nobody can
//! log in" rather than "the migration has not run".
//!
//! **Two tables, six and five columns, and one of them is a `rev`.**
//! `RowPut` replaces the whole row (the engine has no "update this column"), so
//! every in-place verb here is read → change → write-back. Two writers touching
//! one account are ordinary, not exotic: a sign-in stamps `last_login_at` while
//! an administrator disables the account. Without a condition, the slower of the
//! two would write back its stale copy of the other's column — a sign-in that
//! silently re-enables a disabled account is a security bug, not a lost update.
//! So `rev` (8-byte big-endian, bumped by every edit) is the CAS column: a loser
//! gets `EAGAIN`, re-reads, and re-applies its ONE field to the newer row. This is
//! why the column exists in a table that has no use for the number itself.
//!
//! **The grants table's primary key is `account \0 base_view`.** The engine has no
//! prefix predicate, so neither "this account's bases" nor "who holds this view"
//! can be answered from a key alone: both columns are declared `COL_INDEX` and
//! asked through `RowList{column}`. `0x00` is a safe separator because both halves
//! are validated UTF-8 text (an account is `[a-z0-9._-]`, a view name cannot hold
//! a NUL).
//!
//! Deliberately NOT here (both are the caller's, and both are stated in
//! `webauth.rs`): the password policy and PBKDF2 (the digest arrives already
//! stretched), and the last-enabled-admin guard — this module only answers
//! [`Self::active_admins`], which is the *input* to that guard.

use treedb::engine::model::{errno, TableDef, PUT_IF_ABSENT, PUT_IF_EXISTS, COL_INDEX};

use super::{AccountInfo, GrantInfo, Role};
use crate::control_treedb::{col, opt, opt_str, text, Rows};

/// Engine `tables` are one flat namespace (treedb §5.3), so the prefix is the only
/// isolation — and the price is one GROVEFS control plane per engine instance.
const T_ACCOUNTS: &str = "grovefs_control__web_accounts";
const T_GRANTS: &str = "grovefs_control__web_grants";

/// Column order IS the storage format: reordering means a migration or a new
/// table name. The CAS column ([`WA_REV`]) is taken from here too, so the number
/// 5 lives in one place.
const WA_HASH: usize = 0;
const WA_ROLE: usize = 1;
const WA_CREATED_AT: usize = 2;
const WA_DISABLED_AT: usize = 3;
const WA_LAST_LOGIN: usize = 4;
const WA_REV: usize = 5;
const WA_COLS: usize = 6;

const WG_ACCOUNT: usize = 0;
const WG_BASE: usize = 1;
const WG_PERM: usize = 2;
const WG_GRANTED_AT: usize = 3;
const WG_GRANTED_BY: usize = 4;
const WG_COLS: usize = 5;

/// How many times a CAS loser re-reads before giving up. Each round is one
/// round-trip; an account row is written by humans and by sign-ins, so contention
/// that survives eight re-reads is a stuck loop, not load.
const CAS_TRIES: u32 = 8;

pub(super) struct TreedbAccounts {
    rows: Rows,
}

impl TreedbAccounts {
    /// Open (creating on first use) both tables. `EEXIST` counts as success —
    /// several processes starting at once all get a store.
    pub(super) fn open(addr: &str) -> Result<Self, String> {
        let rows = Rows::open(addr)?;
        for def in [accounts_def(), grants_def()] {
            rows.create(def)?;
        }
        Ok(Self { rows })
    }

    // ---- accounts -----------------------------------------------------------

    /// Is the store empty? The install-time bootstrap may only fill an empty one.
    /// `RowCount{ROW_PK}` is O(1) on the engine, so this stays cheap.
    pub(super) fn is_empty(&self) -> Result<bool, String> {
        Ok(self.rows.total(T_ACCOUNTS)? == 0)
    }

    /// Every account with its grants folded in. Primary-key order IS `ORDER BY
    /// account` (the key is the account name), so no sorting pass is needed; the
    /// ASCII/byte order it produces is what SQLite's default collation gives.
    pub(super) fn list(&self) -> Result<Vec<AccountInfo>, String> {
        let mut out = Vec::new();
        for (key, c) in self.rows.scan(T_ACCOUNTS)? {
            let account = text(&key);
            out.push(AccountInfo {
                bases: self.bases_of(&account)?,
                account,
                role: text(&c[WA_ROLE]),
                created_at: text(&c[WA_CREATED_AT]),
                disabled_at: opt_str(&c[WA_DISABLED_AT]),
                last_login_at: opt_str(&c[WA_LAST_LOGIN]),
            });
        }
        Ok(out)
    }

    /// `COUNT(*) WHERE role = 'admin' AND disabled_at IS NULL`. The engine has no
    /// cross-column condition, so this is the index scan on `role` plus a client
    /// filter — the documented workaround, not a derived column: a derived
    /// `admin_active` would need every writer to maintain it, and one that forgets
    /// produces a count nobody can explain.
    pub(super) fn active_admins(&self) -> Result<usize, String> {
        let admins = self
            .rows
            .scan_eq(T_ACCOUNTS, WA_ROLE as u32, Role::Admin.as_str().as_bytes())?;
        Ok(admins.iter().filter(|(_, c)| c[WA_DISABLED_AT].is_empty()).count())
    }

    /// Digest + role + liveness for one account, in the shape the login path
    /// needs. Private for the same reason its SQL twin is: a digest must never be
    /// printable by accident.
    pub(super) fn fetch(
        &self,
        account: &str,
    ) -> Result<Option<(String, String, Option<String>)>, String> {
        Ok(self
            .rows
            .get(T_ACCOUNTS, account.as_bytes())?
            .map(|c| (text(&c[WA_HASH]), text(&c[WA_ROLE]), opt_str(&c[WA_DISABLED_AT]))))
    }

    /// A new account. `PUT_IF_ABSENT` makes "this name is taken" an `EEXIST`
    /// instead of a silent overwrite — a create that reset someone's password
    /// would be a login bypass wearing the wrong name.
    pub(super) fn create(&self, account: &str, pw_hash: &str, role: Role) -> Result<(), String> {
        let mut cols = vec![Vec::new(); WA_COLS];
        cols[WA_HASH] = pw_hash.as_bytes().to_vec();
        cols[WA_ROLE] = role.as_str().as_bytes().to_vec();
        cols[WA_CREATED_AT] = super::now_iso().into_bytes();
        cols[WA_REV] = bump(&[]);
        match self.rows.put(T_ACCOUNTS, account.as_bytes(), cols, PUT_IF_ABSENT, None) {
            Ok(()) => Ok(()),
            // The SQL arms surface the UNIQUE violation; keep the meaning, not the
            // dialect's wording.
            Err(e) if e.is(errno::EEXIST) => Err(format!("account `{account}` already exists")),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Read → change one field → CAS on `rev`. `false` means "no such account",
    /// which is what every `UPDATE … WHERE account = ?` here reports by rows-hit.
    fn edit(&self, account: &str, once: impl Fn(&mut Vec<Vec<u8>>)) -> Result<bool, String> {
        for _ in 0..CAS_TRIES {
            let Some(mut cols) = self.rows.get(T_ACCOUNTS, account.as_bytes())? else {
                return Ok(false);
            };
            let old = cols[WA_REV].clone();
            // The change is applied to the row as just read — a retry re-reads
            // first, otherwise it would write the stale copy back again.
            once(&mut cols);
            cols[WA_REV] = bump(&old);
            match self.rows.put(
                T_ACCOUNTS,
                account.as_bytes(),
                cols,
                PUT_IF_EXISTS,
                Some((WA_REV as u32, old)),
            ) {
                Ok(()) => return Ok(true),
                // Someone edited the same row between the read and this write;
                // their field changes must survive, so start over.
                Err(e) if e.is(errno::EAGAIN) => continue,
                // Deleted in between — "matched no row", the same answer the SQL
                // UPDATE gives.
                Err(e) if e.is(errno::ENOENT) => return Ok(false),
                Err(e) => return Err(e.to_string()),
            }
        }
        Err(format!(
            "treedb control: account `{account}` changed under {CAS_TRIES} retries"
        ))
    }

    pub(super) fn set_password(&self, account: &str, pw_hash: &str) -> Result<bool, String> {
        self.edit(account, |c| c[WA_HASH] = pw_hash.as_bytes().to_vec())
    }

    pub(super) fn set_role(&self, account: &str, role: Role) -> Result<bool, String> {
        self.edit(account, |c| c[WA_ROLE] = role.as_str().as_bytes().to_vec())
    }

    pub(super) fn set_disabled(&self, account: &str, disabled: bool) -> Result<bool, String> {
        let ts = if disabled { Some(super::now_iso()) } else { None };
        self.edit(account, |c| c[WA_DISABLED_AT] = opt(ts.as_deref()))
    }

    /// Sign-in stamp. Unconditional by contract: the SQL `UPDATE … WHERE account`
    /// is a silent no-op for a row that vanished mid-login, so an absent row is
    /// not an error here either — and the timestamp is never a precise count
    /// anybody relies on.
    pub(super) fn stamp_login(&self, account: &str) -> Result<(), String> {
        let ts = super::now_iso();
        self.edit(account, |c| c[WA_LAST_LOGIN] = ts.as_bytes().to_vec())?;
        Ok(())
    }

    /// Drop the account and its grants, **grants first** — the same order as the
    /// SQL arm, so a failure between the two leaves orphan grants, never a
    /// grant-less account. That side is fail-closed already: both login and every
    /// gated call `fetch` the account row first, so an orphan grant is never read.
    /// The engine has no cascade, which turns the SQL's two statements into two
    /// explicit steps here.
    pub(super) fn delete(&self, account: &str) -> Result<bool, String> {
        for (key, _) in self.rows.scan_eq(T_GRANTS, WG_ACCOUNT as u32, account.as_bytes())? {
            self.rows.del(T_GRANTS, &key)?;
        }
        if self.rows.get(T_ACCOUNTS, account.as_bytes())?.is_none() {
            return Ok(false);
        }
        self.rows.del(T_ACCOUNTS, account.as_bytes())?;
        Ok(true)
    }

    // ---- grants -------------------------------------------------------------

    /// The bases delegated to one account, sorted. The index scan already returns
    /// them in key order (`account \0 base_view`), so this is a read, not a sort —
    /// the dedup stays because the SQL arm has it.
    pub(super) fn bases_of(&self, account: &str) -> Result<Vec<String>, String> {
        let mut v: Vec<String> = self
            .rows
            .scan_eq(T_GRANTS, WG_ACCOUNT as u32, account.as_bytes())?
            .into_iter()
            .map(|(_, c)| text(&c[WG_BASE]))
            .collect();
        v.sort();
        v.dedup();
        Ok(v)
    }

    /// Idempotent: a re-grant refreshes `granted_at`/`granted_by` and **keeps**
    /// `perm`, which is what `ON CONFLICT DO UPDATE SET granted_at, granted_by`
    /// promises (the insert's `'manage'` only ever seeds a new row).
    pub(super) fn grant(
        &self,
        account: &str,
        base_view: &str,
        granted_by: Option<&str>,
    ) -> Result<(), String> {
        let ts = super::now_iso();
        let key = grant_key(account, base_view);
        for _ in 0..CAS_TRIES {
            match self.rows.get(T_GRANTS, &key)? {
                Some(mut cols) => {
                    let old = cols[WG_GRANTED_AT].clone();
                    cols[WG_GRANTED_AT] = ts.as_bytes().to_vec();
                    cols[WG_GRANTED_BY] = opt(granted_by);
                    match self.rows.put(
                        T_GRANTS,
                        &key,
                        cols,
                        0,
                        Some((WG_GRANTED_AT as u32, old)),
                    ) {
                        Ok(()) => return Ok(()),
                        Err(e) if e.is(errno::EAGAIN) => continue,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                None => {
                    let mut cols = vec![Vec::new(); WG_COLS];
                    cols[WG_ACCOUNT] = account.as_bytes().to_vec();
                    cols[WG_BASE] = base_view.as_bytes().to_vec();
                    cols[WG_PERM] = b"manage".to_vec();
                    cols[WG_GRANTED_AT] = ts.as_bytes().to_vec();
                    cols[WG_GRANTED_BY] = opt(granted_by);
                    match self.rows.put(T_GRANTS, &key, cols, PUT_IF_ABSENT, None) {
                        Ok(()) => return Ok(()),
                        // Another process granted the same pair first; its row is
                        // a grant too, so refreshing it is the idempotent answer.
                        Err(e) if e.is(errno::EEXIST) => continue,
                        Err(e) => return Err(e.to_string()),
                    }
                }
            }
        }
        Err(format!(
            "treedb control: grant `{account}` → `{base_view}` kept racing ({CAS_TRIES} tries)"
        ))
    }

    pub(super) fn ungrant(&self, account: &str, base_view: &str) -> Result<bool, String> {
        let key = grant_key(account, base_view);
        if self.rows.get(T_GRANTS, &key)?.is_none() {
            return Ok(false);
        }
        self.rows.del(T_GRANTS, &key)?;
        Ok(true)
    }

    /// Every delegation, in `account \0 base_view` order — exactly the SQL's
    /// `ORDER BY account, base_view`, because that is the primary key.
    pub(super) fn list_grants(&self) -> Result<Vec<GrantInfo>, String> {
        Ok(self
            .rows
            .scan(T_GRANTS)?
            .into_iter()
            .map(|(_, c)| GrantInfo {
                account: text(&c[WG_ACCOUNT]),
                base_view: text(&c[WG_BASE]),
                perm: text(&c[WG_PERM]),
                granted_at: text(&c[WG_GRANTED_AT]),
                granted_by: opt_str(&c[WG_GRANTED_BY]),
            })
            .collect())
    }
}

/// `account \0 base_view`. The separator cannot occur in either half: account
/// names are `[a-z0-9._-]` and view names are validated text.
fn grant_key(account: &str, base_view: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(account.len() + 1 + base_view.len());
    k.extend_from_slice(account.as_bytes());
    k.push(0);
    k.extend_from_slice(base_view.as_bytes());
    k
}

/// The next `rev`: 8-byte big-endian, so the bytes compare as numbers. An empty
/// value (a row written before the column existed, or read as absent) is 0.
fn bump(old: &[u8]) -> Vec<u8> {
    let n = <[u8; 8]>::try_from(old).map(u64::from_be_bytes).unwrap_or(0);
    (n + 1).to_be_bytes().to_vec()
}

// ---- table shapes ----------------------------------------------------------

fn accounts_def() -> TableDef {
    TableDef {
        name: T_ACCOUNTS.as_bytes().to_vec(),
        // Column order = [`WA_*`]. `role` is indexed because the two governance
        // guards ask exactly one question of it: "how many enabled admins are
        // left" (`active_admins`). `rev` is not indexed — it is never searched,
        // only compared as a CAS condition on a row already fetched by key.
        columns: vec![
            col("pw_hash", 0),
            col("role", COL_INDEX),
            col("created_at", 0),
            col("disabled_at", 0),
            col("last_login_at", 0),
            col("rev", 0),
        ],
    }
}

fn grants_def() -> TableDef {
    TableDef {
        name: T_GRANTS.as_bytes().to_vec(),
        // Column order = [`WG_*`]. Both directions are asked (this account's
        // bases / who holds this view), and a composite primary key is not a
        // prefix predicate, so both halves are separate index columns.
        columns: vec![
            col("account", COL_INDEX),
            col("base_view", COL_INDEX),
            col("perm", 0),
            col("granted_at", 0),
            col("granted_by", 0),
        ],
    }
}

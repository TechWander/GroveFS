//! **M2 — hand the LOCAL control plane's rows over to the engine's generic
//! tables** (treedb DESIGN.md §10 C 4): `grovefs control-migrate --to engine`.
//!
//! M1 taught the servers to keep credentials, audit, sign-in accounts and
//! delegations in the engine — behind `GROVEFS_TREEDB_CONTROL=engine` and
//! **default off**, precisely because those tables start empty: flipping the
//! switch before this command runs would report every key and account as
//! unknown. This module fills them from the files the switch moves away from:
//! `<base>/meta/api_keys.db` (keys + audit) and `<base>/meta/web_accounts.db`
//! (accounts + grants).
//!
//! Four rules, each with the reason it exists:
//!
//! 1. **Values move byte for byte.** Nothing is re-hashed, nothing is
//!    re-encoded, no timestamp is normalised. (The design's M2 text says
//!    "`secret_enc` is copied as it lies, never re-wrapped, and the wrapping
//!    material in `<base>/meta/store.key` travels with the directory" — that
//!    column has no counterpart in this tree: a credential here is a one-way
//!    SHA-256 digest plus a display hint, so there is no wrapped secret and no
//!    key file. Copying the digest verbatim is the strictest possible reading of
//!    "never re-wrap"; in particular a key still authenticates after the move
//!    because its hash, not a re-derived one, is what lands.)
//! 2. **Idempotent.** Every write is an unconditional `RowPut`, so a rerun
//!    overwrites instead of colliding. There is no "already migrated" marker to
//!    fall out of step with the rows themselves.
//! 3. **Nothing is trusted until it is read back.** After the writes each table
//!    is dumped from the engine and compared with what was read locally: the key
//!    set in both directions, then every column, byte for byte. A mismatch is an
//!    error and a non-zero exit — M3 (delete the switch) must not start on a
//!    partial copy.
//! 4. **Column names, never indices.** The engine column order ↔ local column
//!    name mapping lives in each store's `treedb_ctl` (`KEY_COLS`,
//!    `ACCOUNT_COLS`, …) right beside the indices it restates. Two things follow:
//!    a mismatch can name the column, and a local file that predates a column
//!    answers the empty byte string (the engine's NULL) instead of shifting
//!    every value after it.
//!
//! Three shapes are not a plain copy; [`moves`] carries the reason beside each:
//! `last_used_at` becomes a row of `api_key_usage` (that is where M1 puts a
//! per-request stamp), an audit row's key is derived (`!ts_us || !seq` — the
//! local table has no id), and `web_accounts.rev` is seeded because a local
//! account row has no CAS counter to copy.
//!
//! Deliberately NOT this module's job: creating views, moving trees, and above
//! all **deleting the local files**. They stay where they are, because the
//! rollback is "unset the switch" — a migration that also ate its own source
//! would take that away.
//!
//! Stop the serving process first. The source is opened READ-ONLY (this command
//! may only write to the engine), but a live SQLite writer can still commit a row
//! between the read and the read-back, and that row would be reported as missing.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};
use treedb::engine::model::TableDef;

use crate::apikeys::treedb_ctl as ak;
use crate::control_treedb::{flag, text, Fields, Rows};
use crate::treedb_store;
use crate::webauth::treedb_ctl as wa;

/// How many individual differences an error message spells out before it starts
/// counting instead. Enough to see a pattern, short enough to read.
const SHOW: usize = 5;

/// What one run did. `rows` is what was written — and, because a single
/// difference aborts the run, necessarily also what was read and verified.
pub struct Report {
    pub tables: usize,
    pub rows: u64,
}

/// Read the local control plane under `base` into the engine at `uri`.
///
/// `say` receives one line per source, per table, and the engine address — the
/// command prints them as they happen, so a run that dies halfway still says how
/// far it got.
pub fn run(base: &Path, uri: &str, say: &mut dyn FnMut(&str)) -> Result<Report, String> {
    if crate::apikeys::ApiKeyStore::treedb_control_enabled() {
        return Err(format!(
            "`{}` is set to `engine`: the engine's control tables are the LIVE store right now, so \
             writing the local files into them would put stale rows on top of live ones (a revoked \
             key came back, a rotated password reverted). Unset it, migrate, then set it again",
            crate::apikeys::ApiKeyStore::TREEDB_CONTROL_ENV
        ));
    }
    let addr = treedb_store::parse_addr(uri)?;

    let keys_path = base.join("meta").join("api_keys.db");
    let accounts_path = base.join("meta").join("web_accounts.db");
    let keys = open_local(&keys_path)?;
    let accounts = open_local(&accounts_path)?;
    if keys.is_none() && accounts.is_none() {
        return Err(format!(
            "nothing to migrate: neither {} nor {} exists. This is either not the \
             meta-server's --meta-dir, or the control plane is already where the switch points",
            keys_path.display(),
            accounts_path.display()
        ));
    }
    for (path, conn) in [(&keys_path, &keys), (&accounts_path, &accounts)] {
        say(&match conn {
            Some(_) => format!("source {}", path.display()),
            None => format!("source {} (absent — its tables migrate as empty)", path.display()),
        });
    }

    let rows = Rows::open(&addr)?;
    say(&format!("engine {uri}"));
    let moves = moves();
    for mv in &moves {
        rows.create(mv.def.clone())?;
    }

    let mut total = 0u64;
    for mv in &moves {
        let conn = match mv.db {
            Db::Keys => keys.as_ref(),
            Db::Accounts => accounts.as_ref(),
        };
        let expected = read_source(conn, mv)?;
        for (key, fields) in &expected {
            (mv.put)(&rows, key, fields)?;
        }
        verify(&rows, mv, &expected).map_err(|e| format!("{}: {e}", table_name(mv)))?;
        total += expected.len() as u64;
        say(&format!("{:<38} {:>6} rows written, verified", table_name(mv), expected.len()));
    }
    Ok(Report { tables: moves.len(), rows: total })
}

// ---- what moves, and how ----------------------------------------------------

/// Which local file a move reads from.
#[derive(Clone, Copy)]
enum Db {
    Keys,
    Accounts,
}

/// How a local row's ENGINE key is derived.
enum Key {
    /// The key is this local column, verbatim.
    Column(&'static str),
    /// `account \0 base_view` — through the store's own function, so a migrated
    /// grant lands on the key the serving process will look up.
    Pair(&'static str, &'static str),
    /// `!ts_us || !seq` (see [`ak::call_key`]): the local audit table has no id
    /// at all, so the key has to be derived from `ts` plus scan order.
    Call,
}

struct Move {
    /// The engine table's shape, taken from the store that owns it — the
    /// migration never restates a column count.
    def: TableDef,
    db: Db,
    /// The LOCAL SQLite table.
    source: &'static str,
    /// Engine column order → column names, from the owning store.
    names: &'static [&'static str],
    key: Key,
    /// A local column that must be non-empty for the row to move at all.
    only_if: Option<&'static str>,
    /// A column the local table does not have, and the value to seed it with.
    seed: Option<(&'static str, fn() -> Vec<u8>)>,
    put: fn(&Rows, &[u8], &Fields) -> Result<(), String>,
    dump: fn(&Rows) -> Result<Vec<(Vec<u8>, Fields)>, String>,
}

fn moves() -> Vec<Move> {
    let [keys, usage, calls] = ak::tables();
    let [accounts, grants] = wa::tables();
    vec![
        Move {
            def: keys,
            db: Db::Keys,
            source: "api_keys",
            names: &ak::KEY_COLS,
            key: Key::Column("id"),
            only_if: None,
            seed: None,
            put: ak::put_key,
            dump: ak::dump_keys,
        },
        // Not a second table on disk: M1 split the per-request stamp out of the
        // credential row (so a hot write does not fight rotate/revoke for that
        // row), and the local schema still carries it as `api_keys.last_used_at`.
        // A key that was never used has no row there — a usage row exists only
        // where a stamp exists.
        Move {
            def: usage,
            db: Db::Keys,
            source: "api_keys",
            names: &ak::USAGE_COLS,
            key: Key::Column("id"),
            only_if: Some("last_used_at"),
            seed: None,
            put: ak::put_usage,
            dump: ak::dump_usage,
        },
        Move {
            def: calls,
            db: Db::Keys,
            source: "mcp_calls",
            names: &ak::CALL_COLS,
            key: Key::Call,
            only_if: None,
            seed: None,
            put: ak::put_call,
            dump: ak::dump_calls,
        },
        // The local table has no `rev`: before the move there was only one
        // writer per row, so there was nothing to compare-and-swap. Seed the same
        // value `TreedbAccounts::create` writes, then the migrated row is
        // byte-identical to one created in the engine.
        Move {
            def: accounts,
            db: Db::Accounts,
            source: "web_accounts",
            names: &wa::ACCOUNT_COLS,
            key: Key::Column("account"),
            only_if: None,
            seed: Some(("rev", wa::initial_rev)),
            put: wa::put_account,
            dump: wa::dump_accounts,
        },
        Move {
            def: grants,
            db: Db::Accounts,
            source: "web_grants",
            names: &wa::GRANT_COLS,
            key: Key::Pair("account", "base_view"),
            only_if: None,
            seed: None,
            put: wa::put_grant,
            dump: wa::dump_grants,
        },
    ]
}

fn table_name(mv: &Move) -> String {
    String::from_utf8_lossy(&mv.def.name).into_owned()
}

// ---- reading the local side -------------------------------------------------

/// Open the local file READ-ONLY, or `None` when it is not there. Read-only is
/// part of the contract: the only thing this command writes is the engine, and an
/// aborted run has to leave the source exactly as it found it.
fn open_local(path: &Path) -> Result<Option<Connection>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map(Some)
        .map_err(|e| {
            format!(
                "{}: {e} (a crashed writer can leave a WAL that only a writable connection can \
                 replay — start the meta-server once so SQLite checkpoints it, stop it, then rerun)",
                path.display()
            )
        })
}

/// The rows `mv` should leave in the engine, in local insertion order (`rowid`).
fn read_source(conn: Option<&Connection>, mv: &Move) -> Result<Vec<(Vec<u8>, Fields)>, String> {
    let Some(conn) = conn else { return Ok(Vec::new()) };
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [mv.source],
            |r| r.get(0),
        )
        .map_err(|e| format!("{}: {e}", mv.source))?;
    if present == 0 {
        // The file exists but predates this table: an empty source, not an error.
        return Ok(Vec::new());
    }

    let mut stmt = conn
        .prepare(&format!("SELECT * FROM \"{}\" ORDER BY rowid", mv.source))
        .map_err(|e| format!("{}: {e}", mv.source))?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([]).map_err(|e| format!("{}: {e}", mv.source))?;

    let mut out = Vec::new();
    // `seq` per microsecond, minted in scan order: the process-local counter
    // M1's `log_call` uses, replayed from the order SQLite inserted the rows.
    let mut stamps: HashMap<i64, u64> = HashMap::new();
    let mut line = 0u64;
    while let Some(row) = rows.next().map_err(|e| format!("{}: {e}", mv.source))? {
        line += 1;
        let at = || format!("{} row {line}", mv.source);
        let mut fields = Fields::default();
        for (i, name) in names.iter().enumerate() {
            let v: Value = row.get(i).map_err(|e| format!("{}: {e}", at()))?;
            let bytes = value_bytes(name, v).map_err(|e| format!("{}: {e}", at()))?;
            fields.push(name, bytes);
        }
        if mv.only_if.is_some_and(|c| fields.get(c).is_empty()) {
            continue;
        }
        if let Some((name, seed)) = mv.seed {
            if fields.get(name).is_empty() {
                fields.set(name, seed());
            }
        }
        let key = key_of(mv, &fields, &mut stamps).map_err(|e| format!("{}: {e}", at()))?;
        if key.is_empty() {
            return Err(format!("{}: empty primary key", at()));
        }
        out.push((key, fields));
    }
    Ok(out)
}

/// One SQLite value as engine bytes. The engine's control columns are byte
/// strings, so text passes through untouched, `NULL` becomes the empty string,
/// and the two boolean columns become one byte — anything else would be a value
/// no engine column can hold honestly, so it is refused by name instead of being
/// formatted into a number nobody chose.
fn value_bytes(column: &str, v: Value) -> Result<Vec<u8>, String> {
    Ok(match v {
        Value::Null => Vec::new(),
        Value::Text(s) => s.into_bytes(),
        Value::Blob(b) => b,
        Value::Integer(i) if column == "ro" || column == "ok" => flag(i != 0),
        Value::Integer(i) => {
            return Err(format!(
                "column `{column}` holds the integer {i}; this table stores text and 1-byte booleans"
            ))
        }
        Value::Real(x) => {
            return Err(format!(
                "column `{column}` holds the real {x}; this table stores text and 1-byte booleans"
            ))
        }
    })
}

fn key_of(mv: &Move, fields: &Fields, stamps: &mut HashMap<i64, u64>) -> Result<Vec<u8>, String> {
    Ok(match mv.key {
        Key::Column(c) => fields.get(c).to_vec(),
        Key::Pair(a, b) => wa::grant_key(&text(fields.get(a)), &text(fields.get(b))),
        Key::Call => {
            let ts = text(fields.get("ts"));
            let us = chrono::DateTime::parse_from_rfc3339(&ts)
                .map_err(|e| format!("`ts` = {ts:?} is not an RFC 3339 timestamp: {e}"))?
                .timestamp_micros();
            let seq = stamps.entry(us).or_insert(0);
            let key = ak::call_key(us, *seq);
            *seq += 1;
            key
        }
    })
}

// ---- the read-back comparison ----------------------------------------------

/// Every row we meant to write, against every row the engine actually holds.
fn verify(rows: &Rows, mv: &Move, expected: &[(Vec<u8>, Fields)]) -> Result<(), String> {
    let want: BTreeMap<&Vec<u8>, &Fields> = expected.iter().map(|(k, f)| (k, f)).collect();
    let have = (mv.dump)(rows)?;
    let have_by_key: BTreeMap<&Vec<u8>, &Fields> =
        have.iter().map(|(k, f)| (k, f)).collect();

    let mut faults = 0usize;
    let mut blame: Vec<String> = Vec::new();
    let mut note = |faults: &mut usize, msg: String| {
        *faults += 1;
        if blame.len() < SHOW {
            blame.push(msg);
        }
    };

    for k in want.keys().filter(|k| !have_by_key.contains_key(*k)) {
        note(&mut faults, format!("no such row in the engine: key {}", show(k)));
    }
    for (k, _) in have.iter().filter(|(k, _)| !want.contains_key(k)) {
        note(&mut faults, format!("unexpected row in the engine: key {}", show(k)));
    }
    for (k, fields) in &want {
        let Some(engine) = have_by_key.get(k) else { continue };
        for name in mv.names {
            if fields.get(name) != engine.get(name) {
                note(
                    &mut faults,
                    format!(
                        "key {} column `{name}`: source {} / engine {}",
                        show(k),
                        show(fields.get(name)),
                        show(engine.get(name))
                    ),
                );
            }
        }
    }
    if faults == 0 {
        return Ok(());
    }
    let more = if faults > blame.len() {
        format!(" (and {} more)", faults - blame.len())
    } else {
        String::new()
    };
    Err(format!("read-back differs{more}: {}", blame.join("; ")))
}

/// A key or a value as one line of an error: quoted when it reads as text (a key
/// id, an account), hex when it is binary (an audit key, a `rev`).
fn show(b: &[u8]) -> String {
    if b.iter().all(|c| c.is_ascii_graphic()) {
        return format!("\"{}\"", String::from_utf8_lossy(b));
    }
    let hex: String = b.iter().take(12).map(|c| format!("{c:02x}")).collect();
    format!("{hex}…({} bytes)", b.len())
}

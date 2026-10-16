//! Unit tests for the POSIX advisory record-lock layer added in L1, plus the
//! L4 lease-renewal / session-reap additions. Covers the cross-owner conflict
//! matrix, same-owner conversion, F_GETLK reporting, the lease sweep that reaps
//! locks left behind by a SIGKILL'd client, and the renewal heartbeat that keeps
//! a *live* holder from expiring.

use grovefs::meta::{LockType, MetaEngine, ROOT_INO};

fn tmpdb(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("grovefs_locks_{tag}_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-wal", p.display()));
    let _ = std::fs::remove_file(format!("{}-shm", p.display()));
    p
}

const WHOLE: u64 = u64::MAX;
// A single dummy session id for the conflict-matrix tests (renewal/session-reap
// semantics are exercised separately below with explicit sids).
const S: u64 = 1;

#[test]
fn read_read_same_ino_are_compatible_across_owners() {
    let m = MetaEngine::open(&tmpdb("rr")).unwrap();
    assert!(m.lock_op(ROOT_INO, 1, 0, WHOLE, LockType::Read, 101, S).unwrap().is_none());
    // Different owner, overlapping shared interval -> must be granted.
    assert!(m.lock_op(ROOT_INO, 2, 0, WHOLE, LockType::Read, 102, S).unwrap().is_none());
}

#[test]
fn write_blocks_reader_and_vice_versa() {
    let m = MetaEngine::open(&tmpdb("rw")).unwrap();
    assert!(m.lock_op(ROOT_INO, 1, 0, WHOLE, LockType::Read, 101, S).unwrap().is_none());
    let c = m.lock_op(ROOT_INO, 2, 0, WHOLE, LockType::Write, 102, S).unwrap();
    assert!(c.is_some(), "write must conflict with existing read");
    let c = c.unwrap();
    assert_eq!(c.ty, LockType::Read);
    assert_eq!(c.pid, 101);

    // Reverse: writer first, reader must be denied.
    let m2 = MetaEngine::open(&tmpdb("wr")).unwrap();
    m2.lock_op(ROOT_INO, 1, 0, WHOLE, LockType::Write, 11, S).unwrap();
    assert!(m2.lock_op(ROOT_INO, 2, 0, WHOLE, LockType::Read, 12, S).unwrap().is_some());
}

#[test]
fn write_write_conflict_reports_first_holder() {
    let m = MetaEngine::open(&tmpdb("ww")).unwrap();
    m.lock_op(ROOT_INO, 1, 0, WHOLE, LockType::Write, 21, S).unwrap();
    let c = m.lock_op(ROOT_INO, 2, 0, WHOLE, LockType::Write, 22, S).unwrap().unwrap();
    assert_eq!(c.pid, 21, "must report the original holder");
}

#[test]
fn intervals_only_conflict_when_they_overlap() {
    let m = MetaEngine::open(&tmpdb("interval")).unwrap();
    m.lock_op(ROOT_INO, 1, 0, 100, LockType::Write, 31, S).unwrap();
    // Adjacent, not overlapping (POSIX ranges are inclusive; 101..200 skips 100).
    assert!(m.lock_op(ROOT_INO, 2, 101, 200, LockType::Write, 32, S).unwrap().is_none());
    // Overlapping by one byte.
    assert!(m.lock_op(ROOT_INO, 3, 100, 200, LockType::Write, 33, S).unwrap().is_some());
}

#[test]
fn same_owner_relocking_converts_range() {
    let m = MetaEngine::open(&tmpdb("convert")).unwrap();
    // Owner 7 takes a read, then upgrades to write on the same range. POSIX:
    // same-owner overlapping ranges convert rather than self-conflict.
    assert!(m.lock_op(ROOT_INO, 7, 0, WHOLE, LockType::Read, 70, S).unwrap().is_none());
    assert!(m.lock_op(ROOT_INO, 7, 0, WHOLE, LockType::Write, 70, S).unwrap().is_none());
    // A different owner must now be denied.
    assert!(m.lock_op(ROOT_INO, 8, 0, WHOLE, LockType::Read, 80, S).unwrap().is_some());
}

#[test]
fn unlock_removes_only_that_owner_in_that_range() {
    let m = MetaEngine::open(&tmpdb("unlock")).unwrap();
    // Owner 1 holds a write on [0,100]; owner 2 holds a read on [200,300].
    // Ranges are disjoint so both grants land in the table.
    m.lock_op(ROOT_INO, 1, 0, 100, LockType::Write, 41, S).unwrap();
    m.lock_op(ROOT_INO, 2, 200, 300, LockType::Read, 42, S).unwrap();
    // Owner 1 releases its own range; owner 2's read must survive untouched.
    assert!(m.lock_op(ROOT_INO, 1, 0, 100, LockType::Unlock, 0, S).unwrap().is_none());
    // New writer on the freshly-freed [0,199] must be granted (owner 1 gone,
    // owner 2 at [200,300] does not overlap).
    assert!(m.lock_op(ROOT_INO, 3, 0, 199, LockType::Write, 43, S).unwrap().is_none());
    // Overlap with owner 2's still-live read must be denied.
    assert!(m.lock_op(ROOT_INO, 4, 250, 500, LockType::Write, 44, S).unwrap().is_some());
}

#[test]
fn get_lock_reports_without_mutating() {
    let m = MetaEngine::open(&tmpdb("getlk")).unwrap();
    m.lock_op(ROOT_INO, 1, 0, WHOLE, LockType::Write, 51, S).unwrap();
    let before = m.get_lock(ROOT_INO, 2, 0, WHOLE, LockType::Read);
    assert!(before.is_some());
    // Same query again: state unchanged.
    let after = m.get_lock(ROOT_INO, 2, 0, WHOLE, LockType::Read);
    assert_eq!(after.unwrap().pid, 51);
    // get_lock must not create rows: owner 2 can now still write-lock.
    assert!(m.lock_op(ROOT_INO, 2, 0, WHOLE, LockType::Read, 52, S).unwrap().is_some());
}

#[test]
fn release_owner_clears_all_their_locks_across_inodes() {
    let m = MetaEngine::open(&tmpdb("rel")).unwrap();
    m.lock_op(10, 99, 0, WHOLE, LockType::Write, 61, S).unwrap();
    m.lock_op(11, 99, 0, WHOLE, LockType::Write, 62, S).unwrap();
    m.release_locks_of_owner(99);
    // Both inodes now grant fresh writes from another owner.
    assert!(m.lock_op(10, 100, 0, WHOLE, LockType::Write, 71, S).unwrap().is_none());
    assert!(m.lock_op(11, 100, 0, WHOLE, LockType::Write, 71, S).unwrap().is_none());
}

#[test]
fn lease_sweep_reaps_expired_locks() {
    let m = MetaEngine::open(&tmpdb("lease")).unwrap();
    m.lock_op(ROOT_INO, 1, 0, WHOLE, LockType::Write, 81, S).unwrap();
    // Simulate "now" far past the lease window.
    let future = chrono_like_now_plus(std::time::Duration::from_secs(3600));
    m.sweep_expired_locks(future);
    // After reaping, another owner can acquire.
    assert!(m.lock_op(ROOT_INO, 2, 0, WHOLE, LockType::Write, 82, S).unwrap().is_none());
}

#[test]
fn renewal_extends_lease_and_reaps_only_non_renewing() {
    // L4 crux: a *live* holder that renews never expires within its lease, while
    // a crashed one (stops renewing) ages out. Renewal always re-stamps
    // expires_at = now + lease, so within the window the lock keeps blocking
    // peers; a sweep at a real future clock (no renew in between) reaps it.
    let m = MetaEngine::open(&tmpdb("renew")).unwrap();
    let sid = 777u64;
    m.lock_op(ROOT_INO, 1, 0, WHOLE, LockType::Write, 91, sid).unwrap();
    // A renew keeps the row live: a peer is still denied.
    m.renew_locks(sid);
    assert!(
        m.lock_op(ROOT_INO, 2, 0, WHOLE, LockType::Write, 92, S).unwrap().is_some(),
        "a renewing (live) holder must keep blocking peers"
    );
    // A renew for an unknown sid is a harmless no-op (zero rows affected).
    m.renew_locks(9999);
    // Sweep at a real future horizon: without further renewal the lock expires.
    let future = chrono_like_now_plus(std::time::Duration::from_secs(
        grovefs::meta::LockLease::secs() as u64 * 2,
    ));
    m.sweep_expired_locks(future);
    assert!(
        m.lock_op(ROOT_INO, 3, 0, WHOLE, LockType::Write, 93, S).unwrap().is_none(),
        "a lock that stopped being renewed must age out within the lease"
    );
}

#[test]
fn release_session_reaps_only_that_daemons_locks() {
    // Simulates reclaiming a crashed daemon's locks by session: every lock
    // tagged with the dead sid disappears, another session's locks survive.
    // Distinct inodes are used because two write locks can never coexist on the
    // same inode (that would defeat the point of the lock).
    let m = MetaEngine::open(&tmpdb("relsid")).unwrap();
    let dead_sid = 500u64;
    let live_sid = 600u64;
    m.lock_op(10, 1, 0, WHOLE, LockType::Write, 111, dead_sid).unwrap();
    m.lock_op(11, 2, 0, WHOLE, LockType::Write, 112, dead_sid).unwrap();
    // live_sid holds a lock on a separate inode.
    m.lock_op(12, 3, 0, WHOLE, LockType::Write, 113, live_sid).unwrap();
    m.release_locks_of_session(dead_sid);
    // dead_sid's inodes are free again...
    assert!(m.lock_op(10, 9, 0, WHOLE, LockType::Write, 119, S).unwrap().is_none());
    assert!(m.lock_op(11, 9, 0, WHOLE, LockType::Write, 120, S).unwrap().is_none());
    // ...but live_sid's lock on ino 12 survives.
    assert!(m.lock_op(12, 9, 0, WHOLE, LockType::Write, 121, S).unwrap().is_some());
}

/// Seconds since unix epoch plus a delta. Kept inline so the test file has no
/// extra dependency; matches `MetaEngine::now()`'s unit (unix seconds).
fn chrono_like_now_plus(d: std::time::Duration) -> i64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    t + d.as_secs() as i64
}

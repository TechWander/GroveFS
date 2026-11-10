# `vendor/foundationdb-sys` — why this copy exists

One sentence: upstream `foundationdb-sys` runs **bindgen** in its build script, which makes
**libclang** an installed package on every host that builds GroveFS with `--features fdb`;
this copy installs pre-generated bindings instead, so that last install-time dependency goes
away.

It is not a fork in the usual sense — nothing here is re-implemented. `src/lib.rs` is
byte-identical to upstream, and `build.rs` keeps upstream's linking logic verbatim with one
replacement in its second half (generate → install).

## Provenance

| | |
|---|---|
| upstream crate | `foundationdb-sys 0.11.0` (repo `foundationdb-rs/foundationdb-rs`) |
| `.crate` archive sha256 | `0bac3aa47fdd7af6ac1b2a8ee4684fe4ed3d6c663c7ff5888e1846a790e6601a` |
| obtained from | crates.io index via the configured `rsproxy.cn` mirror |
| licence | `MIT/Apache-2.0` (upstream `license` field kept in `Cargo.toml`, authors kept) |
| upstream build inputs used | bindgen `0.72.1`, clang driver from `/usr/lib/llvm-10`, args `-I ./include/710 -D FDB_API_VERSION=710`, header `./include/710/fdb_c.h`, `generate_comments(true)`, `layout_tests(false)` |
| generated on | x86_64-unknown-linux-gnu, rustc 1.99.0, 2026-11-10 |

Version survey behind the decision: `foundationdb-sys` 0.8.0, 0.9.0, 0.9.1, 0.10.0 and 0.11.0
were each downloaded and inspected; **every one** declares `[build-dependencies] bindgen` and
calls it unconditionally (no feature, no env escape hatch), and 0.11.0 is the newest published
version at the time of writing. Vendoring was therefore the only way to drop libclang without
giving up the feature.

## File inventory and the exact delta

| file | state | sha256 |
|---|---|---|
| `src/lib.rs` | copied unchanged from the `.crate` | `c0033f5b7717cd519317dc4fc4e70e33de24968052c9610a0b703f289b8aef06` |
| `bindings/api710.rs` | the file upstream's bindgen run emitted (`OUT_DIR/bindings.rs`), stored byte-for-byte, unedited | `53ae6dd54d519bdcc9be11873fdb15c9119f69ebc60269bc05c47d17a68b7080` |
| `build.rs` | rewritten second half; link flags and per-OS search paths are upstream's | — |
| `Cargo.toml` | manifest of upstream with `[build-dependencies] bindgen` removed, packaging metadata (readme/documentation/badges/docs.rs) dropped; name, version, licence, edition, `rust-version` and the whole feature list kept so the patch stays a drop-in | — |

Deliberately **not** copied: `include/` (680 KB of `fdb_c.h` for ten API levels — only bindgen
reads them), `README.md`, `CHANGELOG.md`, the packaged `Cargo.lock`, `.cargo_vcs_info.json`.

Consequence of pinning one API level: the vendored `build.rs` `compile_error!`s unless the
`fdb-7_1` feature is on. GroveFS only ever turns on 710 (see the `fdb` feature comment in the
top-level `Cargo.toml`), and 710 is the API level of the cluster under test (FoundationDB
7.1.67).

## What the patch costs

* The bindings are a snapshot. They cannot drift from a header nobody reads, but they **can**
  drift from a future `foundationdb`/`foundationdb-sys` release that expects new C symbols.
  That is caught by the compiler (missing symbol), not at runtime — and the fix is the refresh
  procedure below, not an edit of `bindings/api710.rs` by hand.
* `[patch.crates-io]` in this manifest applies only when GroveFS is the workspace root. A crate
  that depends on GroveFS ignores it; the client builds (`grovefs-client/{win,mac}` →
  `--no-default-features --features remote-only`) never enable `fdb`, so nothing is patched
  there and nothing needs to be.

## Refresh procedure (regenerate the snapshot)

Only needed when the pinned `foundationdb-sys` version, the API level, or the C header changes.
It needs libclang exactly once, on a machine that has it:

```bash
# 1. point the dependency at the new version and REMOVE the [patch.crates-io] entry
# 2. generate with upstream's own build script:
FDB_CLIENT_LIB_PATH=/path/to/lib cargo build --release --features fdb --target-dir target/tmp
# 3. install what bindgen produced, then re-record the sha256 in this file:
cp target/tmp/release/build/foundationdb-sys-*/out/bindings.rs vendor/foundationdb-sys/bindings/api710.rs
sha256sum vendor/foundationdb-sys/bindings/api710.rs
# 4. restore the [patch.crates-io] entry, rebuild, and run the FDB arm:
bash .build-bin/run_all.sh fdb            # must end: SUMMARY: pass=170 fail=0 SKIP=0
```

## Rollback (go back upstream)

Delete the `[patch.crates-io]` section from the top-level `Cargo.toml` (and optionally this
directory). Builds then take the upstream path again: bindings are generated from the crate's
own header for whichever API feature is enabled — and libclang becomes a build dependency of
`--features fdb` once more. Nothing else in GroveFS knows about this directory.

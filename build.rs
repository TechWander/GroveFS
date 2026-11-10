// Link-time rpath for the optional FoundationDB arm.
//
// Why this file exists at all: `libfdb_c` is never installed as a system package
// in this project's environment path (no apt source, no docker daemon — see
// `.build-bin/fdb_env.sh`), it is a verified release asset sitting in a plain
// directory. `foundationdb-sys` therefore gets `-lfdb_c` plus a search path at
// BUILD time via `FDB_CLIENT_LIB_PATH`, but emits no runtime path, so a
// feature-built binary only starts if the caller also exports `LD_LIBRARY_PATH`.
// That is an invisible requirement: it turns "run this binary" into "run this
// binary with someone else's environment".
//
// The fix belongs here, not in documentation: bake search paths into the ELF so
// the binary resolves its own client library.
//
// Scope guard: everything below runs ONLY with `--features fdb`. A default or
// `--no-default-features` build executes the early return and links exactly what
// it linked before, so the supported backends (SQLite/PG) keep their byte-level
// build behaviour and their lack of any FDB dependency.
fn main() {
    println!("cargo::rerun-if-env-changed=FDB_CLIENT_LIB_PATH");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_FDB");
    if std::env::var_os("CARGO_FEATURE_FDB").is_none() {
        return;
    }

    // `$ORIGIN`-relative entries make a staged tree self-contained: a directory
    // holding `grovefs` plus `lib/libfdb_c.so` (or `../lib/…`, the layout of
    // `dist/` archives) needs no environment variable and no loader config.
    // The absolute entry is the build host's convenience, so the in-tree
    // artifacts under target*/release keep working exactly as before.
    let mut paths: Vec<String> = vec![
        "$ORIGIN/lib".to_string(),
        "$ORIGIN/../lib".to_string(),
    ];
    if let Ok(dir) = std::env::var("FDB_CLIENT_LIB_PATH") {
        let dir = dir.trim();
        if !dir.is_empty() {
            paths.push(dir.to_string());
        }
    }

    // The generic form covers every link invocation of this package (bins, tests,
    // examples). Naming target kinds explicitly is not an option: Cargo rejects a
    // `rustc-link-arg-<kind>` instruction for any kind the package does not have.
    for p in &paths {
        println!("cargo::rustc-link-arg=-Wl,-rpath,{p}");
    }
}

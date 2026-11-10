//! Vendored build script — see `VENDOR.md` for provenance and for the one-line
//! diff against upstream `foundationdb-sys 0.11.0`.
//!
//! Upstream's build script runs `bindgen` unconditionally, which means every build
//! host of `--features fdb` needs libclang installed. This copy produces the same
//! `OUT_DIR/bindings.rs` by installing the checked-in `bindings/api710.rs` — the
//! file bindgen emitted from this exact crate version and this exact `fdb_c.h`,
//! recorded byte-for-byte in `VENDOR.md`.
//!
//! Everything else is upstream's logic verbatim: link against `fdb_c`, honour
//! `FDB_CLIENT_LIB_PATH` as the linker search path, and add the per-OS default
//! search directories.
use std::env;
use std::path::PathBuf;

// The vendored bindings cover one API level, and the crate's own feature set would
// happily ask for another. Fail at compile time with the way back, rather than
// silently linking API-710 symbols against a cluster speaking a different version.
#[cfg(not(feature = "fdb-7_1"))]
compile_error!(
    "vendor/foundationdb-sys carries pre-generated bindings for API 710 only.\n\
     Either enable the `fdb-7_1` feature, or drop the `[patch.crates-io]` entry in\n\
     the workspace Cargo.toml to build upstream `foundationdb-sys` (that path needs\n\
     libclang at build time and generates bindings for whichever API feature you pick)."
);

fn main() {
    // Link against fdb_c.
    println!("cargo:rustc-link-lib=fdb_c");

    if let Ok(lib_path) = env::var("FDB_CLIENT_LIB_PATH") {
        println!("cargo:rustc-link-search=native={lib_path}");
    }

    // Include the link directory for macOS.
    // Since Rust 1.91, /usr/local/lib is no longer searched automatically on macOS.
    // See: https://github.com/rust-lang/rust/pull/131477
    // See: https://github.com/rust-lang/rust/issues/147010
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-search=/usr/local/lib");

    // Include the link directory for the .lib file on windows (which will resolve to
    // the shared library, at runtime)
    #[cfg(target_os = "windows")]
    println!("cargo:rustc-link-search=C:/Program Files/foundationdb/lib/foundationdb");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is not defined!"));
    let bindings = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bindings/api710.rs");
    println!("cargo:rerun-if-changed={}", bindings.display());
    println!("cargo:rerun-if-env-changed=FDB_CLIENT_LIB_PATH");

    std::fs::copy(&bindings, out_dir.join("bindings.rs")).unwrap_or_else(|err| {
        panic!(
            "could not install the checked-in bindings {} into {}/bindings.rs: {err}",
            bindings.display(),
            out_dir.display()
        )
    });
}

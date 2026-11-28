//! The shape of what the CLI writes to stderr when it refuses a flag
//! combination.
//!
//! This is the same contract the server's governance replies keep (ADR-0006
//! §7): ONE logical line per message. A terminal re-wraps a long sentence for
//! a human, but `cli_style::width()` reports `usize::MAX` when stderr is not a
//! tty, so nothing re-wraps a pipe — `grovefs --help`'s OUTPUT CONTRACT says
//! "Pipes: … soft wrapping is off (one row per line)". Anything folded in the
//! source therefore reaches `2>log`, `| tee`, a bug report and the doc
//! examples broken mid-sentence, with a run of indent columns where the text
//! was supposed to continue. Measured 2026-11-28: nine of these messages were
//! written that way, and two of the docs had transcribed them verbatim.
//!
//! Unlike the source-shape guard in `server_ext::msg_shape_tests`, this reads
//! the COMPILED strings, so a hand-folded two-column break is caught too — it
//! runs the binary the way an operator does.

#![cfg(feature = "web")]

use std::{fs, process::Command};

/// A config file that exists and says nothing. Pointing `GROVEFS_CONFIG` at a
/// missing path would put a `warning:` line on stderr BEFORE the refusal, and
/// one line is exactly what this file is about.
fn quiet_conf() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("grovefs-cli-msg-shape-conf");
    fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("grovefs.conf");
    fs::write(&conf, "").unwrap();
    conf
}

/// Run the CLI with a clean environment and a CLOSED stdin, returning stderr.
/// An empty stdin is what makes the `--password-stdin` refusal happen here
/// rather than the test blocking on a terminal.
fn stderr_of(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_grovefs"))
        .args(args)
        .env("GROVEFS_CONFIG", quiet_conf())
        .env_remove("GROVEFS_HOME")
        .env_remove("GROVEFS_META_SERVER")
        .env_remove("GROVEFS_DATA_SERVERS")
        .env_remove("GROVEFS_META_STORE")
        .env_remove("GROVEFS_TOKEN")
        .env_remove("GROVEFS_API_KEY")
        .env_remove("GROVEFS_CONTROL_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("grovefs binary");
    assert!(!out.status.success(), "these flags must be refused: {args:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Every refusal below has to come back as exactly one `error:` line.
fn one_refusal(args: &[&str], names: &str) {
    let err = stderr_of(args);
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "{args:?} wrote {} lines, expected one:\n{err}",
        lines.len()
    );
    let line = lines[0];
    assert!(
        line.starts_with("error: ") && line[7..].starts_with(names),
        "{args:?} refused with the wrong message: {line}"
    );
}

#[test]
fn flag_refusals_are_one_line() {
    // A password file to point the meta-server flags at; the refusal is about
    // the flag PAIR, so the contents never matter — but the path must exist for
    // the check that fires to be the one under test.
    let dir = std::env::temp_dir().join("grovefs-cli-msg-shape");
    fs::create_dir_all(&dir).unwrap();
    let pw = dir.join("pw");
    fs::write(&pw, "not-a-real-password\n").unwrap();
    let meta = dir.join("meta");
    fs::create_dir_all(&meta).unwrap();
    let pw = pw.to_str().unwrap();
    let meta = meta.to_str().unwrap();

    // `grovefs web` — the gateway's own flag checks.
    one_refusal(&["web", "--api-key", "a:b"], "grovefs web needs");
    one_refusal(
        &["web", "--control-server", "127.0.0.1:1"],
        "--control-server 127.0.0.1:1 needs",
    );
    one_refusal(
        &[
            "web",
            "--control-server",
            "127.0.0.1:1",
            "--api-key",
            "a:b",
            "--session-ttl",
            "0s",
        ],
        "--session-ttl cannot be 0",
    );
    // An unreachable control plane: the gateway's startup probe names the
    // address and the fix, in one line, before anything binds.
    one_refusal(
        &[
            "web",
            "--control-server",
            "127.0.0.1:1",
            "--api-key",
            "nope:not-a-key",
        ],
        "cannot reach the control plane",
    );
    // Where a password may come from, and what happens when it does not.
    one_refusal(
        &[
            "web",
            "--control-server",
            "127.0.0.1:1",
            "--api-key",
            "a:b",
            "account",
            "create",
            "--account",
            "bob",
        ],
        "a password comes from",
    );
    one_refusal(
        &[
            "web",
            "--control-server",
            "127.0.0.1:1",
            "--api-key",
            "a:b",
            "account",
            "create",
            "--account",
            "bob",
            "--password-stdin",
        ],
        "--password-stdin received nothing",
    );
    // `meta-server --admin-user` — the install-time owner bootstrap.
    one_refusal(
        &["meta-server", "--meta-dir", meta, "--admin-user", "alice"],
        "--admin-user needs --admin-password-file",
    );
    one_refusal(
        &["meta-server", "--meta-dir", meta, "--admin-password-file", pw],
        "--admin-password-file needs --admin-user",
    );
    one_refusal(
        &[
            "meta-server",
            "--meta-dir",
            meta,
            "--admin-user",
            "alice",
            "--admin-password-file",
            pw,
        ],
        "--admin-user needs --control-listen",
    );
    // Two more refusals that were written with the OTHER shape of the mistake:
    // a literal continued across a raw newline, which compiles the newline plus
    // the next line's indent straight into the text.
    one_refusal(
        &["meta-server", "--meta-dir", meta, "--audit-retention", "30d"],
        "--audit-retention needs --control-listen",
    );
    one_refusal(
        &["apikey", "--meta-dir", meta, "create", "--label", "x", "--for", "admin"],
        "`--for admin` needs `--owner <account>`",
    );

    let _ = fs::remove_dir_all(&dir);
}

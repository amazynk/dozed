//! `dozectl … | head` must exit quietly, not panic.
//!
//! Rust installs `SIG_IGN` for `SIGPIPE`, so a write to a closed pipe returns
//! `EPIPE` where a C program would have died silently — and the `print!` macros
//! unwrap it. What that produced was a Rust panic and a `RUST_BACKTRACE` note
//! where every other unix tool prints nothing at all.
//!
//! This has to spawn the real binary and close the pipe on it, because the bug
//! lives in the interaction between the process and its reader; nothing
//! in-process reproduces it.

use std::io::Read as _;
use std::process::{Command, Stdio};

/// The binary under test, as cargo built it next to this integration test.
fn dozectl() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("test binary path");
    p.pop(); // deps/
    p.pop();
    p.join("dozectl")
}

/// Run `dozectl <args>`, read one line, then drop the pipe — the `| head`
/// shape — and report how the process ended.
fn exit_after_closing_the_pipe(args: &[&str]) -> std::process::ExitStatus {
    let mut child = Command::new(dozectl())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dozectl");

    {
        let mut out = child.stdout.take().expect("piped stdout");
        let mut buf = [0u8; 64];
        let _ = out.read(&mut buf);
    } // closed here, so the next write gets EPIPE

    child.wait().expect("wait")
}

#[test]
fn help_survives_a_closed_pipe() {
    // A smoke test rather than the guard: clap writes `--help` itself and does
    // not go through `emit`, so this passed even against the buggy build. Kept
    // because it costs nothing and covers the path a user reaches first.
    let status = exit_after_closing_the_pipe(&["--help"]);
    assert!(
        status.success() || status.code() == Some(0),
        "closing the pipe must not be an error: {status:?}"
    );
    // 101 is a Rust panic, which is the exact regression.
    assert_ne!(status.code(), Some(101), "dozectl panicked on a closed pipe");
}

#[test]
fn trace_list_survives_a_closed_pipe() {
    // This is the one with teeth. It goes through `emit` and writes a row per
    // trace file, so it keeps writing after the reader has gone. Verified by
    // restoring the old `print!` and watching it fail with exit 101.
    let status = exit_after_closing_the_pipe(&["trace", "--list"]);
    assert_ne!(status.code(), Some(101), "dozectl panicked on a closed pipe");
}

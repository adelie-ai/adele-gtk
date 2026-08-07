//! Acceptance tests for `adelie-ai/adele-gtk#152`: the console writer moves
//! to stderr, and the binary stays quiet unless `RUST_LOG` is set.
//!
//! `main.rs` builds a GTK `Application` and blocks in its main loop, so it
//! cannot run headless here. `src/bin/telemetry_probe.rs` installs telemetry
//! the same way `main.rs` does and exits, which is what these tests run as a
//! subprocess so they can inspect the real process's stdout and stderr
//! (`std::process::Command` gives each its own pipe — no FD tricks needed).

use std::process::{Command, Stdio};

fn probe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_telemetry_probe"))
}

/// The MCP stdio transport frames JSON-RPC on stdout, and every other
/// adelie-ai binary writes logs to stderr (epic D1). This is the defect at
/// `src/main.rs:147`: the default `tracing_subscriber::fmt()` writer is
/// stdout.
#[test]
fn logs_go_to_stderr_not_stdout() {
    let output = probe()
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run telemetry_probe");

    assert!(
        output.status.success(),
        "telemetry_probe exited with {:?}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.is_empty(),
        "nothing must reach stdout at any level; got: {stdout:?}"
    );
    assert!(
        stderr.contains("probe line"),
        "the log line must reach stderr; got: {stderr:?}"
    );
}

/// A GUI has no console to read, so the filter must not invent a default
/// level. Absent `RUST_LOG`, this binary must produce nothing on either
/// stream (epic D1/D4; "Keep quiet-by-default" in the ticket).
#[test]
fn silent_when_rust_log_is_unset() {
    let output = probe()
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run telemetry_probe");

    assert!(
        output.status.success(),
        "telemetry_probe exited with {:?}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.is_empty(), "stdout must stay empty; got: {stdout:?}");
    assert!(
        !stderr.contains("probe line"),
        "an info line must not appear with RUST_LOG unset; got: {stderr:?}"
    );
}

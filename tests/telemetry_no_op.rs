//! Acceptance test for `adelie-ai/adele-gtk#152`: a second telemetry install
//! in one process is a no-op, not a panic.
//!
//! This binary hosts mcp-core server libraries in process (built-in MCP
//! servers, `builtin-core`). If one of those libraries ever called the same
//! install path this binary's `main` does, the old
//! `tracing_subscriber::fmt() ... .init()` call would panic on the second
//! `set_global_default` — a library-hosted crash rather than a no-op.
//!
//! Runs `src/bin/telemetry_probe.rs --double-init`, which repeats the
//! install call in-process (see its own doc comment) and exits 0 only if the
//! second call did not panic.

use std::process::{Command, Stdio};

#[test]
fn second_install_is_a_no_op_not_a_panic() {
    let output = Command::new(env!("CARGO_BIN_EXE_telemetry_probe"))
        .arg("--double-init")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run telemetry_probe --double-init");

    assert!(
        output.status.success(),
        "a second telemetry install must be a no-op, not a panic; \
         exit status: {:?}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

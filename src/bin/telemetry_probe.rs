//! Standalone harness for exercising adele-gtk's telemetry wiring without a
//! GTK display server.
//!
//! `main.rs` builds a GTK `Application` and blocks in its main loop, so it
//! cannot run headless in a test. This binary installs telemetry the same
//! way `main.rs` does — including entering a Tokio runtime first, so the
//! gRPC OTLP transport works here too — sharing `src/telemetry.rs` via
//! `#[path]` (this crate has no lib target for the two bins to share
//! through) and then exits, so `tests/telemetry_stderr.rs` and
//! `tests/telemetry_no_op.rs` can assert on the real process's stdout and
//! stderr (epic `adelie-ai/mcp-core#38`, ticket `adelie-ai/adele-gtk#152`).
//!
//! `--double-init` repeats the install call, simulating an in-process
//! mcp-core server library also calling it (D5): that must be a no-op, not
//! a panic.

#[path = "../telemetry.rs"]
mod telemetry;

fn install(rt: &tokio::runtime::Runtime) -> adelie_telemetry::Guard {
    // Mirrors main.rs: the gRPC OTLP transport needs a Tokio runtime at the
    // moment `init` builds the exporter pipelines, and that runtime must
    // still be alive when the returned guard drops and shuts the pipelines
    // down — so the caller holds `rt`, not this function.
    let _enter = rt.enter();
    adelie_telemetry::init(telemetry::config()).expect("telemetry init")
}

fn main() {
    let double_init = std::env::args().any(|arg| arg == "--double-init");

    let rt = tokio::runtime::Runtime::new().expect("build a tokio runtime");
    let _guard = install(&rt);
    if double_init {
        let _second_guard = install(&rt);
    }
    tracing::info!("probe line");
}

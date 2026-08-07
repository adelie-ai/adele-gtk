//! Standalone harness for exercising adele-gtk's telemetry wiring without a
//! GTK display server.
//!
//! `main.rs` builds a GTK `Application` and blocks in its main loop, so it
//! cannot run headless in a test. This binary installs telemetry the same
//! way `main.rs` does (sharing `src/telemetry.rs` via `#[path]`, since this
//! crate has no lib target for the two bins to share through) and then
//! exits, so `tests/telemetry_stderr.rs` and `tests/telemetry_no_op.rs` can
//! assert on the real process's stdout and stderr (epic
//! `adelie-ai/mcp-core#38`, ticket `adelie-ai/adele-gtk#152`).
//!
//! `--double-init` repeats the install call, simulating an in-process
//! mcp-core server library also calling it (D5): that must be a no-op, not
//! a panic.

#[path = "../telemetry.rs"]
mod telemetry;

fn main() {
    let double_init = std::env::args().any(|arg| arg == "--double-init");

    let _guard = adelie_telemetry::init(telemetry::config()).expect("telemetry init");
    if double_init {
        let _second_guard =
            adelie_telemetry::init(telemetry::config()).expect("telemetry init (second call)");
    }
    tracing::info!("probe line");
}

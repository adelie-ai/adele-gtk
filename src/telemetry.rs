//! Telemetry configuration for this binary (epic `adelie-ai/mcp-core#38`,
//! ticket `adelie-ai/adele-gtk#152`).
//!
//! `main.rs` installs the subscriber this module configures, holding the
//! returned [`adelie_telemetry::Guard`] for the life of `main` so the three
//! pipelines flush on exit. The GUI has no console to read, so the filter
//! must stay quiet unless `RUST_LOG` is set (epic D1/D4) — this module owns
//! that default so `main.rs` and `src/bin/telemetry_probe.rs` (included via
//! `#[path]`, since this is a bin-only crate with no lib target) cannot
//! drift apart on it.

/// Build this binary's telemetry configuration.
pub(crate) fn config() -> adelie_telemetry::Config {
    adelie_telemetry::Config::new("adele-gtk")
        // `adelie-telemetry`'s own fallback is "info" (its DEFAULT_FILTER).
        // This binary was silent unless asked before #152 and stays that
        // way: an empty filter has no directives, so EnvFilter enables
        // nothing when RUST_LOG is unset or unparseable.
        .with_default_filter("")
        // The periodic metrics summary has nowhere to be read in a GUI: no
        // console attaches to a desktop launch, and the daemon is where the
        // interesting numbers live (epic mcp-core#38, ticket #152, item 7).
        .with_metrics_dump_interval(std::time::Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Epic D1/D4: a client that was quiet unless asked must stay quiet
    /// unless asked.
    #[test]
    fn silent_when_rust_log_is_unset() {
        assert_eq!(
            config().default_filter(),
            "",
            "an empty default filter is what keeps this binary silent when \
             RUST_LOG is unset — see EnvFilter::new(\"\")"
        );
    }

    /// The periodic metrics summary has no reader in a GUI (item 7): it
    /// must be off, not merely infrequent.
    #[test]
    fn metrics_dump_is_disabled() {
        assert_eq!(config().metrics_dump_interval(), std::time::Duration::ZERO);
    }
}

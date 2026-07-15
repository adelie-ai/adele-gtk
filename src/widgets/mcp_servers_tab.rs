//! MCP-servers tab of the Settings dialog (issue #495).
//!
//! Shows the live list of the daemon's Model Context Protocol servers
//! (`ListMcpServers`) with an *honest* per-server status, an enable/disable
//! toggle, add/edit (transport-aware), remove, and — for an OAuth server that
//! needs sign-in — a Configure/Sign-in button. Like [`super::connections_tab`]
//! it is a passive view: it renders `McpServerView`s and asks the parent (the
//! Settings dialog) to perform the actual RPC work via callbacks. The parent
//! owns the `Connector` and the async bridge.
//!
//! The status → (dot colour, label) and transport → chip mappings are pure and
//! unit-tested here; the row-building GTK code is the thin shell over them.

/// Map the coarse daemon status string to a `(dot CSS modifier class, human
/// label)` pair. Covers the six states the daemon reports; any unrecognized
/// future state renders as a neutral "Unknown" rather than panicking, so an
/// older client degrades honestly against a newer daemon.
///
/// The class is a `mcp-dot-*` modifier applied alongside the base `mcp-dot`
/// class (see `style.css`): `running` → green, `needs_auth`/`auth_expired` →
/// amber, `error` → red, everything else → neutral grey.
pub fn status_display(_status: &str) -> (&'static str, &'static str) {
    unimplemented!()
}

/// The transport chip label: an HTTP server is `"remote"`, anything else
/// (stdio) is `"local"`.
pub fn transport_chip(_transport: &str) -> &'static str {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- status_display -------------------------------------------------------

    #[test]
    fn status_display_covers_all_six_states() {
        assert_eq!(status_display("running"), ("mcp-dot-running", "Running"));
        assert_eq!(status_display("stopped"), ("mcp-dot-neutral", "Stopped"));
        assert_eq!(status_display("disabled"), ("mcp-dot-neutral", "Disabled"));
        assert_eq!(
            status_display("needs_auth"),
            ("mcp-dot-warn", "Sign in required")
        );
        assert_eq!(
            status_display("auth_expired"),
            ("mcp-dot-warn", "Sign in expired")
        );
        assert_eq!(status_display("error"), ("mcp-dot-error", "Error"));
    }

    #[test]
    fn status_display_unknown_is_neutral() {
        assert_eq!(status_display("teleporting"), ("mcp-dot-neutral", "Unknown"));
        assert_eq!(status_display(""), ("mcp-dot-neutral", "Unknown"));
    }

    // --- transport_chip -------------------------------------------------------

    #[test]
    fn transport_chip_http_is_remote_else_local() {
        assert_eq!(transport_chip("http"), "remote");
        assert_eq!(transport_chip("stdio"), "local");
        assert_eq!(transport_chip("something-new"), "local");
    }
}

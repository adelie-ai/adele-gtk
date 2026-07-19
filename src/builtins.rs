//! Compiled-in ("built-in") MCP servers hosted in-process (da#538 Phase C).
//!
//! The core set (fileio/terminal/tasks/web) is compiled in and hosted by
//! default so a fresh gtk client is useful with no `client-mcp.toml`. An
//! external client-mcp server of the SAME NAME overrides (suppresses) the
//! built-in: external > built-in.

use desktop_assistant_client_common::mcp_host::BuiltinServer;

/// Build the enabled built-in servers, skipping any whose name is shadowed by a
/// configured client-mcp server of the same name (external override wins).
pub fn builtin_servers(configured_names: &[String]) -> Vec<BuiltinServer> {
    // Stub: the real cfg-gated registration lands in the implementation commit.
    let _ = configured_names;
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fileio's constructor is infallible, so with nothing shadowing it the
    /// built-in set deterministically contains a server named "fileio",
    /// advertised under the "fileio" namespace.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn fileio_builtin_present_and_namespaced_by_default() {
        let servers = builtin_servers(&[]);
        let fileio = servers
            .iter()
            .find(|s| s.name == "fileio")
            .expect("fileio built-in must be present when nothing shadows it");
        assert_eq!(
            fileio.namespace, "fileio",
            "fileio built-in must be advertised under the 'fileio' namespace"
        );
    }

    /// A configured client-mcp server of the same name suppresses the built-in
    /// (external > built-in), so the built-in set omits "fileio" entirely.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn external_same_name_shadows_builtin() {
        let servers = builtin_servers(&["fileio".to_string()]);
        assert!(
            !servers.iter().any(|s| s.name == "fileio"),
            "an external client-mcp server named 'fileio' must suppress the built-in"
        );
    }
}

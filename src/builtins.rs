//! Compiled-in ("built-in") MCP servers hosted in-process (da#538 Phase C).
//!
//! The core set (fileio/terminal/tasks/web) is compiled in and hosted by
//! default so a fresh gtk client is useful with no `client-mcp.toml`. An
//! external client-mcp server of the SAME NAME overrides (suppresses) the
//! built-in: external > built-in.

use desktop_assistant_client_common::mcp_host::BuiltinServer;
#[cfg(any(
    feature = "mcp-fileio",
    feature = "mcp-terminal",
    feature = "mcp-tasks",
    feature = "mcp-web"
))]
use std::sync::Arc;

/// Build the enabled built-in servers, skipping any whose name is shadowed by a
/// configured client-mcp server of the same name (external override wins).
///
/// Each `#[cfg]` block compiles in only when its `mcp-*` feature is on, so a
/// `--no-default-features` build hosts nothing and gtk behaves as it did before
/// Phase C. The infallible constructors (fileio, web) are always registered;
/// the fallible ones (terminal, tasks) are logged and skipped if their
/// zero-config constructor fails, so a broken environment degrades to the
/// remaining tools rather than losing the whole set.
pub fn builtin_servers(configured_names: &[String]) -> Vec<BuiltinServer> {
    // Unused only when every mcp-* feature is off (`--no-default-features`),
    // where no built-in is compiled in to consult it.
    #[cfg_attr(
        not(any(
            feature = "mcp-fileio",
            feature = "mcp-terminal",
            feature = "mcp-tasks",
            feature = "mcp-web"
        )),
        allow(unused_variables)
    )]
    let shadowed = |name: &str| configured_names.iter().any(|n| n == name);
    #[allow(unused_mut)]
    let mut out: Vec<BuiltinServer> = Vec::new();

    #[cfg(feature = "mcp-fileio")]
    if !shadowed("fileio") {
        out.push(BuiltinServer::new(
            "fileio",
            "fileio",
            Arc::new(fileio_mcp::build_service()),
        ));
    }
    #[cfg(feature = "mcp-terminal")]
    if !shadowed("terminal") {
        match terminal_mcp::build_service() {
            Ok(svc) => out.push(BuiltinServer::new("terminal", "terminal", Arc::new(svc))),
            Err(e) => tracing::warn!("built-in terminal server unavailable: {e}"),
        }
    }
    #[cfg(feature = "mcp-tasks")]
    if !shadowed("tasks") {
        match tasks_mcp::build_service() {
            Ok(svc) => out.push(BuiltinServer::new("tasks", "tasks", Arc::new(svc))),
            Err(e) => tracing::warn!("built-in tasks server unavailable: {e}"),
        }
    }
    #[cfg(feature = "mcp-web")]
    if !shadowed("web") {
        out.push(BuiltinServer::new(
            "web",
            "web",
            Arc::new(web_mcp::build_service()),
        ));
    }

    out
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

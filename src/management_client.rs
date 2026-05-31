//! Thin wrappers over the live `TransportClient` for named-connection and
//! purpose management commands (Settings dialog).
//!
//! These use the WebSocket connection's `send_command` escape hatch (the
//! `AssistantCommands` trait), which the D-Bus surface doesn't expose — the
//! Settings dialog is WS-only, matching the per-conversation model picker.
//! Each wrapper returns a typed result so callers don't pattern-match the
//! protocol envelope.

use anyhow::{Result, anyhow};
use desktop_assistant_api_model as api;
use desktop_assistant_client_common::{AssistantCommands, TransportClient};

/// Resolve the WebSocket client, erroring on transports that can't carry
/// these management commands (D-Bus).
fn ws(transport: &TransportClient) -> Result<&desktop_assistant_client_common::ws_client::WsClient> {
    transport
        .as_ws()
        .ok_or_else(|| anyhow!("named-connection management requires a WebSocket transport"))
}

/// Coerce a `CommandResult` that should be a bare `Ack` into `()`, turning
/// any other variant into a descriptive error. Shared by the mutating
/// wrappers so the "unexpected response" arm is uniform and unit-testable
/// without a live transport. `command` names the command for the message.
fn expect_ack(command: &str, result: api::CommandResult) -> Result<()> {
    match result {
        api::CommandResult::Ack => Ok(()),
        other => Err(anyhow!("unexpected response for {command}: {other:?}")),
    }
}

pub async fn list_connections(transport: &TransportClient) -> Result<Vec<api::ConnectionView>> {
    let result = ws(transport)?
        .send_command(api::Command::ListConnections)
        .await?;
    match result {
        api::CommandResult::Connections(list) => Ok(list),
        other => Err(anyhow!("unexpected response for ListConnections: {other:?}")),
    }
}

pub async fn create_connection(
    transport: &TransportClient,
    id: String,
    config: api::ConnectionConfigView,
) -> Result<()> {
    let result = ws(transport)?
        .send_command(api::Command::CreateConnection { id, config })
        .await?;
    expect_ack("CreateConnection", result)
}

pub async fn update_connection(
    transport: &TransportClient,
    id: String,
    config: api::ConnectionConfigView,
) -> Result<()> {
    let result = ws(transport)?
        .send_command(api::Command::UpdateConnection { id, config })
        .await?;
    expect_ack("UpdateConnection", result)
}

pub async fn delete_connection(
    transport: &TransportClient,
    id: String,
    force: bool,
) -> Result<()> {
    let result = ws(transport)?
        .send_command(api::Command::DeleteConnection { id, force })
        .await?;
    expect_ack("DeleteConnection", result)
}

pub async fn list_available_models(
    transport: &TransportClient,
    connection_id: Option<String>,
    refresh: bool,
) -> Result<Vec<api::ModelListing>> {
    let result = ws(transport)?
        .send_command(api::Command::ListAvailableModels {
            connection_id,
            refresh,
        })
        .await?;
    match result {
        api::CommandResult::Models(m) => Ok(m),
        other => Err(anyhow!(
            "unexpected response for ListAvailableModels: {other:?}"
        )),
    }
}

pub async fn get_purposes(transport: &TransportClient) -> Result<api::PurposesView> {
    let result = ws(transport)?
        .send_command(api::Command::GetPurposes)
        .await?;
    match result {
        api::CommandResult::Purposes(p) => Ok(p),
        other => Err(anyhow!("unexpected response for GetPurposes: {other:?}")),
    }
}

pub async fn set_purpose(
    transport: &TransportClient,
    purpose: api::PurposeKindApi,
    config: api::PurposeConfigView,
) -> Result<()> {
    let result = ws(transport)?
        .send_command(api::Command::SetPurpose { purpose, config })
        .await?;
    expect_ack("SetPurpose", result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GTK-free smoke test for a wrapper error arm: when the daemon answers a
    /// mutating command with something other than `Ack`, the wrapper must
    /// surface a descriptive error naming the command rather than silently
    /// succeeding. `Connections(..)` stands in for any non-`Ack` envelope.
    #[test]
    fn expect_ack_rejects_non_ack_response() {
        let err = expect_ack("CreateConnection", api::CommandResult::Connections(vec![]))
            .expect_err("non-Ack response must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("CreateConnection"),
            "error should name the command: {msg}"
        );
        assert!(
            msg.contains("unexpected response"),
            "error should flag the unexpected envelope: {msg}"
        );
    }

    #[test]
    fn expect_ack_passes_through_ack() {
        assert!(expect_ack("SetPurpose", api::CommandResult::Ack).is_ok());
    }
}

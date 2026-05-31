//! Thin wrappers over the live `TransportClient` for named-connection and
//! purpose management commands (Settings dialog).
//!
//! These use the command channel's `send_command` escape hatch (the
//! `AssistantCommands` trait), which the D-Bus surface doesn't expose — the
//! Settings dialog needs a local-socket or WebSocket connection, matching the
//! per-conversation model picker. Each wrapper returns a typed result so
//! callers don't pattern-match the protocol envelope.

use anyhow::{Result, anyhow};
use desktop_assistant_api_model as api;
use desktop_assistant_client_common::{AssistantCommands, TransportClient};

/// Error surfaced when the live transport can't carry the command channel
/// (the D-Bus surface, which speaks a separate typed zbus interface and so
/// does not implement `AssistantCommands`). Shared with the gate's test so the
/// reworded wording stays in lock-step.
const NO_COMMAND_CHANNEL: &str = "named-connection management requires a local-socket or \
                                  WebSocket connection (not available over D-Bus)";

/// Resolve the command channel, erroring on transports that can't carry
/// these management commands (D-Bus).
fn commands(transport: &TransportClient) -> Result<&(dyn AssistantCommands + '_)> {
    transport
        .as_commands()
        .ok_or_else(|| anyhow!(NO_COMMAND_CHANNEL))
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
    let result = commands(transport)?
        .send_command(api::Command::ListConnections)
        .await?;
    match result {
        api::CommandResult::Connections(list) => Ok(list),
        other => Err(anyhow!(
            "unexpected response for ListConnections: {other:?}"
        )),
    }
}

pub async fn create_connection(
    transport: &TransportClient,
    id: String,
    config: api::ConnectionConfigView,
) -> Result<()> {
    let result = commands(transport)?
        .send_command(api::Command::CreateConnection { id, config })
        .await?;
    expect_ack("CreateConnection", result)
}

pub async fn update_connection(
    transport: &TransportClient,
    id: String,
    config: api::ConnectionConfigView,
) -> Result<()> {
    let result = commands(transport)?
        .send_command(api::Command::UpdateConnection { id, config })
        .await?;
    expect_ack("UpdateConnection", result)
}

pub async fn delete_connection(transport: &TransportClient, id: String, force: bool) -> Result<()> {
    let result = commands(transport)?
        .send_command(api::Command::DeleteConnection { id, force })
        .await?;
    expect_ack("DeleteConnection", result)
}

pub async fn list_available_models(
    transport: &TransportClient,
    connection_id: Option<String>,
    refresh: bool,
) -> Result<Vec<api::ModelListing>> {
    let result = commands(transport)?
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
    let result = commands(transport)?
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
    let result = commands(transport)?
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

    /// adele-gtk#49: the gate now keys on the command channel
    /// (`as_commands`, available on UDS *and* WS) rather than `as_ws`. When no
    /// command channel is present (D-Bus), the wrapper must reject with a
    /// message that names both accepted transports and flags D-Bus as
    /// excluded. Asserting the shared `NO_COMMAND_CHANNEL` constant keeps the
    /// reworded wording from silently regressing; the per-variant
    /// `as_commands` mapping itself is unit-tested daemon-side
    /// (`transport_command_channel.rs`) where a live socket is cheap.
    #[test]
    fn command_channel_gate_message_names_both_socket_transports() {
        let err = anyhow!(NO_COMMAND_CHANNEL).to_string();
        assert!(
            err.contains("local-socket"),
            "must name the UDS path: {err}"
        );
        assert!(err.contains("WebSocket"), "must name the WS path: {err}");
        assert!(
            err.contains("not available over D-Bus"),
            "must flag D-Bus as excluded: {err}"
        );
    }
}

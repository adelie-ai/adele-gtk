# Adele GTK

GTK4 desktop client for the [Adelie AI Platform](https://github.com/adelie-ai/desktop-assistant).
Connects to the `desktop-assistant-daemon` over WebSocket or D-Bus.

## What it does today

- **Streaming chat** rendered via a WebKitGTK web view (with a Label-based
  fallback when WebKit is unavailable).
- **Connection profiles** with login screen, multi-window support, and
  conversation archival.
- **Per-conversation model picker** in the chat header, plus a Select Models
  dialog for filtering the dropdown.
- **Knowledge base browser/editor** from the hamburger menu.
- **Tool-usage cost view** from the hamburger menu, scoped to the open
  conversation: per-tool call counts and resident token cost, ranked by
  either axis, grouped by the hosting MCP server with subtotals.
- **Process manager view** as a sidebar `GtkStack` page with a status dot per
  task and toolbar buttons for Cancel / Open Conversation. Currently polls
  every 5s — streaming via `SignalEvent::Task*` is tracked in
  [#22](https://github.com/adelie-ai/adele-gtk/issues/22).
- **Auto-reconnect** to the last profile, with a hamburger entry to switch
  profiles without restart.

## Requirements

- Rust toolchain (edition 2024, Rust 1.85+)
- GTK4 and WebKitGTK 6.0 system libraries
- A running `desktop-assistant-daemon` instance

### System libraries

| Distro | Packages |
|--------|----------|
| Arch / CachyOS | `gtk4 webkitgtk-6.0` |
| Fedora | `gtk4-devel webkitgtk6.0-devel` |
| Debian / Ubuntu | `libgtk-4-dev libwebkitgtk-6.0-dev` |

## Build

```sh
cargo build
```

To build without WebKitGTK (Label-based fallback instead of webview):

```sh
cargo build --no-default-features
```

## Install

```sh
just install            # binary + desktop entry + icon
just install-desktop    # desktop entry + icon only
just uninstall-desktop  # remove desktop entry and icon
```

## Run

```sh
adele-gtk
```

### CLI options

| Flag | Env var | Description |
|------|---------|-------------|
| `--transport` | `ADELIE_GTK_TRANSPORT` | `ws` or `dbus`. `dbus` forces a connection to the local daemon, overriding the saved startup profile. |
| `--ws-url` | `ADELIE_GTK_WS_URL` | WebSocket URL. Overrides the startup target and bypasses the saved-profile picker. |
| `--ws-subject` | `ADELIE_GTK_WS_SUBJECT` | JWT subject used with `--ws-url` (defaults to `desktop-tui`). |

When `--ws-url` is given (or `--transport dbus`), it overrides the saved
auto-reconnect profile so headless/scripted/remote launches work without a
pre-saved profile; the resulting connection is ephemeral and is not persisted as
the last connection.

## Test

```sh
cargo test
```

## Logging

Telemetry (traces, metrics and logs) goes through the shared
[`adelie-telemetry`](https://github.com/adelie-ai/adelie-telemetry) crate.

**Console.** Always stderr, never stdout — the built-in MCP servers this
client hosts in process use stdout to frame JSON-RPC, and a stray log line
there would corrupt that stream. Silent unless `RUST_LOG` is set; a
desktop app has no console to read on a normal launch, so nothing is printed
by default:

```sh
RUST_LOG=info adele-gtk       # ids, counts, durations, model names — never content
RUST_LOG=debug adele-gtk      # also prompts, assembled context, tool arguments
```

**The `otel` feature.** Off by default; a default build resolves no
`opentelemetry` crate at all. Turn export on with:

```sh
cargo build --features otel
```

With the feature on, traces, metrics and log records export additionally —
console logging keeps working the same way. Configure the destination with
the standard `OTEL_*` environment variables; there are no `adele-gtk`-specific
flags:

| Variable | Effect |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Collector endpoint for all three signals. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Endpoint for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Endpoint for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Endpoint for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc`, `http/protobuf` (default) or `http/json`, for all three. |
| `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_PROTOCOL` | Protocol for one signal. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Headers for all three, as `key=value,key=value`. |
| `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_HEADERS` | Headers for one signal. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in milliseconds, for all three. |
| `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_TIMEOUT` | Timeout for one signal. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_COMPRESSION` | `gzip` or `zstd`, for all three. Per-signal forms exist too. |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, as `key=value,key=value`. |

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
  adele-gtk
```

The metrics summary that a console-only build would otherwise print
periodically is off here — a GUI has nowhere to show it, and the daemon is
where the fleet's aggregate numbers live.

See `adelie-telemetry`'s own README for the full variable reference, the two
transports' certificate-trust differences, and what `--features otel` costs
in build time and binary size.

## Architecture

Shared protocol types and transport clients live in the
[`desktop-assistant`](https://github.com/adelie-ai/desktop-assistant) workspace
under `crates/api-model` and `crates/client-common`. This repo depends on them
via git; `Cargo.lock` pins the revision.

## License

GNU Affero General Public License v3.0 or later (`AGPL-3.0-or-later`).

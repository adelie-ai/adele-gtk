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

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--transport` | `ADELIE_GTK_TRANSPORT` | `ws` | Transport: `ws` or `dbus` |
| `--ws-url` | `ADELIE_GTK_WS_URL` | `ws://127.0.0.1:11339/ws` | WebSocket URL |
| `--ws-jwt` | `ADELIE_GTK_WS_JWT` | | Direct JWT token |
| `--ws-login-username` | `ADELIE_GTK_WS_LOGIN_USERNAME` | | Login username |
| `--ws-login-password` | `ADELIE_GTK_WS_LOGIN_PASSWORD` | | Login password |
| `--ws-subject` | `ADELIE_GTK_WS_SUBJECT` | `desktop-tui` | JWT subject |

## Test

```sh
cargo test
```

## Architecture

Shared protocol types and transport clients live in the
[`desktop-assistant`](https://github.com/adelie-ai/desktop-assistant) workspace
under `crates/api-model` and `crates/client-common`. This repo depends on them
via git; `Cargo.lock` pins the revision.

## License

GNU Affero General Public License v3.0 or later (`AGPL-3.0-or-later`).

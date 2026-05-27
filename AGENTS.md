# Agent Instructions — adele-gtk

Repo-specific conventions for the GTK4 desktop client. Cross-project workflow rules (issue/PR/board sync, parallel worktrees, warnings-are-failures, security review posture, TDD posture) live in the user's memory and are not duplicated here.

## What this repo is

GTK4 + WebKitGTK 6.0 client that talks to `desktop-assistant-daemon` over WebSocket or D-Bus. Shared protocol types come from `adelie-ai/desktop-assistant`'s `api-model` and `client-common` crates pulled in as git dependencies. `Cargo.lock` pins the exact revision.

## Where things live

- `src/main.rs`, `src/window.rs` — entry and root window wiring.
- `src/widgets/` — GTK widgets (chat view, input bar, sidebar, dialogs, etc.). Each widget is its own module; new widgets follow the same `mod.rs`-registers-children pattern.
- `src/webview.rs`, `src/markdown.rs` — message rendering. WebKitGTK is feature-gated (`--no-default-features` gives a Label-based fallback) — anything new that depends on WebKit needs to keep that fallback compilable.
- `src/async_bridge.rs` — the seam between GTK's main-loop callbacks and async transport work. Don't reach for `tokio::spawn` from widget code; route through the bridge so cancellation and error reporting stay centralized.
- `src/credential_store.rs`, `src/oauth.rs` — secret handling. Same posture as the daemon: API keys never appear in logs; `Display` is fingerprint-only.

## GTK conventions

- **Don't block the main loop.** GTK signal handlers run on the main thread. Any IO, daemon call, or long computation goes through `async_bridge` and returns to the main thread via `glib::MainContext::spawn_local` (or the bridge's existing helpers).
- **Property bindings before manual sync.** When two widgets need to track the same state, prefer GTK property bindings / `gtk::Expression` over hand-rolled signal-then-set callbacks. Manual sync drifts.
- **Composite templates for non-trivial widgets.** If a widget owns more than a couple of children, use a composite template (`.ui` file + `#[template_child]`) rather than building the tree imperatively in code.
- **Styles in `style.css`.** Widget-specific styling goes in CSS with a class name applied via `widget.add_css_class(...)`, not inline calls to `set_*`. Keep `style.css` cohesive.

## Shared types & version pinning

`api-model` and `client-common` come from the desktop-assistant repo via git dep. When the daemon's protocol changes, the version bump here is a deliberate update (not an auto-merge), because the TUI / GTK / KDE clients should pick up protocol changes together. If you bump the git rev for `api-model`, mention the corresponding daemon PR in the commit message so the cross-repo coordination is reconstructable later.

## Rust conventions

The desktop-assistant `AGENTS.md` is the canonical Rust style reference for the platform — error handling, async/locking, generics, unsafe, doc comments. This crate follows it. Where this crate diverges (the bridge to GTK's main loop, GTK's Object/Widget patterns), the divergence is documented above.

## Build & install

- `cargo build` — default features (WebKitGTK).
- `cargo build --no-default-features` — Label-based fallback. Keep this compilable.
- `just install`, `just install-desktop`, `just uninstall-desktop` — desktop entry + icon installation.

The `justfile` is the source of truth for install/uninstall recipes.

## Dependency safety

The user-memory security-review rule covers the posture. Repo-specific note: this crate transitively depends on a large native graph (GTK4, WebKitGTK, GIO). When upgrading the WebKit pin in particular, the CVE scan is the part that matters most — the system-library exposure is bigger than for a pure-Rust crate.

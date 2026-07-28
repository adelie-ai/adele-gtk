# Agent Instructions — adele-gtk

Shared standards live in [AGENTS.base.md](AGENTS.base.md), which is generated. This file holds the rules specific to this repo.

Repo-specific conventions for the GTK4 desktop client. The overrides and additions to the base are listed at the end of this file.

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

Base rule 6.1 and the 6.1 override at the end of this file cover the posture. Repo-specific note: this crate transitively depends on a large native graph (GTK4, WebKitGTK, GIO). When upgrading the WebKit pin in particular, the CVE scan is the part that matters most — the system-library exposure is bigger than for a pure-Rust crate.

## Overrides and additions to the shared base

Everything in [AGENTS.base.md](AGENTS.base.md) applies to this repo. This section
records only the points where this repo deliberately differs from the base, or adds a
rule the base does not have.

### 3.1 The gate for this repo (addition)

The `adelie-ai` repos have no CI. The gate is local and the author runs it: `just check`.
Run `just install-hooks` once per clone to put the same gate on pre-push. Warnings are
denied mechanically by the `[lints]` table in `Cargo.toml`, so `cargo build`, `cargo test`,
and `cargo clippy` each hard-fail on a warning.

Run `cargo fmt -p adele-gtk`, never `cargo fmt --all`. This crate path-depends on
`desktop-assistant`, so `--all` reformats the other repo's source.

### 4.3 Branch and pull request - merge when green (override, weaker than the base)

The base opens a pull request and waits for the user. In these repos the merge is delegated:
merge your own pull request as soon as it is green and independently shippable. Green here
means more than a clean build. The gate above passed, the tests cover the new behavior and
not only the absence of a panic, the security pass is done, and the change stands on its own.
Assign `dspadea` with `gh pr edit --add-assignee` and verify it; a review request from the
same account no-ops without an error, so never report a pull request as review-requested.
When in doubt, hold.

### 4.4 Worktrees - the group convention (addition)

Put the worktree at `.worktrees/<repo>/issue-N-slug/` under the group directory, on a branch
that mirrors the slug. Before you run tasks in parallel worktrees, look for shared files,
shared `Cargo.toml` dependency edits, and shared migration ordinals. Serialize the work where
they overlap, and tell each parallel agent the scope it owns.

### 6.1 Dependencies - a high or critical advisory is a hard blocker (override, stricter than the base)

Scan after you add a dependency and before the first build:

1. Add the dependency (`cargo add <crate>`). This writes the lockfile but does not build.
2. Scan the updated lockfile with the `cve-mcp` server's `scan_packages` tool, or with
   `cargo audit`. Pass every (name, version, ecosystem) tuple.
3. A high or critical finding blocks the change. Patch it in the same change, or prove the
   path unreachable and write down why, or file an issue and reference it from the change.
4. Build only after the scan is clean, or after you have accepted the findings in writing.

Never pin around an advisory without a comment or a tracked issue.

### 9.1 Tracker for this project

GitHub Issues on `github.com/adelie-ai/adele-gtk`, together with the shared `adelie-ai` project
board. Manage entries with the `gh` CLI (`gh issue create`, `gh issue list`, `gh issue edit`,
`gh pr create`). The board states in use are In Progress, In Review, and Done.

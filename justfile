default:
    @just --list

# Install binary and desktop entry for the current user
install:
    cargo install --path .
    just install-desktop

# Install only the desktop entry and icon for the current user
install-desktop:
    mkdir -p ~/.local/share/applications
    cp adele-gtk.desktop ~/.local/share/applications/
    mkdir -p ~/.local/share/icons/hicolor/512x512/apps
    cp assets/adele.png ~/.local/share/icons/hicolor/512x512/apps/adele-gtk.png
    update-desktop-database ~/.local/share/applications 2>/dev/null || true

# Install binary, desktop entry, and icon system-wide (requires sudo)
install-system:
    cargo build --release
    sudo install -Dm755 target/release/adele-gtk /usr/local/bin/adele-gtk
    sudo install -Dm644 adele-gtk.desktop /usr/local/share/applications/adele-gtk.desktop
    sudo install -Dm644 assets/adele.png /usr/local/share/icons/hicolor/512x512/apps/adele-gtk.png

# Remove user-local desktop entry and icon
uninstall-desktop:
    rm -f ~/.local/share/applications/adele-gtk.desktop
    rm -f ~/.local/share/icons/hicolor/512x512/apps/adele-gtk.png
    update-desktop-database ~/.local/share/applications 2>/dev/null || true

# --- Local verification ("local CI") -----------------------------------------
# We run these locally instead of GitHub Actions. `install-hooks` wires `check`
# into a git pre-push hook so it runs automatically before every push. fmt/clippy
# are scoped to `-p adele-gtk` because the workspace path-deps desktop-assistant.

# Full local gate: formatting, lints, build, tests (on the pinned toolchain)
check: fmt-check lint build test

# Verify formatting without modifying files (scoped — don't touch the path-dep)
fmt-check:
    cargo fmt -p adele-gtk --check

# Apply formatting (scoped)
fmt:
    cargo fmt -p adele-gtk

# Clippy on this crate; warnings are errors
lint:
    cargo clippy -p adele-gtk --all-targets -- -D warnings

# Build
build:
    cargo build

# Run the test suite (excludes #[ignore] integration tests)
test:
    cargo test

# Real-Secret-Service integration tests (needs a live session bus; mutates + cleans keyring)
test-integration:
    cargo test -- --ignored

# Rebase onto latest origin/main then run the gate (catches clean-rebase-but-broken-build)
premerge:
    git fetch origin
    git rebase origin/main
    just check

# Install git hooks (pre-push runs `just check`). Local config; run once per clone.
install-hooks:
    git config core.hooksPath .githooks
    @echo "pre-push hook active — bypass once with: git push --no-verify"

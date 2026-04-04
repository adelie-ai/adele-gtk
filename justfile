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

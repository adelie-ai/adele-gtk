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
# We run these locally instead of GitHub Actions. `install-hooks` wires `check-all`
# into a git pre-push hook so it runs automatically before every push. fmt/clippy
# are scoped to `-p adele-gtk` because the workspace path-deps desktop-assistant.

# Full local gate, default features: formatting, lints, build, tests, and the
# no-opentelemetry check (epic mcp-core#38 AC2) — on the pinned toolchain.
check: fmt-check lint build test no-opentelemetry-check

# The same gate with the `otel` feature on (epic mcp-core#38, ticket #152):
# proves the OTLP-export build and its tests, not a live export. Verify a
# live export against a real collector by hand — see the README's Logging
# section — when the telemetry wiring itself changes.
check-otel: fmt-check lint-otel build-otel test-otel

# Both configurations. This is what the pre-push hook runs.
check-all: check check-otel

# Verify formatting without modifying files (scoped — don't touch the path-dep)
fmt-check:
    cargo fmt -p adele-gtk --check

# Apply formatting (scoped)
fmt:
    cargo fmt -p adele-gtk

# Clippy on this crate; warnings are errors
lint:
    cargo clippy -p adele-gtk --all-targets -- -D warnings

# Clippy with the `otel` feature on; warnings are errors
lint-otel:
    cargo clippy -p adele-gtk --all-targets --features otel -- -D warnings

# Build
build:
    cargo build

# Build with the `otel` feature on
build-otel:
    cargo build --features otel

# Run the test suite (excludes #[ignore] integration tests)
test:
    cargo test

# Run the test suite with the `otel` feature on
test-otel:
    cargo test --features otel

# Real-Secret-Service integration tests (needs a live session bus; mutates + cleans keyring)
test-integration:
    cargo test -- --ignored

# AC2-equivalent for this repo (epic mcp-core#38, ticket #152): a
# default-feature build must resolve no opentelemetry crate at all.
# `cargo tree --prefix none` lists every dependency name this feature set
# actually resolves, one per line with no tree-drawing prefix, so a match
# here means a default build pulled in the OTLP stack it should not have.
no-opentelemetry-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    # `cargo tree`'s own exit status is checked BEFORE grep runs, on its own
    # line, under `set -e` — a manifest error, a lockfile problem, or a git
    # dependency that fails to resolve must fail this check, not report OK
    # with an empty (because cargo tree produced nothing) hit list.
    tree_output="$(cargo tree --prefix none)"
    # Only this second command may legitimately "fail": grep exits 1 when no
    # line matches, which is the expected, passing case, so `|| true` here
    # covers exactly that and nothing upstream of it.
    hits="$(printf '%s\n' "$tree_output" | grep -i '^opentelemetry' || true)"
    if [ -n "$hits" ]; then
        echo "default build resolves opentelemetry crate(s):" >&2
        echo "$hits" >&2
        exit 1
    fi
    echo "default build resolves no opentelemetry crate — OK"

# Rebase onto latest origin/main then run the gate (catches clean-rebase-but-broken-build)
premerge:
    git fetch origin
    git rebase origin/main
    just check-all

# Install git hooks (pre-push runs `just check-all`). Local config; run once per clone.
install-hooks:
    git config core.hooksPath .githooks
    @echo "pre-push hook active — bypass once with: git push --no-verify"

# --- Built-in MCP servers (da#538) -------------------------------------------
# The compiled-in MCP servers are cargo features: the core set (fileio,
# terminal, tasks, web) is on by default via `builtin-core`; the broad set
# (weather, internet-radio, openstreetmap, geocode, skills) is opt-in via
# `builtin-extras`. These recipes turn that feature arithmetic into one
# argument list, so a build with a different server set is a short command
# instead of a hand-assembled `--no-default-features --features …`.
#
#   just build-with-mcp                    # the default set (core-4)
#   just build-with-mcp weather geocode    # core-4 + weather + geocode
#   just build-with-mcp extras             # core-4 + the whole broad set
#   just build-with-mcp all                # everything compiled in
#   just build-with-mcp only fileio web    # EXACTLY fileio + web
#   just build-with-mcp none               # no built-in servers at all
#
# The same argument grammar applies to run-with-mcp / test-with-mcp /
# lint-with-mcp / release-with-mcp / install-with-mcp. `just mcp-list` prints
# the servers this crate can compile in; `just mcp-args …` prints the cargo
# flags a selection maps to without building anything.

# Features every build needs regardless of the MCP selection. `only` and `none`
# pass `--no-default-features`, which would otherwise also drop `linux` — the
# feature that pulls in webkit6 and the client-common D-Bus transport. Re-adding
# it here keeps an MCP-set change from silently also changing the render backend.
mcp_base_features := "linux"

# Print the cargo flags an MCP selection maps to, without building anything.
mcp-args *SERVERS:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    # Read the available servers out of Cargo.toml rather than hardcoding them,
    # so adding an `mcp-*` feature there is all it takes for these recipes to
    # accept it. Scoped to the `[features]` block on purpose: the `[patch...]`
    # section also has a line starting `mcp-core = `, which is a crate, not a
    # feature, and must not show up as a selectable server.
    avail="$(awk '/^\[features\]/{f=1;next} /^\[/{f=0} f' Cargo.toml \
        | grep -oE '^mcp-[a-z0-9-]+' | sort -u)"
    sel=""
    add() { case ",$sel," in *",$1,"*) ;; *) sel="${sel:+$sel,}$1" ;; esac; }
    replace=0
    first=1
    raw="{{SERVERS}}"
    for tok in $raw; do
      case "$tok" in
        only|none)
          if [ "$first" != 1 ]; then
            echo "mcp-args: '$tok' must be the first argument" >&2
            exit 2
          fi
          replace=1
          ;;
        core)   add builtin-core ;;
        extras) add builtin-extras ;;
        all)    add builtin-core; add builtin-extras ;;
        *)
          case "$tok" in
            radio) f=mcp-internet-radio ;;
            osm)   f=mcp-openstreetmap ;;
            mcp-*) f="$tok" ;;
            *)     f="mcp-$tok" ;;
          esac
          if ! printf '%s\n' "$avail" | grep -qx -- "$f"; then
            {
              echo "mcp-args: unknown MCP server '$tok'"
              echo "available:"
              printf '%s\n' "$avail" | sed 's/^mcp-/  /'
              echo "  (plus the umbrellas: core, extras, all — and none / only)"
            } >&2
            exit 2
          fi
          add "$f"
          ;;
      esac
      first=0
    done
    if [ "$replace" = 1 ]; then
      for b in {{mcp_base_features}}; do add "$b"; done
      printf -- '--no-default-features'
      if [ -n "$sel" ]; then printf -- ' --features %s' "$sel"; fi
      echo
    elif [ -n "$sel" ]; then
      echo "--features $sel"
    else
      echo
    fi

# The servers are `path` deps on sibling repos, so a missing checkout is what an
# otherwise mysterious cargo "failed to read …/Cargo.toml" is really reporting —
# hence the checkout column.
# List the compilable built-in MCP servers, which are on by default, and which are missing.
mcp-list:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    # Same `[features]`-scoped read as mcp-args (see the note there).
    feats="$(awk '/^\[features\]/{f=1;next} /^\[/{f=0} f' Cargo.toml)"
    # Which servers `builtin-core` pulls in. Flattened to one line first so the
    # match works whether the array is written on one line or spread over many;
    # `[^]]*` stops at that array's own closing bracket.
    core_list=" $(printf '%s' "$feats" | tr '\n' ' ' \
        | sed -E 's/.*builtin-core *= *\[([^]]*)\].*/\1/' \
        | grep -oE 'mcp-[a-z0-9-]+' | tr '\n' ' ')"
    echo "built-in MCP servers (feature name / on by default / crate):"
    for f in $(printf '%s\n' "$feats" | grep -oE '^mcp-[a-z0-9-]+' | sort -u); do
      dep="$(printf '%s\n' "$feats" | sed -nE "s/^$f *= *\[.*dep:([A-Za-z0-9_-]+).*/\1/p")"
      path=""
      if [ -n "$dep" ]; then
        path="$(sed -nE "s/^$dep *= *\{.*path *= *\"([^\"]+)\".*/\1/p" Cargo.toml | sed -n 1p)"
      fi
      case "$core_list " in *" $f "*) tag="default" ;; *) tag="opt-in" ;; esac
      note=""
      if [ -n "$path" ] && [ ! -d "$path" ]; then note="   ** sibling crate not checked out: $path"; fi
      printf '  %-16s %-8s %s%s\n' "${f#mcp-}" "$tag" "${dep:-?}" "$note"
    done
    echo
    echo "umbrellas: core (the default set), extras (the broad set), all"
    echo "modifiers: 'only <servers…>' for an exact set, 'none' for no built-ins"
    echo "base features always kept: {{mcp_base_features}}"

# Build with a chosen set of built-in MCP servers (see the grammar above).
build-with-mcp *SERVERS:
    #!/usr/bin/env bash
    set -euo pipefail
    args="$(just mcp-args {{SERVERS}})"
    echo "+ cargo build $args"
    cargo build $args

# Release build with a chosen set of built-in MCP servers.
release-with-mcp *SERVERS:
    #!/usr/bin/env bash
    set -euo pipefail
    args="$(just mcp-args {{SERVERS}})"
    echo "+ cargo build --release $args"
    cargo build --release $args

# Run the gtk client with a chosen set of built-in MCP servers.
run-with-mcp *SERVERS:
    #!/usr/bin/env bash
    set -euo pipefail
    args="$(just mcp-args {{SERVERS}})"
    echo "+ cargo run $args"
    cargo run $args

# Test with a chosen set of built-in MCP servers.
test-with-mcp *SERVERS:
    #!/usr/bin/env bash
    set -euo pipefail
    args="$(just mcp-args {{SERVERS}})"
    echo "+ cargo test $args"
    cargo test $args

# Clippy with a chosen set of built-in MCP servers (warnings are errors).
lint-with-mcp *SERVERS:
    #!/usr/bin/env bash
    set -euo pipefail
    args="$(just mcp-args {{SERVERS}})"
    echo "+ cargo clippy -p adele-gtk --all-targets $args -- -D warnings"
    cargo clippy -p adele-gtk --all-targets $args -- -D warnings

# Mirrors `just install`, which is the default-feature equivalent.
# Install adele-gtk (+ desktop entry) with a chosen set of built-in MCP servers.
install-with-mcp *SERVERS:
    #!/usr/bin/env bash
    set -euo pipefail
    args="$(just mcp-args {{SERVERS}})"
    echo "+ cargo install --path . $args"
    cargo install --path . $args
    just install-desktop

# Cheap insurance when adding a server, or moving one between the core and broad
# sets — a stray `#[cfg]` typically compiles in one combo and breaks another.
# Lint the three feature combos: no built-ins, the default set, everything.
mcp-matrix:
    #!/usr/bin/env bash
    set -euo pipefail
    for sel in none "" all; do
      args="$(just mcp-args $sel)"
      echo "== clippy ${args:-(default features)}"
      cargo clippy -p adele-gtk --all-targets $args -- -D warnings
    done

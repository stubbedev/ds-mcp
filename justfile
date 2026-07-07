# justfile for ds-mcp
# Run `just` to see all available commands.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Default — list recipes.
default:
    @just --list --unsorted

# ─────────────────────────── Build & Test ───────────────────────────

# Build the release binary into ./bin/.
build:
    cargo build --release
    mkdir -p bin
    cp target/release/ds-mcp bin/
    @echo "Built ./bin/ds-mcp"

# Install into ~/.cargo/bin.
install:
    cargo install --path . --locked

# Auto-fix formatting drift.
fmt:
    cargo fmt

# Format, then the full clippy gate.
lint: fmt
    cargo clippy --all-targets -- -D warnings

# Strict read-only gate — same logic CI runs, exposed for local pre-push
# verification. Fails if formatting would change or clippy fires.
lint-check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

# Run everything CI runs as the merge gate.
check: lint test sync-schema

# Enable the pre-commit fmt + clippy gate (git core.hooksPath = .githooks).
install-hooks:
    git config core.hooksPath .githooks
    @echo "pre-commit fmt + clippy gate is now active (bypass with --no-verify)."

# ─────────────────────────── Generated artifacts ───────────────────────────

# Regenerate config.schema.json from the config types. Cheap, runs on every
# `just check`; CI asserts no drift.
sync-schema:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run -q -- gen-schema -o config.schema.json
    if [ -n "$(git status --porcelain config.schema.json)" ]; then
        echo "sync-schema: regenerated config.schema.json"
    else
        echo "sync-schema: schema already in sync"
    fi

clean:
    rm -rf bin/
    cargo clean

# ─────────────────────────── Nix ───────────────────────────

nix-build:
    nix build .#default --print-build-logs

nix-check:
    nix flake check --print-build-logs

# ─────────────────────────── Release ───────────────────────────

release-preview:
    #!/usr/bin/env bash
    set -euo pipefail
    CURRENT=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
    MAJOR=$(echo "$CURRENT" | cut -d. -f1)
    MINOR=$(echo "$CURRENT" | cut -d. -f2)
    PATCH=$(echo "$CURRENT" | cut -d. -f3)
    echo "Current version: $CURRENT"
    echo "  release-major: v$((MAJOR + 1)).0.0"
    echo "  release-minor: v${MAJOR}.$((MINOR + 1)).0"
    echo "  release-patch: v${MAJOR}.${MINOR}.$((PATCH + 1))"

_release-checks:
    #!/usr/bin/env bash
    set -euo pipefail
    BRANCH=$(git rev-parse --abbrev-ref HEAD)
    DEFAULT_BRANCH=$(git rev-parse --abbrev-ref origin/HEAD 2>/dev/null | sed 's|^origin/||' || true)
    # origin/HEAD is unset on fresh clones; rev-parse then echoes "HEAD".
    if [ -z "$DEFAULT_BRANCH" ] || [ "$DEFAULT_BRANCH" = "HEAD" ]; then DEFAULT_BRANCH=master; fi
    if [ "$BRANCH" != "$DEFAULT_BRANCH" ]; then
        echo "Error: not on default branch '$DEFAULT_BRANCH' (currently '$BRANCH')." >&2
        exit 1
    fi
    just check
    if [ -n "$(git status --porcelain)" ]; then
        echo "check produced changes — staging + committing."
        git add -A
        git commit -m "chore: regenerate artifacts for release"
    fi

# Cargo.toml is the single source of truth for the version; the flake reads
# it and the tag mirrors it.
_release bump:
    #!/usr/bin/env bash
    set -euo pipefail
    just _release-checks
    CURRENT=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
    MAJOR=$(echo "$CURRENT" | cut -d. -f1)
    MINOR=$(echo "$CURRENT" | cut -d. -f2)
    PATCH=$(echo "$CURRENT" | cut -d. -f3)
    case "{{bump}}" in
        major) NEW="$((MAJOR + 1)).0.0" ;;
        minor) NEW="${MAJOR}.$((MINOR + 1)).0" ;;
        patch) NEW="${MAJOR}.${MINOR}.$((PATCH + 1))" ;;
        *) echo "unknown bump kind: {{bump}}"; exit 1 ;;
    esac
    sed -i "0,/^version = \".*\"/s//version = \"${NEW}\"/" Cargo.toml
    cargo build -q   # refresh Cargo.lock
    git add Cargo.toml Cargo.lock
    git commit -m "chore: bump to v${NEW}"
    git tag -a "v${NEW}" -m "v${NEW}"
    git push origin HEAD
    git push origin "v${NEW}"
    echo
    echo "Tagged v${NEW}. Watch the release build with: gh run watch"

release-patch: (_release "patch")
release-minor: (_release "minor")
release-major: (_release "major")

# ─────────────────────────── E2E (docker) ───────────────────────────

# Exercise the tools against a throwaway docker MySQL on port 13306.
# The always-on sqlite e2e runs in plain `cargo test`; this covers a real
# network engine locally.
test-e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    name=dsmcp-e2e-mysql
    trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" \
        -e MYSQL_ROOT_PASSWORD=testpw -e MYSQL_DATABASE=demo \
        -p 127.0.0.1:13306:3306 mysql:latest >/dev/null
    echo "waiting for mysql…"
    ready=0
    for i in $(seq 1 45); do
        # An authenticated query is the real readiness signal — mysqladmin ping
        # reports "alive" during init before the root password is applied.
        if docker exec "$name" mysql -uroot -ptestpw -e "SELECT 1" >/dev/null 2>&1; then
            ready=1; break
        fi
        sleep 2
    done
    [ "$ready" = "1" ] || { echo "mysql did not become ready"; exit 1; }
    docker exec "$name" mysql -uroot -ptestpw demo \
        -e "CREATE TABLE IF NOT EXISTS widgets(id INT PRIMARY KEY, name VARCHAR(32));"
    cfg=$(mktemp)
    cat > "$cfg" <<'JSON'
    {"sources":{"demo":{"engine":"mysql","host":"127.0.0.1","port":13306,"user":"root","password":"testpw","database":"demo","readonly":true}}}
    JSON
    cargo build
    { printf '%s\n' \
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}' \
        '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
        '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_tables","arguments":{"source":"demo"}}}'; sleep 2; } \
    | target/debug/ds-mcp serve --config "$cfg" 2>/dev/null | grep -q 'widgets' \
        && echo "e2e OK" || { echo "e2e FAILED"; exit 1; }

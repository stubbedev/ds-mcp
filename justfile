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

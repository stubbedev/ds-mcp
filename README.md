# DataStore MCP

One MCP server for many databases. `ds-mcp` exposes named data sources —
MySQL/MariaDB, PostgreSQL, SQLite, DuckDB, SQL Server, ClickHouse, MongoDB
and Redis — to MCP clients over stdio or streamable HTTP, behind a single
unified tool surface.

## Install

```sh
brew install stubbedev/ds-mcp/ds-mcp # macOS / Linux
cargo install --path . --locked     # from a checkout (or: just install)
nix build .#default                 # via the flake
```

Prebuilt binaries for linux/macos/windows are attached to
[GitHub releases](../../releases); an AUR PKGBUILD lives in
[packaging/aur](packaging/aur).

## Configure

Global config lives at `~/.config/ds-mcp/config.json` (or pass `--config`).
See [config.example.json](config.example.json); the full reference is the
generated [config.schema.json](config.schema.json).

```json
{
  "sources": {
    "app": {
      "engine": "mysql",
      "description": "Local dev database; safe to read and write.",
      "host": "127.0.0.1",
      "user": "root",
      "password": "${DB_PASSWORD}",
      "database": "app"
    }
  }
}
```

Per source: `engine` (`mysql` | `mariadb` | `postgres` | `sqlite` | `duckdb` |
`mssql` | `clickhouse` | `redis` | `mongodb`), discrete `host`/`port`/`user`/`password`/`database` fields or a
full `dsn` (alias `uri`), `readonly`, a `description` the model uses to pick
the right source, `path` for sqlite/duckdb files, and `default_database` for
mongo. Everything defaults sanely: a bare `{"engine": "postgres"}` connects
to localhost on the default port. In per-repo `.ds-mcp.json` files, relative
paths (`path`, ssh key files) resolve against the config file's directory.

Each source can reach its database through an `ssh` tunnel or a `docker`
container:

```json
"ssh":    { "host": "bastion.example.com", "user": "deploy" }
"docker": { "container": "myapp-postgres-1" }
```

`ssh` tunnels combine with `dsn` too (the dsn's host is dialed through the
tunnel). Host keys are verified against `~/.ssh/known_hosts` (override with
`known_hosts_file`); auth tries `identity_file`, then the ssh-agent, then
`password` — with nothing configured, the agent and `~/.ssh` default keys
are tried automatically. `docker` dials the container's published port (or
the container IP for unpublished ones); `port` picks the in-container port
when it isn't the engine default. `${ENV_VAR}` references in secret-bearing
fields (`password`, `dsn`, ssh `password`/`passphrase`) are expanded at load
time from the process environment, falling back to a `.env` file next to the
config (real env vars win). So a repo can commit `.ds-mcp.json` with
`"password": "${DB_PASSWORD}"` and keep the value in a git-ignored `.env`
beside it. Tunneled mongo sources are forced to `directConnection` — point
the URI at one reachable host.

### Per-workspace sources (roots)

A `.ds-mcp.json` file at an MCP client's workspace root overrides the global
config for that client, so one server process can serve several projects each
with their own sources. With no global config at all the server runs in
roots-only mode. A trusted proxy can also inject roots per request via the
`X-Mcp-Roots` header (comma-separated `file://` URIs or absolute paths).

## Run

```sh
ds-mcp serve                          # stdio (default)
ds-mcp serve -t http                  # streamable HTTP on http.addr (default 127.0.0.1:7100)
ds-mcp serve --read-only              # force every source read-only
ds-mcp gen-schema                     # regenerate config.schema.json
```

Claude Code registration:

```sh
claude mcp add datastore -- ds-mcp serve
```

HTTP mode serves the MCP endpoint at `http.path` (default `/mcp`) plus a
`/healthz`. There is no auth layer: keep the default loopback bind or put an
authenticating proxy in front. The Host allowlist blocks DNS-rebinding by
default; `http.allowed_origins` extends it, `["*"]` disables it for proxied
setups.

## Tools

Five tools cover every engine — the payload is engine-native and each tool
dispatches internally:

| tool | what |
|---|---|
| `list_sources` | list configured sources: name, engine, description, readonly, remote |
| `ping` | check connectivity + latency for a source |
| `schema` | introspect: list tables/collections (or keyspace); with `table`, describe columns/indexes (or a key's type + ttl) |
| `query` | run a **read** |
| `execute` | run a **write** (refused on read-only sources) |

`query` and `execute` take a `query` argument in the source's native form:

| engine | `query` payload | example |
|---|---|---|
| SQL (mysql/mariadb/postgres/sqlite/duckdb/mssql/clickhouse) | a statement string | `"SELECT * FROM t WHERE id = 1"` |
| MongoDB | a runCommand document (Extended JSON) | `{"find": "t", "filter": {"id": 1}}` |
| Redis | a command array | `["GET", "k"]` |

Read/write is enforced per engine: SQL through a real parser (only
SELECT/SHOW/DESCRIBE/EXPLAIN pass `query`); MongoDB by command name (find /
aggregate / count / ... are reads; insert / update / delete / createIndexes /
drop / ... are writes, and aggregate with `$out`/`$merge` counts as a write);
Redis by a read-command allowlist. Anything that writes is rejected from
`query` and pointed at `execute`. `execute` runs the payload verbatim on
writable sources — no implicit guards, so a `DELETE` without a filter deletes
everything, exactly as that engine's shell would.

Reads are capped at `limit` rows/documents (default 1000) with a
`truncated`/`has_more` flag; paginate with LIMIT/OFFSET (SQL) or skip/limit
(mongo). MongoDB find/aggregate results are normalized to
`{documents, count, has_more}`; other commands return their raw result.
Results come back as text and as MCP `structuredContent`. Each source also
exposes an MCP resource `ds://<source>/schema`.

Index/collection creation is just a write: `execute` with
`CREATE INDEX ...` (SQL) or `{"createIndexes": ...}` / `{"create": ...}`
(mongo).

## Develop

```sh
just            # list recipes
just check      # the CI gate: lint, tests, schema drift
just test-e2e   # docker mysql smoke test (sqlite e2e runs in plain cargo test)
just install-hooks
```

`config.schema.json` is generated from the config types — edit
`src/config.rs`, then `just sync-schema`. Releases: `just release-patch`
(or `-minor` / `-major`) bumps Cargo.toml, tags, and pushes; the Release
workflow builds binaries for all platforms and publishes them.

## License

MIT

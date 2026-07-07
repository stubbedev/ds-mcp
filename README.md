# DataStore MCP

One MCP server for many databases. `ds-mcp` exposes named data sources —
MySQL/MariaDB, PostgreSQL, SQLite, SQL Server and MongoDB — to MCP clients
over stdio or streamable HTTP, behind a single unified tool surface.

It replaces the separate [mysql-mcp](https://github.com/stubbedev/mysql-mcp)
and [mongodb-mcp](https://github.com/stubbedev/mongodb-mcp) servers.

## Install

```sh
cargo install --path . --locked   # or: just install
nix build .#default               # or via the flake
```

Prebuilt binaries for linux/macos/windows are attached to
[GitHub releases](../../releases).

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

Per source: `engine` (`mysql` | `mariadb` | `postgres` | `sqlite` | `mssql` |
`mongodb`), discrete `host`/`port`/`user`/`password`/`database` fields or a
full `dsn` (alias `uri`), `readonly`, a `description` the model uses to pick
the right source, `path` for sqlite files, `default_database` for mongo, and
an optional `ssh` tunnel:

```json
"ssh": {
  "host": "bastion.example.com",
  "user": "deploy",
  "identity_file": "~/.ssh/id_ed25519"
}
```

Host keys are verified against `~/.ssh/known_hosts` (override with
`known_hosts_file`); auth tries `identity_file`, then the ssh-agent
(`use_agent`), then `password`. `${ENV_VAR}` references in secret-bearing
fields are expanded at load time. Tunneled mongo sources are forced to
`directConnection` — point the URI at one reachable host.

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

Every tool takes a `source` argument naming a configured source; errors come
back as tool results the model can read. Reads are capped at 1000 rows/docs
with a truncated flag.

| | |
|---|---|
| any engine | `list_sources`, `list_databases` |
| SQL | `list_tables`, `describe_table`, `read_query`, `write_query`, `explain_query` |
| MongoDB | `find`, `aggregate`, `count`, `distinct`, `list_collections`, `list_indexes`, `insert`, `update`, `delete`, `create_index`, `drop_index`, `create_collection`, `drop_collection` |

`read_query` accepts a single SELECT/SHOW/DESCRIBE/EXPLAIN statement,
enforced with a real SQL parser — anything else (or anything unparseable) is
rejected and pointed at `write_query`, which in turn refuses read-only
sources. Mongo write tools do the same; `aggregate` pipelines containing
`$out`/`$merge` count as writes, and `delete` refuses an empty filter.
MongoDB filter/document arguments are Extended JSON, so `{"$oid": ...}` and
friends work.

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

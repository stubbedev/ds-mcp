# DataStore MCP

One MCP server for many databases. `ds-mcp` exposes named data sources —
MySQL/MariaDB, PostgreSQL, SQLite, DuckDB, SQL Server, ClickHouse, MongoDB,
Redis/Valkey, Elasticsearch/OpenSearch and Qdrant — to MCP clients over stdio
or streamable HTTP, behind a single unified tool surface.

## Install

```sh
brew install stubbedev/ds-mcp/ds-mcp # macOS / Linux
cargo install --path . --locked     # from a checkout (or: just install)
nix build .#default                 # via the flake
```

Prebuilt binaries for linux/macos/windows are attached to
[GitHub releases](../../releases); an AUR PKGBUILD lives in
[packaging/aur](packaging/aur).

For Claude Desktop, grab the `.mcpb` bundle for your platform from the same
release and open it (or Settings → Extensions → Advanced → install from file).
It carries the binary, so there is no client JSON to edit and no PATH to fix —
the connection is filled in from the extension's settings form. See
[Claude Desktop](#claude-desktop-mcpb) below.

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
`mssql` | `clickhouse` | `redis` | `valkey` | `mongodb` | `elasticsearch` |
`opensearch` | `qdrant`), discrete `host`/`port`/`user`/`password`/`database`
fields or a full `dsn` (alias `uri`), `readonly`, `pii`, a `description` the
model uses to pick the right source, `path` for sqlite/duckdb files,
`default_database` for mongo, and `api_key` for elasticsearch/opensearch/qdrant. Everything defaults sanely: a bare `{"engine": "postgres"}` connects
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

### Claude Desktop (.mcpb)

<a id="claude-desktop-mcpb"></a>The `.mcpb` bundle has no config file to edit,
so the extension's settings form maps onto `DS_MCP_*` environment variables
instead — every option a config file allows:

| variable | config field |
|---|---|
| `DS_MCP_ENGINE` | `engine` — the switch: without it no env source is built |
| `DS_MCP_SOURCE_NAME` | the source's name (default `db`) |
| `DS_MCP_DESCRIPTION`, `DS_MCP_READONLY` | `description`, `readonly` |
| `DS_MCP_DSN`, `DS_MCP_HOST`, `DS_MCP_PORT`, `DS_MCP_USER`, `DS_MCP_PASSWORD`, `DS_MCP_DATABASE` | the connection |
| `DS_MCP_PATH`, `DS_MCP_API_KEY`, `DS_MCP_DEFAULT_DATABASE`, `DS_MCP_CONNECT_TIMEOUT_SECONDS` | `path`, `api_key`, `default_database`, `connect_timeout_seconds` |
| `DS_MCP_PII`, `DS_MCP_PII_COLUMNS`, `DS_MCP_PII_VALUES`, `DS_MCP_PII_MODE` | `pii` (bool alone, or the object form once a list is set) |
| `DS_MCP_SSH_*` | `ssh.host`/`port`/`user`/`password`/`identity_file`/`passphrase`/`use_agent`/`known_hosts_file` |
| `DS_MCP_DOCKER_CONTAINER`, `DS_MCP_DOCKER_PORT` | `docker` |
| `DS_MCP_QUERY_TIMEOUT_SECONDS`, `DS_MCP_READ_ONLY` | `query_timeout_seconds`, and `--read-only` for every source |
| `DS_MCP_CONFIG` | a config file path, same as `--config` |
| `DS_MCP_SOURCES` | a whole `sources` object as inline JSON |

More than one database: number the slot. `DS_MCP_2_ENGINE`,
`DS_MCP_2_HOST`, … build a second source (named `db2` unless
`DS_MCP_2_SOURCE_NAME` says otherwise), `DS_MCP_3_*` a third, and so on with
no upper bound — every per-source variable above takes a number (the last
four rows are server-wide and do not). The bundle's form ships
four numbered slots on top of the first source; past that, use a config file
or `DS_MCP_SOURCES`. Everything found is merged into one list, and a later
slot wins a name clash.

Blank counts as unset, so untouched fields fall away. Lists are
comma-separated (`[]` for an empty one) and booleans take
`true`/`false`/`1`/`0`. Env sources are merged into a config file's sources
when both are present, winning on a name clash. None of this is
Desktop-specific — the same variables work for any client that can set an
environment.

Build a bundle locally with `just bundle`; the template lives in
[packaging/mcpb](packaging/mcpb).

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
| Redis/Valkey | a command array | `["GET", "k"]` |
| Elasticsearch/OpenSearch/Qdrant | a REST request document (`method` defaults to GET) | `{"method": "GET", "path": "/t/_search", "body": {"query": {"match_all": {}}}}` |

Read/write is enforced per engine: SQL through a real parser (only
SELECT/SHOW/DESCRIBE/EXPLAIN pass `query`); MongoDB by command name (find /
aggregate / count / ... are reads; insert / update / delete / createIndexes /
drop / ... are writes, and aggregate with `$out`/`$merge` counts as a write);
Redis/Valkey by a read-command allowlist; Elasticsearch/OpenSearch/Qdrant by
HTTP method + path (GET/HEAD and read POSTs — ES `_search`/`_count`/..., Qdrant
`points/search`/`scroll`/`count`/`query` — are reads; PUT / DELETE and mutating
POSTs like `_bulk`/`_doc` or `points/delete`/`points` upserts are writes).
Anything that writes is rejected from
`query` and pointed at `execute`. `execute` runs the payload verbatim on
writable sources — no implicit guards, so a `DELETE` without a filter deletes
everything, exactly as that engine's shell would.

A `readonly` source (or `--read-only`) refuses `execute` outright, and its
`query` classifier is allowlist / default-deny — an unknown or unparseable
payload is treated as a write, and the classifier inspects the exact
statement/command/normalized-path the engine will run (so tricks like a
data-modifying CTE, `EXPLAIN ANALYZE <write>`, a `$merge` nested in a
sub-pipeline, `CONFIG SET`, or a `../` path segment cannot slip a write onto
the read path). For defense in depth the readonly flag is also pushed down to
the connection where the engine supports it — sqlite/duckdb open the file
read-only, postgres sets `default_transaction_read_only`, clickhouse sets
`readonly=2` — so even a side-effecting function the parser can't see is
refused. mysql/mariadb/mssql have no equivalent per-session switch here; for a
hard guarantee on those, point the source at a read-only database user.

### Redacting sensitive columns

`readonly` protects the database from the model; `pii` protects the data
subjects from the client. It redacts matching column/field values on the way
out, before they reach the transcript and whatever the client logs or ships
upstream:

```json
"prod": { "engine": "postgres", "readonly": true, "pii": true }
```

`"pii": true` is the short form, and it turns on both halves of the filter.

**Column names.** A built-in glob set covering the obvious ones —
`*password*`, `*secret*`, `*token*`, `*api_key*`, `*email*`, `*phone*`,
`*ssn*`, `*credit_card*`, `*iban*`, `*address*`, `*first_name*`,
`*date_of_birth*`, ... — each matching value replaced with `"[redacted]"`.

**Values.** Column names only catch the columns someone named honestly. A
`note`, a `payload`, a Redis string or a Mongo document with invented keys
gets scanned for values that are self-evidently sensitive whatever the field
is called, and only the matched span is masked:

| detector | matches | checked by |
| --- | --- | --- |
| `email` | `alex@example.com` | — |
| `credit_card` | contiguous, `4-4-4-4`, Amex `4-6-5` | Luhn |
| `iban` | `DE89 3704 0044 0532 0130 00` | ISO 13616 mod-97 |
| `ssn` | `219-09-9999` (separators required) | SSA area/group/serial blocks |
| `phone` | E.164 only, `+45 12 34 56 78` | length + country code |
| `jwt` | `eyJ...` | — |
| `private_key` | `-----BEGIN … PRIVATE KEY-----` | — |
| `aws_key` | `AKIA…`/`ASIA…`/`AIDA…`/`AROA…` | — |

Every format with a check digit is verified rather than matched by shape, and
the shape-only patterns are kept narrow, because a false positive here quietly
destroys real data on every query: a bare run of digits, a UUID, a timestamp,
a local phone number and a version string are all left alone.

The object form overrides any part of that:

```json
"prod": {
  "engine": "postgres",
  "readonly": true,
  "pii": { "columns": ["email", "*_ssn", "users.address_*"],
           "values": ["credit_card", "iban"],
           "mode": "hash" }
}
```

`columns` are case-insensitive globs (`*` = any run) matched against the
returned column/field name, optionally qualified `table.column` — a qualified
pattern only fires when the payload names that relation (parsed out of the SQL,
the Mongo command's collection, or the REST path). `values` selects detectors
by name; `[]` turns value scanning off and leaves the column globs. Omitting
either keeps its default. `mode` is `redact` (default), `hash` (a stable
`sha256:` prefix, so equal values still group and join) or `drop` (the
column/field disappears from the result). NULL stays NULL in every mode; a
value detected *inside* a longer string is redacted or hashed in place, since
`drop` cannot remove half a sentence.

The filter runs on every result of every tool, for every engine — tabular rows
by column position, documents by field name at any depth (including JSON/JSONB
cells inside a SQL result), and string values everywhere. `list_sources`
reports `pii: true`, and `schema` with a `table` adds a `pii` column to the
described columns, so the model knows which fields come back masked instead of
re-querying to find out.

Two deliberate limits, both visible in the results rather than hidden: it
filters on the way out only, so a `WHERE email = '...'` predicate the model
wrote blind still works, and a server-side aggregate over a redacted column
(`COUNT(DISTINCT email)`) is computed by the engine and comes back untouched.

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

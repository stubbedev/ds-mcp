# DataStore MCP

One MCP server for many databases. `ds-mcp` exposes named data sources to MCP
clients over stdio or streamable HTTP, behind a single unified tool surface.

| engines |
|---|
| SQL — MySQL/MariaDB, PostgreSQL, SQLite, DuckDB, SQL Server, ClickHouse |
| documents — MongoDB |
| key-value — Redis/Valkey |
| search/vectors — Elasticsearch/OpenSearch, Qdrant |

## Install

```sh
brew install stubbedev/tap/ds-mcp # macOS / Linux
cargo install --path . --locked   # from a checkout (or: just install)
nix build .#default               # via the flake
```

Prebuilt binaries for linux/macos/windows are attached to
[GitHub releases](../../releases); an AUR PKGBUILD lives in
[packaging/aur](packaging/aur). For Claude Desktop, install the platform
`.mcpb` from the same release (Settings → Extensions → Advanced → install from
file) — see [Claude Desktop](#claude-desktop-mcpb).

## Configure

Global config lives at `~/.config/ds-mcp/config.json` (works on every
platform; the macOS/Windows platform directories are picked up too), or pass
`--config`. A `.ds-mcp.json` at a client's workspace root overrides the global
config for that client; with no global config the server runs roots-only. A
trusted proxy can inject roots per request via `X-Mcp-Roots`. See
[config.example.json](config.example.json); the full reference is the
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
    },
    "search": {
      "engine": "elasticsearch",
      "description": "Search cluster. Read-only.",
      "uri": "https://es.example.com:9200",
      "api_key": "${ES_API_KEY}",
      "readonly": true
    }
  }
}
```

| source field | what |
|---|---|
| `engine` | `mysql` `mariadb` `postgres` `sqlite` `duckdb` `mssql` `clickhouse` `redis` `valkey` `mongodb` `elasticsearch` `opensearch` `qdrant` |
| connection | discrete `host`/`port`/`user`/`password`/`database` or a full `dsn` (alias `uri`); `path` for sqlite/duckdb files; `api_key` for ES/OpenSearch/Qdrant; `default_database` for mongo |
| `readonly` | refuse `execute`, gate `query` through the read classifier ([below](#readwrite-enforcement)) |
| `pii` | redact matching values on the way out ([below](#redacting-sensitive-columns)) |
| `description` | shown to the model so it picks the right source |
| `ssh` | dial through a bastion — `{ "host": "bastion.example.com", "user": "deploy" }`, optional `port`, `password`, `identity_file`, `passphrase`, `use_agent`, `known_hosts_file` (default `~/.ssh/known_hosts`) |
| `docker` | dial a container — `{ "container": "myapp-postgres-1" }`; `port` selects the in-container port when it isn't the engine default |
| `connect_timeout_seconds` / `query_timeout_seconds` | timeouts |

Everything defaults sanely — a bare `{"engine": "postgres"}` connects to
localhost on the default port. Relative `path`/ssh-key paths in a
`.ds-mcp.json` resolve against the config file's directory. `${ENV_VAR}` in
secret-bearing fields (`password`, `dsn`, ssh `password`/`passphrase`)
expands at load time from the process environment, falling back to a `.env`
next to the config file (real env vars win) — so a repo can commit
`.ds-mcp.json` with `"password": "${DB_PASSWORD}"` and git-ignore the `.env`
beside it. `ssh` combines with `dsn` too (the dsn's host is dialed through the
tunnel); auth tries `identity_file`, then the ssh-agent, then `password`, with
nothing configured the agent and `~/.ssh` default keys are tried. Tunneled
mongo sources are forced to `directConnection`.

### Claude Desktop (.mcpb)

<a id="claude-desktop-mcpb"></a>The bundle has no config file; the extension's
settings form maps onto `DS_MCP_*` variables. Every config field is available,
and none of it is Desktop-specific — any client that can set an environment
works the same:

| variable | config field |
|---|---|
| `DS_MCP_ENGINE` | `engine` — without it no env source is built |
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

More sources: number the slot — `DS_MCP_2_ENGINE`, `DS_MCP_2_HOST`, … build a
second source (named `db2` unless `DS_MCP_2_SOURCE_NAME` says otherwise), and
so on without bound (the last four rows above are server-wide). The bundle's
form ships four numbered slots. Blank counts as unset; lists are
comma-separated; booleans take `true`/`false`/`1`/`0`; env sources merge over
a config file's sources on a name clash. Build a bundle locally with
`just bundle`; the template lives in [packaging/mcpb](packaging/mcpb).

## Run

```sh
ds-mcp serve                          # stdio (default)
ds-mcp serve -t http                  # streamable HTTP on http.addr (default 127.0.0.1:7100)
ds-mcp serve --read-only              # force every source read-only
ds-mcp gen-schema                     # regenerate config.schema.json
claude mcp add datastore -- ds-mcp serve
```

HTTP serves MCP at `http.path` (default `/mcp`) plus `/healthz`. There is no
auth layer: keep the default loopback bind or put an authenticating proxy in
front. The Host allowlist blocks DNS-rebinding by default;
`http.allowed_origins` extends it, `["*"]` disables it for proxied setups.

## Tools

Five tools cover every engine; the payload is engine-native and each tool
dispatches internally:

| tool | what |
|---|---|
| `list_sources` | configured sources: name, engine, description, readonly, remote |
| `ping` | connectivity + latency for a source |
| `schema` | list tables/collections/indices; with `table`: columns, indexes, mappings, a key's type + ttl |
| `query` | run a **read** — writes are refused here and pointed at `execute` |
| `execute` | run a **write** — verbatim, no implicit guards; refused on read-only sources |

`query` and `execute` take a `query` argument in the source's native form:

| engine | payload | example |
|---|---|---|
| SQL | statement string | `"SELECT * FROM t WHERE id = 1"` |
| MongoDB | runCommand document (Extended JSON) | `{"find": "t", "filter": {"id": 1}}` |
| Redis/Valkey | command array | `["GET", "k"]` |
| Elasticsearch/OpenSearch/Qdrant | REST request document (`method` defaults to GET) | `{"method": "GET", "path": "/t/_search", "body": {"query": {"match_all": {}}}}` |

Index/collection creation is just a write: `execute` with `CREATE INDEX ...`
(SQL) or `{"createIndexes": ...}` / `{"create": ...}` (mongo).

### Read/write enforcement

Every `query` passes a default-deny per-engine read classifier: anything not
positively recognized as a read is refused.

| engine | a `query` is a read when... |
|---|---|
| SQL | the parsed statement tree holds only SELECT/SHOW/DESCRIBE/EXPLAIN — a data-modifying CTE, `SELECT ... INTO` in a UNION arm, or `EXPLAIN ANALYZE <write>` is refused |
| MongoDB | the command name is a read (`find`, `aggregate`, `count`, ...); `insert`/`update`/`delete`/`drop`/... are writes, and `aggregate` with `$out`/`$merge` — including nested in `$facet`/`$unionWith`/`$lookup` sub-pipelines — counts as a write |
| Redis/Valkey | the command is on a read allowlist (`CONFIG GET` yes, `CONFIG SET` no) |
| Elasticsearch/OpenSearch | verb × endpoint: GET/HEAD any path; POST only the read endpoints (`_search`, `_search/scroll`, `_search/template`, `_msearch`, `_async_search`, `_eql/search`, `_eql/async_search`, `_count`, `_explain`, `_validate/query`, `_field_caps`, `_mget`, `_termvectors`, `_analyze`, `_rank_eval`, `_search_shards`, `_terms_enum`, `_sql`, templates via `_render`, `_pit`, ...); PUT/DELETE/PATCH and every other POST (`_bulk`, `_doc`, `_update_by_query`, `_delete_by_query`, `_reindex`, `_refresh`, `_forcemerge`, `_aliases`, `_close`, `_scripts/painless/_execute`, ...) are refused |
| Qdrant | same verb rule, with `points/search`/`scroll`/`count`/`query`/... as read ops and upserts/`payload`/`vectors`/`index` refused |

The classifiers inspect the exact payload the engine will run — the
normalized path after `../` resolution, the whole SQL statement tree, the full
aggregate pipeline — and match path/command segments, never substrings, so an
index named `my_search` or a nested `$merge` cannot spoof a read. A missing
method or unparseable path is refused too.

A `readonly` source (or `--read-only`) refuses `execute` outright. For defense
in depth the flag is also pushed down to the connection where the engine
supports it — sqlite/duckdb open the file read-only, postgres sets
`default_transaction_read_only`, mysql/mariadb open every pooled session with
`SET SESSION TRANSACTION READ ONLY` (MySQL 5.6.5+/MariaDB 10.0+), clickhouse
sets `readonly=2` — so a side-effecting function the parser can't see is
refused too. mssql has no per-session switch; for a hard guarantee on it,
point the source at a read-only database user.

### Redacting sensitive columns

`readonly` protects the database from the model; `pii` protects data subjects
from the client — matching values are redacted on the way out, before they
reach the transcript:

```json
"prod": {
  "engine": "postgres",
  "readonly": true,
  "pii": { "columns": ["email", "*_ssn", "users.address_*"],
           "values": ["credit_card", "iban"],
           "mode": "hash" }
}
```

`"pii": true` is the short form: the built-in column glob set (`*password*`,
`*secret*`, `*token*`, `*api_key*`, `*email*`, `*phone*`, `*ssn*`, ...) plus
every value detector.

| `pii` field | what |
|---|---|
| `columns` | case-insensitive globs over returned column/field names, optionally qualified `table.column` — a qualified pattern only fires when the payload names that relation (parsed from the SQL, the mongo collection, or the REST path); omit to keep the built-ins |
| `values` | value detectors scanned in any string: `email` · `credit_card` (Luhn) · `iban` (ISO 13616 mod-97) · `ssn` (SSA blocks) · `phone` (E.164) · `jwt` · `private_key` · `aws_key`; `[]` turns scanning off; omit to keep all |
| `mode` | `redact` → `"[redacted]"` (default) · `hash` → stable `sha256:` prefix · `drop` → the field disappears |

Formats with a check digit are verified, not shape-matched, and shape-only
patterns stay narrow: a bare run of digits, a UUID, a timestamp, a local phone
number or a version string is left alone, because a false positive quietly
destroys real data on every query.

`hash` is keyed only on the value, so equal inputs hash equal in every source,
process and run — hash `users.email` in Postgres and `contacts.email` in Mongo
and the model can join customers across both without either address reaching
the transcript (set `"mode": "hash"` on every source you want to join). Two
caveats: matching is byte-exact (`A@B.com` ≠ `a@b.com` — normalize on the way
in), and a hash is a pseudonym, not a secret — low-entropy values stay
guessable from hash + dictionary.

The filter runs on every result of every tool, for every engine — rows by
column position, documents by field name at any depth (JSON/JSONB cells
included), string values everywhere. `list_sources` reports `pii: true`, and
`schema` with a `table` adds a `pii` column to the described columns. Two
deliberate limits, visible in results: it filters on the way out only, so a
`WHERE email = '...'` the model wrote blind still works, and a server-side
aggregate over a redacted column is computed by the engine and comes back
untouched.

## Limits

Reads are capped at `limit` rows/documents (default 1000) with a
`truncated`/`has_more` flag; paginate with LIMIT/OFFSET (SQL) or skip/limit
(mongo). Mongo find/aggregate results come back as `{documents, count,
has_more}` — uniform documents project to SQL-style `{columns, rows}`. Values
too large for one result are cut in place with a `read_more` marker; call the
`read_more` tool with its embedded arguments to page through. Results return
as text and as MCP `structuredContent`; each source also exposes an MCP
resource `ds://<source>/schema`.

## Develop

```sh
just check      # the CI gate: fmt, clippy, tests, schema drift
just test-e2e   # docker mysql smoke test (sqlite e2e runs in plain cargo test)
just install-hooks
```

`config.schema.json` is generated from the config types — edit
`src/config.rs`, then `just sync-schema`. Releases: `just release-patch`
(or `-minor` / `-major`) bumps Cargo.toml, tags, and pushes; the Release
workflow builds binaries for all platforms and publishes them.

## License

MIT

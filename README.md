# Taproot

Auto-generate a GraphQL API from a PostgreSQL database. Point Taproot at your database, and it introspects your schema to produce a fully-featured GraphQL endpoint — queries with filtering, sorting, and Relay-style pagination; create/update/delete mutations; and real-time subscriptions via Postgres NOTIFY.

## Quick start

```bash
cargo install --path crates/taproot-server
```

Create a `taproot.yaml`:

```yaml
database:
  url: "postgres://user:pass@localhost:5432/mydb"
server:
  listen: "0.0.0.0:4000"
  playground: true
```

Run the server:

```bash
taproot -c taproot.yaml
```

Open `http://localhost:4000/graphql` in a browser to explore your API with GraphiQL.

## Features

- **Schema introspection** — tables, views, columns, primary keys, foreign keys, enums, composite types. A column named `id` is exposed as `rowId` (a common convention)
- **Relay pagination** — cursor-based connections with `first`/`after`/`last`/`before`/`offset`
- **Filtering** — a flat `condition` for per-column equality, plus a rich `filter` with `equalTo`, `notEqualTo`, `in`, `notIn`, `lessThan`, `lessThanOrEqualTo`, `greaterThan`, `greaterThanOrEqualTo`, `isNull`, string operators (`includes`, `includesInsensitive`, `startsWith`, `endsWith`, `like`, `likeInsensitive`, ...), array-column membership (`anyEqualTo`, `anyGreaterThan`, ...), and `and`/`or`/`not` combinators
- **Mutations** — auto-generated `create`, `update`, `delete` operations, bulk variants, and nested `create`/`connect` for related records, for tables with single-column primary keys
- **Subscriptions** — real-time change notifications via Postgres `LISTEN`/`NOTIFY`
- **Row-Level Security** — JWT-based auth with RLS passthrough to Postgres
- **Apollo Federation** — expose tables as federated subgraph entities
- **Lattice integration** — embed as a subgraph executor in the Lattice gateway
- **Plugin system** — `on_introspect`, `on_plan`, and `filter_table` hooks, plus `query_fields`/`mutation_fields` to contribute custom root fields backed by SQL
- **Smart comments** — `@omit`, `@name`, `@deprecated` table/column annotations
- **Query limits** — configurable depth and complexity limits to prevent abuse
- **Embeddable** — use as a Rust library via `taproot-core`

## Configuration

See the [full configuration reference](https://docs.omni.dev/products/taproot/configuration) or the example below:

```yaml
database:
  url: "postgres://localhost:5432/mydb"
  pool_size: 20
  schemas: ["public"]
  exclude_tables: ["migrations"]

server:
  listen: "0.0.0.0:4000"
  graphql_path: "/graphql"
  playground: true

schema:
  relay_pagination: true
  default_page_size: 25
  max_page_size: 100
  federation: false
  subscriptions: false
  max_depth: 15
  max_complexity: 1000

auth:
  rls: false
  role_claim: "pg_role"
  default_role: "anonymous"
  jwt_secret: null

subscriptions:
  enabled: false
  notify_channel: "graphql_subscriptions"
```

## Development

Requires a nightly Rust toolchain (pinned in `rust-toolchain.toml`) and, for the integration tests, a reachable PostgreSQL instance.

```bash
# Unit + integration suite (integration tests need a database; they create and
# drop their own schemas, so point this at a throwaway DB — when unset they skip)
export DATABASE_URL="postgres://postgres:postgres@localhost:5432/taproot_test"
cargo test

# Lint
cargo clippy --all-targets

# Build the server binary
cargo build --release -p taproot-server
```

## Documentation

Full documentation is available at [docs.omni.dev/products/taproot](https://docs.omni.dev/products/taproot).

## License

Apache-2.0

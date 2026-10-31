<div align="center">
  <img src="/assets/logo.png" width="100" />

  <h1 align="center">Taproot</h1>

[Website](https://taproot.omni.dev) | [Docs](https://docs.omni.dev/products/taproot) | [Feedback](https://backfeed.omni.dev/workspaces/omni/projects/taproot) | [Discord](https://discord.gg/omnidotdev) | [X](https://x.com/omnidotdev) | [Threads](https://www.threads.com/@omnidotdev)

</div>

**Taproot** is a pure Rust library and server that auto-generates a full GraphQL API from a PostgreSQL database. Point it at your database and it introspects tables, columns, relationships, and enums at startup to serve a dynamic, queryable schema with zero codegen.

## Installation

| Platform | Channel | Command / Link |
| --- | --- | --- |
| All | [GitHub Releases](https://github.com/omnidotdev/taproot/releases) | Download the `taproot` server binary |
| All | [crates.io](https://crates.io/crates/taproot-server) | `cargo install taproot-server` |
| Library | [crates.io](https://crates.io/crates/taproot-core) | `taproot-core = "0.1"` in `Cargo.toml` |

### Build from source

```bash
git clone https://github.com/omnidotdev/taproot
cd taproot
cargo build --release          # binary at target/release/taproot
```

## Quick Start

Create a `taproot.yaml`:

```yaml
database:
  url: "postgres://user:pass@localhost:5432/mydb"
server:
  listen: "0.0.0.0:4000"
  playground: true
```

Run the server and open `http://localhost:4000/graphql` to explore the API in GraphiQL:

```bash
taproot -c taproot.yaml
```

## Features

- **Schema introspection** — tables, views, columns, primary keys, foreign keys, enums, composite types (a column named `id` is exposed as `rowId`)
- **Relay pagination** — cursor connections with `first`/`after`/`last`/`before`/`offset`
- **Filtering** — a flat `condition` (equality) plus a rich `filter` with comparison, pattern, and array operators and `and`/`or`/`not`
- **Mutations** — `create`/`update`/`delete`, bulk variants, and nested `create`/`connect` for related records
- **Subscriptions** — real-time change notifications over Postgres `LISTEN`/`NOTIFY`
- **Row-Level Security** — JWT-based auth with RLS passthrough to Postgres
- **Apollo Federation** — expose tables as federated subgraph entities
- **Plugin system** — `on_introspect`, `on_plan`, `filter_table`, plus `query_fields`/`mutation_fields` for custom SQL-backed root fields
- **Smart comments** — `@omit`, `@name`, `@deprecated` annotations
- **Embeddable** — use as a Rust library via `taproot-core`

See the [configuration reference](https://docs.omni.dev/products/taproot) for all options.

## Development

### Prerequisites

- [Rust](https://rustup.rs) nightly (pinned in `rust-toolchain.toml`)
- A PostgreSQL instance for the integration tests

### Commands

```bash
# integration tests create and drop their own schemas; point this at a
# throwaway database (tests are skipped when it is unset)
export DATABASE_URL="postgres://postgres:postgres@localhost:5432/taproot_test"
cargo test                     # unit + integration suite
cargo clippy --all-targets     # lint
cargo build --release -p taproot-server
```

## Documentation

Full documentation is at [docs.omni.dev/products/taproot](https://docs.omni.dev/products/taproot).

## License

Apache-2.0

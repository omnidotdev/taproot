<div align="center">
  <img src="/assets/logo.png" width="100" />

  <h1 align="center">Taproot</h1>

[Website](https://taproot.omni.dev) | [Docs](https://docs.omni.dev/products/taproot) | [Feedback](https://backfeed.omni.dev/workspaces/omni/projects/taproot) | [Discord](https://discord.gg/omnidotdev) | [X](https://x.com/omnidotdev) | [Threads](https://www.threads.com/@omnidotdev)

</div>

**Taproot** is a pure Rust library and server that auto-generates a full GraphQL API from a PostgreSQL database. Point it at your database and it introspects tables, columns, relationships, and enums at startup to serve a dynamic, queryable schema with zero codegen.

## Installation

Install the latest release with the install script. It downloads the prebuilt
binary for your platform and places it in `~/.local/bin` (or `/usr/local/bin`
when run as root):

```bash
curl -fsSL https://taproot.omni.dev/install.sh | sh
```

Other channels:

| Platform | Channel | Command / Link |
| --- | --- | --- |
| macOS, Linux | [Homebrew](https://github.com/omnidotdev/homebrew-tap) | `brew install omnidotdev/tap/taproot` |
| Arch Linux | [AUR](https://aur.archlinux.org/packages/omnidotdev-taproot) | `paru -S omnidotdev-taproot` (`-bin` for the prebuilt) |
| All | [GitHub Releases](https://github.com/omnidotdev/taproot/releases) | Download the `taproot` server binary |

### Build from source

```bash
git clone https://github.com/omnidotdev/taproot
cd taproot
cargo build --release          # binary at target/release/taproot
```

### Use as a library

`taproot-core` embeds the schema generator in a Rust service. Until it is
published to a registry, depend on it by git:

```toml
taproot-core = { git = "https://github.com/omnidotdev/taproot" }
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

- **Schema introspection**: tables, views, columns, primary keys, foreign keys, enums, composite types (a column named `id` is exposed as `rowId`)
- **Relay pagination**: cursor connections with `first`/`after`/`last`/`before`/`offset`
- **Global object identification**: every row carries an opaque Relay `id`, refetchable through the `node(id:)` root field
- **Filtering**: a flat `condition` (equality) plus a rich `filter` with comparison, pattern, and array operators and `and`/`or`/`not`
- **Mutations**: `create`/`update`/`delete`, bulk variants, and nested `create`/`connect` for related records
- **Subscriptions**: real-time change notifications over Postgres `LISTEN`/`NOTIFY`
- **Row-Level Security**: JWT-based auth with RLS passthrough to Postgres
- **Apollo Federation**: expose tables as federated subgraph entities
- **Plugin system**: `on_introspect`, `on_plan`, `filter_table`, plus `query_fields`/`mutation_fields` for custom SQL-backed root fields
- **Smart comments**: `@omit`, `@name`, `@deprecated` annotations
- **Embeddable**: use as a Rust library via `taproot-core`

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

The code in this repository is licensed under Apache 2.0, &copy; [Omni LLC](https://omni.dev). See [LICENSE.md](LICENSE.md) for more information.

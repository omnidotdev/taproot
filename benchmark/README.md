# Taproot benchmark

A reproducible benchmark that puts Taproot next to the GraphQL-from-Postgres
alternatives on the **same database, same query set, same load**.

## What it measures

Four query shapes, each run under identical load against each contender:

| Scenario    | Shape                                                        |
| ----------- | ----------------------------------------------------------- |
| `single`    | point lookup of one row by a unique, indexed column         |
| `list`      | filtered, ordered, paginated connection (20 rows)           |
| `nested`    | 20 rows with a belongs-to parent and a has-many child (the N+1 path) |
| `aggregate` | a filtered count over the full set                          |

For each it reports throughput (req/s) and latency (p50 / p95 / p99), plus an
error rate. The load script **fails any response that is non-200 or carries
GraphQL `errors`**, so a mismatched field name surfaces as an error instead of
a deceptively fast number.

## Contenders

| Contender    | How it runs                                   | Query file                |
| ------------ | --------------------------------------------- | ------------------------- |
| Taproot      | host binary (`target/release/taproot`)        | `queries/taproot.json`    |
| PostGraphile | v5 (Amber + simplify), Docker                 | `queries/postgraphile.json` |
| Hasura       | `hasura/graphql-engine`, Docker (opt-in)      | `queries/hasura.json`     |

Taproot and PostGraphile share almost the same query shape by design (Taproot
mirrors PostGraphile's simplified inflection: pluralized fields, `id` -> `rowId`,
`condition` filters, Relay connections). They differ only in relationship field
names, so each has its own query file. Hasura's schema differs more and has its
own.

## Requirements

- Docker (with the Compose plugin) and `curl` + `jq`
- A Rust toolchain (to build the Taproot binary)

No host install of k6 is needed; it runs from the `grafana/k6` image.

## Run it

```bash
cd benchmark
./run.sh                      # Taproot vs PostGraphile
WITH_HASURA=1 ./run.sh        # also include Hasura
DURATION=60s VUS=50 ./run.sh  # heavier load
```

Results print as a Markdown table and are written to `results/results.md`; the
raw k6 summaries land in `results/*.json`.

## Fairness notes

- One shared Postgres instance and dataset (20k users, 100k posts, 300k
  comments) seeded from `schema.sql`, with indexes on every foreign key.
- Connection pools are matched (Taproot `pool_size: 10`; the node `pg` pool
  defaults to 10).
- Every contender is queried over HTTP on the same host, warmed by the load
  tool's ramp, and measured for the same duration.
- Taproot runs as a native binary and the others in Docker; the gap this adds is
  negligible next to the per-request work, but for a byte-identical setup you can
  containerize Taproot too (build `taproot-server` into a slim image).

## Adjusting

If a scenario reports errors, the contender's generated field names differ from
the query file; open its GraphiQL/playground and update the matching
`queries/<contender>.json`. Versions are pinned in `postgraphile/package.json`
and `docker-compose.yml`; bump them to benchmark a newer release.

#!/usr/bin/env bash
# Reproducible Taproot benchmark runner.
#
# Brings up Postgres + PostGraphile (and optionally Hasura) in Docker, runs
# Taproot as a host binary against the same database, then drives every
# contender with the same k6 query set and prints a Markdown results table.
#
# Usage:
#   ./run.sh                 # Taproot vs PostGraphile, default load
#   WITH_HASURA=1 ./run.sh   # also include Hasura
#   DURATION=60s VUS=50 ./run.sh
set -euo pipefail

cd "$(dirname "$0")"
BENCH_DIR="$PWD"
REPO_ROOT="$(cd .. && pwd)"
OUT="$BENCH_DIR/results"
mkdir -p "$OUT"

DURATION="${DURATION:-30s}"
VUS="${VUS:-30}"
SCENARIOS=(single list nested aggregate)
K6_IMAGE="grafana/k6:0.54.0"

TAPROOT_PID=""
cleanup() {
  [ -n "$TAPROOT_PID" ] && kill "$TAPROOT_PID" 2>/dev/null || true
  docker compose --profile hasura down -v >/dev/null 2>&1 || true
}
trap cleanup EXIT

log() { printf '\n\033[1;32m==> %s\033[0m\n' "$*"; }

wait_graphql() {
  local url="$1" name="$2" tries=0
  until curl -fsS "$url" -H 'content-type: application/json' \
        -d '{"query":"{ __typename }"}' >/dev/null 2>&1; do
    tries=$((tries + 1))
    [ "$tries" -gt 120 ] && { echo "timed out waiting for $name ($url)"; return 1; }
    sleep 1
  done
  echo "$name ready"
}

# ---------------------------------------------------------------------------
log "Building Taproot server (release)"
(cd "$REPO_ROOT" && cargo build --release -p taproot-server)
TAPROOT_BIN="$REPO_ROOT/target/release/taproot"

log "Starting Postgres + PostGraphile"
docker compose up -d --build postgres postgraphile
# compose waits for the DB healthcheck before postgraphile; give the seed a moment
docker compose exec -T postgres bash -c 'until pg_isready -U postgres -d bench; do sleep 1; done'

CONTENDERS=()

log "Starting Taproot (host binary) on :4000"
"$TAPROOT_BIN" -c "$BENCH_DIR/config/taproot.yaml" >"$OUT/taproot.log" 2>&1 &
TAPROOT_PID=$!
wait_graphql "http://localhost:4000/graphql" "taproot"
CONTENDERS+=("taproot|http://localhost:4000/graphql|taproot")

wait_graphql "http://localhost:5000/graphql" "postgraphile"
CONTENDERS+=("postgraphile|http://localhost:5000/graphql|postgraphile")

if [ "${WITH_HASURA:-0}" = "1" ]; then
  log "Starting Hasura + tracking tables"
  docker compose --profile hasura up -d hasura
  wait_graphql "http://localhost:8081/v1/graphql" "hasura"
  ./hasura-track.sh || echo "hasura tracking failed, skipping hasura"
  CONTENDERS+=("hasura|http://localhost:8081/v1/graphql|hasura")
fi

log "Pulling k6 image"
docker pull -q "$K6_IMAGE" >/dev/null

log "Running load (${VUS} VUs x ${DURATION} per scenario)"
for entry in "${CONTENDERS[@]}"; do
  IFS='|' read -r name url dialect <<<"$entry"
  for sc in "${SCENARIOS[@]}"; do
    echo "  - $name / $sc"
    docker run --rm --network host \
      -v "$BENCH_DIR/k6:/k6:ro" -v "$BENCH_DIR/queries:/queries:ro" -v "$OUT:/out" \
      "$K6_IMAGE" run --quiet \
      -e TARGET="$url" -e QUERY_FILE="/queries/$dialect.json" -e SCENARIO="$sc" \
      -e VUS="$VUS" -e DURATION="$DURATION" \
      --summary-export="/out/${name}_${sc}.json" /k6/bench.js >/dev/null 2>&1 || true
  done
done

# ---------------------------------------------------------------------------
log "Results (req/s, higher is better; latency ms, lower is better)"
RESULTS="$OUT/results.md"
{
  echo "# Taproot benchmark results"
  echo
  echo "Load: ${VUS} VUs x ${DURATION} per scenario. Generated $(date -u +%Y-%m-%dT%H:%MZ)."
  echo
  echo "| Scenario | Contender | req/s | p50 ms | p95 ms | p99 ms | err |"
  echo "| --- | --- | --- | --- | --- | --- | --- |"
  for sc in "${SCENARIOS[@]}"; do
    for entry in "${CONTENDERS[@]}"; do
      IFS='|' read -r name _ _ <<<"$entry"
      f="$OUT/${name}_${sc}.json"
      [ -f "$f" ] || continue
      rps=$(jq -r '.metrics.http_reqs.rate // 0 | floor' "$f")
      p50=$(jq -r '.metrics.gql_latency["p(50)"] // 0 | (.*100|round/100)' "$f")
      p95=$(jq -r '.metrics.gql_latency["p(95)"] // 0 | (.*100|round/100)' "$f")
      p99=$(jq -r '.metrics.gql_latency["p(99)"] // 0 | (.*100|round/100)' "$f")
      err=$(jq -r '.metrics.gql_errors.value // 0 | (.*100|round)' "$f")
      echo "| $sc | $name | $rps | $p50 | $p95 | $p99 | ${err}% |"
    done
  done
} | tee "$RESULTS"

echo
echo "Full summaries in $OUT/, table in $RESULTS"

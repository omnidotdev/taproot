#!/usr/bin/env bash
# Track the benchmark tables and relationships in Hasura so its GraphQL schema
# exposes the same shape the query set expects (post.author, post.comments).
set -euo pipefail

HASURA="${HASURA_URL:-http://localhost:8081}"

meta() {
  curl -fsS "$HASURA/v1/metadata" -H 'content-type: application/json' -d "$1" >/dev/null
}

for t in user post comment; do
  meta "{\"type\":\"pg_track_table\",\"args\":{\"source\":\"default\",\"table\":{\"schema\":\"public\",\"name\":\"$t\"}}}" || true
done

# object relationship post.author -> user (via author_id)
meta '{"type":"pg_create_object_relationship","args":{"source":"default","table":{"schema":"public","name":"post"},"name":"author","using":{"foreign_key_constraint_on":"author_id"}}}' || true

# array relationship post.comments -> comment (via comment.post_id)
meta '{"type":"pg_create_array_relationship","args":{"source":"default","table":{"schema":"public","name":"post"},"name":"comments","using":{"foreign_key_constraint_on":{"table":{"schema":"public","name":"comment"},"column":"post_id"}}}}' || true

echo "hasura tracked"

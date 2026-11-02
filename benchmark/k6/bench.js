// k6 load script for one (contender, scenario) pair.
//
// Env:
//   TARGET      GraphQL endpoint URL
//   QUERY_FILE  path to a JSON map of { scenario: queryString }
//   SCENARIO    which scenario key to run
//   VUS         virtual users (default 30)
//   DURATION    test duration (default 30s)
//
// Fails the run loudly if any response is non-200 or carries GraphQL `errors`,
// so a mismatched field name can never masquerade as a fast result.

import http from "k6/http";
import { check } from "k6";
import { Trend, Rate } from "k6/metrics";

const QUERIES = JSON.parse(open(__ENV.QUERY_FILE));
const SCENARIO = __ENV.SCENARIO;
const QUERY = QUERIES[SCENARIO];
const TARGET = __ENV.TARGET;

const latency = new Trend("gql_latency", true);
const errorRate = new Rate("gql_errors");

export const options = {
  vus: Number(__ENV.VUS || 30),
  duration: __ENV.DURATION || "30s",
  summaryTrendStats: ["avg", "p(50)", "p(95)", "p(99)", "max"],
};

export default function () {
  const res = http.post(TARGET, JSON.stringify({ query: QUERY }), {
    headers: { "Content-Type": "application/json" },
  });

  latency.add(res.timings.duration);

  let body;
  try {
    body = res.json();
  } catch (_e) {
    body = null;
  }

  const ok = res.status === 200 && body && !body.errors;
  errorRate.add(!ok);

  check(res, {
    "status 200": () => res.status === 200,
    "no graphql errors": () => body && !body.errors,
  });

  if (!ok && __ENV.DEBUG) {
    console.error(`[${SCENARIO}] status=${res.status} body=${res.body}`);
  }
}

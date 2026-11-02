// PostGraphile v5 standalone server for the benchmark.
//
// Uses the same preset family as the Omni fleet (Amber + simplify inflection)
// so the generated schema mirrors what production actually runs, which is also
// the convention Taproot follows (pluralized fields, `id` -> `rowId`).

import { createServer } from "node:http";
import { postgraphile } from "postgraphile";
import { grafserv } from "postgraphile/grafserv/node";
import { makePgService } from "postgraphile/adaptors/pg";
import { PostGraphileAmberPreset } from "postgraphile/presets/amber";
import { PgSimplifyInflectionPreset } from "@graphile/simplify-inflection";

const preset = {
  extends: [PostGraphileAmberPreset, PgSimplifyInflectionPreset],
  pgServices: [
    makePgService({
      connectionString: process.env.DATABASE_URL,
      schemas: ["public"],
    }),
  ],
  grafserv: { graphqlPath: "/graphql", watch: false },
};

const pgl = postgraphile(preset);
const serv = pgl.createServ(grafserv);

const server = createServer();

serv.addTo(server).catch((err) => {
  console.error(err);
  process.exit(1);
});

const port = Number(process.env.PORT || 5000);
server.listen(port, () => console.log(`postgraphile listening on :${port}`));

-- Benchmark schema and seed data, shared by every contender.
--
-- Three related tables (user -> post -> comment) with foreign keys, so the
-- query set can exercise point lookups, filtered pagination, nested relations
-- (the N+1 path), and aggregates. Volumes are deliberately large enough that
-- the GraphQL layer, not an empty table, is what is being measured.

create extension if not exists pgcrypto;

create table "user" (
  id         uuid primary key default gen_random_uuid(),
  name       text not null,
  email      text unique not null,
  role       text not null default 'viewer',
  created_at timestamptz not null default now()
);

create table post (
  id         uuid primary key default gen_random_uuid(),
  title      text not null,
  body       text,
  author_id  uuid not null references "user"(id),
  published  boolean not null default false,
  created_at timestamptz not null default now()
);

create table comment (
  id         uuid primary key default gen_random_uuid(),
  body       text not null,
  post_id    uuid not null references post(id),
  author_id  uuid not null references "user"(id),
  created_at timestamptz not null default now()
);

-- 20k users
insert into "user" (name, email, role)
select 'User ' || g,
       'user' || g || '@example.com',
       (array['admin', 'editor', 'viewer'])[1 + (g % 3)]
from generate_series(1, 20000) g;

-- 100k posts, evenly distributed across users via a row-number join (fast,
-- deterministic, no per-row subquery)
insert into post (title, body, author_id, published)
select 'Post ' || p.g,
       'Body text for post ' || p.g,
       u.id,
       (p.g % 2 = 0)
from generate_series(1, 100000) p(g)
join (select id, row_number() over (order by id) rn from "user") u
  on u.rn = 1 + (p.g % 20000);

-- 300k comments across posts and users
insert into comment (body, post_id, author_id)
select 'Comment ' || c.g,
       po.id,
       us.id
from generate_series(1, 300000) c(g)
join (select id, row_number() over (order by id) rn from post) po
  on po.rn = 1 + (c.g % 100000)
join (select id, row_number() over (order by id) rn from "user") us
  on us.rn = 1 + (c.g % 20000);

create index on post (author_id);
create index on post (published);
create index on comment (post_id);
create index on comment (author_id);

analyze;

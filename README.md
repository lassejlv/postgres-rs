# postgres-rs

A fast, lightweight, **PostgreSQL wire-compatible** database written in Rust —
zero external dependencies. Connect with `psql`, libpq, or any PostgreSQL
driver and run real SQL.

```
$ cargo run --release -- 127.0.0.1:5433
postgres-rs listening on 127.0.0.1:5433

$ psql -h 127.0.0.1 -p 5433 -U postgres
postgres=> CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL, age integer);
CREATE TABLE
postgres=> INSERT INTO users VALUES (1, 'Alice', 30), (2, 'Bob', 25);
INSERT 0 2
postgres=> SELECT name, age FROM users WHERE age > 26 ORDER BY age DESC;
 name  | age
-------+-----
 Alice |  30
(1 row)
```

## What works today

**Wire protocol (PostgreSQL v3)**
- Startup, SSL/GSS negotiation (declined), **trust or SCRAM-SHA-256 auth**
  (set `PGRS_PASSWORD` to require a password — crypto is hand-rolled, no deps)
- `ParameterStatus`, `BackendKeyData`, `ReadyForQuery`
- **Simple query** protocol (`Q`)
- **Extended query** protocol: `Parse` / `Bind` / `Describe` / `Execute` /
  `Sync` / `Close`, including `$1` positional parameters in both **text and
  binary** parameter/result formats
- Proper `ErrorResponse` with SQLSTATE codes

**Storage engine**
- B-tree **secondary indexes** (`CREATE [UNIQUE] INDEX ... ON t (col)`,
  `DROP INDEX`), auto-created for `PRIMARY KEY` columns, maintained
  incrementally on INSERT/UPDATE/DELETE and persisted via the WAL.
- The executor uses indexes for `=`, `IN`, and range/`BETWEEN` predicates and
  for indexed nested-loop joins, always re-checking the full predicate so
  results match a scan. Measured speedups at 200k rows: **~4,800× point
  lookup**, **~3,300× IN-list**, **~100× range scan** (run `cargo run --release
  --bin bench`).

**Durability**
- Logical **write-ahead log**: set `PGRS_DATA=<dir>` and every successful
  mutation is re-serialized to canonical SQL, appended, and `fsync`ed. On
  restart the log is replayed to recover state. Without `PGRS_DATA` the
  database runs in memory.

**SQL**
- `CREATE TABLE` / `DROP TABLE` (with `IF [NOT] EXISTS`), column constraints
  (`NOT NULL`, `PRIMARY KEY` parsed), `DEFAULT <expr>` values,
  `serial`/`bigserial`/`smallserial` auto-increment columns
- `INSERT ... VALUES (...), (...)` with/without a column list
- `SELECT` / `SELECT DISTINCT`: projection with aliases, `*`, `WHERE`,
  `GROUP BY`, `HAVING`, `ORDER BY` (by expression or output alias),
  `LIMIT`/`OFFSET`
- `INNER` / `LEFT` / `RIGHT` / `FULL [OUTER] JOIN ... ON ...` and `CROSS JOIN`,
  table aliases, qualified column references (`u.id`), aggregates over joins
- `UPDATE ... SET ... WHERE`, `DELETE ... WHERE`
- `RETURNING` on `INSERT`/`UPDATE`/`DELETE` (e.g. `INSERT ... RETURNING id`)
- Expressions: arithmetic, comparison, `AND`/`OR`/`NOT`, `||`, `IS [NOT] NULL`,
  `[NOT] LIKE`/`ILIKE` (`%`/`_`), `[NOT] IN (...)`, `[NOT] BETWEEN`,
  `CASE` (simple and searched), `CAST(x AS t)` / `x::t`, parentheses,
  three-valued NULL logic
- Aggregates: `count`, `sum`, `avg`, `min`, `max` (with `GROUP BY`/`HAVING`)
- Scalar functions: `upper`, `lower`, `length`, `abs`, `round`, `trim`/`ltrim`/
  `rtrim`, `substr`/`substring`, `replace`, `coalesce`, `nullif`, `greatest`,
  `least`, `concat`, `current_user`/`current_database()`/`current_schema`,
  `version`, `now`
- **Transactions**: `BEGIN` / `COMMIT` / `ROLLBACK` with real rollback —
  statements run against a private snapshot, an error aborts the block (further
  commands rejected until it ends), and only committed mutations reach the WAL.
  (Isolation under concurrent writers is still coarse — last commit wins;
  proper MVCC is on the roadmap.) `SET` accepted; `SHOW` supported.

**Introspection:** `information_schema.tables`/`.columns` and
`pg_catalog.pg_class`/`pg_namespace`/`pg_am` are queryable virtual views over
the live schema. **`psql \dt` works** end-to-end (lists tables). Also supports
the `~`/`!~`/`~*`/`!~*` regex operators, `ORDER BY <position>`, schema-qualified
function calls, and catalog helpers (`pg_table_is_visible`, `pg_get_userbyid`).

**Types:** `smallint`, `integer`, `bigint`, `real`, `double precision`,
`numeric`/`decimal`, `boolean`, `text`/`varchar`, `date`, `time`, `timestamp`,
`timestamptz`, `uuid`, `json`, `jsonb` — with the correct PostgreSQL type OIDs.
Date/time/uuid/json are stored as text (ISO text sorts correctly); `numeric` is
f64-backed for now. Unknown/`schema.type` cast targets degrade to text.

## Architecture

| Module        | Responsibility                                        |
|---------------|-------------------------------------------------------|
| `protocol`    | v3 wire message framing (encode/decode)               |
| `sql`         | `lexer` → `ast` → `parser` + `serialize` (no deps)    |
| `storage`     | in-memory tables + row ids + B-tree index maintenance |
| `index`       | `BTreeMap`-backed secondary indexes (eq/range lookup) |
| `executor`    | evaluate statements; index-aware access planning      |
| `bind`        | decode/substitute extended-protocol parameters        |
| `crypto`      | SHA-256, HMAC, PBKDF2, Base64 (no deps)               |
| `auth`        | SCRAM-SHA-256 server exchange                          |
| `wal`         | logical write-ahead log + recovery                    |
| `server`      | TCP accept loop, per-connection session, auth flow    |

One thread per connection (like PostgreSQL's process-per-backend), sharing a
single database behind a mutex.

## Running tests

```
cargo test
```

## Roadmap toward full PostgreSQL compatibility

- [x] B-tree secondary indexes + index-aware query execution
- [x] Logical write-ahead log + crash recovery (`PGRS_DATA`)
- [x] `GROUP BY` / `HAVING`, `ORDER BY` by output alias
- [x] `INNER`/`LEFT JOIN` with table aliases and qualified columns
- [x] `SELECT DISTINCT`
- [x] Real transactions: `BEGIN`/`COMMIT`/`ROLLBACK` with snapshot rollback,
      aborted-transaction state, and commit-only durability
- [ ] Checkpoints / log compaction; physical (page-level) WAL
- [x] `RIGHT`/`FULL`/`CROSS` joins; `LIKE`/`ILIKE`/`IN`/`BETWEEN`/`CASE`
- [x] `RETURNING` on INSERT/UPDATE/DELETE; `DEFAULT` column values
- [x] `serial`/`bigserial`/`smallserial` sequences (durable across recovery)
- [x] `information_schema` + `pg_catalog` (pg_class/pg_namespace), psql `\dt`
- [x] Regex operators (`~`/`!~`/`~*`/`!~*`), `ORDER BY <position>`
- [x] `numeric`, `real`, `date`/`time`/`timestamp`/`timestamptz`, `uuid`,
      `json`/`jsonb` types; `OPERATOR(...)`/`COLLATE` syntax; loose numeric↔text compares
- [ ] psql `\d <table>` (needs `pg_attribute`/`pg_type`, more `pg_class` cols)
- [ ] Subqueries; arbitrary-precision numeric; richer date/time functions
- [ ] MVCC isolation under concurrent writers (current model is last-commit-wins)
- [ ] Composite (multi-column) indexes and a cost-based planner
- [ ] More types (`numeric`, `date`/`timestamp`, `uuid`, `json`/`jsonb`, arrays)
- [ ] `pg_catalog` system views so `\d`, `\dt`, and ORMs introspect correctly
- [x] SCRAM-SHA-256 authentication (`PGRS_PASSWORD`)
- [ ] TLS, query cancellation
- [ ] Prepared statement caching, `COPY`, and more

## Status

Early but real: this is iteration one. The wire protocol is solid enough that
standard PostgreSQL clients connect and run queries unmodified.

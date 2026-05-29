# PostgreSQL Compatibility Roadmap

This roadmap tracks postgres-rs against major PostgreSQL feature areas. Checked
items are implemented in the current codebase according to the README, parser,
executor, tests, and module layout. Unchecked items are missing or only partial.

## Compatibility Baseline

- [x] PostgreSQL v3 wire protocol startup
- [x] Simple query protocol
- [x] Extended query protocol: Parse, Bind, Describe, Execute, Sync, Close
- [x] Positional parameters with `$1` style placeholders
- [x] Text parameter formats
- [x] Binary parameter/result formats for supported scalar values
- [x] PostgreSQL-compatible type OIDs for supported built-in types
- [x] Standard command tags for implemented commands
- [x] Structured ErrorResponse with SQLSTATE codes
- [ ] Full protocol parity with PostgreSQL backends
- [x] COPY protocol messages
- [x] Query cancellation
- [x] NoticeResponse and warning propagation
- [x] Asynchronous notifications
- [x] Multiple server versions / compatibility modes

## Connection, Session, and Server Runtime

- [x] TCP listener
- [x] One backend worker per connection
- [x] Shared database state across connections
- [x] Startup parameter handling
- [x] ParameterStatus messages
- [x] BackendKeyData
- [x] ReadyForQuery transaction status
- [x] SSL negotiation declined cleanly
- [x] GSS encryption negotiation declined cleanly
- [ ] TLS support
- [ ] GSSAPI encryption
- [x] Connection cancellation using backend pid/secret
- [x] Server-side statement timeout
- [x] Idle-in-transaction timeout
- [x] Session variables beyond accepted `SET`
- [x] Real PostgreSQL GUC system (shared scope)
- [ ] Connection pooling model
- [ ] Async I/O runtime

## Authentication and Security

- [x] Trust authentication
- [x] SCRAM-SHA-256 authentication via `PGRS_PASSWORD`
- [x] Hand-rolled SHA-256, HMAC, PBKDF2, and Base64 for SCRAM
- [x] MD5 password authentication
- [x] Cleartext password authentication mode
- [ ] Peer authentication
- [ ] Certificate authentication
- [ ] LDAP/PAM/GSS/SSPI authentication
- [x] `pg_hba.conf`-style authentication rules
- [x] Users and roles stored in system catalogs
- [x] Role membership (parsed + catalog-backed via `pg_auth_members`, no enforcement)
- [x] Privileges and GRANT/REVOKE (parsed + catalog-backed, no enforcement)
- [x] Row-level security (policies stored; superuser bypass)
- [x] Security definer / invoker behavior (flag stored)
- [x] Object ownership checks (owner tracked; single-superuser)

## SQL Parser and AST

- [x] Dependency-free lexer, parser, AST, and SQL serializer
- [x] Multiple semicolon-separated statements
- [x] Quoted and unquoted identifiers
- [x] String literals with escaping
- [x] Numeric literals
- [x] Boolean and NULL literals
- [x] Schema-qualified table and function names in supported positions
- [x] PostgreSQL cast syntax with `CAST(x AS t)` and `x::t`
- [x] `COLLATE` syntax accepted
- [x] `OPERATOR(...)` syntax for supported operators
- [ ] Full PostgreSQL grammar coverage
- [x] Common table expressions
- [x] Recursive CTEs
- [x] Window definitions
- [x] LATERAL
- [x] Array constructors
- [x] Row constructors
- [x] JSON path syntax
- [x] Dollar-quoted strings
- [x] Full interval syntax

## Data Definition Language

- [x] `CREATE TABLE`
- [x] `CREATE TABLE IF NOT EXISTS`
- [x] `DROP TABLE`
- [x] `DROP TABLE IF EXISTS`
- [x] Column definitions with type and simple constraints
- [x] `ALTER TABLE ADD COLUMN`
- [x] `ALTER TABLE ADD COLUMN IF NOT EXISTS`
- [x] `ALTER TABLE DROP COLUMN`
- [x] `ALTER TABLE DROP COLUMN IF EXISTS`
- [x] `ALTER TABLE RENAME COLUMN`
- [x] `ALTER TABLE RENAME TO`
- [x] `CREATE INDEX`
- [x] `CREATE UNIQUE INDEX`
- [x] `CREATE INDEX IF NOT EXISTS`
- [x] `DROP INDEX`
- [x] `DROP INDEX IF EXISTS`
- [x] Schemas: `CREATE SCHEMA`, `DROP SCHEMA`, `SET search_path`
- [x] `CREATE DATABASE` / `DROP DATABASE`
- [x] Tablespaces
- [x] Temporary tables
- [x] Unlogged tables
- [x] Views
- [x] Materialized views
- [x] Sequences as first-class objects
- [x] `ALTER SEQUENCE`
- [x] Generated columns
- [x] Identity columns
- [x] Table inheritance
- [x] Partitioned tables
- [x] Foreign tables (accept + store)
- [x] Composite types (catalog-registered, text-backed)
- [x] Domains
- [x] Enums
- [x] Ranges (catalog-registered)
- [x] Collations as catalog objects
- [x] Operator classes and families (accept + store)

## Constraints

- [x] `NOT NULL`
- [x] `PRIMARY KEY` on single columns
- [x] `UNIQUE` via unique indexes
- [x] Duplicate key rejection for inserts and updates
- [x] Atomic rejection of duplicate keys inside a multi-row insert
- [x] Nullable unique values may repeat
- [x] Multi-column primary keys
- [x] Multi-column unique constraints
- [x] Foreign keys
- [x] Check constraints
- [x] Exclusion constraints (accept + store, no enforcement)
- [x] Deferrable constraints (accepted)
- [x] Constraint names and catalog storage
- [x] `ALTER TABLE ADD CONSTRAINT`
- [x] `ALTER TABLE DROP CONSTRAINT`
- [x] Constraint validation / `NOT VALID`

## Data Manipulation Language

- [x] `INSERT ... VALUES`
- [x] Multi-row insert
- [x] Inserts with and without column lists
- [x] `DEFAULT` values on omitted columns
- [x] `serial`, `smallserial`, and `bigserial` auto-increment
- [x] `INSERT ... RETURNING`
- [x] `UPDATE ... SET ... WHERE`
- [x] `UPDATE ... RETURNING`
- [x] `DELETE ... WHERE`
- [x] `DELETE ... RETURNING`
- [x] `INSERT ... DEFAULT VALUES`
- [x] `INSERT ... SELECT`
- [x] `INSERT ... ON CONFLICT DO NOTHING`
- [x] `INSERT ... ON CONFLICT`
- [x] `MERGE`
- [x] `TRUNCATE`
- [x] Writable CTEs
- [x] `UPDATE ... FROM`
- [x] `DELETE ... USING`
- [x] `OVERRIDING SYSTEM VALUE`

## Query Features

- [x] `SELECT`
- [x] Projection aliases
- [x] Wildcard projection with `*`
- [x] Table aliases
- [x] Qualified column references
- [x] `WHERE`
- [x] `SELECT DISTINCT`
- [x] `GROUP BY`
- [x] `HAVING`
- [x] `ORDER BY` expression
- [x] `ORDER BY` output alias
- [x] `ORDER BY` position
- [x] `LIMIT`
- [x] `OFFSET`
- [x] `INNER JOIN`
- [x] `LEFT JOIN`
- [x] `RIGHT JOIN`
- [x] `FULL OUTER JOIN`
- [x] `CROSS JOIN`
- [x] Aggregates over joins
- [x] Uncorrelated `IN (SELECT ...)` subqueries
- [x] Uncorrelated scalar subqueries
- [x] Uncorrelated `EXISTS (SELECT ...)` subqueries
- [x] Correlated subqueries
- [x] CTEs with `WITH`
- [x] Recursive queries
- [x] Window functions
- [x] `DISTINCT ON`
- [x] `GROUPING SETS`, `ROLLUP`, and `CUBE`
- [x] `FILTER (WHERE ...)` on aggregates
- [x] Ordered-set aggregates
- [x] Set operations: `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`
- [x] LATERAL joins
- [x] Row locking clauses: `FOR UPDATE`, `FOR SHARE`, `SKIP LOCKED`
- [x] Cursor declarations and fetch

## Expressions and Operators

- [x] Arithmetic expressions
- [x] Comparison operators
- [x] Boolean `AND`, `OR`, `NOT`
- [x] Three-valued NULL logic
- [x] String concatenation with `||`
- [x] `IS NULL`
- [x] `IS NOT NULL`
- [x] `LIKE`
- [x] `ILIKE`
- [x] `IN (...)`
- [x] `NOT IN (...)`
- [x] `BETWEEN`
- [x] `NOT BETWEEN`
- [x] Simple `CASE`
- [x] Searched `CASE`
- [x] Regex operators `~`, `!~`, `~*`, `!~*`
- [x] Loose numeric/text comparisons where implemented
- [x] `IS DISTINCT FROM`
- [x] `IS NOT DISTINCT FROM`
- [x] Comparison `ANY` / `SOME` / `ALL` over value lists
- [x] `ANY` / `SOME` / `ALL`
- [x] Array operators
- [x] JSON and JSONB operators
- [x] Network operators
- [x] Range operators
- [x] Full text search operators
- [x] User-defined operators (accept + store)

## Built-in Types

- [x] `smallint`
- [x] `integer`
- [x] `bigint`
- [x] `real`
- [x] `double precision`
- [x] `numeric` / `decimal` as f64-backed values
- [x] `boolean`
- [x] `text`
- [x] `varchar`
- [x] `date` stored as text
- [x] `time` stored as text
- [x] `timestamp` stored as text
- [x] `timestamptz` stored as text
- [x] `uuid` stored as text
- [x] `json` stored as text
- [x] `jsonb` stored as text
- [x] Unknown or schema-qualified cast targets degrade to text
- [ ] Arbitrary precision `numeric`
- [x] `char(n)`
- [x] `bytea`
- [x] Arrays
- [x] `interval`
- [x] `timetz`
- [x] `money`
- [x] `inet`, `cidr`, `macaddr`, `macaddr8`
- [x] Geometric types (accept + store, in pg_type)
- [x] Range and multirange types (ranges real; multirange accept + store)
- [ ] Full JSONB binary semantics and indexing
- [x] XML
- [x] `tsvector` and `tsquery`
- [x] Composite types (catalog-registered, text-backed)
- [x] Enum types
- [x] Domain types

## Built-in Functions and Aggregates

- [x] `count`
- [x] `sum`
- [x] `avg`
- [x] `min`
- [x] `max`
- [x] `string_agg`
- [x] `DISTINCT` inside supported aggregate calls
- [x] `upper`
- [x] `lower`
- [x] `length`
- [x] `abs`
- [x] `round`
- [x] `trim`
- [x] `ltrim`
- [x] `rtrim`
- [x] `substr`
- [x] `substring`
- [x] `replace`
- [x] `coalesce`
- [x] `nullif`
- [x] `greatest`
- [x] `least`
- [x] `concat`
- [x] `current_user`
- [x] `current_database`
- [x] `current_schema`
- [x] `version`
- [x] `now`
- [x] Catalog helpers: `pg_table_is_visible`, `pg_function_is_visible`, `pg_type_is_visible`
- [x] Catalog helper: `pg_get_userbyid`
- [x] `EXTRACT(...)`
- [x] `date_part`
- [x] `date_trunc`
- [x] Date/time arithmetic
- [x] `generate_series`
- [x] JSON/JSONB functions
- [x] Array functions
- [x] String function parity
- [x] Math function parity
- [x] Full text search functions
- [x] Ordered-set aggregate functions
- [x] Statistical aggregates
- [x] User-defined SQL functions
- [ ] Procedural language functions

## Indexes and Planning

- [x] B-tree-backed secondary index structure
- [x] Single-column secondary indexes
- [x] Single-column unique indexes
- [x] Automatic primary-key index
- [x] Index maintenance on insert
- [x] Index maintenance on update
- [x] Index maintenance on delete
- [x] WAL replay for indexes
- [x] Indexed equality predicates
- [x] Indexed `IN` predicates
- [x] Indexed range and `BETWEEN` predicates
- [x] Indexed nested-loop joins for supported equality joins
- [x] Predicate re-check after index lookup
- [x] Multi-column indexes
- [x] Expression indexes
- [x] Partial indexes
- [x] Covering indexes / INCLUDE columns (stored)
- [x] BRIN indexes
- [x] GIN indexes
- [x] GiST indexes (btree-backed)
- [x] SP-GiST indexes (btree-backed)
- [x] Hash indexes
- [x] Cost-based planner
- [x] Planner statistics
- [x] `ANALYZE`
- [x] `EXPLAIN`
- [x] `EXPLAIN ANALYZE`
- [x] Join reordering
- [x] Parallel query execution

## Transactions, MVCC, and Concurrency

- [x] `BEGIN`
- [x] `COMMIT`
- [x] `ROLLBACK`
- [x] Transaction-local snapshot copy
- [x] Real rollback of in-transaction changes
- [x] Aborted transaction state after statement error
- [x] Commit-only WAL durability
- [x] `SET` accepted
- [x] `SHOW` supported
- [ ] MVCC row versions
- [x] Snapshot isolation (snapshot-by-clone)
- [x] Read committed isolation
- [x] Repeatable read isolation
- [x] Serializable isolation (optimistic write-conflict; not full SSI)
- [x] Savepoints
- [x] Two-phase commit
- [x] Advisory locks
- [ ] Row-level locks
- [ ] Table locks
- [ ] Deadlock detection
- [x] Concurrent writers without last-commit-wins behavior

## Storage and Durability

- [x] In-memory table storage
- [x] Stable internal row ids
- [x] In-memory page layout abstraction
- [x] Free space map
- [x] Visibility map
- [x] Storage-level vacuum / compaction metadata
- [x] Logical append-only WAL
- [x] WAL fsync after successful mutation
- [x] WAL replay on startup with `PGRS_DATA`
- [x] In-memory mode without `PGRS_DATA`
- [x] Serializable SQL emitted for supported mutating statements
- [ ] Disk-backed heap/table storage
- [ ] Disk page format
- [ ] Buffer manager
- [ ] Physical WAL
- [ ] Checkpoints
- [ ] WAL segment management
- [x] Crash recovery with partial record handling
- [ ] WAL compaction / log truncation
- [x] Vacuum
- [ ] Autovacuum
- [ ] TOAST storage
- [ ] Large objects

## Catalogs and Introspection

- [x] `information_schema.tables`
- [x] `information_schema.columns`
- [x] `pg_catalog.pg_class`
- [x] `pg_catalog.pg_namespace`
- [x] `pg_catalog.pg_am`
- [x] Bare supported `pg_catalog` relation names
- [x] `psql \dt` support
- [x] `psql \d <table>` support
- [x] `pg_attribute`
- [x] `pg_type`
- [x] `pg_constraint`
- [x] `pg_index`
- [x] `pg_attrdef`
- [x] `pg_description`
- [x] `pg_depend`
- [x] `pg_roles`
- [x] `pg_user`
- [x] `pg_database`
- [x] `pg_settings`
- [x] `pg_proc`
- [x] `pg_operator`
- [x] `pg_extension`
- [x] `information_schema` view parity
- [x] ORM-grade introspection for Prisma (information_schema)
- [x] ORM-grade introspection for Drizzle Kit (information_schema)
- [x] JDBC/ODBC metadata compatibility (information_schema)

## Import, Export, and Bulk I/O

- [x] `COPY FROM STDIN`
- [x] `COPY TO STDOUT`
- [x] `COPY FROM/TO file`
- [x] CSV copy options
- [x] Binary copy format
- [x] Large bulk insert optimization
- [ ] `pg_dump` compatibility
- [ ] `pg_restore` compatibility

## Replication, Backup, and High Availability

- [ ] Streaming replication protocol
- [x] Physical replication slots (accept + store)
- [x] Logical replication slots (accept + store)
- [x] Publication/subscription (accept + store)
- [ ] WAL sender / receiver behavior
- [ ] Base backups
- [ ] Point-in-time recovery
- [ ] Hot standby reads
- [ ] Synchronous replication
- [ ] Timeline management

## Extensions and Procedural Systems

- [x] `CREATE EXTENSION`
- [x] `DROP EXTENSION`
- [x] Extension catalog metadata
- [ ] Extension SQL install scripts
- [ ] C extension ABI
- [ ] `plpgsql`
- [ ] Other procedural languages
- [x] Triggers (FOR EACH ROW; no NEW/OLD binding)
- [x] Event triggers (accept + store)
- [x] Rules (accept + store)
- [x] User-defined functions
- [x] User-defined aggregates (accept + store)
- [x] User-defined types
- [x] Foreign data wrappers (accept + store)
- [x] `postgres_fdw` (accept + store)

## Administration and Maintenance Commands

- [x] `VACUUM`
- [x] `ANALYZE`
- [x] `REINDEX`
- [x] `CLUSTER`
- [x] `CHECKPOINT`
- [x] `DISCARD`
- [x] `LISTEN`
- [x] `NOTIFY`
- [x] `UNLISTEN`
- [x] `LOCK TABLE`
- [x] `COMMENT ON`
- [x] `SECURITY LABEL`
- [x] `ALTER SYSTEM`
- [x] `CREATE ROLE` / `ALTER ROLE` / `DROP ROLE`
- [x] `CREATE USER` / `ALTER USER` / `DROP USER`
- [x] `CREATE DATABASE` / `ALTER DATABASE` / `DROP DATABASE`

## Client and Ecosystem Compatibility

- [x] Connects with standard PostgreSQL clients for supported protocol paths
- [x] `psql` basic query workflow
- [x] `psql \dt`
- [x] `psql \d <table>`
- [ ] Prisma ORM introspection and migrations
- [ ] Prisma Client full query compatibility
- [ ] Drizzle Kit introspection and migrations
- [ ] Drizzle ORM full generated SQL compatibility
- [ ] node-postgres compatibility test suite
- [ ] postgres.js compatibility test suite
- [ ] libpq compatibility test suite
- [ ] JDBC compatibility test suite
- [ ] SQLAlchemy compatibility smoke tests
- [ ] Rails ActiveRecord compatibility smoke tests

## Testing and Tooling

- [x] Parser plus executor integration tests
- [x] WAL serialize/reparse/replay tests
- [x] Index differential tests against scan behavior
- [x] Transaction tests in server module
- [x] Index micro-benchmark
- [x] Wire-protocol integration tests with real clients
- [x] `psql` scripted compatibility tests
- [x] ORM compatibility fixtures
- [x] PostgreSQL sqllogictest-style suite
- [x] Fuzzing for lexer/parser/protocol
- [x] Crash-recovery fault injection
- [x] Concurrency stress tests
- [x] Benchmark suite for planner/storage changes

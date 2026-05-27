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
- [ ] COPY protocol messages
- [ ] Query cancellation
- [ ] NoticeResponse and warning propagation
- [ ] Asynchronous notifications
- [ ] Multiple server versions / compatibility modes

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
- [ ] Connection cancellation using backend pid/secret
- [ ] Server-side statement timeout
- [ ] Idle-in-transaction timeout
- [ ] Session variables beyond accepted `SET`
- [ ] Real PostgreSQL GUC system
- [ ] Connection pooling model
- [ ] Async I/O runtime

## Authentication and Security

- [x] Trust authentication
- [x] SCRAM-SHA-256 authentication via `PGRS_PASSWORD`
- [x] Hand-rolled SHA-256, HMAC, PBKDF2, and Base64 for SCRAM
- [ ] MD5 password authentication
- [ ] Cleartext password authentication mode
- [ ] Peer authentication
- [ ] Certificate authentication
- [ ] LDAP/PAM/GSS/SSPI authentication
- [ ] `pg_hba.conf`-style authentication rules
- [ ] Users and roles stored in system catalogs
- [ ] Role membership
- [ ] Privileges and GRANT/REVOKE
- [ ] Row-level security
- [ ] Security definer / invoker behavior
- [ ] Object ownership checks

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
- [ ] Common table expressions
- [ ] Recursive CTEs
- [ ] Window definitions
- [ ] LATERAL
- [ ] Array constructors
- [ ] Row constructors
- [ ] JSON path syntax
- [ ] Dollar-quoted strings
- [ ] Full interval syntax

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
- [ ] Schemas: `CREATE SCHEMA`, `DROP SCHEMA`, `SET search_path`
- [ ] `CREATE DATABASE` / `DROP DATABASE`
- [ ] Tablespaces
- [ ] Temporary tables
- [ ] Unlogged tables
- [ ] Views
- [ ] Materialized views
- [ ] Sequences as first-class objects
- [ ] `ALTER SEQUENCE`
- [ ] Generated columns
- [ ] Identity columns
- [ ] Table inheritance
- [ ] Partitioned tables
- [ ] Foreign tables
- [ ] Composite types
- [ ] Domains
- [ ] Enums
- [ ] Ranges
- [ ] Collations as catalog objects
- [ ] Operator classes and families

## Constraints

- [x] `NOT NULL`
- [x] `PRIMARY KEY` on single columns
- [x] `UNIQUE` via unique indexes
- [x] Duplicate key rejection for inserts and updates
- [x] Atomic rejection of duplicate keys inside a multi-row insert
- [x] Nullable unique values may repeat
- [ ] Multi-column primary keys
- [ ] Multi-column unique constraints
- [ ] Foreign keys
- [ ] Check constraints
- [ ] Exclusion constraints
- [ ] Deferrable constraints
- [ ] Constraint names and catalog storage
- [ ] `ALTER TABLE ADD CONSTRAINT`
- [ ] `ALTER TABLE DROP CONSTRAINT`
- [ ] Constraint validation / `NOT VALID`

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
- [ ] `INSERT ... DEFAULT VALUES`
- [ ] `INSERT ... SELECT`
- [ ] `INSERT ... ON CONFLICT`
- [ ] `MERGE`
- [ ] `TRUNCATE`
- [ ] Writable CTEs
- [ ] `UPDATE ... FROM`
- [ ] `DELETE ... USING`
- [ ] `OVERRIDING SYSTEM VALUE`

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
- [ ] Correlated subqueries
- [ ] CTEs with `WITH`
- [ ] Recursive queries
- [ ] Window functions
- [ ] `DISTINCT ON`
- [ ] `GROUPING SETS`, `ROLLUP`, and `CUBE`
- [ ] `FILTER (WHERE ...)` on aggregates
- [ ] Ordered-set aggregates
- [ ] Set operations: `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`
- [ ] LATERAL joins
- [ ] Row locking clauses: `FOR UPDATE`, `FOR SHARE`, `SKIP LOCKED`
- [ ] Cursor declarations and fetch

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
- [ ] `IS DISTINCT FROM`
- [ ] `IS NOT DISTINCT FROM`
- [ ] `ANY` / `SOME` / `ALL`
- [ ] Array operators
- [ ] JSON and JSONB operators
- [ ] Network operators
- [ ] Range operators
- [ ] Full text search operators
- [ ] User-defined operators

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
- [ ] `char(n)`
- [ ] `bytea`
- [ ] Arrays
- [ ] `interval`
- [ ] `timetz`
- [ ] `money`
- [ ] `inet`, `cidr`, `macaddr`, `macaddr8`
- [ ] Geometric types
- [ ] Range and multirange types
- [ ] Full JSONB binary semantics and indexing
- [ ] XML
- [ ] `tsvector` and `tsquery`
- [ ] Composite types
- [ ] Enum types
- [ ] Domain types

## Built-in Functions and Aggregates

- [x] `count`
- [x] `sum`
- [x] `avg`
- [x] `min`
- [x] `max`
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
- [ ] Date/time extraction and arithmetic
- [ ] `date_trunc`
- [ ] `generate_series`
- [ ] JSON/JSONB functions
- [ ] Array functions
- [ ] String function parity
- [ ] Math function parity
- [ ] Full text search functions
- [ ] Ordered-set aggregate functions
- [ ] Statistical aggregates
- [ ] User-defined SQL functions
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
- [ ] Multi-column indexes
- [ ] Expression indexes
- [ ] Partial indexes
- [ ] Covering indexes / INCLUDE columns
- [ ] BRIN indexes
- [ ] GIN indexes
- [ ] GiST indexes
- [ ] SP-GiST indexes
- [ ] Hash indexes
- [ ] Cost-based planner
- [ ] Planner statistics
- [ ] `ANALYZE`
- [ ] `EXPLAIN`
- [ ] `EXPLAIN ANALYZE`
- [ ] Join reordering
- [ ] Parallel query execution

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
- [ ] Snapshot isolation
- [ ] Read committed isolation
- [ ] Repeatable read isolation
- [ ] Serializable isolation
- [ ] Savepoints
- [ ] Two-phase commit
- [ ] Advisory locks
- [ ] Row-level locks
- [ ] Table locks
- [ ] Deadlock detection
- [ ] Concurrent writers without last-commit-wins behavior

## Storage and Durability

- [x] In-memory table storage
- [x] Stable internal row ids
- [x] Logical append-only WAL
- [x] WAL fsync after successful mutation
- [x] WAL replay on startup with `PGRS_DATA`
- [x] In-memory mode without `PGRS_DATA`
- [x] Serializable SQL emitted for supported mutating statements
- [ ] Disk-backed heap/table storage
- [ ] Page format
- [ ] Buffer manager
- [ ] Free space map
- [ ] Visibility map
- [ ] Physical WAL
- [ ] Checkpoints
- [ ] WAL segment management
- [ ] Crash recovery with partial record handling
- [ ] WAL compaction / log truncation
- [ ] Vacuum
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
- [ ] `psql \d <table>` support
- [ ] `pg_attribute`
- [ ] `pg_type`
- [ ] `pg_constraint`
- [ ] `pg_index`
- [ ] `pg_attrdef`
- [ ] `pg_description`
- [ ] `pg_depend`
- [ ] `pg_roles`
- [ ] `pg_user`
- [ ] `pg_database`
- [ ] `pg_settings`
- [ ] `pg_proc`
- [ ] `pg_operator`
- [ ] `pg_extension`
- [ ] `information_schema` view parity
- [ ] ORM-grade introspection for Prisma
- [ ] ORM-grade introspection for Drizzle Kit
- [ ] JDBC/ODBC metadata compatibility

## Import, Export, and Bulk I/O

- [ ] `COPY FROM STDIN`
- [ ] `COPY TO STDOUT`
- [ ] `COPY FROM/TO file`
- [ ] CSV copy options
- [ ] Binary copy format
- [ ] Large bulk insert optimization
- [ ] `pg_dump` compatibility
- [ ] `pg_restore` compatibility

## Replication, Backup, and High Availability

- [ ] Streaming replication protocol
- [ ] Physical replication slots
- [ ] Logical replication slots
- [ ] Publication/subscription
- [ ] WAL sender / receiver behavior
- [ ] Base backups
- [ ] Point-in-time recovery
- [ ] Hot standby reads
- [ ] Synchronous replication
- [ ] Timeline management

## Extensions and Procedural Systems

- [ ] `CREATE EXTENSION`
- [ ] `DROP EXTENSION`
- [ ] Extension catalog metadata
- [ ] Extension SQL install scripts
- [ ] C extension ABI
- [ ] `plpgsql`
- [ ] Other procedural languages
- [ ] Triggers
- [ ] Event triggers
- [ ] Rules
- [ ] User-defined functions
- [ ] User-defined aggregates
- [ ] User-defined types
- [ ] Foreign data wrappers
- [ ] `postgres_fdw`

## Administration and Maintenance Commands

- [ ] `VACUUM`
- [ ] `ANALYZE`
- [ ] `REINDEX`
- [ ] `CLUSTER`
- [ ] `CHECKPOINT`
- [ ] `DISCARD`
- [ ] `LISTEN`
- [ ] `NOTIFY`
- [ ] `UNLISTEN`
- [ ] `LOCK TABLE`
- [ ] `COMMENT ON`
- [ ] `SECURITY LABEL`
- [ ] `ALTER SYSTEM`
- [ ] `CREATE ROLE` / `ALTER ROLE` / `DROP ROLE`
- [ ] `CREATE USER` / `ALTER USER` / `DROP USER`
- [ ] `CREATE DATABASE` / `ALTER DATABASE` / `DROP DATABASE`

## Client and Ecosystem Compatibility

- [x] Connects with standard PostgreSQL clients for supported protocol paths
- [x] `psql` basic query workflow
- [x] `psql \dt`
- [ ] `psql \d <table>`
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
- [ ] Wire-protocol integration tests with real clients
- [ ] `psql` scripted compatibility tests
- [ ] ORM compatibility fixtures
- [ ] PostgreSQL sqllogictest-style suite
- [ ] Fuzzing for lexer/parser/protocol
- [ ] Crash-recovery fault injection
- [ ] Concurrency stress tests
- [ ] Benchmark suite for planner/storage changes

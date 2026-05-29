# Postgres Rust

A PostgreSQL compatible database written in Rust. 

## Run

```bash
cargo run --release -- 127.0.0.1:5433
psql -h 127.0.0.1 -p 5433 -U postgres
```

Optional:

- `PGRS_PASSWORD` — require SCRAM-SHA-256 auth
- `PGRS_DATA=<dir>` — persist data with a write-ahead log
- `PGRS_DISK=<dir>` — enable opt-in checkpointed page/heap storage
- `PGRS_EXTENSION_DIR=<dir>` — lookup directory for `LANGUAGE c` libraries

## Native C Functions

`CREATE FUNCTION ... LANGUAGE c` can load `PG_FUNCTION_ARGS`-style scalar
functions from a shared library without external Rust dependencies:

```sql
CREATE FUNCTION c_add(a integer, b integer) RETURNS integer
AS '/absolute/path/libexample.dylib', 'pgrs_c_add'
LANGUAGE c;
```

The supported fmgr subset uses `Datum`, `FunctionCallInfo`, nullable arguments,
and `fcinfo->isnull` returns for NULL handling. It currently covers scalar
NULL, integer, float, boolean, and varlena text values.
The call frame includes `FmgrInfo` metadata (`fcinfo->flinfo`, `fn_oid`,
`fn_nargs`, `fn_extra`) and per-call `palloc`/`palloc0`/`repalloc`/`pfree`
callbacks for transient allocations.
It also exposes a minimal `MemoryContext` API, including
`MemoryContextAlloc`, `MemoryContextAllocZero`, and `MemoryContextStrdup`, so
cached `fn_mcxt` allocations can survive across calls.
`CurrentMemoryContext` and `MemoryContextSwitchTo` are available for extensions
that follow PostgreSQL's usual switch-allocate-switch-back pattern.
`STRICT` / `RETURNS NULL ON NULL INPUT` functions short-circuit NULL arguments
without entering C and expose `fn_strict` in `FmgrInfo`.
The bundled `include/postgres_rs_fmgr.h` compatibility header also provides
common extension conveniences such as `PG_MODULE_MAGIC`,
`PG_FUNCTION_INFO_V1`, `palloc`/`pfree`, and `cstring_to_text` /
`text_to_cstring`.
It supports `elog(ERROR, ...)` and `ereport(ERROR, (errmsg(...)))` style
extension failures, which are propagated as SQL execution errors. Lower severity
messages such as `NOTICE` are accepted without aborting the call.
`GetConfigOptionByName` can read the live GUC snapshot from C functions, and
the header includes no-op custom-GUC registration shims (`DefineCustom*Variable`)
that initialize extension-owned defaults during `_PG_init`.
It also exposes a small syscache/catalog slice for type metadata:
`SearchSysCache1(TYPEOID, ...)`,
`SearchSysCache2(TYPENAMENSP, ..., PG_CATALOG_NAMESPACE_OID)`, `GETSTRUCT`,
`HeapTupleIsValid`, `ReleaseSysCache`, and `Form_pg_type`.
`SearchSysCache1(PROCOID, ObjectIdGetDatum(fcinfo->flinfo->fn_oid))` can also
return `Form_pg_proc` metadata for the currently executing C function.
Common catalog helper shims such as `get_typlenbyvalalign`, `get_typlen`,
`get_typbyval`, `get_typalign`, `format_type_be`, `get_func_name`,
`get_func_rettype`, and `get_func_nargs` are layered on top of that syscache
surface.
Loaded libraries are checked for `PG_MODULE_MAGIC`, each loaded C symbol must
expose `PG_FUNCTION_INFO_V1`, and `_PG_init` is called once when present.
It also includes a minimal SPI surface (`SPI_connect`, `SPI_execute`,
`SPI_exec`, `SPI_prepare`, `SPI_execute_plan`, `SPI_getvalue`, `SPI_tuptable`,
`SPI_getbinval`, `SPI_processed`, `SPI_result`) for engine-backed `SELECT`,
`INSERT`, `UPDATE`, and `DELETE` statements.

Extensions can also be installed from `PGRS_EXTENSION_DIR` using PostgreSQL-like
control and SQL files:

```text
my_ext.control
my_ext--1.0.sql
my_ext--1.0--2.0.sql
```

The control file supports `default_version` and `module_path`. SQL scripts may
use `MODULE_PATHNAME`, which is replaced with `module_path` before execution.
Functions created by extension scripts are recorded in `pg_depend` and are
removed when the extension is dropped.

## Test

```bash
cargo test
```

The workspace default includes every private crate, so this runs both the
facade/integration tests and internal unit tests.

## Project structure

This is a Cargo workspace with private internal crates. The root `postgres-rs`
crate remains the public facade, so existing paths such as
`postgres_rs::executor`, `postgres_rs::sql`, and `postgres_rs::server` continue
to work while the implementation is split by subsystem.

```text
crates/types      SQL types, values, and exact numeric arithmetic
crates/sql        Lexer, parser, AST, and SQL serialization
crates/auth       SCRAM/md5 auth, crypto helpers, and HBA matching
crates/protocol   PostgreSQL frontend/backend wire protocol
crates/engine     Executor, storage, indexes, locks, WAL, PL/pgSQL, disk store
crates/server     TCP server, sessions, transactions, COPY, bind handling
src/lib.rs        Compatibility facade over the private crates
src/main.rs       Thin CLI entrypoint
```

See [POSTGRES_ROADMAP.md](POSTGRES_ROADMAP.md) for planned work.

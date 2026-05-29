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

## Test

```bash
cargo test
```

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

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

## Test

```bash
cargo test
```

See [POSTGRES_ROADMAP.md](POSTGRES_ROADMAP.md) for planned work.

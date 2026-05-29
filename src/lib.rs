//! postgres-rs: a fast, lightweight, PostgreSQL wire-compatible database.
//!
//! This root crate is a compatibility facade over private workspace crates.
//! Existing callers can continue to use paths such as [`executor`], [`sql`],
//! [`storage`], and [`server`] while the implementation lives under `crates/`.

pub use postgres_auth::{auth, crypto, hba};
pub use postgres_engine::{disk, executor, index, lock, native, plpgsql, storage, wal};
pub use postgres_protocol as protocol;
pub use postgres_server::{bind, server};
pub use postgres_sql as sql;
pub use postgres_types::{BigDecimal, DataType, Value};
pub use postgres_types::{numeric, types};

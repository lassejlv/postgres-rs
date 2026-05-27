//! postgres-rs: a fast, lightweight, PostgreSQL wire-compatible database.
//!
//! The crate is split into:
//! - [`protocol`]: the PostgreSQL v3 frontend/backend wire format.
//! - [`sql`]: lexer, AST, and parser for the supported SQL subset.
//! - [`storage`]: the (currently in-memory) storage engine.
//! - [`executor`]: turns parsed statements into results.
//! - [`bind`]: extended-protocol parameter decoding/substitution.
//! - [`server`]: TCP server and per-connection session handling.

pub mod bind;
pub mod executor;
pub mod protocol;
pub mod server;
pub mod sql;
pub mod storage;
pub mod types;
pub mod wal;

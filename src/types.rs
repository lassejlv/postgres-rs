//! PostgreSQL data types, their wire OIDs, and runtime values.
//!
//! Type OIDs are fixed constants in PostgreSQL (defined in `pg_type.dat`).
//! Clients and drivers key off these exact numbers, so we must reproduce them.

use std::fmt;

/// A column's declared SQL type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// 16-bit integer (`smallint`).
    Int2,
    /// 32-bit integer (`integer`, `int4`).
    Int4,
    /// 64-bit integer (`bigint`, `int8`).
    Int8,
    /// 32-bit IEEE float (`real`).
    Float4,
    /// 64-bit IEEE float (`double precision`).
    Float8,
    /// Arbitrary-precision `numeric`/`decimal` (approximated as f64 for now).
    Numeric,
    /// Boolean.
    Bool,
    /// UTF-8 text of unbounded length.
    Text,
    /// `date` (stored as ISO text).
    Date,
    /// `time` (stored as ISO text).
    Time,
    /// `timestamp` without time zone (stored as ISO text).
    Timestamp,
    /// `timestamptz` (stored as ISO text).
    TimestampTz,
    /// `uuid` (stored as text).
    Uuid,
    /// `json` (stored as text).
    Json,
    /// `jsonb` (stored as text).
    Jsonb,
}

impl DataType {
    /// The stable PostgreSQL OID for this type, as found in `pg_type`.
    pub fn oid(self) -> i32 {
        match self {
            DataType::Bool => 16,
            DataType::Int8 => 20,
            DataType::Int2 => 21,
            DataType::Int4 => 23,
            DataType::Text => 25,
            DataType::Json => 114,
            DataType::Float4 => 700,
            DataType::Float8 => 701,
            DataType::Date => 1082,
            DataType::Time => 1083,
            DataType::Timestamp => 1114,
            DataType::TimestampTz => 1184,
            DataType::Numeric => 1700,
            DataType::Uuid => 2950,
            DataType::Jsonb => 3802,
        }
    }

    /// The fixed wire size in bytes, or -1 for variable-length types.
    pub fn type_size(self) -> i16 {
        match self {
            DataType::Bool => 1,
            DataType::Int2 => 2,
            DataType::Int4 | DataType::Float4 | DataType::Date => 4,
            DataType::Int8 | DataType::Float8 | DataType::Time | DataType::Timestamp
            | DataType::TimestampTz => 8,
            DataType::Uuid => 16,
            DataType::Numeric
            | DataType::Text
            | DataType::Json
            | DataType::Jsonb => -1,
        }
    }

    /// Whether values of this type are represented as text internally.
    pub fn is_text_stored(self) -> bool {
        matches!(
            self,
            DataType::Text
                | DataType::Date
                | DataType::Time
                | DataType::Timestamp
                | DataType::TimestampTz
                | DataType::Uuid
                | DataType::Json
                | DataType::Jsonb
        )
    }

    /// Parse a type name as it appears in SQL (case-insensitive).
    pub fn from_sql_name(name: &str) -> Option<DataType> {
        let n = name.to_ascii_lowercase();
        Some(match n.as_str() {
            "smallint" | "int2" => DataType::Int2,
            "integer" | "int" | "int4" => DataType::Int4,
            "bigint" | "int8" => DataType::Int8,
            "real" | "float4" => DataType::Float4,
            "double precision" | "float8" | "float" => DataType::Float8,
            "numeric" | "decimal" => DataType::Numeric,
            "boolean" | "bool" => DataType::Bool,
            "text" | "varchar" | "char" | "character varying" | "character" | "bpchar"
            | "name" => DataType::Text,
            "date" => DataType::Date,
            "time" | "time without time zone" | "time with time zone" => DataType::Time,
            "timestamp" | "timestamp without time zone" => DataType::Timestamp,
            "timestamptz" | "timestamp with time zone" => DataType::TimestampTz,
            "uuid" => DataType::Uuid,
            "json" => DataType::Json,
            "jsonb" => DataType::Jsonb,
            _ => return None,
        })
    }

    pub fn sql_name(self) -> &'static str {
        match self {
            DataType::Int2 => "smallint",
            DataType::Int4 => "integer",
            DataType::Int8 => "bigint",
            DataType::Float4 => "real",
            DataType::Float8 => "double precision",
            DataType::Numeric => "numeric",
            DataType::Bool => "boolean",
            DataType::Text => "text",
            DataType::Date => "date",
            DataType::Time => "time without time zone",
            DataType::Timestamp => "timestamp without time zone",
            DataType::TimestampTz => "timestamp with time zone",
            DataType::Uuid => "uuid",
            DataType::Json => "json",
            DataType::Jsonb => "jsonb",
        }
    }
}

/// A concrete runtime value occupying one cell of a row.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Render this value into its PostgreSQL text-format representation.
    ///
    /// Returns `None` for SQL NULL, which is encoded specially on the wire.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Int(i) => Some(i.to_string()),
            Value::Float(f) => Some(format_float(*f)),
            Value::Text(s) => Some(s.clone()),
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
        }
    }

    /// The natural type of this value, used when no column type is known
    /// (e.g. a literal in a SELECT list).
    pub fn inferred_type(&self) -> DataType {
        match self {
            Value::Null => DataType::Text,
            Value::Int(_) => DataType::Int8,
            Value::Float(_) => DataType::Float8,
            Value::Text(_) => DataType::Text,
            Value::Bool(_) => DataType::Bool,
        }
    }

    /// SQL three-valued-logic truthiness, used by `WHERE`.
    /// NULL is treated as "not true".
    pub fn is_true(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Null => false,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Text(s) => !s.is_empty(),
        }
    }
}

/// Format a float the way PostgreSQL does: integral values keep no decimals
/// beyond what `f64` round-trips, NaN/Inf use the SQL spellings.
fn format_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        // Rust's default float formatting round-trips and avoids trailing
        // zeros, which matches PostgreSQL closely enough for now.
        let s = format!("{f}");
        s
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_text() {
            Some(s) => write!(f, "{s}"),
            None => write!(f, "NULL"),
        }
    }
}

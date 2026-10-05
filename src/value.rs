//! The one runtime value type used across the storage layer and the
//! query engine — row input when loading data, decoded output when
//! scanning, and literals when a query compares a column to a constant.
//! Keeping a single `Value` type (rather than one type per layer plus
//! conversions) is deliberate: this project is small enough that the
//! conversions would be pure overhead, not safety.
//!
//! Only `Int64` and `Utf8` can be *stored*. `Float64` and `Null` exist
//! purely as query outputs: `AVG` produces a float, and an aggregate over
//! zero rows (`SELECT MAX(x) FROM t WHERE <nothing matches>`) produces
//! `NULL`, exactly as SQL specifies.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Utf8(String),
}

impl Value {
    pub fn as_int64(&self) -> Option<i64> {
        match self {
            Value::Int64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_utf8(&self) -> Option<&str> {
        match self {
            Value::Utf8(v) => Some(v),
            _ => None,
        }
    }

    /// Total order used by `ORDER BY`. `NULL` sorts before every other
    /// value (SQL leaves this implementation-defined; Stratum picks one
    /// and sticks to it). Values of different types never meet in
    /// practice — an output column has one type — so the cross-type arm
    /// only needs to be consistent, not meaningful.
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::Int64(a), Value::Int64(b)) => a.cmp(b),
            (Value::Float64(a), Value::Float64(b)) => a.total_cmp(b),
            (Value::Utf8(a), Value::Utf8(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::Utf8(v) => write!(f, "{v}"),
        }
    }
}

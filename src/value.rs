//! The one runtime value type used across the storage layer and the
//! query engine — row input when loading data, decoded output when
//! scanning, and literals when a query compares a column to a constant.
//! Keeping a single `Value` type (rather than one type per layer plus
//! conversions) is deliberate: this project is small enough that the
//! conversions would be pure overhead, not safety.

use std::fmt;

#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum Value {
    Int64(i64),
    Utf8(String),
}

impl Value {
    pub fn as_int64(&self) -> Option<i64> {
        match self {
            Value::Int64(v) => Some(*v),
            Value::Utf8(_) => None,
        }
    }

    pub fn as_utf8(&self) -> Option<&str> {
        match self {
            Value::Utf8(v) => Some(v),
            Value::Int64(_) => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int64(v) => write!(f, "{v}"),
            Value::Utf8(v) => write!(f, "{v}"),
        }
    }
}

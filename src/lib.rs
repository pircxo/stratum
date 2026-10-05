//! Stratum: a small columnar storage engine and SQL-subset query
//! processor. See `README.md` for what it's for and `ARCHITECTURE.md`
//! for why it's built the way it is.

pub mod encoding;
pub mod query;
pub mod storage;
pub mod value;

pub use storage::{ColumnSchema, DataType, TableReader, TableWriter};
pub use value::Value;

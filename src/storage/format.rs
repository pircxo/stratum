//! The on-disk file layout.
//!
//! A `.strat` file is a sequence of **row groups** (the same idea Parquet
//! uses): a row group is a batch of up to [`BLOCK_SIZE`] rows, stored
//! column-by-column ("column chunks") so a scan that only needs two of a
//! table's ten columns never has to touch the other eight. After all row
//! groups, a **footer** describes where every chunk lives and, for integer
//! columns, each chunk's min/max — the "zone map" that lets the query
//! engine skip whole row groups without decoding them (see
//! `query::executor`).
//!
//! ```text
//! [row group 0: column 0 chunk][column 1 chunk]...[column N-1 chunk]
//! [row group 1: column 0 chunk][column 1 chunk]...[column N-1 chunk]
//! ...
//! [footer: bincode-serialized FileFooter]
//! [footer_len: u64 little-endian]
//! [magic: 8 bytes, "STRTFTR1"]
//! ```
//!
//! Putting the footer at the end (rather than a fixed-size header at the
//! start) means a writer never has to know the final layout in advance or
//! seek backward to patch a header — it can stream row groups out as they
//! fill up and only needs to remember metadata in memory until the very
//! end. This is the same trade-off Parquet makes.

use serde::{Deserialize, Serialize};

/// Rows per row group. Chosen as a round, "page-like" size — large enough
/// that per-chunk overhead (the footer entry, the dictionary header for
/// string columns) is negligible, small enough that a selective query
/// still benefits from skipping groups it doesn't need. Real systems tune
/// this per workload; Stratum hard-codes one value and says so.
pub const BLOCK_SIZE: usize = 8192;

pub const FOOTER_MAGIC: &[u8; 8] = b"STRTFTR1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    Int64,
    Utf8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnSchema {
    pub name: String,
    pub dtype: DataType,
}

/// Where one column's data for one row group lives, plus its zone map.
/// `min`/`max` are only ever `Some` for `Int64` columns — string zone maps
/// are deliberately out of scope (see ARCHITECTURE.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMeta {
    pub offset: u64,
    pub length: u64,
    pub min: Option<i64>,
    pub max: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowGroupMeta {
    pub row_count: u32,
    /// Same order as `FileFooter::columns`.
    pub chunks: Vec<ChunkMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFooter {
    pub row_count: u64,
    pub columns: Vec<ColumnSchema>,
    pub row_groups: Vec<RowGroupMeta>,
}

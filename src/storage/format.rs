//! The on-disk file layout.
//!
//! A `.strat` file is a sequence of **row groups** (the same idea Parquet
//! uses): a row group is a batch of up to [`BLOCK_SIZE`] rows, stored
//! column-by-column ("column chunks") so a scan that only needs two of a
//! table's ten columns never has to touch the other eight. After all row
//! groups, a **footer** describes where every chunk lives, how it is
//! encoded, and its min/max — the "zone map" that lets the query engine
//! skip whole row groups without decoding them (see `query::executor`).
//!
//! ```text
//! [row group 0: column 0 chunk][column 1 chunk]...[column N-1 chunk]
//! [row group 1: column 0 chunk][column 1 chunk]...[column N-1 chunk]
//! ...
//! [footer: bincode-serialized FileFooter]
//! [footer_len: u64 little-endian]
//! [magic: 8 bytes, "STRTFTR2"]
//! ```
//!
//! Putting the footer at the end (rather than a fixed-size header at the
//! start) means a writer never has to know the final layout in advance or
//! seek backward to patch a header — it can stream row groups out as they
//! fill up and only needs to remember metadata in memory until the very
//! end. This is the same trade-off Parquet makes.

use bincode::Options;
use serde::{Deserialize, Serialize};

/// Default rows per row group. Chosen as a round, "page-like" size —
/// large enough that per-chunk overhead (the footer entry, the dictionary
/// header for string columns) is negligible, small enough that a
/// selective query still benefits from skipping groups it doesn't need.
/// Overridable per file via `TableWriter::with_row_group_size`, mainly so
/// tests can exercise many row groups with little data.
pub const BLOCK_SIZE: usize = 8192;

/// Format version 2: per-chunk encodings (delta / bit-packing) and string
/// zone maps. Version 1 files (`STRTFTR1`, Stratum v0.1) are recognised
/// and rejected with a clear message rather than misread.
pub const FOOTER_MAGIC: &[u8; 8] = b"STRTFTR2";
pub const FOOTER_MAGIC_V1: &[u8; 8] = b"STRTFTR1";

/// Upper bound on footer size. A corrupt `footer_len` must not be able
/// to make the reader allocate gigabytes before deserialization fails.
pub const MAX_FOOTER_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    Int64,
    Utf8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnSchema {
    pub name: String,
    pub dtype: DataType,
}

/// How one chunk's bytes are laid out. Integer chunks pick whichever of
/// the first three is smallest for *that chunk's* data (see
/// `encoding::encode_int64_chunk`); string chunks are always dictionary
/// encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding {
    /// 8 bytes per value, little-endian.
    Plain,
    /// Frame of reference: store the chunk minimum once, then each value
    /// as `value - min` packed into just enough bits for the chunk's range.
    BitPacked,
    /// Store the first value, then each consecutive difference,
    /// frame-of-reference bit-packed. Near-free for ids and timestamps.
    DeltaBitPacked,
    /// Distinct strings once, then one bit-packed dictionary index per row.
    Dictionary,
}

/// A chunk's min and max value. Pruning compares predicates against this
/// instead of the data — see `query::executor::can_skip`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZoneMap {
    Int64 { min: i64, max: i64 },
    Utf8 { min: String, max: String },
}

/// Where one column's data for one row group lives, how it's encoded,
/// and its zone map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMeta {
    pub offset: u64,
    pub length: u64,
    pub encoding: Encoding,
    pub zone_map: Option<ZoneMap>,
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

/// The one bincode configuration used to both write and read footers, so
/// the two sides can't drift apart, with a hard size limit on reads.
pub(crate) fn footer_codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_FOOTER_BYTES)
}

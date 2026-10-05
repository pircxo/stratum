//! Opens a `.strat` file and serves decoded column chunks on demand.
//!
//! Reads are *positioned* (`pread` on Unix, `ReadFile` with an explicit
//! offset on Windows) rather than `seek` + `read`, specifically so
//! multiple threads can read different chunks of the same open file
//! concurrently without a shared cursor or a lock — which is what lets
//! `query::executor` scan row groups in parallel with nothing fancier
//! than a shared `&TableReader`.
//!
//! The footer is validated once on open (every chunk inside the data
//! region, one chunk per column, row counts adding up), so a truncated or
//! corrupt file fails at `open` with a clear message instead of
//! mid-query.

use std::fs::File;
use std::io;
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use bincode::Options;

use crate::encoding::{decode_int64_chunk, decode_utf8_chunk, DictArray};
use crate::storage::format::{
    footer_codec, ColumnSchema, DataType, Encoding, FileFooter, RowGroupMeta, ZoneMap, BLOCK_SIZE,
    FOOTER_MAGIC, FOOTER_MAGIC_V1, MAX_FOOTER_BYTES,
};
use crate::storage::writer::validate_schema;
use crate::value::Value;

/// One decoded chunk. Strings stay dictionary-encoded after decoding —
/// see [`DictArray`] for why.
#[derive(Debug, Clone)]
pub enum ColumnArray {
    Int64(Vec<i64>),
    Utf8(DictArray),
}

impl ColumnArray {
    pub fn len(&self) -> usize {
        match self {
            ColumnArray::Int64(v) => v.len(),
            ColumnArray::Utf8(d) => d.indices.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Value {
        match self {
            ColumnArray::Int64(v) => Value::Int64(v[i]),
            ColumnArray::Utf8(d) => Value::Utf8(d.get(i).to_string()),
        }
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    // `read_exact_at`, not `read_at`: a single pread may legally return
    // fewer bytes than asked for.
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    // `seek_read` passes the offset per call (OVERLAPPED), so concurrent
    // calls don't race on the cursor even though they move it.
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub struct TableReader {
    file: File,
    footer: FileFooter,
    file_len: u64,
}

impl TableReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();
        let name = path.display();
        if len < 16 {
            bail!("{name}: too small to be a valid .strat file");
        }

        let mut trailer = [0u8; 16];
        read_exact_at(&file, &mut trailer, len - 16).context("reading file trailer")?;
        let footer_len = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let magic = &trailer[8..16];
        if magic == FOOTER_MAGIC_V1 {
            bail!("{name}: written by Stratum v0.1 (format 1), which this version can't read — re-load it from the source CSV");
        }
        if magic != FOOTER_MAGIC {
            bail!("{name}: bad magic bytes — not a stratum file, or it's corrupt");
        }
        if footer_len > MAX_FOOTER_BYTES || footer_len + 16 > len {
            bail!("{name}: corrupt footer length {footer_len}");
        }

        let footer_start = len - 16 - footer_len;
        let mut footer_bytes = vec![0u8; footer_len as usize];
        read_exact_at(&file, &mut footer_bytes, footer_start).context("reading footer")?;
        let footer: FileFooter = footer_codec()
            .deserialize(&footer_bytes)
            .with_context(|| format!("{name}: corrupt footer"))?;
        validate_footer(&footer, footer_start)
            .with_context(|| format!("{name}: corrupt footer"))?;

        Ok(Self {
            file,
            footer,
            file_len: len,
        })
    }

    pub fn schema(&self) -> &[ColumnSchema] {
        &self.footer.columns
    }

    pub fn row_groups(&self) -> &[RowGroupMeta] {
        &self.footer.row_groups
    }

    pub fn row_count(&self) -> u64 {
        self.footer.row_count
    }

    pub fn file_len(&self) -> u64 {
        self.file_len
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.footer.columns.iter().position(|c| c.name == name)
    }

    /// Reads and decodes one column's chunk for one row group. Safe to
    /// call from multiple threads at once on a shared `&TableReader`.
    pub fn read_column_chunk(
        &self,
        row_group_idx: usize,
        column_idx: usize,
    ) -> Result<ColumnArray> {
        let rg = self
            .footer
            .row_groups
            .get(row_group_idx)
            .with_context(|| format!("no row group {row_group_idx}"))?;
        let chunk = rg
            .chunks
            .get(column_idx)
            .with_context(|| format!("no column {column_idx}"))?;
        let rows = rg.row_count as usize;
        let ctx = || format!("row group {row_group_idx}, column {column_idx}");

        let length = usize::try_from(chunk.length).context("chunk too large for this platform")?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .context("allocating column chunk")?;
        bytes.resize(length, 0);
        read_exact_at(&self.file, &mut bytes, chunk.offset).with_context(ctx)?;

        Ok(match self.footer.columns[column_idx].dtype {
            DataType::Int64 => ColumnArray::Int64(
                decode_int64_chunk(chunk.encoding, &bytes, rows).with_context(ctx)?,
            ),
            DataType::Utf8 => ColumnArray::Utf8(decode_utf8_chunk(&bytes, rows).with_context(ctx)?),
        })
    }
}

fn validate_footer(footer: &FileFooter, data_end: u64) -> Result<()> {
    validate_schema(&footer.columns)?;
    let mut rows = 0u64;
    let mut offset = 0u64;
    for (i, rg) in footer.row_groups.iter().enumerate() {
        ensure!(
            (1..=BLOCK_SIZE as u32).contains(&rg.row_count),
            "row group {i} claims {} rows",
            rg.row_count
        );
        ensure!(
            rg.chunks.len() == footer.columns.len(),
            "row group {i} has {} chunks for {} columns",
            rg.chunks.len(),
            footer.columns.len()
        );
        for (chunk, col) in rg.chunks.iter().zip(&footer.columns) {
            ensure!(
                chunk.offset == offset && chunk.length > 0,
                "invalid chunk offset or length"
            );
            let end = chunk.offset.checked_add(chunk.length);
            ensure!(
                end.is_some_and(|e| e <= data_end),
                "row group {i}, column '{}': chunk lies outside the data region",
                col.name
            );
            offset = end.unwrap();
            let consistent = matches!(
                (col.dtype, chunk.encoding, &chunk.zone_map),
                (
                    DataType::Int64,
                    Encoding::Plain | Encoding::BitPacked | Encoding::DeltaBitPacked,
                    Some(ZoneMap::Int64 { .. })
                ) | (
                    DataType::Utf8,
                    Encoding::Dictionary,
                    Some(ZoneMap::Utf8 { .. })
                )
            );
            ensure!(
                consistent,
                "row group {i}, column '{}': encoding/zone map don't match type {:?}",
                col.name,
                col.dtype
            );
            let ordered = match &chunk.zone_map {
                Some(ZoneMap::Int64 { min, max }) => min <= max,
                Some(ZoneMap::Utf8 { min, max }) => min <= max,
                None => false,
            };
            ensure!(
                ordered,
                "row group {i}, column '{}': zone map min > max",
                col.name
            );
        }
        rows += rg.row_count as u64;
    }
    ensure!(
        rows == footer.row_count,
        "row groups hold {rows} rows but footer says {}",
        footer.row_count
    );
    ensure!(
        offset == data_end,
        "column chunks do not cover the data region"
    );
    Ok(())
}

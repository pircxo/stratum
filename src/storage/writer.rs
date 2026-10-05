//! Streams rows into a `.strat` file, buffering up to one row group of
//! rows at a time and flushing each full buffer as a row group. The
//! writer never needs to know the total row count in advance and never
//! seeks backward — see `storage::format` for why that's the point.
//!
//! [`TableWriter::create`] refuses to overwrite an existing file. Callers
//! that want all-or-nothing output (the CLI's `load`) write through
//! [`TableWriter::new`] into a temp file and rename it into place only
//! once `finish` succeeds.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use bincode::Options;

use crate::encoding::encode_column_chunk;
use crate::query::lexer::is_keyword;
use crate::storage::format::{
    footer_codec, ChunkMeta, ColumnSchema, DataType, FileFooter, RowGroupMeta, BLOCK_SIZE,
    FOOTER_MAGIC,
};
use crate::value::Value;

pub struct TableWriter {
    file: BufWriter<File>,
    schema: Vec<ColumnSchema>,
    buffers: Vec<Vec<Value>>,
    row_groups: Vec<RowGroupMeta>,
    row_group_size: usize,
    total_rows: u64,
    pos: u64,
}

impl TableWriter {
    /// Creates a new table file. Fails if `path` already exists, or if
    /// the schema is invalid — in both cases without touching the disk.
    pub fn create(path: &Path, schema: Vec<ColumnSchema>) -> Result<Self> {
        validate_schema(&schema)?;
        let file =
            File::create_new(path).with_context(|| format!("creating {}", path.display()))?;
        Self::new(file, schema)
    }

    /// Writes a table into an already-open, empty file.
    pub fn new(mut file: File, schema: Vec<ColumnSchema>) -> Result<Self> {
        validate_schema(&schema)?;
        ensure!(file.metadata()?.len() == 0, "output file must be empty");
        file.rewind()?;
        let buffers = schema.iter().map(|_| Vec::new()).collect();
        Ok(Self {
            file: BufWriter::new(file),
            schema,
            buffers,
            row_groups: Vec::new(),
            row_group_size: BLOCK_SIZE,
            total_rows: 0,
            pos: 0,
        })
    }

    /// Uses smaller row groups than [`BLOCK_SIZE`] (which is also the
    /// maximum the reader accepts). Smaller groups mean finer pruning but
    /// more per-chunk overhead; tests use tiny groups to get many of them
    /// out of little data.
    pub fn with_row_group_size(mut self, rows: usize) -> Self {
        assert!(
            (1..=BLOCK_SIZE).contains(&rows),
            "row group size must be in 1..={BLOCK_SIZE}"
        );
        self.row_group_size = rows;
        self
    }

    pub fn schema(&self) -> &[ColumnSchema] {
        &self.schema
    }

    /// Appends one row. `row` must have exactly one value per schema
    /// column, in schema order, each of the column's type — checked, not
    /// assumed, because a silent mismatch would corrupt every row group
    /// written after it.
    pub fn add_row(&mut self, row: Vec<Value>) -> Result<()> {
        ensure!(
            row.len() == self.schema.len(),
            "row has {} values but schema has {} columns",
            row.len(),
            self.schema.len()
        );
        for (value, col) in row.iter().zip(&self.schema) {
            let ok = matches!(
                (value, col.dtype),
                (Value::Int64(_), DataType::Int64) | (Value::Utf8(_), DataType::Utf8)
            );
            if !ok {
                bail!(
                    "column '{}' is {:?}, but the row gives it {value:?}",
                    col.name,
                    col.dtype
                );
            }
        }
        for (col_buf, value) in self.buffers.iter_mut().zip(row) {
            col_buf.push(value);
        }
        self.total_rows += 1;
        if self.buffers[0].len() >= self.row_group_size {
            self.flush_row_group()?;
        }
        Ok(())
    }

    fn flush_row_group(&mut self) -> Result<()> {
        let row_count = self.buffers[0].len();
        if row_count == 0 {
            return Ok(());
        }

        let mut chunks = Vec::with_capacity(self.schema.len());
        for col_buf in self.buffers.iter_mut() {
            let values = std::mem::take(col_buf);
            let (encoding, bytes, zone_map) = encode_column_chunk(&values);
            self.file.write_all(&bytes)?;
            chunks.push(ChunkMeta {
                offset: self.pos,
                length: bytes.len() as u64,
                encoding,
                zone_map,
            });
            self.pos += bytes.len() as u64;
        }

        self.row_groups.push(RowGroupMeta {
            row_count: row_count as u32,
            chunks,
        });
        Ok(())
    }

    /// Flushes any buffered rows as a final (possibly short) row group,
    /// writes the footer, and fsyncs. Consumes `self` so a caller can't
    /// accidentally `add_row` after the footer's already written.
    pub fn finish(mut self) -> Result<()> {
        self.flush_row_group()?;

        let footer = FileFooter {
            row_count: self.total_rows,
            columns: self.schema,
            row_groups: self.row_groups,
        };
        let footer_bytes = footer_codec()
            .serialize(&footer)
            .context("footer exceeds the maximum footer size")?;

        self.file.write_all(&footer_bytes)?;
        self.file
            .write_all(&(footer_bytes.len() as u64).to_le_bytes())?;
        self.file.write_all(FOOTER_MAGIC)?;
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(())
    }
}

/// Column names must be usable unquoted in a query: non-empty
/// identifiers that aren't SQL keywords, and unique.
pub(crate) fn validate_schema(schema: &[ColumnSchema]) -> Result<()> {
    ensure!(!schema.is_empty(), "a table needs at least one column");
    for (i, col) in schema.iter().enumerate() {
        let name = &col.name;
        let mut chars = name.chars();
        let valid_ident = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
            && chars.all(|c| c.is_alphanumeric() || c == '_');
        ensure!(
            valid_ident,
            "invalid column name {name:?}: use letters, digits and '_', not starting with a digit"
        );
        ensure!(
            !is_keyword(name),
            "invalid column name {name:?}: it's a reserved SQL keyword"
        );
        ensure!(
            !schema[..i].iter().any(|c| &c.name == name),
            "duplicate column name {name:?}"
        );
    }
    Ok(())
}

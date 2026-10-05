//! Streams rows into a `.strat` file, buffering up to [`BLOCK_SIZE`] rows
//! at a time and flushing them as one row group per call to
//! [`TableWriter::finish`] or whenever a buffer fills. The writer never
//! needs to know the total row count in advance and never seeks
//! backward — see `storage::format` for why that's the point.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::encoding::encode_column_chunk;
use crate::storage::format::{
    ChunkMeta, ColumnSchema, FileFooter, RowGroupMeta, BLOCK_SIZE, FOOTER_MAGIC,
};
use crate::value::Value;

pub struct TableWriter {
    file: BufWriter<File>,
    schema: Vec<ColumnSchema>,
    buffers: Vec<Vec<Value>>,
    row_groups: Vec<RowGroupMeta>,
    total_rows: u64,
    pos: u64,
}

impl TableWriter {
    pub fn create(path: &Path, schema: Vec<ColumnSchema>) -> std::io::Result<Self> {
        let file = File::create(path)?;
        let buffers = schema
            .iter()
            .map(|_| Vec::with_capacity(BLOCK_SIZE))
            .collect();
        Ok(Self {
            file: BufWriter::new(file),
            schema,
            buffers,
            row_groups: Vec::new(),
            total_rows: 0,
            pos: 0,
        })
    }

    pub fn schema(&self) -> &[ColumnSchema] {
        &self.schema
    }

    /// Appends one row. `row` must have exactly one value per schema
    /// column, in schema order — this is checked, not just assumed,
    /// because a silent column-count mismatch would corrupt every row
    /// group written after it.
    pub fn add_row(&mut self, row: Vec<Value>) -> std::io::Result<()> {
        assert_eq!(
            row.len(),
            self.schema.len(),
            "row has {} values but schema has {} columns",
            row.len(),
            self.schema.len()
        );
        for (col_buf, value) in self.buffers.iter_mut().zip(row) {
            col_buf.push(value);
        }
        self.total_rows += 1;
        if self.buffers[0].len() >= BLOCK_SIZE {
            self.flush_row_group()?;
        }
        Ok(())
    }

    fn flush_row_group(&mut self) -> std::io::Result<()> {
        let row_count = self.buffers[0].len();
        if row_count == 0 {
            return Ok(());
        }

        let mut chunks = Vec::with_capacity(self.schema.len());
        for col_buf in self.buffers.iter_mut() {
            let values = std::mem::take(col_buf);
            let (bytes, min, max) = encode_column_chunk(&values);
            self.file.write_all(&bytes)?;
            chunks.push(ChunkMeta {
                offset: self.pos,
                length: bytes.len() as u64,
                min,
                max,
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
    /// writes the footer, and syncs to disk. Consumes `self` so a caller
    /// can't accidentally `add_row` after the footer's already written.
    pub fn finish(mut self) -> std::io::Result<()> {
        self.flush_row_group()?;

        let footer = FileFooter {
            row_count: self.total_rows,
            columns: self.schema,
            row_groups: self.row_groups,
        };
        let footer_bytes =
            bincode::serialize(&footer).expect("in-memory footer is always serializable");

        self.file.write_all(&footer_bytes)?;
        self.file
            .write_all(&(footer_bytes.len() as u64).to_le_bytes())?;
        self.file.write_all(FOOTER_MAGIC)?;
        self.file.flush()
    }
}

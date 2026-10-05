//! Opens a `.strat` file and serves decoded column chunks on demand.
//!
//! Reads go through [`std::os::unix::fs::FileExt::read_at`] (a positioned
//! read / `pread`) rather than `seek` + `read`, specifically so multiple
//! threads can read different chunks of the same open file concurrently
//! without a shared cursor or a lock — which is what lets
//! `query::executor` scan row groups in parallel with nothing fancier
//! than a shared `&TableReader` (see `executor::execute`).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::encoding::{decode_int64_chunk, decode_utf8_chunk};
use crate::storage::format::{ColumnSchema, DataType, FileFooter, RowGroupMeta, FOOTER_MAGIC};
use crate::value::Value;

pub enum ColumnArray {
    Int64(Vec<i64>),
    Utf8(Vec<String>),
}

impl ColumnArray {
    pub fn len(&self) -> usize {
        match self {
            ColumnArray::Int64(v) => v.len(),
            ColumnArray::Utf8(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Value {
        match self {
            ColumnArray::Int64(v) => Value::Int64(v[i]),
            ColumnArray::Utf8(v) => Value::Utf8(v[i].clone()),
        }
    }
}

pub struct TableReader {
    file: File,
    footer: FileFooter,
}

impl TableReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();
        if len < 16 {
            bail!("{}: too small to be a valid .strat file", path.display());
        }

        let mut trailer = [0u8; 16];
        file.read_at(&mut trailer, len - 16)
            .context("reading file trailer")?;
        let footer_len = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
        let magic = &trailer[8..16];
        if magic != FOOTER_MAGIC {
            bail!(
                "{}: bad magic bytes — not a stratum file, or it's corrupt",
                path.display()
            );
        }
        if footer_len + 16 > len {
            bail!("{}: corrupt footer length", path.display());
        }

        let footer_start = len - 16 - footer_len;
        let mut footer_bytes = vec![0u8; footer_len as usize];
        file.read_at(&mut footer_bytes, footer_start)
            .context("reading footer")?;
        let footer: FileFooter =
            bincode::deserialize(&footer_bytes).context("deserializing footer")?;

        Ok(Self { file, footer })
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
        let rg = &self.footer.row_groups[row_group_idx];
        let chunk = &rg.chunks[column_idx];

        let mut bytes = vec![0u8; chunk.length as usize];
        self.file
            .read_at(&mut bytes, chunk.offset)
            .with_context(|| format!("reading row group {row_group_idx} column {column_idx}"))?;

        Ok(match self.footer.columns[column_idx].dtype {
            DataType::Int64 => {
                ColumnArray::Int64(decode_int64_chunk(&bytes, rg.row_count as usize))
            }
            DataType::Utf8 => ColumnArray::Utf8(decode_utf8_chunk(&bytes, rg.row_count as usize)),
        })
    }
}

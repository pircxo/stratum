//! Turns a parsed [`SelectStmt`] into a result set.
//!
//! The scan is organised around row groups, the same unit the storage
//! layer uses (`storage::format`): for each row group, first ask "can
//! this be skipped entirely?" using the zone map (`is_prunable`), and
//! only if not, decode its columns and evaluate the predicates row by
//! row. Row groups are independent of each other — nothing about scanning
//! one depends on another — which is exactly what makes it safe to hand
//! them to `rayon` and scan several at once (`ExecuteOptions::parallel`).

use std::cmp::Ordering;
use std::collections::HashMap;

use anyhow::{bail, Result};
use rayon::prelude::*;

use crate::storage::{ColumnArray, DataType, TableReader};
use crate::value::Value;

use super::ast::{CompareOp, Literal, Predicate, SelectColumns, SelectStmt};

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub row_groups_total: usize,
    pub row_groups_scanned: usize,
    pub rows_scanned: usize,
    pub rows_returned: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecuteOptions {
    pub use_pruning: bool,
    pub parallel: bool,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            use_pruning: true,
            parallel: true,
        }
    }
}

pub fn execute(
    reader: &TableReader,
    stmt: &SelectStmt,
    opts: &ExecuteOptions,
) -> Result<(QueryResult, ScanStats)> {
    let projected: Vec<String> = match &stmt.columns {
        SelectColumns::Star => reader.schema().iter().map(|c| c.name.clone()).collect(),
        SelectColumns::List(cols) => cols.clone(),
    };

    for name in &projected {
        require_column(reader, name)?;
    }
    for pred in &stmt.predicates {
        require_column(reader, &pred.column)?;
        check_predicate_type(reader, pred)?;
    }
    if let Some(ob) = &stmt.order_by {
        require_column(reader, &ob.column)?;
    }

    // Everything that has to be decoded per row group: the projection,
    // plus whatever WHERE and ORDER BY touch but didn't ask to see.
    let mut needed: Vec<String> = projected.clone();
    for pred in &stmt.predicates {
        if !needed.contains(&pred.column) {
            needed.push(pred.column.clone());
        }
    }
    if let Some(ob) = &stmt.order_by {
        if !needed.contains(&ob.column) {
            needed.push(ob.column.clone());
        }
    }
    let needed_idx: Vec<usize> = needed
        .iter()
        .map(|c| reader.column_index(c).expect("already validated above"))
        .collect();

    let total_groups = reader.row_groups().len();

    let scan_one_group = |rg_idx: usize| -> Result<Option<(Vec<Vec<Value>>, usize)>> {
        if opts.use_pruning && is_prunable(reader, &stmt.predicates, rg_idx) {
            return Ok(None);
        }

        let row_count = reader.row_groups()[rg_idx].row_count as usize;
        let mut decoded: HashMap<usize, ColumnArray> = HashMap::with_capacity(needed_idx.len());
        for &idx in &needed_idx {
            decoded.insert(idx, reader.read_column_chunk(rg_idx, idx)?);
        }

        let mut kept = Vec::new();
        'rows: for r in 0..row_count {
            for pred in &stmt.predicates {
                let col_idx = reader.column_index(&pred.column).unwrap();
                let value = decoded[&col_idx].get(r);
                if !eval_predicate(&value, pred.op, &pred.value) {
                    continue 'rows;
                }
            }
            kept.push(
                needed_idx
                    .iter()
                    .map(|&idx| decoded[&idx].get(r))
                    .collect::<Vec<_>>(),
            );
        }
        Ok(Some((kept, row_count)))
    };

    let group_results: Vec<Option<(Vec<Vec<Value>>, usize)>> = if opts.parallel {
        (0..total_groups)
            .into_par_iter()
            .map(scan_one_group)
            .collect::<Result<Vec<_>>>()?
    } else {
        (0..total_groups)
            .map(scan_one_group)
            .collect::<Result<Vec<_>>>()?
    };

    let mut stats = ScanStats {
        row_groups_total: total_groups,
        ..Default::default()
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (group_rows, row_count) in group_results.into_iter().flatten() {
        stats.row_groups_scanned += 1;
        stats.rows_scanned += row_count;
        rows.extend(group_rows);
    }

    if let Some(ob) = &stmt.order_by {
        let pos = needed.iter().position(|c| c == &ob.column).unwrap();
        rows.sort_by(|a, b| {
            let ord = compare_values(&a[pos], &b[pos]);
            if ob.ascending {
                ord
            } else {
                ord.reverse()
            }
        });
    }

    if let Some(limit) = stmt.limit {
        rows.truncate(limit);
    }
    stats.rows_returned = rows.len();

    let projected_positions: Vec<usize> = projected
        .iter()
        .map(|c| needed.iter().position(|n| n == c).unwrap())
        .collect();
    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .map(|row| {
            projected_positions
                .iter()
                .map(|&i| row[i].clone())
                .collect()
        })
        .collect();

    Ok((
        QueryResult {
            columns: projected,
            rows,
        },
        stats,
    ))
}

fn require_column(reader: &TableReader, name: &str) -> Result<()> {
    if reader.column_index(name).is_none() {
        bail!("unknown column '{name}'");
    }
    Ok(())
}

fn check_predicate_type(reader: &TableReader, pred: &Predicate) -> Result<()> {
    let idx = reader.column_index(&pred.column).unwrap();
    let dtype = reader.schema()[idx].dtype;
    let ok = matches!(
        (dtype, &pred.value),
        (DataType::Int64, Literal::Int(_)) | (DataType::Utf8, Literal::Str(_))
    );
    if !ok {
        bail!(
            "type mismatch: column '{}' is {:?} but the predicate compares it to {:?}",
            pred.column,
            dtype,
            pred.value
        );
    }
    Ok(())
}

/// Can this row group be skipped without decoding it? True only when the
/// zone map *proves* no row could possibly satisfy every predicate — this
/// never produces a false "yes, skip it" for a row group that actually
/// has matching rows, only (at worst) a missed opportunity to skip one
/// that turns out to have none. That asymmetry is what keeps pruning an
/// optimization rather than a correctness risk.
fn is_prunable(reader: &TableReader, predicates: &[Predicate], rg_idx: usize) -> bool {
    let rg = &reader.row_groups()[rg_idx];
    for pred in predicates {
        let Literal::Int(v) = pred.value else {
            continue;
        };
        let col_idx = reader.column_index(&pred.column).unwrap();
        let chunk = &rg.chunks[col_idx];
        let (Some(min), Some(max)) = (chunk.min, chunk.max) else {
            continue;
        };

        let impossible = match pred.op {
            CompareOp::Eq => v < min || v > max,
            CompareOp::Ne => min == max && min == v,
            CompareOp::Lt => min >= v,
            CompareOp::Le => min > v,
            CompareOp::Gt => max <= v,
            CompareOp::Ge => max < v,
        };
        if impossible {
            return true;
        }
    }
    false
}

fn eval_predicate(value: &Value, op: CompareOp, literal: &Literal) -> bool {
    let ord = match (value, literal) {
        (Value::Int64(v), Literal::Int(l)) => v.cmp(l),
        (Value::Utf8(v), Literal::Str(l)) => v.as_str().cmp(l.as_str()),
        _ => return false,
    };
    match op {
        CompareOp::Eq => ord == Ordering::Equal,
        CompareOp::Ne => ord != Ordering::Equal,
        CompareOp::Lt => ord == Ordering::Less,
        CompareOp::Le => ord != Ordering::Greater,
        CompareOp::Gt => ord == Ordering::Greater,
        CompareOp::Ge => ord != Ordering::Less,
    }
}

fn compare_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int64(x), Value::Int64(y)) => x.cmp(y),
        (Value::Utf8(x), Value::Utf8(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

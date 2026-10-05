//! Turns a parsed [`SelectStmt`] into a result set.
//!
//! The scan is organised around row groups, the same unit the storage
//! layer uses (`storage::format`). For each row group:
//!
//! 1. **Prune.** Ask the zone maps whether the filter can match *no* row
//!    in this group (`can_skip`) — if so, never read it — or *every* row
//!    (`all_match`) — if so, skip evaluating the filter at all.
//! 2. **Filter, vectorized.** Decode only the filter's columns and
//!    evaluate the expression a whole column at a time into a selection
//!    mask. String comparisons run once per *dictionary entry*, not once
//!    per row.
//! 3. **Late materialization.** Only if some row survived, decode the
//!    remaining columns the query needs, and only build values for the
//!    surviving rows.
//! 4. **Per-group work.** Either collect rows (with `ORDER BY ... LIMIT k`
//!    pushed down: each group keeps only its own top k), or fold them into
//!    partial aggregates keyed by the `GROUP BY` columns.
//!
//! Row groups are independent of each other, which is what makes it safe
//! to hand them to `rayon` and scan several at once; partial results are
//! merged afterwards in row-group order, so parallel and serial scans
//! return byte-identical results.

use std::cmp::Ordering;
use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::storage::{ColumnArray, DataType, RowGroupMeta, TableReader, ZoneMap};
use crate::value::Value;

use super::ast::{
    AggFunc, CompareOp, Expr, Literal, OrderBy, OrderTarget, SelectColumns, SelectItem, SelectStmt,
};

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanStats {
    pub row_groups_total: usize,
    pub row_groups_scanned: usize,
    /// Scanned row groups whose zone maps proved every row matches the
    /// filter, so the filter was never evaluated.
    pub row_groups_fully_matched: usize,
    pub rows_scanned: usize,
    pub rows_returned: usize,
    /// Encoded bytes actually read from disk.
    pub bytes_read: u64,
    /// The query was answered from footer metadata alone (row counts and
    /// zone maps), without reading any column data.
    pub metadata_only: bool,
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
    let filter = stmt.filter.as_ref().map(|f| bind(reader, f)).transpose()?;
    let (result, mut stats) = if stmt.is_aggregate() {
        execute_aggregate(reader, stmt, filter.as_ref(), opts)?
    } else {
        execute_projection(reader, stmt, filter.as_ref(), opts)?
    };
    stats.rows_returned = result.rows.len();
    Ok((result, stats))
}

// ---------------------------------------------------------------------
// Binding: names -> column indices, with type checks
// ---------------------------------------------------------------------

/// An [`Expr`] with column names resolved to indices and literal types
/// checked against the schema, so the hot loop never looks anything up.
#[derive(Debug)]
enum Bound {
    Compare {
        col: usize,
        op: CompareOp,
        value: Literal,
    },
    And(Box<Bound>, Box<Bound>),
    Or(Box<Bound>, Box<Bound>),
    Not(Box<Bound>),
}

impl Bound {
    fn columns(&self, out: &mut Vec<usize>) {
        match self {
            Bound::Compare { col, .. } => {
                if !out.contains(col) {
                    out.push(*col);
                }
            }
            Bound::And(a, b) | Bound::Or(a, b) => {
                a.columns(out);
                b.columns(out);
            }
            Bound::Not(e) => e.columns(out),
        }
    }
}

fn column_index(reader: &TableReader, name: &str) -> Result<usize> {
    reader
        .column_index(name)
        .with_context(|| format!("unknown column '{name}'"))
}

fn bind(reader: &TableReader, expr: &Expr) -> Result<Bound> {
    Ok(match expr {
        Expr::Compare { column, op, value } => {
            let col = column_index(reader, column)?;
            let dtype = reader.schema()[col].dtype;
            let ok = matches!(
                (dtype, value),
                (DataType::Int64, Literal::Int(_)) | (DataType::Utf8, Literal::Str(_))
            );
            if !ok {
                bail!(
                    "type mismatch: column '{column}' is {dtype:?} but the predicate compares it to {value:?}"
                );
            }
            Bound::Compare {
                col,
                op: *op,
                value: value.clone(),
            }
        }
        Expr::And(a, b) => Bound::And(Box::new(bind(reader, a)?), Box::new(bind(reader, b)?)),
        Expr::Or(a, b) => Bound::Or(Box::new(bind(reader, a)?), Box::new(bind(reader, b)?)),
        Expr::Not(e) => Bound::Not(Box::new(bind(reader, e)?)),
    })
}

// ---------------------------------------------------------------------
// Zone-map reasoning
// ---------------------------------------------------------------------

/// How the chunk's min and max compare to the literal, or `None` if
/// there's no usable zone map (then nothing can be proven either way).
fn zone_vs_literal(rg: &RowGroupMeta, col: usize, lit: &Literal) -> Option<(Ordering, Ordering)> {
    match (rg.chunks[col].zone_map.as_ref()?, lit) {
        (ZoneMap::Int64 { min, max }, Literal::Int(v)) => Some((min.cmp(v), max.cmp(v))),
        (ZoneMap::Utf8 { min, max }, Literal::Str(v)) => {
            Some((min.as_str().cmp(v.as_str()), max.as_str().cmp(v.as_str())))
        }
        _ => None,
    }
}

/// True only when the zone maps *prove* no row in the group satisfies
/// `e`. Never a false "skip it" for a group that has a matching row —
/// at worst a missed opportunity — which is what keeps pruning an
/// optimization rather than a correctness risk.
fn can_skip(e: &Bound, rg: &RowGroupMeta) -> bool {
    use Ordering::*;
    match e {
        Bound::Compare { col, op, value } => {
            let Some((lo, hi)) = zone_vs_literal(rg, *col, value) else {
                return false;
            };
            match op {
                CompareOp::Eq => lo == Greater || hi == Less,
                CompareOp::Ne => lo == Equal && hi == Equal,
                CompareOp::Lt => lo != Less,
                CompareOp::Le => lo == Greater,
                CompareOp::Gt => hi != Greater,
                CompareOp::Ge => hi == Less,
            }
        }
        Bound::And(a, b) => can_skip(a, rg) || can_skip(b, rg),
        Bound::Or(a, b) => can_skip(a, rg) && can_skip(b, rg),
        // NOT e matches nothing exactly when e matches everything.
        Bound::Not(inner) => all_match(inner, rg),
    }
}

/// True only when the zone maps *prove* every row satisfies `e`. The
/// mirror image of [`can_skip`]; needed both to push `NOT` through and
/// to skip filter evaluation entirely for groups that fully match.
fn all_match(e: &Bound, rg: &RowGroupMeta) -> bool {
    use Ordering::*;
    match e {
        Bound::Compare { col, op, value } => {
            let Some((lo, hi)) = zone_vs_literal(rg, *col, value) else {
                return false;
            };
            match op {
                CompareOp::Eq => lo == Equal && hi == Equal,
                CompareOp::Ne => lo == Greater || hi == Less,
                CompareOp::Lt => hi == Less,
                CompareOp::Le => hi != Greater,
                CompareOp::Gt => lo == Greater,
                CompareOp::Ge => lo != Less,
            }
        }
        Bound::And(a, b) => all_match(a, rg) && all_match(b, rg),
        Bound::Or(a, b) => all_match(a, rg) || all_match(b, rg),
        Bound::Not(inner) => can_skip(inner, rg),
    }
}

// ---------------------------------------------------------------------
// Vectorized filter evaluation
// ---------------------------------------------------------------------

fn test_ordering(op: CompareOp, ord: Ordering) -> bool {
    match op {
        CompareOp::Eq => ord == Ordering::Equal,
        CompareOp::Ne => ord != Ordering::Equal,
        CompareOp::Lt => ord == Ordering::Less,
        CompareOp::Le => ord != Ordering::Greater,
        CompareOp::Gt => ord == Ordering::Greater,
        CompareOp::Ge => ord != Ordering::Less,
    }
}

/// Evaluates `e` over a whole row group at once, returning one bool per row.
fn eval(e: &Bound, cols: &[Option<ColumnArray>]) -> Vec<bool> {
    match e {
        Bound::Compare { col, op, value } => {
            match (
                cols[*col]
                    .as_ref()
                    .expect("filter columns are decoded first"),
                value,
            ) {
                (ColumnArray::Int64(v), Literal::Int(lit)) => {
                    v.iter().map(|x| test_ordering(*op, x.cmp(lit))).collect()
                }
                (ColumnArray::Utf8(d), Literal::Str(lit)) => {
                    let hits: Vec<bool> = d
                        .dict
                        .iter()
                        .map(|s| test_ordering(*op, s.as_str().cmp(lit.as_str())))
                        .collect();
                    d.indices.iter().map(|&i| hits[i as usize]).collect()
                }
                _ => unreachable!("types were checked in bind()"),
            }
        }
        Bound::And(a, b) => {
            let mut m = eval(a, cols);
            m.iter_mut().zip(eval(b, cols)).for_each(|(x, y)| *x &= y);
            m
        }
        Bound::Or(a, b) => {
            let mut m = eval(a, cols);
            m.iter_mut().zip(eval(b, cols)).for_each(|(x, y)| *x |= y);
            m
        }
        Bound::Not(inner) => eval(inner, cols).into_iter().map(|x| !x).collect(),
    }
}

// ---------------------------------------------------------------------
// The scan loop shared by both query kinds
// ---------------------------------------------------------------------

struct GroupOutput<T> {
    value: T,
    rows: usize,
    bytes: u64,
    fully_matched: bool,
}

/// Runs `per_group(decoded columns, selected row indices)` on every row
/// group that survives pruning, in parallel if asked, and returns the
/// outputs in row-group order. `needed` columns are only decoded if at
/// least one row passed the filter.
fn scan<T, F>(
    reader: &TableReader,
    filter: Option<&Bound>,
    needed: &[usize],
    opts: &ExecuteOptions,
    per_group: F,
) -> Result<(Vec<T>, ScanStats)>
where
    T: Send,
    F: Fn(&[Option<ColumnArray>], &[usize]) -> Result<T> + Sync,
{
    let mut filter_cols = Vec::new();
    if let Some(f) = filter {
        f.columns(&mut filter_cols);
    }

    let scan_one = |rg_idx: usize| -> Result<Option<GroupOutput<T>>> {
        let rg = &reader.row_groups()[rg_idx];
        let fully_matched = match filter {
            None => true,
            Some(f) if opts.use_pruning => {
                if can_skip(f, rg) {
                    return Ok(None);
                }
                all_match(f, rg)
            }
            Some(_) => false,
        };

        let rows = rg.row_count as usize;
        let mut cols: Vec<Option<ColumnArray>> = (0..reader.schema().len()).map(|_| None).collect();
        let mut bytes = 0u64;
        let mut load = |c: usize, cols: &mut Vec<Option<ColumnArray>>| -> Result<()> {
            if cols[c].is_none() {
                cols[c] = Some(reader.read_column_chunk(rg_idx, c)?);
                bytes += rg.chunks[c].length;
            }
            Ok(())
        };

        let selected: Vec<usize> = if fully_matched {
            (0..rows).collect()
        } else {
            let f = filter.expect("only unfiltered scans fully match without pruning");
            for &c in &filter_cols {
                load(c, &mut cols)?;
            }
            eval(f, &cols)
                .into_iter()
                .enumerate()
                .filter_map(|(i, keep)| keep.then_some(i))
                .collect()
        };
        if !selected.is_empty() {
            for &c in needed {
                load(c, &mut cols)?;
            }
        }

        Ok(Some(GroupOutput {
            value: per_group(&cols, &selected)?,
            rows,
            bytes,
            fully_matched: fully_matched && filter.is_some(),
        }))
    };

    let total = reader.row_groups().len();
    let outputs: Vec<Option<GroupOutput<T>>> = if opts.parallel {
        (0..total)
            .into_par_iter()
            .map(scan_one)
            .collect::<Result<_>>()?
    } else {
        (0..total).map(scan_one).collect::<Result<_>>()?
    };

    let mut stats = ScanStats {
        row_groups_total: total,
        ..Default::default()
    };
    let mut values = Vec::with_capacity(outputs.len());
    for out in outputs.into_iter().flatten() {
        stats.row_groups_scanned += 1;
        stats.row_groups_fully_matched += out.fully_matched as usize;
        stats.rows_scanned += out.rows;
        stats.bytes_read += out.bytes;
        values.push(out.value);
    }
    Ok((values, stats))
}

fn dedup_push(v: &mut Vec<usize>, x: usize) -> usize {
    match v.iter().position(|&y| y == x) {
        Some(p) => p,
        None => {
            v.push(x);
            v.len() - 1
        }
    }
}

/// Stable multi-key sort. `keys` are `(position in row, ascending)`.
fn sort_rows(rows: &mut [Vec<Value>], keys: &[(usize, bool)]) {
    if keys.is_empty() {
        return;
    }
    rows.sort_by(|a, b| {
        for &(pos, asc) in keys {
            let ord = a[pos].sort_cmp(&b[pos]);
            if ord != Ordering::Equal {
                return if asc { ord } else { ord.reverse() };
            }
        }
        Ordering::Equal
    });
}

// ---------------------------------------------------------------------
// Plain SELECT (no aggregates)
// ---------------------------------------------------------------------

fn execute_projection(
    reader: &TableReader,
    stmt: &SelectStmt,
    filter: Option<&Bound>,
    opts: &ExecuteOptions,
) -> Result<(QueryResult, ScanStats)> {
    // (output name, column index) per output column.
    let outputs: Vec<(String, usize)> = match &stmt.columns {
        SelectColumns::Star => reader
            .schema()
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect(),
        SelectColumns::Items(items) => items
            .iter()
            .map(|item| match item {
                SelectItem::Column { name, .. } => {
                    Ok((item.output_name(), column_index(reader, name)?))
                }
                SelectItem::Aggregate { .. } => unreachable!("not an aggregate query"),
            })
            .collect::<Result<_>>()?,
    };

    // Every column a materialized row carries: the projection, plus any
    // ORDER BY column that isn't projected.
    let mut needed: Vec<usize> = Vec::new();
    let out_pos: Vec<usize> = outputs
        .iter()
        .map(|(_, c)| dedup_push(&mut needed, *c))
        .collect();
    let mut sort_keys = Vec::with_capacity(stmt.order_by.len());
    for OrderBy { target, ascending } in &stmt.order_by {
        let col = match target {
            OrderTarget::Position(p) => {
                outputs
                    .get(p - 1)
                    .with_context(|| format!("ORDER BY position {p} is out of range"))?
                    .1
            }
            OrderTarget::Name(n) => match outputs.iter().find(|(name, _)| name == n) {
                Some((_, c)) => *c,
                None => column_index(reader, n)?,
            },
        };
        sort_keys.push((dedup_push(&mut needed, col), *ascending));
    }

    let limit = stmt.limit;
    let (groups, stats) = scan(reader, filter, &needed, opts, |cols, selected| {
        let mut rows: Vec<Vec<Value>> = selected
            .iter()
            .map(|&r| {
                needed
                    .iter()
                    .map(|&c| cols[c].as_ref().unwrap().get(r))
                    .collect()
            })
            .collect();
        // Top-k pushdown: the final top k can only contain each group's
        // own top k, and a stable sort keeps ties in row order, so this
        // never changes the answer.
        if let Some(k) = limit {
            sort_rows(&mut rows, &sort_keys);
            rows.truncate(k);
        }
        Ok(rows)
    })?;

    let mut rows: Vec<Vec<Value>> = groups.into_iter().flatten().collect();
    sort_rows(&mut rows, &sort_keys);
    if let Some(k) = limit {
        rows.truncate(k);
    }
    let rows = rows
        .into_iter()
        .map(|row| out_pos.iter().map(|&p| row[p].clone()).collect())
        .collect();

    Ok((
        QueryResult {
            columns: outputs.into_iter().map(|(n, _)| n).collect(),
            rows,
        },
        stats,
    ))
}

// ---------------------------------------------------------------------
// Aggregates and GROUP BY
// ---------------------------------------------------------------------

/// One `GROUP BY` key component. Hashable and totally ordered, unlike
/// `Value` (which can hold a float).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum KeyPart {
    Int(i64),
    Str(String),
}

impl KeyPart {
    fn into_value(self) -> Value {
        match self {
            KeyPart::Int(v) => Value::Int64(v),
            KeyPart::Str(s) => Value::Utf8(s),
        }
    }
}

/// A partial aggregate. Every variant can absorb rows (`update`) and
/// absorb another partial for the same group (`merge`) — the second is
/// what lets each row group aggregate independently, in parallel.
#[derive(Debug, Clone)]
enum Acc {
    Count(u64),
    /// i128 so no realistic number of i64s can overflow mid-sum; the
    /// final value is range-checked in `finish`.
    Sum {
        sum: i128,
        n: u64,
    },
    Avg {
        sum: i128,
        n: u64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Acc {
    fn new(func: AggFunc) -> Self {
        match func {
            AggFunc::Count => Acc::Count(0),
            AggFunc::Sum => Acc::Sum { sum: 0, n: 0 },
            AggFunc::Avg => Acc::Avg { sum: 0, n: 0 },
            AggFunc::Min => Acc::Min(None),
            AggFunc::Max => Acc::Max(None),
        }
    }

    fn update(&mut self, col: Option<&ColumnArray>, row: usize) {
        match self {
            Acc::Count(n) => *n += 1,
            Acc::Sum { sum, n } | Acc::Avg { sum, n } => {
                let Some(ColumnArray::Int64(v)) = col else {
                    unreachable!("SUM/AVG args are checked to be Int64")
                };
                *sum += v[row] as i128;
                *n += 1;
            }
            Acc::Min(cur) => replace_if(cur, col.unwrap(), row, Ordering::Less),
            Acc::Max(cur) => replace_if(cur, col.unwrap(), row, Ordering::Greater),
        }
    }

    fn merge(&mut self, other: Acc) {
        match (self, other) {
            (Acc::Count(a), Acc::Count(b)) => *a += b,
            (Acc::Sum { sum, n }, Acc::Sum { sum: s2, n: n2 })
            | (Acc::Avg { sum, n }, Acc::Avg { sum: s2, n: n2 }) => {
                *sum += s2;
                *n += n2;
            }
            (Acc::Min(a), Acc::Min(b)) => merge_extreme(a, b, Ordering::Less),
            (Acc::Max(a), Acc::Max(b)) => merge_extreme(a, b, Ordering::Greater),
            _ => unreachable!("partials for the same output are the same kind"),
        }
    }

    fn finish(self) -> Result<Value> {
        Ok(match self {
            Acc::Count(n) => Value::Int64(n as i64),
            Acc::Sum { n: 0, .. } | Acc::Avg { n: 0, .. } => Value::Null,
            Acc::Sum { sum, .. } => Value::Int64(
                i64::try_from(sum).map_err(|_| anyhow::anyhow!("SUM overflowed Int64: {sum}"))?,
            ),
            Acc::Avg { sum, n } => Value::Float64(sum as f64 / n as f64),
            Acc::Min(v) | Acc::Max(v) => v.unwrap_or(Value::Null),
        })
    }
}

/// Replaces `cur` with `col[row]` if it compares `want` against `cur`,
/// without allocating a String unless it actually replaces.
fn replace_if(cur: &mut Option<Value>, col: &ColumnArray, row: usize, want: Ordering) {
    let better = match (cur.as_ref(), col) {
        (None, _) => true,
        (Some(Value::Int64(c)), ColumnArray::Int64(v)) => v[row].cmp(c) == want,
        (Some(Value::Utf8(c)), ColumnArray::Utf8(d)) => d.get(row).cmp(c.as_str()) == want,
        _ => unreachable!("one column, one type"),
    };
    if better {
        *cur = Some(col.get(row));
    }
}

fn merge_extreme(a: &mut Option<Value>, b: Option<Value>, want: Ordering) {
    if let Some(b) = b {
        if a.as_ref().is_none_or(|a| b.sort_cmp(a) == want) {
            *a = Some(b);
        }
    }
}

enum OutputCol {
    Key(usize),
    Agg(usize),
}

type Groups = HashMap<Vec<KeyPart>, Vec<Acc>>;

fn execute_aggregate(
    reader: &TableReader,
    stmt: &SelectStmt,
    filter: Option<&Bound>,
    opts: &ExecuteOptions,
) -> Result<(QueryResult, ScanStats)> {
    let SelectColumns::Items(items) = &stmt.columns else {
        bail!("SELECT * can't be combined with GROUP BY; list the grouped columns and aggregates");
    };

    let key_cols: Vec<usize> = stmt
        .group_by
        .iter()
        .map(|n| column_index(reader, n))
        .collect::<Result<_>>()?;

    let mut aggs: Vec<(AggFunc, Option<usize>)> = Vec::new();
    let mut outputs = Vec::with_capacity(items.len());
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        names.push(item.output_name());
        match item {
            SelectItem::Column { name, .. } => {
                let Some(k) = stmt.group_by.iter().position(|g| g == name) else {
                    bail!("column '{name}' must appear in GROUP BY or be inside an aggregate");
                };
                outputs.push(OutputCol::Key(k));
            }
            SelectItem::Aggregate { func, arg, .. } => {
                let col = arg
                    .as_deref()
                    .map(|a| column_index(reader, a))
                    .transpose()?;
                if let (AggFunc::Sum | AggFunc::Avg, Some(c)) = (func, col) {
                    if reader.schema()[c].dtype != DataType::Int64 {
                        bail!(
                            "{}() needs an Int64 column, but '{}' is {:?}",
                            func.name().to_uppercase(),
                            reader.schema()[c].name,
                            reader.schema()[c].dtype
                        );
                    }
                }
                outputs.push(OutputCol::Agg(aggs.len()));
                aggs.push((*func, col));
            }
        }
    }

    let fresh = || aggs.iter().map(|(f, _)| Acc::new(*f)).collect::<Vec<_>>();

    let (mut groups, stats) = match answer_from_metadata(reader, stmt, &aggs) {
        Some(accs) => {
            let stats = ScanStats {
                row_groups_total: reader.row_groups().len(),
                metadata_only: true,
                ..Default::default()
            };
            (Groups::from([(Vec::new(), accs)]), stats)
        }
        None => {
            let mut needed = key_cols.clone();
            for (_, c) in &aggs {
                if let Some(c) = c {
                    dedup_push(&mut needed, *c);
                }
            }
            let (partials, stats) = scan(reader, filter, &needed, opts, |cols, selected| {
                // Group on integer codes — the value for Int64 columns,
                // the dictionary index for Utf8 ones — so the hot loop
                // never allocates a String. Codes become real keys once
                // per distinct group, below.
                if selected.is_empty() {
                    // Non-filter columns weren't decoded; nothing to do.
                    return Ok(Groups::new());
                }
                let key_arrays: Vec<&ColumnArray> = key_cols
                    .iter()
                    .map(|&c| cols[c].as_ref().unwrap())
                    .collect();
                let mut local: HashMap<Vec<i64>, Vec<Acc>> = HashMap::new();
                let mut codes: Vec<i64> = Vec::with_capacity(key_cols.len());
                for &r in selected {
                    codes.clear();
                    codes.extend(key_arrays.iter().map(|col| match col {
                        ColumnArray::Int64(v) => v[r],
                        ColumnArray::Utf8(d) => d.indices[r] as i64,
                    }));
                    let accs = match local.get_mut(codes.as_slice()) {
                        Some(accs) => accs,
                        None => local.entry(codes.clone()).or_insert_with(fresh),
                    };
                    for (acc, (_, c)) in accs.iter_mut().zip(&aggs) {
                        acc.update(c.and_then(|c| cols[c].as_ref()), r);
                    }
                }
                Ok(local
                    .into_iter()
                    .map(|(codes, accs)| {
                        let key = codes
                            .iter()
                            .zip(&key_arrays)
                            .map(|(&code, col)| match col {
                                ColumnArray::Int64(_) => KeyPart::Int(code),
                                ColumnArray::Utf8(d) => KeyPart::Str(d.dict[code as usize].clone()),
                            })
                            .collect();
                        (key, accs)
                    })
                    .collect::<Groups>())
            })?;

            let mut merged = Groups::new();
            for part in partials {
                for (key, accs) in part {
                    match merged.get_mut(&key) {
                        Some(existing) => {
                            for (a, b) in existing.iter_mut().zip(accs) {
                                a.merge(b);
                            }
                        }
                        None => {
                            merged.insert(key, accs);
                        }
                    }
                }
            }
            (merged, stats)
        }
    };

    // An aggregate without GROUP BY always returns exactly one row, even
    // over zero input rows (COUNT = 0, everything else NULL).
    if key_cols.is_empty() && groups.is_empty() {
        groups.insert(Vec::new(), fresh());
    }

    // Deterministic output: groups in key order, then any ORDER BY on top.
    let mut sorted: Vec<(Vec<KeyPart>, Vec<Acc>)> = groups.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut rows = Vec::with_capacity(sorted.len());
    for (key, accs) in sorted {
        let mut accs: Vec<Option<Acc>> = accs.into_iter().map(Some).collect();
        let mut row = Vec::with_capacity(outputs.len());
        for out in &outputs {
            row.push(match *out {
                OutputCol::Key(k) => key[k].clone().into_value(),
                OutputCol::Agg(a) => accs[a]
                    .take()
                    .expect("each aggregate is output once")
                    .finish()?,
            });
        }
        rows.push(row);
    }

    let mut sort_keys = Vec::with_capacity(stmt.order_by.len());
    for OrderBy { target, ascending } in &stmt.order_by {
        let pos = match target {
            OrderTarget::Position(p) if *p <= names.len() => p - 1,
            OrderTarget::Position(p) => bail!("ORDER BY position {p} is out of range"),
            OrderTarget::Name(n) => names.iter().position(|o| o == n).with_context(|| {
                format!("ORDER BY '{n}' must name an output column of an aggregate query (use an alias: COUNT(*) AS n ... ORDER BY n)")
            })?,
        };
        sort_keys.push((pos, *ascending));
    }
    sort_rows(&mut rows, &sort_keys);
    if let Some(k) = stmt.limit {
        rows.truncate(k);
    }

    Ok((
        QueryResult {
            columns: names,
            rows,
        },
        stats,
    ))
}

/// `SELECT COUNT(*), MIN(x), MAX(y) FROM t` with no `WHERE` and no
/// `GROUP BY` doesn't need to read any data: the footer already knows the
/// row count and every chunk's min/max. Returns the finished partials if
/// the query is of that shape.
fn answer_from_metadata(
    reader: &TableReader,
    stmt: &SelectStmt,
    aggs: &[(AggFunc, Option<usize>)],
) -> Option<Vec<Acc>> {
    if stmt.filter.is_some() || !stmt.group_by.is_empty() {
        return None;
    }
    // Outer `None`: some chunk has no zone map, so this can't be answered
    // from metadata. Inner `None`: the table is empty, so the answer is NULL.
    let zone_extreme = |col: usize, want: Ordering| -> Option<Option<Value>> {
        let mut best: Option<Value> = None;
        for rg in reader.row_groups() {
            let v = match rg.chunks[col].zone_map.as_ref()? {
                ZoneMap::Int64 { min, max } => {
                    Value::Int64(if want == Ordering::Less { *min } else { *max })
                }
                ZoneMap::Utf8 { min, max } => {
                    Value::Utf8(if want == Ordering::Less { min } else { max }.clone())
                }
            };
            merge_extreme(&mut best, Some(v), want);
        }
        Some(best)
    };

    aggs.iter()
        .map(|&(func, col)| match func {
            AggFunc::Count => Some(Acc::Count(reader.row_count())),
            AggFunc::Min => zone_extreme(col?, Ordering::Less).map(Acc::Min),
            AggFunc::Max => zone_extreme(col?, Ordering::Greater).map(Acc::Max),
            AggFunc::Sum | AggFunc::Avg => None,
        })
        .collect()
}

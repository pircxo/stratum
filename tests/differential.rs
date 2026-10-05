//! Differential testing: generate random tables and random queries, and
//! check the real engine (pruning, vectorized filters, top-k pushdown,
//! parallel partial aggregation) against a deliberately naive oracle that
//! evaluates the same SQL row by row over an in-memory `Vec` of rows.
//!
//! Hand-written tests check the cases someone thought of; this checks the
//! interactions nobody did — e.g. `NOT` over an `OR` whose zone maps only
//! partially overlap, or a `LIMIT` cutting through ties that span row
//! groups. Every run is seeded, so a failure prints a reproducible case.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use stratum::query::ast::{
    AggFunc, CompareOp, Expr, Literal, OrderBy, OrderTarget, SelectColumns, SelectItem, SelectStmt,
};
use stratum::query::{execute, ExecuteOptions};
use stratum::storage::{ColumnSchema, DataType, TableReader, TableWriter};
use stratum::Value;

const TABLES: u64 = 12;
const QUERIES_PER_TABLE: usize = 60;

/// Column 0 `a`: clustered int (prunes well). Column 1 `b`: small-range
/// random int (many ties). Column 2 `c`: low-cardinality string, sometimes
/// clustered. Column 3 `d`: wide-range random int.
fn schema() -> Vec<ColumnSchema> {
    [
        ("a", DataType::Int64),
        ("b", DataType::Int64),
        ("c", DataType::Utf8),
        ("d", DataType::Int64),
    ]
    .into_iter()
    .map(|(name, dtype)| ColumnSchema {
        name: name.to_string(),
        dtype,
    })
    .collect()
}

const STRINGS: [&str; 6] = ["", "apple", "banana", "cherry", "date", "elder"];

fn random_table(rng: &mut StdRng) -> Vec<Vec<Value>> {
    let n = rng.gen_range(0..1500);
    let clustered_strings = rng.gen_bool(0.5);
    (0..n)
        .map(|i| {
            let c = if clustered_strings {
                STRINGS[i * STRINGS.len() / n.max(1)]
            } else {
                STRINGS[rng.gen_range(0..STRINGS.len())]
            };
            vec![
                Value::Int64(i as i64 * 3 + rng.gen_range(-20..20)),
                Value::Int64(rng.gen_range(-5..5)),
                Value::Utf8(c.to_string()),
                Value::Int64(rng.gen_range(i64::MIN / 2..i64::MAX / 2)),
            ]
        })
        .collect()
}

fn random_literal(rng: &mut StdRng, col: usize) -> Literal {
    match col {
        0 => Literal::Int(rng.gen_range(-50..4600)),
        1 => Literal::Int(rng.gen_range(-6..6)),
        2 => {
            // Include strings that aren't in the data, to hit range edges.
            let pool = [
                "", "a", "apple", "b", "banana", "cherry", "cz", "date", "elder", "zzz",
            ];
            Literal::Str(pool[rng.gen_range(0..pool.len())].to_string())
        }
        _ => Literal::Int(rng.gen_range(i64::MIN / 2..i64::MAX / 2)),
    }
}

const NAMES: [&str; 4] = ["a", "b", "c", "d"];
const OPS: [CompareOp; 6] = [
    CompareOp::Eq,
    CompareOp::Ne,
    CompareOp::Lt,
    CompareOp::Le,
    CompareOp::Gt,
    CompareOp::Ge,
];

fn random_expr(rng: &mut StdRng, depth: u32) -> Expr {
    if depth == 0 || rng.gen_bool(0.4) {
        let col = rng.gen_range(0..4);
        return Expr::compare(
            NAMES[col],
            OPS[rng.gen_range(0..OPS.len())],
            random_literal(rng, col),
        );
    }
    match rng.gen_range(0..3) {
        0 => Expr::And(
            Box::new(random_expr(rng, depth - 1)),
            Box::new(random_expr(rng, depth - 1)),
        ),
        1 => Expr::Or(
            Box::new(random_expr(rng, depth - 1)),
            Box::new(random_expr(rng, depth - 1)),
        ),
        _ => Expr::Not(Box::new(random_expr(rng, depth - 1))),
    }
}

fn random_query(rng: &mut StdRng) -> SelectStmt {
    let filter = rng.gen_bool(0.8).then(|| random_expr(rng, 3));
    let limit = rng.gen_bool(0.5).then(|| rng.gen_range(0..40));
    let order_key = |rng: &mut StdRng, outputs: usize| OrderBy {
        target: OrderTarget::Position(rng.gen_range(1..=outputs)),
        ascending: rng.gen_bool(0.5),
    };

    if rng.gen_bool(0.5) {
        // Projection.
        let cols: Vec<SelectItem> = (0..rng.gen_range(1..=4))
            .map(|_| SelectItem::Column {
                name: NAMES[rng.gen_range(0..4)].to_string(),
                alias: None,
            })
            .collect();
        let order_by = (0..rng.gen_range(0..3))
            .map(|_| order_key(rng, cols.len()))
            .collect();
        SelectStmt {
            columns: SelectColumns::Items(cols),
            table: "t".into(),
            filter,
            group_by: Vec::new(),
            order_by,
            limit,
        }
    } else {
        // Aggregate, with 0-2 group columns drawn from the low-cardinality ones.
        let group_by: Vec<String> = match rng.gen_range(0..4) {
            0 => vec![],
            1 => vec!["b".into()],
            2 => vec!["c".into()],
            _ => vec!["c".into(), "b".into()],
        };
        let mut items: Vec<SelectItem> = group_by
            .iter()
            .map(|g| SelectItem::Column {
                name: g.clone(),
                alias: None,
            })
            .collect();
        for _ in 0..rng.gen_range(1..=3) {
            let (func, arg) = match rng.gen_range(0..6) {
                0 => (AggFunc::Count, None),
                1 => (AggFunc::Count, Some("c")),
                2 => (AggFunc::Sum, Some(["a", "b"][rng.gen_range(0..2)])),
                3 => (AggFunc::Avg, Some(["a", "b"][rng.gen_range(0..2)])),
                4 => (AggFunc::Min, Some(NAMES[rng.gen_range(0..4)])),
                _ => (AggFunc::Max, Some(NAMES[rng.gen_range(0..4)])),
            };
            items.push(SelectItem::Aggregate {
                func,
                arg: arg.map(str::to_string),
                alias: None,
            });
        }
        let order_by = (0..rng.gen_range(0..3))
            .map(|_| order_key(rng, items.len()))
            .collect();
        SelectStmt {
            columns: SelectColumns::Items(items),
            table: "t".into(),
            filter,
            group_by,
            order_by,
            limit,
        }
    }
}

// ---------------------------------------------------------------------
// The oracle: obviously-correct, row-at-a-time, no cleverness.
// ---------------------------------------------------------------------

fn idx(name: &str) -> usize {
    NAMES.iter().position(|n| *n == name).unwrap()
}

fn oracle_eval(e: &Expr, row: &[Value]) -> bool {
    match e {
        Expr::Compare { column, op, value } => {
            let ord = match (&row[idx(column)], value) {
                (Value::Int64(x), Literal::Int(y)) => x.cmp(y),
                (Value::Utf8(x), Literal::Str(y)) => x.cmp(y),
                _ => unreachable!(),
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
        Expr::And(a, b) => oracle_eval(a, row) && oracle_eval(b, row),
        Expr::Or(a, b) => oracle_eval(a, row) || oracle_eval(b, row),
        Expr::Not(e) => !oracle_eval(e, row),
    }
}

fn oracle_sort_and_limit(rows: &mut Vec<Vec<Value>>, stmt: &SelectStmt) {
    rows.sort_by(|x, y| {
        for OrderBy { target, ascending } in &stmt.order_by {
            let OrderTarget::Position(p) = target else {
                unreachable!()
            };
            let ord = x[p - 1].sort_cmp(&y[p - 1]);
            if ord != Ordering::Equal {
                return if *ascending { ord } else { ord.reverse() };
            }
        }
        Ordering::Equal
    });
    if let Some(k) = stmt.limit {
        rows.truncate(k);
    }
}

fn oracle(table: &[Vec<Value>], stmt: &SelectStmt) -> Result<Vec<Vec<Value>>, String> {
    let matching: Vec<&Vec<Value>> = table
        .iter()
        .filter(|row| stmt.filter.as_ref().is_none_or(|f| oracle_eval(f, row)))
        .collect();
    let SelectColumns::Items(items) = &stmt.columns else {
        unreachable!()
    };

    let mut out: Vec<Vec<Value>> = if !stmt.is_aggregate() {
        matching
            .iter()
            .map(|row| {
                items
                    .iter()
                    .map(|i| match i {
                        SelectItem::Column { name, .. } => row[idx(name)].clone(),
                        _ => unreachable!(),
                    })
                    .collect()
            })
            .collect()
    } else {
        // BTreeMap keyed on the group values' Display + sort order gives
        // the same "groups in key order" the engine promises.
        let mut groups: BTreeMap<Vec<GroupKey>, Vec<&Vec<Value>>> = BTreeMap::new();
        for row in &matching {
            let key = stmt
                .group_by
                .iter()
                .map(|g| GroupKey(row[idx(g)].clone()))
                .collect();
            groups.entry(key).or_default().push(row);
        }
        if stmt.group_by.is_empty() && groups.is_empty() {
            groups.insert(Vec::new(), Vec::new());
        }
        let mut out = Vec::new();
        for (key, rows) in groups {
            let mut row = Vec::new();
            for item in items {
                row.push(match item {
                    SelectItem::Column { name, .. } => {
                        let k = stmt.group_by.iter().position(|g| g == name).unwrap();
                        key[k].0.clone()
                    }
                    SelectItem::Aggregate { func, arg, .. } => {
                        let vals: Vec<&Value> = match arg {
                            Some(a) => rows.iter().map(|r| &r[idx(a)]).collect(),
                            None => rows.iter().map(|r| &r[0]).collect(),
                        };
                        aggregate(*func, &vals)?
                    }
                });
            }
            out.push(row);
        }
        out
    };
    oracle_sort_and_limit(&mut out, stmt);
    Ok(out)
}

/// `Value` can hold a float, so it isn't `Ord`; group keys are only
/// ever ints or strings, where `sort_cmp` is a true total order.
#[derive(Clone)]
struct GroupKey(Value);
impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for GroupKey {}
impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.sort_cmp(&other.0)
    }
}

fn aggregate(func: AggFunc, vals: &[&Value]) -> Result<Value, String> {
    if func == AggFunc::Count {
        return Ok(Value::Int64(vals.len() as i64));
    }
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    let ints = || vals.iter().map(|v| v.as_int64().unwrap() as i128);
    Ok(match func {
        AggFunc::Sum => {
            let sum: i128 = ints().sum();
            Value::Int64(i64::try_from(sum).map_err(|_| "overflow".to_string())?)
        }
        AggFunc::Avg => Value::Float64(ints().sum::<i128>() as f64 / vals.len() as f64),
        AggFunc::Min => (*vals.iter().min_by(|a, b| a.sort_cmp(b)).unwrap()).clone(),
        AggFunc::Max => (*vals.iter().rev().max_by(|a, b| a.sort_cmp(b)).unwrap()).clone(),
        AggFunc::Count => unreachable!(),
    })
}

// ---------------------------------------------------------------------

fn temp_path(seed: u64) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "stratum_differential_{seed}_{}.strat",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn engine_matches_naive_oracle_on_random_queries() {
    let mut checked = 0;
    for seed in 0..TABLES {
        let mut rng = StdRng::seed_from_u64(seed);
        let table = random_table(&mut rng);
        let row_group_size = rng.gen_range(1..200);

        let path = temp_path(seed);
        let mut w = TableWriter::create(&path, schema())
            .unwrap()
            .with_row_group_size(row_group_size);
        for row in &table {
            w.add_row(row.clone()).unwrap();
        }
        w.finish().unwrap();
        let reader = TableReader::open(&path).unwrap();

        for q in 0..QUERIES_PER_TABLE {
            let stmt = random_query(&mut rng);
            let expected = oracle(&table, &stmt);
            for (use_pruning, parallel) in
                [(true, true), (true, false), (false, true), (false, false)]
            {
                let opts = ExecuteOptions {
                    use_pruning,
                    parallel,
                };
                let got = execute(&reader, &stmt, &opts).map(|(r, _)| r.rows);
                match (&expected, &got) {
                    (Ok(e), Ok(g)) => assert_eq!(
                        e, g,
                        "seed {seed}, query {q}, rows {}, group size {row_group_size}, {opts:?}\n{stmt:#?}",
                        table.len()
                    ),
                    (Err(_), Err(e)) => assert!(format!("{e:#}").contains("overflow")),
                    _ => panic!(
                        "seed {seed}, query {q}: oracle {expected:?} vs engine {:?}\n{stmt:#?}",
                        got.map_err(|e| e.to_string())
                    ),
                }
            }
            checked += 1;
        }
        let _ = std::fs::remove_file(&path);
    }
    assert_eq!(checked, TABLES as usize * QUERIES_PER_TABLE);
}

/// Pruning must be *sound*: for any filter, the engine with pruning on
/// never scans more than without it, and never returns different rows.
/// (The main test already checks results; this pins down that pruning
/// actually fires on the clustered column, so it isn't vacuously sound.)
#[test]
fn pruning_fires_on_clustered_data() {
    let mut rng = StdRng::seed_from_u64(7);
    let table: Vec<Vec<Value>> = (0..1000)
        .map(|i| {
            vec![
                Value::Int64(i),
                Value::Int64(rng.gen_range(-5..5)),
                Value::Utf8(STRINGS[i as usize * 6 / 1000].to_string()),
                Value::Int64(rng.gen()),
            ]
        })
        .collect();
    let path = temp_path(999);
    let mut w = TableWriter::create(&path, schema())
        .unwrap()
        .with_row_group_size(50);
    for row in &table {
        w.add_row(row.clone()).unwrap();
    }
    w.finish().unwrap();
    let reader = TableReader::open(&path).unwrap();

    let stmt = stratum::query::parse(
        "SELECT a FROM t WHERE (a < 100 OR c = 'elder') AND NOT (a BETWEEN 50 AND 99)",
    )
    .unwrap();
    let (result, stats) = execute(&reader, &stmt, &ExecuteOptions::default()).unwrap();
    assert_eq!(Ok(result.rows), oracle(&table, &stmt));
    // a < 100 lives in groups 0-1, 'elder' in the last ~3; NOT BETWEEN
    // then rules out group 1 entirely.
    assert!(stats.row_groups_scanned <= 5, "{stats:?}");
    let _ = std::fs::remove_file(&path);
}

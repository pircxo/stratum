//! End-to-end tests: write a real `.strat` file, then query it through
//! the actual parser + executor, the same path the CLI uses. These are
//! what actually prove query *correctness*, as opposed to the unit tests
//! in `src/`, which prove individual pieces (the lexer, one encoding)
//! work in isolation.

use std::path::{Path, PathBuf};

use stratum::query::{execute, parse, ExecuteOptions};
use stratum::query::{QueryResult, ScanStats};
use stratum::storage::{ColumnSchema, DataType, TableReader, TableWriter};
use stratum::Value;

/// A fresh temp file path per test, so tests run in parallel without
/// colliding (`cargo test` runs test functions concurrently by default).
fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("stratum_test_{name}_{}.strat", std::process::id()));
    // The writer refuses to overwrite; clear anything a crashed run left.
    let _ = std::fs::remove_file(&p);
    p
}

fn write_sample_table(path: &Path, row_count: i64) {
    let schema = vec![
        ColumnSchema {
            name: "id".to_string(),
            dtype: DataType::Int64,
        },
        ColumnSchema {
            name: "value".to_string(),
            dtype: DataType::Int64,
        },
        ColumnSchema {
            name: "label".to_string(),
            dtype: DataType::Utf8,
        },
    ];
    let mut writer = TableWriter::create(path, schema).unwrap();
    for id in 0..row_count {
        let label = if id % 2 == 0 { "even" } else { "odd" };
        writer
            .add_row(vec![
                Value::Int64(id),
                Value::Int64(id * 10),
                Value::Utf8(label.to_string()),
            ])
            .unwrap();
    }
    writer.finish().unwrap();
}

fn col(name: &str, dtype: DataType) -> ColumnSchema {
    ColumnSchema {
        name: name.to_string(),
        dtype,
    }
}

fn query(path: &Path, sql: &str) -> (QueryResult, ScanStats) {
    let reader = TableReader::open(path).unwrap();
    execute(&reader, &parse(sql).unwrap(), &ExecuteOptions::default()).unwrap()
}

fn query_err(path: &Path, sql: &str) -> String {
    let reader = TableReader::open(path).unwrap();
    format!(
        "{:#}",
        execute(&reader, &parse(sql).unwrap(), &ExecuteOptions::default()).unwrap_err()
    )
}

fn int(v: i64) -> Value {
    Value::Int64(v)
}

fn s(v: &str) -> Value {
    Value::Utf8(v.to_string())
}

#[test]
fn round_trips_a_small_table_through_a_full_query() {
    let path = temp_path("small_roundtrip");
    write_sample_table(&path, 10);

    let reader = TableReader::open(&path).unwrap();
    let stmt = parse("SELECT id, label FROM t WHERE value >= 50 ORDER BY id DESC LIMIT 3").unwrap();
    let (result, _stats) = execute(&reader, &stmt, &ExecuteOptions::default()).unwrap();

    assert_eq!(result.columns, vec!["id".to_string(), "label".to_string()]);
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(9), Value::Utf8("odd".to_string())],
            vec![Value::Int64(8), Value::Utf8("even".to_string())],
            vec![Value::Int64(7), Value::Utf8("odd".to_string())],
        ]
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn select_star_returns_every_column_in_schema_order() {
    let path = temp_path("select_star");
    write_sample_table(&path, 3);

    let reader = TableReader::open(&path).unwrap();
    let stmt = parse("SELECT * FROM t ORDER BY id").unwrap();
    let (result, _) = execute(&reader, &stmt, &ExecuteOptions::default()).unwrap();

    assert_eq!(result.columns, vec!["id", "value", "label"]);
    assert_eq!(
        result.rows[0],
        vec![
            Value::Int64(0),
            Value::Int64(0),
            Value::Utf8("even".to_string())
        ]
    );

    let _ = std::fs::remove_file(&path);
}

/// The whole point of zone-map pruning is that it changes *how much work
/// happens*, never *what the answer is*. This spans several row groups
/// (BLOCK_SIZE is 8192) specifically so pruning has something to do.
#[test]
fn pruning_never_changes_the_result_only_how_much_gets_scanned() {
    let path = temp_path("pruning_equivalence");
    write_sample_table(&path, 50_000);

    let reader = TableReader::open(&path).unwrap();
    let stmt = parse("SELECT id FROM t WHERE value > 495000 ORDER BY id").unwrap();

    let (with_pruning, pruned_stats) = execute(
        &reader,
        &stmt,
        &ExecuteOptions {
            use_pruning: true,
            parallel: true,
        },
    )
    .unwrap();
    let (without_pruning, full_stats) = execute(
        &reader,
        &stmt,
        &ExecuteOptions {
            use_pruning: false,
            parallel: true,
        },
    )
    .unwrap();

    assert_eq!(
        with_pruning.rows, without_pruning.rows,
        "pruning changed the result set"
    );
    assert!(
        pruned_stats.row_groups_scanned < full_stats.row_groups_scanned,
        "this test's data/query should give pruning something real to skip"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn sequential_and_parallel_scans_agree() {
    let path = temp_path("sequential_vs_parallel");
    write_sample_table(&path, 30_000);

    let reader = TableReader::open(&path).unwrap();
    let stmt =
        parse("SELECT id, value FROM t WHERE label = 'odd' AND value < 1000 ORDER BY id").unwrap();

    let (serial, _) = execute(
        &reader,
        &stmt,
        &ExecuteOptions {
            use_pruning: true,
            parallel: false,
        },
    )
    .unwrap();
    let (parallel, _) = execute(
        &reader,
        &stmt,
        &ExecuteOptions {
            use_pruning: true,
            parallel: true,
        },
    )
    .unwrap();

    assert_eq!(serial.rows, parallel.rows);
    assert!(
        !serial.rows.is_empty(),
        "sanity check: the query should actually match something"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn string_equality_predicate_works() {
    let path = temp_path("string_predicate");
    write_sample_table(&path, 20);

    let reader = TableReader::open(&path).unwrap();
    let stmt = parse("SELECT id FROM t WHERE label = 'even' ORDER BY id LIMIT 2").unwrap();
    let (result, _) = execute(&reader, &stmt, &ExecuteOptions::default()).unwrap();

    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(0)], vec![Value::Int64(2)]]
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn unknown_column_in_select_is_a_clean_error() {
    let path = temp_path("unknown_column");
    write_sample_table(&path, 5);

    let reader = TableReader::open(&path).unwrap();
    let stmt = parse("SELECT nonexistent FROM t").unwrap();
    let result = execute(&reader, &stmt, &ExecuteOptions::default());

    assert!(result.is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn comparing_an_int_column_to_a_string_literal_is_a_type_error() {
    let path = temp_path("type_mismatch");
    write_sample_table(&path, 5);

    let reader = TableReader::open(&path).unwrap();
    let stmt = parse("SELECT id FROM t WHERE value = 'not a number'").unwrap();
    let result = execute(&reader, &stmt, &ExecuteOptions::default());

    assert!(result.is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn opening_a_corrupt_or_foreign_file_fails_cleanly_instead_of_panicking() {
    let path = temp_path("not_a_stratum_file");
    std::fs::write(&path, b"this is definitely not a stratum file, just text").unwrap();

    let result = TableReader::open(&path);
    assert!(result.is_err());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn reading_a_truncated_file_fails_cleanly() {
    let path = temp_path("truncated");
    write_sample_table(&path, 20_000);
    let bytes = std::fs::read(&path).unwrap();
    for keep in [bytes.len() - 1, bytes.len() / 2, 20] {
        std::fs::write(&path, &bytes[..keep]).unwrap();
        assert!(TableReader::open(&path).is_err());
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_v1_file_is_rejected_with_a_helpful_message() {
    let path = temp_path("v1_file");
    let mut bytes = vec![0u8; 32];
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(b"STRTFTR1");
    std::fs::write(&path, bytes).unwrap();
    let err = format!("{:#}", TableReader::open(&path).err().unwrap());
    assert!(err.contains("v0.1"), "{err}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn writer_rejects_rows_of_the_wrong_shape_or_type() {
    let path = temp_path("bad_rows");
    let mut w = TableWriter::create(&path, vec![col("a", DataType::Int64)]).unwrap();
    assert!(w.add_row(vec![s("not an int")]).is_err());
    assert!(w.add_row(vec![int(1), int(2)]).is_err());
    assert!(w.add_row(vec![int(1)]).is_ok());
    assert!(TableWriter::create(
        &path,
        vec![col("a", DataType::Int64), col("a", DataType::Utf8)]
    )
    .is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn or_not_in_and_between_filters() {
    let path = temp_path("boolean_filters");
    write_sample_table(&path, 20);

    let ids = |sql: &str| -> Vec<i64> {
        query(&path, sql)
            .0
            .rows
            .iter()
            .map(|r| r[0].as_int64().unwrap())
            .collect()
    };
    assert_eq!(
        ids("SELECT id FROM t WHERE id < 2 OR id > 17 ORDER BY id"),
        [0, 1, 18, 19]
    );
    assert_eq!(
        ids("SELECT id FROM t WHERE id IN (3, 5, 99) ORDER BY id"),
        [3, 5]
    );
    assert_eq!(
        ids("SELECT id FROM t WHERE id BETWEEN 4 AND 6 ORDER BY id"),
        [4, 5, 6]
    );
    assert_eq!(
        ids("SELECT id FROM t WHERE NOT (id >= 2) OR (label = 'odd' AND id > 15) ORDER BY id"),
        [0, 1, 17, 19]
    );
    assert_eq!(
        ids("SELECT id FROM t WHERE 18 <= id ORDER BY id DESC"),
        [19, 18]
    );

    let _ = std::fs::remove_file(&path);
}

/// A table whose string column is sorted by row order, so string zone
/// maps have something to prune.
fn write_regions_table(path: &Path) {
    let regions = ["africa", "americas", "asia", "europe", "oceania"];
    let mut w = TableWriter::create(
        path,
        vec![col("id", DataType::Int64), col("region", DataType::Utf8)],
    )
    .unwrap()
    .with_row_group_size(100);
    for id in 0..1000 {
        w.add_row(vec![int(id), s(regions[id as usize / 200])])
            .unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn string_zone_maps_prune_row_groups() {
    let path = temp_path("string_zone_maps");
    write_regions_table(&path);

    let (result, stats) = query(&path, "SELECT COUNT(*) FROM t WHERE region = 'europe'");
    assert_eq!(result.rows, vec![vec![int(200)]]);
    assert_eq!(stats.row_groups_total, 10);
    assert_eq!(stats.row_groups_scanned, 2);

    // 'europe' and 'oceania' sort after 'b'; 'asia' doesn't.
    let (result, stats) = query(&path, "SELECT COUNT(*) FROM t WHERE region >= 'b'");
    assert_eq!(result.rows, vec![vec![int(400)]]);
    assert_eq!(stats.row_groups_scanned, 4);
    // Each of those groups is entirely >= 'b', so the filter never ran
    // and, since COUNT(*) reads no column, nothing was read at all.
    assert_eq!(stats.row_groups_fully_matched, 4);
    assert_eq!(stats.bytes_read, 0);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn not_is_pruned_through_its_mirror_image() {
    let path = temp_path("not_pruning");
    write_sample_table(&path, 50_000);

    // NOT (value < 400000) can only match where value >= 400000; the
    // first row groups are entirely below that, so they must be skipped.
    let (pruned, stats) = query(
        &path,
        "SELECT id FROM t WHERE NOT value < 400000 ORDER BY id",
    );
    let (plain, _) = query(&path, "SELECT id FROM t WHERE value >= 400000 ORDER BY id");
    assert_eq!(pruned.rows, plain.rows);
    assert!(stats.row_groups_scanned < stats.row_groups_total);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn aggregates_without_group_by() {
    let path = temp_path("aggregates");
    write_sample_table(&path, 10); // id 0..9, value = id * 10

    let (result, stats) = query(
        &path,
        "SELECT COUNT(*), SUM(value), MIN(id), MAX(label), AVG(id) FROM t WHERE id >= 5",
    );
    assert_eq!(
        result.columns,
        ["count(*)", "sum(value)", "min(id)", "max(label)", "avg(id)"]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            int(5),
            int(350),
            int(5),
            s("odd"),
            Value::Float64(7.0)
        ]]
    );
    assert!(!stats.metadata_only);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn aggregates_over_no_rows_return_one_row_of_nulls() {
    let path = temp_path("aggregates_empty");
    write_sample_table(&path, 10);

    let (result, _) = query(
        &path,
        "SELECT COUNT(*), SUM(value), MIN(id), AVG(id) FROM t WHERE id > 100",
    );
    assert_eq!(
        result.rows,
        vec![vec![int(0), Value::Null, Value::Null, Value::Null]]
    );

    // ...but a GROUP BY over no rows returns no groups at all.
    let (result, _) = query(
        &path,
        "SELECT label, COUNT(*) FROM t WHERE id > 100 GROUP BY label",
    );
    assert!(result.rows.is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn count_min_max_are_answered_from_metadata() {
    let path = temp_path("metadata_only");
    write_sample_table(&path, 20_000);

    let (result, stats) = query(
        &path,
        "SELECT COUNT(*), MIN(value), MAX(value), MIN(label) FROM t",
    );
    assert_eq!(
        result.rows,
        vec![vec![int(20_000), int(0), int(199_990), s("even")]]
    );
    assert!(stats.metadata_only);
    assert_eq!(stats.bytes_read, 0);
    assert_eq!(stats.row_groups_scanned, 0);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn group_by_with_order_by_alias_and_position() {
    let path = temp_path("group_by");
    write_regions_table(&path);

    let (result, _) = query(
        &path,
        "SELECT region, COUNT(*) AS n, MIN(id), MAX(id) FROM t \
         WHERE id < 500 OR region = 'oceania' GROUP BY region ORDER BY n DESC, 1 LIMIT 3",
    );
    assert_eq!(result.columns, ["region", "n", "min(id)", "max(id)"]);
    assert_eq!(
        result.rows,
        vec![
            vec![s("africa"), int(200), int(0), int(199)],
            vec![s("americas"), int(200), int(200), int(399)],
            vec![s("oceania"), int(200), int(800), int(999)],
        ]
    );

    // Without ORDER BY, groups come back in key order — deterministic.
    let (result, _) = query(&path, "SELECT region FROM t GROUP BY region");
    let regions: Vec<Value> = result.rows.into_iter().map(|mut r| r.remove(0)).collect();
    assert_eq!(
        regions,
        ["africa", "americas", "asia", "europe", "oceania"].map(s)
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn aggregate_query_errors_are_clean() {
    let path = temp_path("aggregate_errors");
    write_sample_table(&path, 5);

    assert!(query_err(&path, "SELECT id, COUNT(*) FROM t").contains("GROUP BY"));
    assert!(query_err(&path, "SELECT * FROM t GROUP BY id").contains("SELECT *"));
    assert!(query_err(&path, "SELECT SUM(label) FROM t").contains("Int64"));
    assert!(query_err(
        &path,
        "SELECT label, COUNT(*) FROM t GROUP BY label ORDER BY id"
    )
    .contains("output column"));
    assert!(query_err(&path, "SELECT id FROM t ORDER BY 2").contains("out of range"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn sum_overflow_is_an_error_not_a_wrong_answer() {
    let path = temp_path("sum_overflow");
    let mut w = TableWriter::create(&path, vec![col("x", DataType::Int64)]).unwrap();
    w.add_row(vec![int(i64::MAX)]).unwrap();
    w.add_row(vec![int(1)]).unwrap();
    w.finish().unwrap();

    assert!(query_err(&path, "SELECT SUM(x) FROM t").contains("overflow"));
    // AVG accumulates in i128 too, so it's still exact-ish here.
    let (result, _) = query(&path, "SELECT AVG(x) FROM t");
    assert_eq!(
        result.rows,
        vec![vec![Value::Float64(i64::MAX as f64 / 2.0)]]
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_empty_table_round_trips() {
    let path = temp_path("empty_table");
    TableWriter::create(&path, vec![col("x", DataType::Int64)])
        .unwrap()
        .finish()
        .unwrap();

    let (result, _) = query(&path, "SELECT x FROM t WHERE x > 0");
    assert!(result.rows.is_empty());
    let (result, stats) = query(&path, "SELECT COUNT(*), MAX(x) FROM t");
    assert_eq!(result.rows, vec![vec![int(0), Value::Null]]);
    assert!(stats.metadata_only);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn order_by_limit_pushdown_matches_a_full_sort() {
    let path = temp_path("topk");
    write_sample_table(&path, 30_000);

    // Ties on `label` across many row groups: the per-group top-k must
    // keep exactly the rows a full stable sort would.
    let (result, _) = query(&path, "SELECT id, label FROM t ORDER BY label DESC LIMIT 4");
    assert_eq!(
        result.rows,
        vec![
            vec![int(1), s("odd")],
            vec![int(3), s("odd")],
            vec![int(5), s("odd")],
            vec![int(7), s("odd")],
        ]
    );

    let _ = std::fs::remove_file(&path);
}

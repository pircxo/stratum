//! End-to-end tests: write a real `.strat` file, then query it through
//! the actual parser + executor, the same path the CLI uses. These are
//! what actually prove query *correctness*, as opposed to the unit tests
//! in `src/`, which prove individual pieces (the lexer, one encoding)
//! work in isolation.

use std::path::{Path, PathBuf};

use stratum::query::{execute, parse, ExecuteOptions};
use stratum::storage::{ColumnSchema, DataType, TableReader, TableWriter};
use stratum::Value;

/// A fresh temp file path per test, so tests run in parallel without
/// colliding (`cargo test` runs test functions concurrently by default).
fn temp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("stratum_test_{name}_{}.strat", std::process::id()));
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

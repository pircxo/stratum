use std::process::{Command, Output};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stratum"))
        .args(args)
        .output()
        .unwrap()
}

fn success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn csv_import_query_export_and_inspection_work_together() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.csv");
    let table = dir.path().join("table.strat");
    std::fs::write(&input, "id,name\r\n1,\"hello, world\"\r\n2,\"O'Brien\"\r\n3,\"line one\nline two\"\r\n4,\"say \"\"hi\"\"\"\r\n5, padded \r\n").unwrap();
    let input = input.to_str().unwrap();
    let table = table.to_str().unwrap();
    assert!(
        success(&cli(&["load", input, table, "--schema", "id:int,name:str"]))
            .contains("Loaded 5 rows")
    );
    let output = cli(&[
        "query",
        table,
        "SELECT name FROM t ORDER BY id",
        "--format",
        "csv",
        "--explain",
    ]);
    let csv = success(&output);
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let values: Vec<_> = reader
        .records()
        .map(|r| r.unwrap()[0].to_string())
        .collect();
    assert_eq!(
        values,
        vec![
            "hello, world",
            "O'Brien",
            "line one\nline two",
            "say \"hi\"",
            " padded "
        ]
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("rows returned: 5"));
    assert!(success(&cli(&[
        "query",
        table,
        "SELECT id FROM t WHERE name = 'O''Brien'"
    ]))
    .contains("2\n(1 row)"));
    let inspection = success(&cli(&["inspect", table]));
    assert!(inspection.contains("Rows: 5"));
    assert!(inspection.contains("name: Utf8 (dictionary)"));
}

#[test]
fn headerless_and_empty_inputs_work() {
    let dir = tempfile::tempdir().unwrap();
    for (name, csv, header, expected) in [
        ("raw", "7,seven\n", "false", 1),
        ("empty", "id,name\n", "true", 0),
    ] {
        let input = dir.path().join(format!("{name}.csv"));
        let table = dir.path().join(format!("{name}.strat"));
        std::fs::write(&input, csv).unwrap();
        success(&cli(&[
            "load",
            input.to_str().unwrap(),
            table.to_str().unwrap(),
            "--schema",
            "id:int,name:str",
            &format!("--header={header}"),
        ]));
        assert_eq!(
            stratum::TableReader::open(&table).unwrap().row_count(),
            expected
        );
    }
}

#[test]
fn failed_imports_leave_no_output_or_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("bad.csv");
    let table = dir.path().join("table.strat");
    for csv in [
        "id,name\n1,ok\nnot-an-int,bad\n",
        "id,name\n1,ok\n2\n",
        "name,id\nwrong,1\n",
        "name,id\n1,ok\n",
    ] {
        std::fs::write(&input, csv).unwrap();
        let output = cli(&[
            "load",
            input.to_str().unwrap(),
            table.to_str().unwrap(),
            "--schema",
            "id:int,name:str",
        ]);
        assert!(!output.status.success());
        assert!(!table.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn import_refuses_existing_output_including_the_input_file() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.csv");
    let table = dir.path().join("table.strat");
    std::fs::write(&input, "id\n1\n").unwrap();
    std::fs::write(&table, "keep this").unwrap();
    for target in [&table, &input] {
        assert!(!cli(&[
            "load",
            input.to_str().unwrap(),
            target.to_str().unwrap(),
            "--schema",
            "id:int"
        ])
        .status
        .success());
    }
    assert_eq!(std::fs::read_to_string(table).unwrap(), "keep this");
    assert_eq!(std::fs::read_to_string(input).unwrap(), "id\n1\n");
}

#[test]
fn invalid_queries_and_schemas_fail_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.csv");
    let table = dir.path().join("table.strat");
    std::fs::write(&input, "id\n1\n").unwrap();
    for schema in [
        "",
        "id:int,id:str",
        "bad name:int",
        "SELECT:int",
        "id:float",
    ] {
        assert!(!cli(&[
            "load",
            input.to_str().unwrap(),
            table.to_str().unwrap(),
            "--schema",
            schema
        ])
        .status
        .success());
        assert!(!table.exists());
    }
    success(&cli(&[
        "load",
        input.to_str().unwrap(),
        table.to_str().unwrap(),
        "--schema",
        "id:int",
    ]));
    for sql in [
        "SELECT * FROM t LIMIT -1",
        "SELECT * FROM t WHERE id = 'x'",
        "SELECT missing FROM t",
        "SELECT * FROM t WHERE id = 1 OR",
    ] {
        let output = cli(&["query", table.to_str().unwrap(), sql]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn benchmark_smoke_test_checks_both_layouts_and_rejects_zero_rows() {
    let output = Command::new(env!("CARGO_BIN_EXE_bench_prune"))
        .args(["--rows", "20000", "--iterations", "2", "--threads", "2"])
        .output()
        .unwrap();
    let text = success(&output);
    assert!(text.contains("Layout: Clustered"));
    assert!(text.contains("Layout: Random"));
    assert!(text.contains("Pruned 0/3 groups"));
    let output = Command::new(env!("CARGO_BIN_EXE_bench_prune"))
        .args(["--rows", "0"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

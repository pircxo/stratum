use std::io::{Seek, SeekFrom, Write};
use stratum::storage::format::{FileFooter, ZoneMap, FOOTER_MAGIC};
use stratum::{ColumnSchema, DataType, TableReader, TableWriter, Value};

fn schema() -> Vec<ColumnSchema> {
    vec![ColumnSchema {
        name: "id".into(),
        dtype: DataType::Int64,
    }]
}

#[test]
fn invalid_schemas_do_not_create_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("table.strat");
    for columns in [
        vec![],
        vec![ColumnSchema {
            name: "".into(),
            dtype: DataType::Int64,
        }],
        vec![ColumnSchema {
            name: "bad name".into(),
            dtype: DataType::Int64,
        }],
        vec![ColumnSchema {
            name: "SELECT".into(),
            dtype: DataType::Int64,
        }],
        vec![schema()[0].clone(), schema()[0].clone()],
    ] {
        assert!(TableWriter::create(&path, columns).is_err());
        assert!(!path.exists());
    }
}

#[test]
fn invalid_rows_do_not_mutate_buffers_or_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("table.strat");
    let columns = vec![
        schema()[0].clone(),
        ColumnSchema {
            name: "name".into(),
            dtype: DataType::Utf8,
        },
    ];
    let mut writer = TableWriter::create(&path, columns).unwrap();
    assert!(writer.add_row(vec![]).is_err());
    assert!(writer
        .add_row(vec![Value::Int64(99), Value::Int64(99)])
        .is_err());
    writer
        .add_row(vec![Value::Int64(1), Value::Utf8("valid".into())])
        .unwrap();
    writer.finish().unwrap();
    let reader = TableReader::open(&path).unwrap();
    assert_eq!(reader.row_count(), 1);
    assert_eq!(
        reader.read_column_chunk(0, 0).unwrap().get(0),
        Value::Int64(1)
    );
    assert_eq!(
        reader.read_column_chunk(0, 1).unwrap().get(0),
        Value::Utf8("valid".into())
    );
    assert!(reader.read_column_chunk(1, 0).is_err());
    assert!(reader.read_column_chunk(0, 2).is_err());
}

#[test]
fn writer_does_not_overwrite_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.strat");
    std::fs::write(&path, b"keep me").unwrap();
    assert!(TableWriter::create(&path, schema()).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"keep me");
}

#[test]
fn writer_requires_an_empty_file_and_resets_its_cursor() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"keep me").unwrap();
    assert!(TableWriter::new(file, schema()).is_err());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.strat");
    let mut file = std::fs::File::create(&path).unwrap();
    file.seek(SeekFrom::Start(100)).unwrap();
    let mut writer = TableWriter::new(file, schema()).unwrap();
    writer.add_row(vec![Value::Int64(9)]).unwrap();
    writer.finish().unwrap();
    assert_eq!(
        TableReader::open(&path)
            .unwrap()
            .read_column_chunk(0, 0)
            .unwrap()
            .get(0),
        Value::Int64(9)
    );
}

#[test]
fn unaccounted_data_and_invalid_empty_table_schemas_are_rejected() {
    let (_dir, path, data, mut footer) = fixture();
    footer.row_groups.clear();
    footer.row_count = 0;
    write_footer(&path, &data, &footer);
    assert!(TableReader::open(&path).is_err());
    footer.columns.clear();
    write_footer(&path, &[], &footer);
    assert!(TableReader::open(&path).is_err());
    footer.columns = vec![schema()[0].clone(), schema()[0].clone()];
    write_footer(&path, &[], &footer);
    assert!(TableReader::open(&path).is_err());
}

#[test]
fn empty_tables_are_valid_and_queryable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.strat");
    TableWriter::create(&path, schema())
        .unwrap()
        .finish()
        .unwrap();
    let reader = TableReader::open(&path).unwrap();
    assert_eq!(reader.row_count(), 0);
    assert!(reader.row_groups().is_empty());
    let stmt = stratum::query::parse("SELECT * FROM t ORDER BY id LIMIT 0").unwrap();
    let (result, stats) = stratum::query::execute(&reader, &stmt, &Default::default()).unwrap();
    assert!(result.rows.is_empty());
    assert_eq!(result.columns, vec!["id"]);
    assert_eq!(stats.rows_scanned, 0);
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>, FileFooter) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("table.strat");
    let mut writer = TableWriter::create(&path, schema()).unwrap();
    writer.add_row(vec![Value::Int64(42)]).unwrap();
    writer.finish().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let length =
        u64::from_le_bytes(bytes[bytes.len() - 16..bytes.len() - 8].try_into().unwrap()) as usize;
    let start = bytes.len() - 16 - length;
    let footer = bincode::deserialize(&bytes[start..bytes.len() - 16]).unwrap();
    (dir, path, bytes[..start].to_vec(), footer)
}

fn write_footer(path: &std::path::Path, data: &[u8], footer: &FileFooter) {
    let footer = bincode::serialize(footer).unwrap();
    let mut bytes = data.to_vec();
    bytes.extend_from_slice(&footer);
    bytes.extend_from_slice(&(footer.len() as u64).to_le_bytes());
    bytes.extend_from_slice(FOOTER_MAGIC);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn corrupt_metadata_is_rejected_before_scanning() {
    let (_dir, path, data, footer) = fixture();
    let mutations: Vec<fn(&mut FileFooter)> = vec![
        |f| f.row_count += 1,
        |f| f.columns.clear(),
        |f| f.row_groups[0].row_count = 0,
        |f| f.row_groups[0].row_count = 8193,
        |f| f.row_groups[0].chunks.clear(),
        |f| f.row_groups[0].chunks[0].offset = u64::MAX,
        |f| f.row_groups[0].chunks[0].length = u64::MAX,
        |f| f.row_groups[0].chunks[0].zone_map = Some(ZoneMap::Int64 { min: 100, max: 42 }),
        |f| f.row_groups[0].chunks[0].zone_map = None,
    ];
    for mutate in mutations {
        let mut bad = footer.clone();
        mutate(&mut bad);
        write_footer(&path, &data, &bad);
        assert!(
            TableReader::open(&path).is_err(),
            "accepted invalid footer: {bad:?}"
        );
    }
    let mut trailer = Vec::from(u64::MAX.to_le_bytes());
    trailer.extend_from_slice(FOOTER_MAGIC);
    std::fs::write(&path, trailer).unwrap();
    assert!(TableReader::open(&path).is_err());
}

#[test]
fn truncated_files_return_errors() {
    let (_dir, path, data, footer) = fixture();
    write_footer(&path, &data, &footer);
    let bytes = std::fs::read(&path).unwrap();
    for end in 0..bytes.len() {
        std::fs::write(&path, &bytes[..end]).unwrap();
        assert!(
            TableReader::open(&path).is_err(),
            "accepted truncation at byte {end}"
        );
    }
    std::fs::write(&path, &bytes).unwrap();
    let reader = TableReader::open(&path).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(reader.read_column_chunk(0, 0).is_err());
}

#[test]
fn invalid_dictionary_data_returns_an_error_during_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("string.strat");
    let mut writer = TableWriter::create(
        &path,
        vec![ColumnSchema {
            name: "name".into(),
            dtype: DataType::Utf8,
        }],
    )
    .unwrap();
    writer.add_row(vec![Value::Utf8("hello".into())]).unwrap();
    writer.finish().unwrap();
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    file.write_all(&[0xff]).unwrap();
    drop(file);
    let reader = TableReader::open(&path).unwrap();
    let stmt = stratum::query::parse("SELECT name FROM t").unwrap();
    assert!(stratum::query::execute(&reader, &stmt, &Default::default()).is_err());
}

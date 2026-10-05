//! Column chunk encodings. Each function turns one row group's worth of
//! one column's values into bytes, or back. These are the only two
//! encodings Stratum has; both are deliberately simple, documented
//! trade-offs rather than the fastest possible scheme (see
//! ARCHITECTURE.md's "what a v2 would add").

use crate::value::Value;
use std::collections::BTreeMap;

/// Int64: plain, fixed-width little-endian. No bit-packing or delta
/// encoding — every value takes exactly 8 bytes, encode and decode are
/// both a single pass with no branching. The zone map (min/max,
/// computed by the caller while it has the values in hand) is what makes
/// this column type pruneable; the bytes-on-disk encoding itself stays
/// intentionally dumb.
pub fn encode_int64_chunk(values: &[i64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 8);
    for v in values {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

pub fn decode_int64_chunk(bytes: &[u8], row_count: usize) -> Vec<i64> {
    assert_eq!(
        bytes.len(),
        row_count * 8,
        "corrupt int64 chunk: size mismatch"
    );
    (0..row_count)
        .map(|i| {
            let start = i * 8;
            i64::from_le_bytes(bytes[start..start + 8].try_into().unwrap())
        })
        .collect()
}

/// Utf8: dictionary-encoded. Real-world string columns are almost always
/// low-cardinality relative to row count (status codes, categories,
/// repeated names), so storing each *distinct* string once and the rest
/// of the chunk as small integer indices is usually a large win over
/// storing every row's bytes — the same technique Parquet, ORC, and
/// Snowflake's own micro-partitions all use for strings.
///
/// Layout: `[dict_len: u32][dict entries: (len: u32, utf8 bytes)*][row
/// indices: u32 * row_count]`.
pub fn encode_utf8_chunk(values: &[String]) -> Vec<u8> {
    let mut dict_index: BTreeMap<&str, u32> = BTreeMap::new();
    for v in values {
        dict_index.entry(v.as_str()).or_insert(0);
    }
    // BTreeMap iterates in sorted key order, so this assigns dictionary
    // indices 0..N in sorted string order — not load-bearing for
    // correctness, just makes the encoded bytes deterministic for a
    // given input, which is what the storage round-trip tests rely on.
    for (i, (_, idx)) in dict_index.iter_mut().enumerate() {
        *idx = i as u32;
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(&(dict_index.len() as u32).to_le_bytes());
    for key in dict_index.keys() {
        let bytes = key.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    for v in values {
        let idx = dict_index[v.as_str()];
        buf.extend_from_slice(&idx.to_le_bytes());
    }
    buf
}

pub fn decode_utf8_chunk(bytes: &[u8], row_count: usize) -> Vec<String> {
    let mut pos = 0usize;
    let dict_len = read_u32(bytes, &mut pos) as usize;
    let mut dict = Vec::with_capacity(dict_len);
    for _ in 0..dict_len {
        let len = read_u32(bytes, &mut pos) as usize;
        let s = std::str::from_utf8(&bytes[pos..pos + len])
            .expect("corrupt utf8 chunk: invalid dictionary entry")
            .to_string();
        pos += len;
        dict.push(s);
    }

    let mut out = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let idx = read_u32(bytes, &mut pos) as usize;
        out.push(dict[idx].clone());
    }
    out
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> u32 {
    let v = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    v
}

/// Convenience used by the writer: split a homogeneous column batch into
/// its encoded bytes plus (for Int64) its zone map, in one call.
pub fn encode_column_chunk(values: &[Value]) -> (Vec<u8>, Option<i64>, Option<i64>) {
    if values.is_empty() {
        return (Vec::new(), None, None);
    }
    match &values[0] {
        Value::Int64(_) => {
            let ints: Vec<i64> = values.iter().map(|v| v.as_int64().unwrap()).collect();
            let min = *ints.iter().min().unwrap();
            let max = *ints.iter().max().unwrap();
            (encode_int64_chunk(&ints), Some(min), Some(max))
        }
        Value::Utf8(_) => {
            let strs: Vec<String> = values
                .iter()
                .map(|v| v.as_utf8().unwrap().to_string())
                .collect();
            (encode_utf8_chunk(&strs), None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int64_round_trips() {
        let values = vec![-5_i64, 0, 42, i64::MAX, i64::MIN, 100_000];
        let encoded = encode_int64_chunk(&values);
        assert_eq!(decode_int64_chunk(&encoded, values.len()), values);
    }

    #[test]
    fn utf8_round_trips_with_repeats() {
        let values = vec![
            "chestnut".to_string(),
            "black".to_string(),
            "chestnut".to_string(),
            "burgundy".to_string(),
            "chestnut".to_string(),
        ];
        let encoded = encode_utf8_chunk(&values);
        assert_eq!(decode_utf8_chunk(&encoded, values.len()), values);
    }

    #[test]
    fn utf8_dictionary_is_smaller_than_raw_for_repetitive_data() {
        // 1000 rows, one distinct 14-byte string: raw storage would be
        // 14,000 bytes. Dictionary-encoded, it's one 14-byte dictionary
        // entry plus 1000 four-byte indices (the index width is the
        // actual cost here, not the string length) — real compression,
        // just not a huge multiple at this row count. The win grows with
        // row count and shrinks with cardinality; see the `bench_prune`
        // numbers in ARCHITECTURE.md for a realistic end-to-end case.
        let values: Vec<String> = (0..1000).map(|_| "repeated-value".to_string()).collect();
        let encoded = encode_utf8_chunk(&values);
        let raw_size: usize = values.iter().map(|s| s.len()).sum();
        assert!(
            encoded.len() < raw_size / 3,
            "expected dictionary encoding to meaningfully shrink 1000x repeated strings, got {} vs raw {}",
            encoded.len(),
            raw_size
        );
    }

    #[test]
    fn empty_chunk_round_trips() {
        assert_eq!(
            decode_int64_chunk(&encode_int64_chunk(&[]), 0),
            Vec::<i64>::new()
        );
        assert_eq!(
            decode_utf8_chunk(&encode_utf8_chunk(&[]), 0),
            Vec::<String>::new()
        );
    }
}

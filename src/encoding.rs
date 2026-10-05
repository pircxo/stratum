//! Column chunk encodings. Each function turns one row group's worth of
//! one column's values into bytes, or back.
//!
//! Integer chunks are *adaptively* encoded: the writer computes the exact
//! encoded size of each candidate from the chunk's min/max (and, for
//! delta, its min/max consecutive difference) without encoding anything,
//! then encodes once with whichever is smallest. That's the same
//! per-chunk choice Parquet and ORC writers make, in miniature.
//!
//! Decoders never trust their input: every length and index is checked,
//! and corrupt bytes produce an `Err`, never a panic or an out-of-bounds
//! read.

use anyhow::{bail, ensure, Context, Result};

use crate::storage::format::{Encoding, ZoneMap, BLOCK_SIZE};
use crate::value::Value;

// ---------------------------------------------------------------------
// Bit packing
// ---------------------------------------------------------------------

/// Bits needed to represent every value in `0..=max`.
fn bit_width(max: u64) -> u32 {
    64 - max.leading_zeros()
}

fn packed_len(count: usize, width: u32) -> usize {
    (count * width as usize).div_ceil(8)
}

/// Packs each value into exactly `width` bits, little-endian bit order.
/// `width == 0` writes nothing at all — a chunk whose values are all
/// identical costs only its header.
fn pack_bits(values: impl Iterator<Item = u64>, width: u32, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    // At most 7 leftover bits + 64 new ones are ever pending: fits u128.
    let mut acc: u128 = 0;
    let mut pending = 0u32;
    for v in values {
        acc |= (v as u128) << pending;
        pending += width;
        while pending >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            pending -= 8;
        }
    }
    if pending > 0 {
        out.push(acc as u8);
    }
}

fn unpack_bits(bytes: &[u8], width: u32, count: usize) -> Result<Vec<u64>> {
    ensure!(width <= 64, "corrupt chunk: bit width {width} > 64");
    ensure!(
        count <= BLOCK_SIZE,
        "corrupt chunk: row count exceeds row group limit"
    );
    ensure!(
        bytes.len() == packed_len(count, width),
        "corrupt chunk: packed size mismatch"
    );
    if width == 0 {
        return Ok(vec![0; count]);
    }
    ensure!(
        bytes.len() == packed_len(count, width),
        "corrupt chunk: {} packed bytes for {count} values of {width} bits",
        bytes.len()
    );
    let mask: u128 = (1u128 << width) - 1;
    let mut out = Vec::with_capacity(count);
    let mut acc: u128 = 0;
    let mut pending = 0u32;
    let mut next = bytes.iter();
    for _ in 0..count {
        while pending < width {
            // Length was checked above, so this can't run dry.
            acc |= (*next.next().expect("length checked") as u128) << pending;
            pending += 8;
        }
        out.push((acc & mask) as u64);
        acc >>= width;
        pending -= width;
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Int64
// ---------------------------------------------------------------------

/// Frame-of-reference: `[min: i64][width: u8][packed (v - min)]`.
/// Differences are taken with wrapping arithmetic, which is exact here:
/// `max - min` of any two i64s always fits in a u64.
fn encode_for(values: &[i64], min: i64, max: i64, out: &mut Vec<u8>) {
    let width = bit_width(max.wrapping_sub(min) as u64);
    out.extend_from_slice(&min.to_le_bytes());
    out.push(width as u8);
    pack_bits(
        values.iter().map(|v| v.wrapping_sub(min) as u64),
        width,
        out,
    );
}

fn decode_for(bytes: &[u8], count: usize) -> Result<Vec<i64>> {
    ensure!(bytes.len() >= 9, "corrupt bit-packed chunk: missing header");
    let min = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let width = bytes[8] as u32;
    Ok(unpack_bits(&bytes[9..], width, count)?
        .into_iter()
        .map(|off| min.wrapping_add(off as i64))
        .collect())
}

fn for_size(count: usize, min: i64, max: i64) -> usize {
    9 + packed_len(count, bit_width(max.wrapping_sub(min) as u64))
}

fn deltas(values: &[i64]) -> impl Iterator<Item = i64> + '_ {
    values.windows(2).map(|w| w[1].wrapping_sub(w[0]))
}

fn min_max(values: impl Iterator<Item = i64>) -> Option<(i64, i64)> {
    values.fold(None, |acc, v| match acc {
        None => Some((v, v)),
        Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
    })
}

/// Encodes an Int64 chunk with whichever encoding makes it smallest.
/// Ties go to the cheaper-to-decode encoding (plain, then FOR, then delta).
pub fn encode_int64_chunk(values: &[i64]) -> (Encoding, Vec<u8>) {
    let Some((min, max)) = min_max(values.iter().copied()) else {
        return (Encoding::Plain, Vec::new());
    };

    let plain_size = values.len() * 8;
    let for_bytes = for_size(values.len(), min, max);
    let delta_size = match min_max(deltas(values)) {
        Some((dmin, dmax)) => 8 + for_size(values.len() - 1, dmin, dmax),
        None => usize::MAX, // a single value has no deltas; FOR wins anyway
    };

    let mut buf = Vec::new();
    if plain_size <= for_bytes && plain_size <= delta_size {
        buf.reserve(plain_size);
        for v in values {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        (Encoding::Plain, buf)
    } else if for_bytes <= delta_size {
        encode_for(values, min, max, &mut buf);
        (Encoding::BitPacked, buf)
    } else {
        let d: Vec<i64> = deltas(values).collect();
        let (dmin, dmax) = min_max(d.iter().copied()).expect("len >= 2 here");
        buf.extend_from_slice(&values[0].to_le_bytes());
        encode_for(&d, dmin, dmax, &mut buf);
        (Encoding::DeltaBitPacked, buf)
    }
}

pub fn decode_int64_chunk(encoding: Encoding, bytes: &[u8], count: usize) -> Result<Vec<i64>> {
    ensure!(
        count <= BLOCK_SIZE,
        "corrupt chunk: row count exceeds row group limit"
    );
    match encoding {
        Encoding::Plain => {
            ensure!(
                bytes.len() == count * 8,
                "corrupt plain int64 chunk: {} bytes for {count} rows",
                bytes.len()
            );
            Ok((0..count)
                .map(|i| i64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap()))
                .collect())
        }
        Encoding::BitPacked => decode_for(bytes, count),
        Encoding::DeltaBitPacked => {
            if count == 0 {
                ensure!(
                    bytes.is_empty(),
                    "corrupt delta chunk: unexpected bytes for empty chunk"
                );
                return Ok(Vec::new());
            }
            ensure!(bytes.len() >= 8, "corrupt delta chunk: missing first value");
            let first = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
            let d = decode_for(&bytes[8..], count - 1)?;
            let mut out = Vec::with_capacity(count);
            let mut cur = first;
            out.push(cur);
            for delta in d {
                cur = cur.wrapping_add(delta);
                out.push(cur);
            }
            Ok(out)
        }
        Encoding::Dictionary => bail!("corrupt footer: int64 column marked dictionary-encoded"),
    }
}

// ---------------------------------------------------------------------
// Utf8
// ---------------------------------------------------------------------

/// A decoded string chunk, kept in dictionary form. The query engine
/// evaluates string predicates once per *distinct* value and then maps
/// the answer through `indices`, instead of comparing every row's string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictArray {
    /// Sorted, distinct.
    pub dict: Vec<String>,
    /// One per row; every entry is `< dict.len()` (checked on decode).
    pub indices: Vec<u32>,
}

impl DictArray {
    pub fn get(&self, row: usize) -> &str {
        &self.dict[self.indices[row] as usize]
    }
}

/// Utf8: dictionary-encoded. Real-world string columns are almost always
/// low-cardinality relative to row count (status codes, categories,
/// repeated names), so storing each *distinct* string once and the rest
/// of the chunk as small integer indices is usually a large win over
/// storing every row's bytes — the same technique Parquet, ORC, and
/// Snowflake's own micro-partitions all use for strings.
///
/// Layout: `[dict_len: u32][dict entries: (len: u32, utf8 bytes)*]
/// [index width: u8][bit-packed row indices]`. The dictionary is sorted,
/// so its first and last entries are the chunk's zone map for free.
pub fn encode_utf8_chunk(values: &[&str]) -> Vec<u8> {
    let mut dict: Vec<&str> = values.to_vec();
    dict.sort_unstable();
    dict.dedup();

    let mut buf = Vec::new();
    buf.extend_from_slice(&(dict.len() as u32).to_le_bytes());
    for s in &dict {
        buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
        buf.extend_from_slice(s.as_bytes());
    }
    let width = bit_width(dict.len().saturating_sub(1) as u64);
    buf.push(width as u8);
    pack_bits(
        values.iter().map(|v| {
            dict.binary_search(v)
                .expect("every value is in its own dictionary") as u64
        }),
        width,
        &mut buf,
    );
    buf
}

pub fn decode_utf8_chunk(bytes: &[u8], count: usize) -> Result<DictArray> {
    ensure!(
        count <= BLOCK_SIZE,
        "corrupt utf8 chunk: row count exceeds row group limit"
    );
    let mut pos = 0usize;
    let dict_len = read_u32(bytes, &mut pos)? as usize;
    // Every entry needs at least 4 bytes, so this bounds the allocation
    // by the input size even if `dict_len` is garbage.
    ensure!(
        dict_len <= count && dict_len <= bytes.len() / 4,
        "corrupt utf8 chunk: dictionary length {dict_len}"
    );
    let mut dict = Vec::with_capacity(dict_len);
    for _ in 0..dict_len {
        let len = read_u32(bytes, &mut pos)? as usize;
        let end = pos
            .checked_add(len)
            .filter(|&e| e <= bytes.len())
            .context("corrupt utf8 chunk: dictionary entry runs past end")?;
        let s = std::str::from_utf8(&bytes[pos..end])
            .context("corrupt utf8 chunk: invalid UTF-8 in dictionary")?;
        dict.push(s.to_string());
        pos = end;
    }
    ensure!(pos < bytes.len(), "corrupt utf8 chunk: missing index width");
    let width = bytes[pos] as u32;
    ensure!(
        width == bit_width(dict.len().saturating_sub(1) as u64),
        "corrupt utf8 chunk: invalid index width"
    );
    ensure!(
        dict.windows(2).all(|w| w[0] < w[1]),
        "corrupt utf8 chunk: dictionary is not sorted and distinct"
    );
    let raw = unpack_bits(&bytes[pos + 1..], width, count)?;
    let mut indices = Vec::with_capacity(count);
    for idx in raw {
        ensure!(
            idx < dict.len() as u64,
            "corrupt utf8 chunk: index {idx} outside dictionary of {}",
            dict.len()
        );
        indices.push(idx as u32);
    }
    Ok(DictArray { dict, indices })
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let slice = bytes
        .get(*pos..*pos + 4)
        .context("corrupt utf8 chunk: truncated")?;
    *pos += 4;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

// ---------------------------------------------------------------------
// Writer entry point
// ---------------------------------------------------------------------

/// Used by the writer: encode one homogeneous column batch, returning
/// the encoding chosen, the bytes, and the chunk's zone map. The caller
/// has already checked every value matches the column's type.
pub fn encode_column_chunk(values: &[Value]) -> (Encoding, Vec<u8>, Option<ZoneMap>) {
    match values.first() {
        None => (Encoding::Plain, Vec::new(), None),
        Some(Value::Utf8(_)) => {
            let strs: Vec<&str> = values.iter().map(|v| v.as_utf8().unwrap()).collect();
            let zone = ZoneMap::Utf8 {
                min: strs.iter().min().unwrap().to_string(),
                max: strs.iter().max().unwrap().to_string(),
            };
            (Encoding::Dictionary, encode_utf8_chunk(&strs), Some(zone))
        }
        Some(_) => {
            let ints: Vec<i64> = values.iter().map(|v| v.as_int64().unwrap()).collect();
            let (min, max) = min_max(ints.iter().copied()).unwrap();
            let (enc, bytes) = encode_int64_chunk(&ints);
            (enc, bytes, Some(ZoneMap::Int64 { min, max }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[i64]) -> Encoding {
        let (enc, bytes) = encode_int64_chunk(values);
        assert_eq!(
            decode_int64_chunk(enc, &bytes, values.len()).unwrap(),
            values
        );
        enc
    }

    #[test]
    fn bit_packing_round_trips_every_width() {
        for width in 0..=64u32 {
            let max = if width == 0 {
                0
            } else {
                u64::MAX >> (64 - width)
            };
            let values: Vec<u64> = (0..37u64).map(|i| max.wrapping_sub(i * 7) & max).collect();
            let mut buf = Vec::new();
            pack_bits(values.iter().copied(), width, &mut buf);
            assert_eq!(buf.len(), packed_len(values.len(), width));
            assert_eq!(unpack_bits(&buf, width, values.len()).unwrap(), values);
        }
    }

    #[test]
    fn int64_extremes_round_trip() {
        // max - min overflows i64 here; wrapping arithmetic must still
        // reconstruct every value exactly.
        round_trip(&[-5, 0, 42, i64::MAX, i64::MIN, 100_000]);
        round_trip(&[i64::MAX, i64::MIN, i64::MAX, i64::MIN]);
        round_trip(&[7]);
        round_trip(&[]);
    }

    #[test]
    fn sequential_ids_pick_delta_and_cost_almost_nothing() {
        let ids: Vec<i64> = (1_000_000..1_008_192).collect();
        let (enc, bytes) = encode_int64_chunk(&ids);
        assert_eq!(enc, Encoding::DeltaBitPacked);
        // first value + delta header, and zero bits per row: every delta is 1.
        assert_eq!(bytes.len(), 8 + 9);
        assert_eq!(round_trip(&ids), Encoding::DeltaBitPacked);
    }

    #[test]
    fn narrow_range_unordered_values_pick_bit_packing() {
        // Values in 0..16 in scrambled order: 4 bits each beats both
        // 64-bit plain and delta (whose differences span -15..15, 5 bits).
        let values: Vec<i64> = (0..1000).map(|i| (i * 7919) % 16).collect();
        assert_eq!(round_trip(&values), Encoding::BitPacked);
        let (_, bytes) = encode_int64_chunk(&values);
        assert_eq!(bytes.len(), 9 + 500);
    }

    #[test]
    fn full_range_random_values_stay_plain() {
        let values = vec![i64::MIN, 3, i64::MAX, -9, 0, i64::MIN + 1];
        assert_eq!(round_trip(&values), Encoding::Plain);
    }

    #[test]
    fn utf8_round_trips_with_repeats() {
        let values = ["chestnut", "black", "chestnut", "burgundy", "chestnut"];
        let decoded = decode_utf8_chunk(&encode_utf8_chunk(&values), values.len()).unwrap();
        assert_eq!(decoded.dict, vec!["black", "burgundy", "chestnut"]);
        let rows: Vec<&str> = (0..values.len()).map(|i| decoded.get(i)).collect();
        assert_eq!(rows, values);
    }

    #[test]
    fn utf8_dictionary_is_far_smaller_than_raw_for_repetitive_data() {
        // 1000 rows, one distinct 14-byte string: raw storage would be
        // 14,000 bytes. With one dictionary entry the index width is 0
        // bits, so the whole chunk is just its header.
        let values: Vec<&str> = vec!["repeated-value"; 1000];
        let encoded = encode_utf8_chunk(&values);
        assert_eq!(encoded.len(), 4 + 4 + 14 + 1);
    }

    #[test]
    fn empty_chunks_round_trip() {
        assert!(decode_int64_chunk(Encoding::Plain, &[], 0)
            .unwrap()
            .is_empty());
        let d = decode_utf8_chunk(&encode_utf8_chunk(&[]), 0).unwrap();
        assert!(d.dict.is_empty() && d.indices.is_empty());
    }

    #[test]
    fn corrupt_chunks_are_errors_not_panics() {
        assert!(decode_int64_chunk(Encoding::Plain, &[], usize::MAX).is_err());
        assert!(decode_utf8_chunk(&[0; 5], usize::MAX).is_err());
        let mut constant = encode_utf8_chunk(&["same", "same"]);
        constant.push(0);
        assert!(decode_utf8_chunk(&constant, 2).is_err());
        assert!(decode_int64_chunk(Encoding::DeltaBitPacked, &[0], 0).is_err());
        let values: Vec<i64> = (0..100).map(|i| i * 3).collect();
        for enc in [
            Encoding::Plain,
            Encoding::BitPacked,
            Encoding::DeltaBitPacked,
        ] {
            let (_, bytes) = encode_int64_chunk(&values);
            for cut in [0, 1, 5, bytes.len() / 2] {
                let _ = decode_int64_chunk(enc, &bytes[..cut], values.len());
            }
            assert!(decode_int64_chunk(enc, &[0xff; 3], 100).is_err());
        }

        let strs = ["a", "bb", "a", "ccc"];
        let bytes = encode_utf8_chunk(&strs);
        for cut in 0..bytes.len() {
            assert!(decode_utf8_chunk(&bytes[..cut], strs.len()).is_err());
        }
        // A huge claimed dictionary length must not allocate or panic.
        assert!(decode_utf8_chunk(&[0xff, 0xff, 0xff, 0x7f], 1).is_err());
    }
}

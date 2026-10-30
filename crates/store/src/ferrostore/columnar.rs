//! Columnar row groups. One non-empty append is one group; the codec is stored in the blob.

use super::Txn;
use super::doc::{self, ATTEMPTS, GATE_RUNS};
use fl_core::StoreError;
use fl_core::ids::{GateId, ProjectId};
use fl_core::log::{Attempt, GateRun};
use fl_core::verdict::Verdict;
use serde_json::{Number, Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

const CODEC_DELTA: u8 = 1;
const CODEC_DELTA_RLE: u8 = 2;
const CODEC_PLAIN: u8 = 3;
const CODEC_DICT: u8 = 4;
const CODEC_RLE: u8 = 5;
const NAME_MAX: usize = 128;

fn backend(err: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(err.to_string())
}

fn corrupt(detail: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("corrupt ferrostore data: {detail}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int64,
    Float64,
    Bool,
    Utf8,
    Bytes,
    TimestampMs,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnValue {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    Utf8(String),
    Bytes(Vec<u8>),
    TimestampMs(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnPredicate {
    Eq(ColumnValue),
    Lt(ColumnValue),
    Le(ColumnValue),
    Gt(ColumnValue),
    Ge(ColumnValue),
    Between { low: ColumnValue, high: ColumnValue },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnFilter {
    pub column: String,
    pub predicate: ColumnPredicate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZoneMap {
    pub null_count: u64,
    pub min: Option<ColumnValue>,
    pub max: Option<ColumnValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnScan {
    pub rows: Vec<Vec<ColumnValue>>,
    pub row_groups_total: u64,
    pub row_groups_skipped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttemptRollup {
    pub count: u64,
    pub cost_usd_micros: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GateRunRollup {
    pub pass: u64,
    pub fail: u64,
    pub error: u64,
    pub duration_ms: u64,
    pub cost_usd_micros: u64,
}

#[derive(Clone, Debug)]
pub(super) struct ColumnTable {
    pub(super) columns: Vec<ColumnDef>,
    pub(super) groups: Vec<Vec<u8>>,
}

impl ZoneMap {
    /// Whether every value in this chunk fails `predicate`. `min`/`max` of `None`
    /// (all null, or only NaN) excludes every predicate.
    pub fn excludes(&self, predicate: &ColumnPredicate) -> bool {
        let (Some(min), Some(max)) = (&self.min, &self.max) else {
            return true;
        };
        match predicate {
            ColumnPredicate::Eq(value) => !(is_ge(value, min) && is_le(value, max)),
            ColumnPredicate::Lt(value) => !is_lt(min, value),
            ColumnPredicate::Le(value) => !is_le(min, value),
            ColumnPredicate::Gt(value) => !is_gt(max, value),
            ColumnPredicate::Ge(value) => !is_ge(max, value),
            ColumnPredicate::Between { low, high } => {
                !(is_le(low, high) && is_ge(max, low) && is_le(min, high))
            }
        }
    }
}

pub(super) fn type_tag(ty: ColumnType) -> u8 {
    match ty {
        ColumnType::Int64 => 1,
        ColumnType::Float64 => 2,
        ColumnType::Bool => 3,
        ColumnType::Utf8 => 4,
        ColumnType::Bytes => 5,
        ColumnType::TimestampMs => 6,
    }
}

pub(super) fn type_from_tag(tag: u8) -> Result<ColumnType, StoreError> {
    match tag {
        1 => Ok(ColumnType::Int64),
        2 => Ok(ColumnType::Float64),
        3 => Ok(ColumnType::Bool),
        4 => Ok(ColumnType::Utf8),
        5 => Ok(ColumnType::Bytes),
        6 => Ok(ColumnType::TimestampMs),
        _ => Err(corrupt(format!("unknown column type tag {tag}"))),
    }
}

pub(super) fn check_new_table(
    tables: &BTreeMap<String, ColumnTable>,
    name: &str,
    columns: &[ColumnDef],
) -> Result<(), StoreError> {
    check_name("table", name)?;
    if tables.contains_key(name) {
        return Err(backend(format!("column table `{name}` already exists")));
    }
    let mut seen = HashSet::new();
    for column in columns {
        check_name("column", &column.name)?;
        if !seen.insert(column.name.as_str()) {
            return Err(backend(format!("duplicate column `{}`", column.name)));
        }
    }
    Ok(())
}

pub(super) fn check_name(kind: &str, name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(backend(format!("{kind} name is empty")));
    }
    if name.len() > NAME_MAX || name.as_bytes().contains(&b'/') || name.as_bytes().contains(&0) {
        return Err(backend(format!("invalid {kind} name {name:?}")));
    }
    Ok(())
}

/// Decode `bytes` against `columns`. A bad header, a bad codec, or a row
/// count of zero is rejected here, before a frame is published.
pub(super) fn validate_row_group(columns: &[ColumnDef], bytes: &[u8]) -> Result<(), StoreError> {
    let indexed = index_group(bytes, columns)?;
    decode_indexed(columns, &indexed)?;
    Ok(())
}

pub(super) fn encode_row_group(
    columns: &[ColumnDef],
    rows: &[Vec<ColumnValue>],
) -> Result<Vec<u8>, StoreError> {
    validate_rows(columns, rows)?;
    let mut buf = Vec::new();
    let row_count = u32::try_from(rows.len()).map_err(|_| backend("too many rows in one group"))?;
    write_u32(&mut buf, row_count);
    for (index, column) in columns.iter().enumerate() {
        let values = rows
            .iter()
            .map(|row| row[index].clone())
            .collect::<Vec<_>>();
        write_column(&mut buf, column.ty, &values)?;
    }
    Ok(buf)
}

fn validate_rows(columns: &[ColumnDef], rows: &[Vec<ColumnValue>]) -> Result<(), StoreError> {
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(backend(format!(
                "row {row_index} width {} does not match table width {}",
                row.len(),
                columns.len()
            )));
        }
        for (column, cell) in columns.iter().zip(row.iter()) {
            if !cell_matches(column.ty, cell) {
                return Err(backend(format!(
                    "row {row_index} column `{}` has the wrong type",
                    column.name
                )));
            }
        }
    }
    Ok(())
}

fn cell_matches(ty: ColumnType, value: &ColumnValue) -> bool {
    matches!(
        (ty, value),
        (_, ColumnValue::Null)
            | (ColumnType::Int64, ColumnValue::Int64(_))
            | (ColumnType::Float64, ColumnValue::Float64(_))
            | (ColumnType::Bool, ColumnValue::Bool(_))
            | (ColumnType::Utf8, ColumnValue::Utf8(_))
            | (ColumnType::Bytes, ColumnValue::Bytes(_))
            | (ColumnType::TimestampMs, ColumnValue::TimestampMs(_))
    )
}

fn write_column(
    buf: &mut Vec<u8>,
    ty: ColumnType,
    values: &[ColumnValue],
) -> Result<(), StoreError> {
    let zone = zone_map(values);
    write_zone(buf, ty, &zone)?;
    let non_null = values
        .iter()
        .filter(|value| !matches!(value, ColumnValue::Null))
        .cloned()
        .collect::<Vec<_>>();
    let codec = choose_codec(ty, &non_null)?;
    buf.push(codec);
    let mut body = Vec::new();
    write_bitset(&mut body, values);
    write_payload(&mut body, ty, codec, &non_null)?;
    let len = u32::try_from(body.len()).map_err(|_| backend("column chunk exceeds u32"))?;
    write_u32(buf, len);
    buf.extend_from_slice(&body);
    Ok(())
}

fn zone_map(values: &[ColumnValue]) -> ZoneMap {
    let mut null_count = 0u64;
    let mut bounds: Option<(ColumnValue, ColumnValue)> = None;
    for value in values {
        if matches!(value, ColumnValue::Null) {
            null_count = null_count.saturating_add(1);
            continue;
        }
        if let ColumnValue::Float64(number) = value
            && number.is_nan()
        {
            continue;
        }
        bounds = Some(match bounds {
            None => (value.clone(), value.clone()),
            Some((min, max)) => {
                let min = if is_lt(value, &min) {
                    value.clone()
                } else {
                    min
                };
                let max = if is_gt(value, &max) {
                    value.clone()
                } else {
                    max
                };
                (min, max)
            }
        });
    }
    match bounds {
        Some((min, max)) => ZoneMap {
            null_count,
            min: Some(min),
            max: Some(max),
        },
        None => ZoneMap {
            null_count,
            min: None,
            max: None,
        },
    }
}

fn write_zone(buf: &mut Vec<u8>, ty: ColumnType, zone: &ZoneMap) -> Result<(), StoreError> {
    write_u64(buf, zone.null_count);
    match (&zone.min, &zone.max) {
        (None, None) => buf.push(0),
        (Some(min), Some(max)) => {
            buf.push(1);
            write_typed(buf, ty, min)?;
            write_typed(buf, ty, max)?;
        }
        _ => return Err(backend("zone map has only one bound")),
    }
    Ok(())
}

fn choose_codec(ty: ColumnType, values: &[ColumnValue]) -> Result<u8, StoreError> {
    match ty {
        ColumnType::Int64 | ColumnType::TimestampMs => {
            let ints = collect_i64(ty, values)?;
            let deltas = int_deltas(&ints);
            if has_run_at_least(&deltas, 3) {
                Ok(CODEC_DELTA_RLE)
            } else {
                Ok(CODEC_DELTA)
            }
        }
        ColumnType::Bool => Ok(CODEC_RLE),
        ColumnType::Float64 => {
            let nums = collect_f64(values)?;
            if nums.len() >= 3 && all_same_bits(&nums) {
                Ok(CODEC_RLE)
            } else {
                Ok(CODEC_PLAIN)
            }
        }
        ColumnType::Bytes => {
            let blobs = collect_bytes(values)?;
            if blobs.len() >= 3 && blobs.iter().all(|blob| blob == &blobs[0]) {
                Ok(CODEC_RLE)
            } else {
                Ok(CODEC_PLAIN)
            }
        }
        ColumnType::Utf8 => Ok(utf8_codec(&collect_utf8(values)?)),
    }
}

fn utf8_codec(texts: &[String]) -> u8 {
    if texts.len() >= 3 && texts.iter().all(|text| text == &texts[0]) {
        return CODEC_RLE;
    }
    if texts.len() >= 2 {
        let mut seen = HashSet::new();
        for text in texts {
            seen.insert(text.as_str());
        }
        if seen.len() <= texts.len() / 2 {
            return CODEC_DICT;
        }
    }
    CODEC_PLAIN
}

fn write_payload(
    buf: &mut Vec<u8>,
    ty: ColumnType,
    codec: u8,
    values: &[ColumnValue],
) -> Result<(), StoreError> {
    match (ty, codec) {
        (ColumnType::Int64 | ColumnType::TimestampMs, CODEC_DELTA) => {
            write_delta(buf, &collect_i64(ty, values)?);
        }
        (ColumnType::Int64 | ColumnType::TimestampMs, CODEC_DELTA_RLE) => {
            write_delta_rle(buf, &collect_i64(ty, values)?);
        }
        (ColumnType::Utf8, CODEC_PLAIN) => {
            for text in collect_utf8(values)? {
                write_lp(buf, text.as_bytes())?;
            }
        }
        (ColumnType::Utf8, CODEC_DICT) => write_dict(buf, &collect_utf8(values)?)?,
        (ColumnType::Utf8, CODEC_RLE) => {
            let texts = collect_utf8(values)?;
            if let Some(text) = texts.first() {
                write_lp(buf, text.as_bytes())?;
            }
        }
        (ColumnType::Bytes, CODEC_PLAIN) => {
            for blob in collect_bytes(values)? {
                write_lp(buf, &blob)?;
            }
        }
        (ColumnType::Bytes, CODEC_RLE) => {
            let blobs = collect_bytes(values)?;
            if let Some(blob) = blobs.first() {
                write_lp(buf, blob)?;
            }
        }
        (ColumnType::Float64, CODEC_PLAIN) => {
            for number in collect_f64(values)? {
                buf.extend_from_slice(&number.to_le_bytes());
            }
        }
        (ColumnType::Float64, CODEC_RLE) => {
            let nums = collect_f64(values)?;
            if let Some(number) = nums.first() {
                buf.extend_from_slice(&number.to_le_bytes());
            }
        }
        (ColumnType::Bool, CODEC_RLE) => write_bool_rle(buf, &collect_bool(values)?),
        _ => {
            return Err(backend(format!(
                "codec {codec} does not match the column type"
            )));
        }
    }
    Ok(())
}

fn write_delta(buf: &mut Vec<u8>, values: &[i64]) {
    let Some(&first) = values.first() else {
        return;
    };
    write_i64(buf, first);
    let mut prev = first;
    for &next in &values[1..] {
        write_u64(buf, zigzag_encode(next.wrapping_sub(prev)));
        prev = next;
    }
}

fn write_delta_rle(buf: &mut Vec<u8>, values: &[i64]) {
    let Some(&first) = values.first() else {
        return;
    };
    write_i64(buf, first);
    let deltas = int_deltas(values);
    let Some(&mut_first) = deltas.first() else {
        return;
    };
    let mut current = mut_first;
    let mut len = 1u32;
    for &delta in &deltas[1..] {
        if delta == current {
            len += 1;
            continue;
        }
        write_u64(buf, zigzag_encode(current));
        write_u32(buf, len);
        current = delta;
        len = 1;
    }
    write_u64(buf, zigzag_encode(current));
    write_u32(buf, len);
}

fn write_dict(buf: &mut Vec<u8>, values: &[String]) -> Result<(), StoreError> {
    let mut dict: Vec<&str> = Vec::new();
    let mut index_of: HashMap<&str, u32> = HashMap::new();
    let mut indexes = Vec::with_capacity(values.len());
    for value in values {
        if let Some(&index) = index_of.get(value.as_str()) {
            indexes.push(index);
            continue;
        }
        let index = u32::try_from(dict.len()).map_err(|_| backend("dictionary exceeds u32"))?;
        index_of.insert(value.as_str(), index);
        dict.push(value.as_str());
        indexes.push(index);
    }
    write_u32(
        buf,
        u32::try_from(dict.len()).map_err(|_| backend("dictionary exceeds u32"))?,
    );
    for entry in dict {
        write_lp(buf, entry.as_bytes())?;
    }
    for index in indexes {
        write_u32(buf, index);
    }
    Ok(())
}

fn write_bool_rle(buf: &mut Vec<u8>, values: &[bool]) {
    let Some(&first) = values.first() else {
        return;
    };
    let mut current = first;
    let mut len = 1u32;
    for &value in &values[1..] {
        if value == current {
            len += 1;
            continue;
        }
        buf.push(u8::from(current));
        write_u32(buf, len);
        current = value;
        len = 1;
    }
    buf.push(u8::from(current));
    write_u32(buf, len);
}

fn write_bitset(buf: &mut Vec<u8>, values: &[ColumnValue]) {
    let mut bits = vec![0u8; values.len().div_ceil(8)];
    for (index, value) in values.iter().enumerate() {
        if matches!(value, ColumnValue::Null) {
            bits[index / 8] |= 1 << (index % 8);
        }
    }
    buf.extend_from_slice(&bits);
}

struct IndexedColumn<'a> {
    zone: ZoneMap,
    codec: u8,
    body: &'a [u8],
}

struct IndexedGroup<'a> {
    row_count: usize,
    columns: Vec<IndexedColumn<'a>>,
}

pub(super) fn scan_groups(
    columns: &[ColumnDef],
    groups: &[Vec<u8>],
    filters: &[ColumnFilter],
) -> Result<ColumnScan, StoreError> {
    let bound = bind_filters(columns, filters)?;
    let mut rows = Vec::new();
    let mut skipped = 0u64;
    for group in groups {
        let indexed = index_group(group, columns)?;
        let excluded = bound.iter().any(|filter| {
            indexed.columns[filter.index]
                .zone
                .excludes(filter.predicate)
        });
        if excluded {
            skipped = skipped.saturating_add(1);
            continue;
        }
        for row in decode_indexed(columns, &indexed)? {
            if bound
                .iter()
                .all(|filter| matches_predicate(&row[filter.index], filter.predicate))
            {
                rows.push(row);
            }
        }
    }
    Ok(ColumnScan {
        rows,
        row_groups_total: u64::try_from(groups.len())
            .map_err(|_| backend("too many row groups"))?,
        row_groups_skipped: skipped,
    })
}

struct BoundFilter<'a> {
    index: usize,
    predicate: &'a ColumnPredicate,
}

fn bind_filters<'a>(
    columns: &[ColumnDef],
    filters: &'a [ColumnFilter],
) -> Result<Vec<BoundFilter<'a>>, StoreError> {
    let mut bound = Vec::with_capacity(filters.len());
    for filter in filters {
        let Some(index) = columns
            .iter()
            .position(|column| column.name == filter.column)
        else {
            return Err(backend(format!("no column `{}`", filter.column)));
        };
        if !predicate_matches_type(columns[index].ty, &filter.predicate) {
            return Err(backend(format!(
                "predicate type does not match column `{}`",
                filter.column
            )));
        }
        bound.push(BoundFilter {
            index,
            predicate: &filter.predicate,
        });
    }
    Ok(bound)
}

fn predicate_matches_type(ty: ColumnType, predicate: &ColumnPredicate) -> bool {
    match predicate {
        ColumnPredicate::Eq(value)
        | ColumnPredicate::Lt(value)
        | ColumnPredicate::Le(value)
        | ColumnPredicate::Gt(value)
        | ColumnPredicate::Ge(value) => value_type(value) == Some(ty),
        ColumnPredicate::Between { low, high } => {
            value_type(low) == Some(ty) && value_type(high) == Some(ty)
        }
    }
}

fn value_type(value: &ColumnValue) -> Option<ColumnType> {
    match value {
        ColumnValue::Null => None,
        ColumnValue::Int64(_) => Some(ColumnType::Int64),
        ColumnValue::Float64(_) => Some(ColumnType::Float64),
        ColumnValue::Bool(_) => Some(ColumnType::Bool),
        ColumnValue::Utf8(_) => Some(ColumnType::Utf8),
        ColumnValue::Bytes(_) => Some(ColumnType::Bytes),
        ColumnValue::TimestampMs(_) => Some(ColumnType::TimestampMs),
    }
}

fn index_group<'a>(blob: &'a [u8], columns: &[ColumnDef]) -> Result<IndexedGroup<'a>, StoreError> {
    let mut cur = Cur::new(blob);
    let row_count_u32 = cur.u32()?;
    if row_count_u32 == 0 {
        return Err(corrupt("row group has no rows"));
    }
    let row_count =
        usize::try_from(row_count_u32).map_err(|_| corrupt("row count does not fit"))?;
    let mut indexed = Vec::with_capacity(columns.len());
    for column in columns {
        let zone = read_zone(&mut cur, column.ty)?;
        if zone.null_count > u64::from(row_count_u32) {
            return Err(corrupt("null count exceeds the row count"));
        }
        let codec = cur.u8()?;
        let body_len =
            usize::try_from(cur.u32()?).map_err(|_| corrupt("chunk length does not fit"))?;
        let body = cur.take(body_len)?;
        indexed.push(IndexedColumn { zone, codec, body });
    }
    cur.finish()?;
    Ok(IndexedGroup {
        row_count,
        columns: indexed,
    })
}

fn decode_indexed(
    columns: &[ColumnDef],
    indexed: &IndexedGroup<'_>,
) -> Result<Vec<Vec<ColumnValue>>, StoreError> {
    let mut decoded = Vec::with_capacity(columns.len());
    for (column, indexed_column) in columns.iter().zip(&indexed.columns) {
        let values = decode_column(
            column.ty,
            indexed.row_count,
            &indexed_column.zone,
            indexed_column.codec,
            indexed_column.body,
        )?;
        let computed = zone_map(&values);
        if !same_zone(&indexed_column.zone, &computed) {
            return Err(corrupt("zone map does not match the column values"));
        }
        decoded.push(values);
    }
    let mut rows = vec![Vec::with_capacity(columns.len()); indexed.row_count];
    for column in decoded {
        for (row, value) in rows.iter_mut().zip(column) {
            row.push(value);
        }
    }
    Ok(rows)
}

fn decode_column(
    ty: ColumnType,
    row_count: usize,
    zone: &ZoneMap,
    codec: u8,
    body: &[u8],
) -> Result<Vec<ColumnValue>, StoreError> {
    let bit_len = row_count.div_ceil(8);
    if body.len() < bit_len {
        return Err(corrupt("null bitset is truncated"));
    }
    let (bits, payload) = body.split_at(bit_len);
    if !row_count.is_multiple_of(8) {
        let used = row_count % 8;
        let last = bits[bits.len() - 1];
        if last >> used != 0 {
            return Err(corrupt("null bitset has unused bits set"));
        }
    }
    let ones = bits
        .iter()
        .map(|byte| u64::from(byte.count_ones()))
        .sum::<u64>();
    if ones != zone.null_count {
        return Err(corrupt("null bitset does not match null_count"));
    }
    let nulls = usize::try_from(zone.null_count).map_err(|_| corrupt("null count does not fit"))?;
    if nulls > row_count {
        return Err(corrupt("null count exceeds the row count"));
    }
    let non_null_count = row_count - nulls;
    let non_null = decode_non_null(ty, codec, payload, non_null_count)?;
    let mut out = Vec::with_capacity(row_count);
    let mut cursor = 0usize;
    for index in 0..row_count {
        let is_null = bits[index / 8] & (1 << (index % 8)) != 0;
        if is_null {
            out.push(ColumnValue::Null);
            continue;
        }
        let Some(value) = non_null.get(cursor) else {
            return Err(corrupt("column ran out of non-null values"));
        };
        out.push(value.clone());
        cursor += 1;
    }
    if cursor != non_null.len() {
        return Err(corrupt("column has unused non-null values"));
    }
    Ok(out)
}

fn decode_non_null(
    ty: ColumnType,
    codec: u8,
    payload: &[u8],
    n: usize,
) -> Result<Vec<ColumnValue>, StoreError> {
    match (ty, codec) {
        (ColumnType::Int64 | ColumnType::TimestampMs, CODEC_DELTA) => decode_delta(ty, payload, n),
        (ColumnType::Int64 | ColumnType::TimestampMs, CODEC_DELTA_RLE) => {
            decode_delta_rle(ty, payload, n)
        }
        (ColumnType::Utf8, CODEC_PLAIN) => decode_plain_text(payload, n),
        (ColumnType::Utf8, CODEC_DICT) => decode_dict_text(payload, n),
        (ColumnType::Utf8, CODEC_RLE) => decode_rle_text(payload, n),
        (ColumnType::Bytes, CODEC_PLAIN) => decode_plain_bytes(payload, n),
        (ColumnType::Bytes, CODEC_RLE) => decode_rle_bytes(payload, n),
        (ColumnType::Float64, CODEC_PLAIN) => decode_plain_f64(payload, n),
        (ColumnType::Float64, CODEC_RLE) => decode_rle_f64(payload, n),
        (ColumnType::Bool, CODEC_RLE) => decode_rle_bool(payload, n),
        _ => Err(corrupt(format!(
            "codec {codec} does not match the column type"
        ))),
    }
}

fn decode_delta(ty: ColumnType, payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    if n == 0 {
        return empty_payload(payload, "delta");
    }
    let mut cur = Cur::new(payload);
    let mut prev = cur.i64()?;
    let mut ints = Vec::with_capacity(n);
    ints.push(prev);
    for _ in 1..n {
        prev = prev.wrapping_add(zigzag_decode(cur.u64()?));
        ints.push(prev);
    }
    cur.finish()?;
    Ok(ints.into_iter().map(|value| int_value(ty, value)).collect())
}

fn decode_delta_rle(
    ty: ColumnType,
    payload: &[u8],
    n: usize,
) -> Result<Vec<ColumnValue>, StoreError> {
    if n == 0 {
        return empty_payload(payload, "delta_rle");
    }
    let mut cur = Cur::new(payload);
    let mut prev = cur.i64()?;
    let mut ints = Vec::with_capacity(n);
    ints.push(prev);
    while ints.len() < n {
        let delta = zigzag_decode(cur.u64()?);
        let len = usize::try_from(cur.u32()?).map_err(|_| corrupt("delta run does not fit"))?;
        if len == 0 || ints.len() + len > n {
            return Err(corrupt("delta run length is outside the column"));
        }
        for _ in 0..len {
            prev = prev.wrapping_add(delta);
            ints.push(prev);
        }
    }
    cur.finish()?;
    Ok(ints.into_iter().map(|value| int_value(ty, value)).collect())
}

fn empty_payload(payload: &[u8], codec: &str) -> Result<Vec<ColumnValue>, StoreError> {
    if payload.is_empty() {
        Ok(Vec::new())
    } else {
        Err(corrupt(format!("{codec} payload is not empty")))
    }
}

fn decode_plain_text(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    let mut cur = Cur::new(payload);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(ColumnValue::Utf8(cur.lp_str()?));
    }
    cur.finish()?;
    Ok(out)
}

fn decode_dict_text(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    let mut cur = Cur::new(payload);
    let dict_len =
        usize::try_from(cur.u32()?).map_err(|_| corrupt("dictionary count does not fit"))?;
    if dict_len > cur.remaining() {
        return Err(corrupt("dictionary count exceeds the payload"));
    }
    let mut dict = Vec::with_capacity(dict_len);
    for _ in 0..dict_len {
        dict.push(cur.lp_str()?);
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let index =
            usize::try_from(cur.u32()?).map_err(|_| corrupt("dictionary index does not fit"))?;
        let Some(text) = dict.get(index) else {
            return Err(corrupt("dictionary index is out of range"));
        };
        out.push(ColumnValue::Utf8(text.clone()));
    }
    cur.finish()?;
    Ok(out)
}

fn decode_rle_text(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    if n == 0 {
        return empty_payload(payload, "rle");
    }
    let mut cur = Cur::new(payload);
    let text = cur.lp_str()?;
    cur.finish()?;
    Ok(vec![ColumnValue::Utf8(text); n])
}

fn decode_plain_bytes(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    let mut cur = Cur::new(payload);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(ColumnValue::Bytes(cur.lp()?.to_vec()));
    }
    cur.finish()?;
    Ok(out)
}

fn decode_rle_bytes(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    if n == 0 {
        return empty_payload(payload, "rle");
    }
    let mut cur = Cur::new(payload);
    let blob = cur.lp()?.to_vec();
    cur.finish()?;
    Ok(vec![ColumnValue::Bytes(blob); n])
}

fn decode_plain_f64(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    let mut cur = Cur::new(payload);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(ColumnValue::Float64(cur.f64()?));
    }
    cur.finish()?;
    Ok(out)
}

fn decode_rle_f64(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    if n == 0 {
        return empty_payload(payload, "rle");
    }
    let mut cur = Cur::new(payload);
    let number = cur.f64()?;
    cur.finish()?;
    Ok(vec![ColumnValue::Float64(number); n])
}

fn decode_rle_bool(payload: &[u8], n: usize) -> Result<Vec<ColumnValue>, StoreError> {
    let mut cur = Cur::new(payload);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let bit = cur.u8()?;
        if bit > 1 {
            return Err(corrupt("bool run is not 0 or 1"));
        }
        let len = usize::try_from(cur.u32()?).map_err(|_| corrupt("bool run does not fit"))?;
        if len == 0 || out.len() + len > n {
            return Err(corrupt("bool run length is outside the column"));
        }
        out.extend(std::iter::repeat_n(ColumnValue::Bool(bit == 1), len));
    }
    cur.finish()?;
    Ok(out)
}

fn read_zone(cur: &mut Cur<'_>, ty: ColumnType) -> Result<ZoneMap, StoreError> {
    let null_count = cur.u64()?;
    match cur.u8()? {
        0 => Ok(ZoneMap {
            null_count,
            min: None,
            max: None,
        }),
        1 => {
            let min = read_typed(cur, ty)?;
            let max = read_typed(cur, ty)?;
            if !is_le(&min, &max) {
                return Err(corrupt("zone map bounds are not ordered"));
            }
            Ok(ZoneMap {
                null_count,
                min: Some(min),
                max: Some(max),
            })
        }
        _ => Err(corrupt("zone map flag is not 0 or 1")),
    }
}

fn same_zone(stored: &ZoneMap, computed: &ZoneMap) -> bool {
    stored.null_count == computed.null_count
        && same_bound(&stored.min, &computed.min)
        && same_bound(&stored.max, &computed.max)
}

fn same_bound(left: &Option<ColumnValue>, right: &Option<ColumnValue>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => same_value(left, right),
        _ => false,
    }
}

fn same_value(left: &ColumnValue, right: &ColumnValue) -> bool {
    match (left, right) {
        (ColumnValue::Float64(a), ColumnValue::Float64(b)) => a.to_bits() == b.to_bits(),
        _ => left == right,
    }
}

fn matches_predicate(value: &ColumnValue, predicate: &ColumnPredicate) -> bool {
    if matches!(value, ColumnValue::Null) {
        return false;
    }
    match predicate {
        ColumnPredicate::Eq(bound) => is_eq(value, bound),
        ColumnPredicate::Lt(bound) => is_lt(value, bound),
        ColumnPredicate::Le(bound) => is_le(value, bound),
        ColumnPredicate::Gt(bound) => is_gt(value, bound),
        ColumnPredicate::Ge(bound) => is_ge(value, bound),
        ColumnPredicate::Between { low, high } => is_ge(value, low) && is_le(value, high),
    }
}

fn rel(left: &ColumnValue, right: &ColumnValue) -> Option<Ordering> {
    match (left, right) {
        (ColumnValue::Int64(a), ColumnValue::Int64(b)) => Some(a.cmp(b)),
        (ColumnValue::TimestampMs(a), ColumnValue::TimestampMs(b)) => Some(a.cmp(b)),
        (ColumnValue::Float64(a), ColumnValue::Float64(b)) => a.partial_cmp(b),
        (ColumnValue::Bool(a), ColumnValue::Bool(b)) => Some(a.cmp(b)),
        (ColumnValue::Utf8(a), ColumnValue::Utf8(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
        (ColumnValue::Bytes(a), ColumnValue::Bytes(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn is_lt(left: &ColumnValue, right: &ColumnValue) -> bool {
    matches!(rel(left, right), Some(Ordering::Less))
}

fn is_gt(left: &ColumnValue, right: &ColumnValue) -> bool {
    matches!(rel(left, right), Some(Ordering::Greater))
}

fn is_le(left: &ColumnValue, right: &ColumnValue) -> bool {
    matches!(rel(left, right), Some(Ordering::Less | Ordering::Equal))
}

fn is_ge(left: &ColumnValue, right: &ColumnValue) -> bool {
    matches!(rel(left, right), Some(Ordering::Greater | Ordering::Equal))
}

fn is_eq(left: &ColumnValue, right: &ColumnValue) -> bool {
    matches!(rel(left, right), Some(Ordering::Equal))
}

fn int_deltas(values: &[i64]) -> Vec<i64> {
    let mut out = Vec::new();
    let Some(&first) = values.first() else {
        return out;
    };
    let mut prev = first;
    for &next in &values[1..] {
        out.push(next.wrapping_sub(prev));
        prev = next;
    }
    out
}

fn has_run_at_least(deltas: &[i64], min: usize) -> bool {
    let mut run = 0usize;
    let mut prev: Option<i64> = None;
    for delta in deltas {
        if Some(*delta) == prev {
            run += 1;
        } else {
            prev = Some(*delta);
            run = 1;
        }
        if run >= min {
            return true;
        }
    }
    false
}

fn zigzag_encode(value: i64) -> u64 {
    ((value as u64) << 1) ^ ((value >> 63) as u64)
}

fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn int_value(ty: ColumnType, value: i64) -> ColumnValue {
    match ty {
        ColumnType::TimestampMs => ColumnValue::TimestampMs(value),
        _ => ColumnValue::Int64(value),
    }
}

fn collect_i64(ty: ColumnType, values: &[ColumnValue]) -> Result<Vec<i64>, StoreError> {
    values
        .iter()
        .map(|value| match (ty, value) {
            (ColumnType::Int64, ColumnValue::Int64(number)) => Ok(*number),
            (ColumnType::TimestampMs, ColumnValue::TimestampMs(number)) => Ok(*number),
            _ => Err(backend("integer column has a non-integer value")),
        })
        .collect()
}

fn collect_f64(values: &[ColumnValue]) -> Result<Vec<f64>, StoreError> {
    values
        .iter()
        .map(|value| match value {
            ColumnValue::Float64(number) => Ok(*number),
            _ => Err(backend("float column has a non-float value")),
        })
        .collect()
}

fn collect_bool(values: &[ColumnValue]) -> Result<Vec<bool>, StoreError> {
    values
        .iter()
        .map(|value| match value {
            ColumnValue::Bool(bit) => Ok(*bit),
            _ => Err(backend("bool column has a non-bool value")),
        })
        .collect()
}

fn collect_utf8(values: &[ColumnValue]) -> Result<Vec<String>, StoreError> {
    values
        .iter()
        .map(|value| match value {
            ColumnValue::Utf8(text) => Ok(text.clone()),
            _ => Err(backend("utf8 column has a non-string value")),
        })
        .collect()
}

fn collect_bytes(values: &[ColumnValue]) -> Result<Vec<Vec<u8>>, StoreError> {
    values
        .iter()
        .map(|value| match value {
            ColumnValue::Bytes(blob) => Ok(blob.clone()),
            _ => Err(backend("bytes column has a non-bytes value")),
        })
        .collect()
}

fn all_same_bits(values: &[f64]) -> bool {
    let Some(first) = values.first() else {
        return true;
    };
    let bits = first.to_bits();
    values.iter().all(|value| value.to_bits() == bits)
}

fn write_typed(buf: &mut Vec<u8>, ty: ColumnType, value: &ColumnValue) -> Result<(), StoreError> {
    match (ty, value) {
        (ColumnType::Int64, ColumnValue::Int64(number))
        | (ColumnType::TimestampMs, ColumnValue::TimestampMs(number)) => write_i64(buf, *number),
        (ColumnType::Float64, ColumnValue::Float64(number)) => {
            buf.extend_from_slice(&number.to_le_bytes());
        }
        (ColumnType::Bool, ColumnValue::Bool(bit)) => buf.push(u8::from(*bit)),
        (ColumnType::Utf8, ColumnValue::Utf8(text)) => write_lp(buf, text.as_bytes())?,
        (ColumnType::Bytes, ColumnValue::Bytes(blob)) => write_lp(buf, blob)?,
        _ => return Err(backend("zone map bound does not match the column type")),
    }
    Ok(())
}

fn read_typed(cur: &mut Cur<'_>, ty: ColumnType) -> Result<ColumnValue, StoreError> {
    match ty {
        ColumnType::Int64 => Ok(ColumnValue::Int64(cur.i64()?)),
        ColumnType::TimestampMs => Ok(ColumnValue::TimestampMs(cur.i64()?)),
        ColumnType::Float64 => Ok(ColumnValue::Float64(cur.f64()?)),
        ColumnType::Bool => match cur.u8()? {
            0 => Ok(ColumnValue::Bool(false)),
            1 => Ok(ColumnValue::Bool(true)),
            _ => Err(corrupt("bool bound is not 0 or 1")),
        },
        ColumnType::Utf8 => Ok(ColumnValue::Utf8(cur.lp_str()?)),
        ColumnType::Bytes => Ok(ColumnValue::Bytes(cur.lp()?.to_vec())),
    }
}

fn write_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn write_i64(buf: &mut Vec<u8>, value: i64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn write_lp(buf: &mut Vec<u8>, bytes: &[u8]) -> Result<(), StoreError> {
    let len = u32::try_from(bytes.len()).map_err(|_| backend("length does not fit in u32"))?;
    write_u32(buf, len);
    buf.extend_from_slice(bytes);
    Ok(())
}

struct Cur<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cur<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.at)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StoreError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| corrupt("length overflow"))?;
        if end > self.buf.len() {
            return Err(corrupt("truncated record"));
        }
        let bytes = &self.buf[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, StoreError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, StoreError> {
        let bytes = self.take(4)?;
        let mut arr = [0u8; 4];
        arr.copy_from_slice(bytes);
        Ok(u32::from_le_bytes(arr))
    }

    fn u64(&mut self) -> Result<u64, StoreError> {
        let bytes = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(arr))
    }

    fn i64(&mut self) -> Result<i64, StoreError> {
        Ok(i64::from_le_bytes(self.u64()?.to_le_bytes()))
    }

    fn f64(&mut self) -> Result<f64, StoreError> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn lp(&mut self) -> Result<&'a [u8], StoreError> {
        let n = usize::try_from(self.u32()?).map_err(|_| corrupt("length does not fit"))?;
        if n > self.remaining() {
            return Err(corrupt("length prefix exceeds the record"));
        }
        self.take(n)
    }

    fn lp_str(&mut self) -> Result<String, StoreError> {
        let bytes = self.lp()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("text is not utf-8"))
    }

    fn finish(self) -> Result<(), StoreError> {
        if self.at != self.buf.len() {
            return Err(corrupt("trailing bytes inside a committed record"));
        }
        Ok(())
    }
}

pub(super) fn attempt_rollup(
    txn: &Txn,
    project: Option<&ProjectId>,
) -> Result<AttemptRollup, StoreError> {
    let attempts = doc::scan_u64::<Attempt>(txn, ATTEMPTS)?;
    let columns = attempt_columns();
    let mut rows = Vec::with_capacity(attempts.len());
    for (_, attempt) in attempts {
        rows.push(vec![
            ColumnValue::Utf8(attempt.project.as_str().to_string()),
            ColumnValue::Int64(u_to_i(attempt.cost_usd_micros)?),
            ColumnValue::Int64(u_to_i(attempt.tokens_in)?),
            ColumnValue::Int64(u_to_i(attempt.tokens_out)?),
            ColumnValue::Int64(u_to_i(attempt.duration_ms)?),
        ]);
    }
    let mut filters = Vec::new();
    if let Some(project) = project {
        filters.push(ColumnFilter {
            column: "project".to_string(),
            predicate: ColumnPredicate::Eq(ColumnValue::Utf8(project.as_str().to_string())),
        });
    }
    let scan = scan_projected(&columns, &rows, &filters)?;
    let mut rollup = AttemptRollup::default();
    for row in scan.rows {
        rollup.count = rollup
            .count
            .checked_add(1)
            .ok_or_else(|| backend("rollup overflow"))?;
        add_cell(&mut rollup.cost_usd_micros, &row[1])?;
        add_cell(&mut rollup.tokens_in, &row[2])?;
        add_cell(&mut rollup.tokens_out, &row[3])?;
        add_cell(&mut rollup.duration_ms, &row[4])?;
    }
    Ok(rollup)
}

pub(super) fn gate_run_rollup(
    txn: &Txn,
    gate: Option<&GateId>,
) -> Result<GateRunRollup, StoreError> {
    let runs = doc::scan_u64::<GateRun>(txn, GATE_RUNS)?;
    let columns = gate_columns();
    let mut rows = Vec::with_capacity(runs.len());
    for (_, run) in runs {
        rows.push(vec![
            ColumnValue::Utf8(run.gate.as_str().to_string()),
            ColumnValue::Utf8(verdict_name(&run.verdict).to_string()),
            ColumnValue::Int64(u_to_i(run.duration_ms)?),
            ColumnValue::Int64(u_to_i(run.cost_usd_micros)?),
        ]);
    }
    let mut filters = Vec::new();
    if let Some(gate) = gate {
        filters.push(ColumnFilter {
            column: "gate".to_string(),
            predicate: ColumnPredicate::Eq(ColumnValue::Utf8(gate.as_str().to_string())),
        });
    }
    let scan = scan_projected(&columns, &rows, &filters)?;
    let mut rollup = GateRunRollup::default();
    for row in scan.rows {
        match &row[1] {
            ColumnValue::Utf8(name) if name == "pass" => {
                rollup.pass = rollup
                    .pass
                    .checked_add(1)
                    .ok_or_else(|| backend("rollup overflow"))?;
            }
            ColumnValue::Utf8(name) if name == "fail" => {
                rollup.fail = rollup
                    .fail
                    .checked_add(1)
                    .ok_or_else(|| backend("rollup overflow"))?;
            }
            ColumnValue::Utf8(name) if name == "error" => {
                rollup.error = rollup
                    .error
                    .checked_add(1)
                    .ok_or_else(|| backend("rollup overflow"))?;
            }
            _ => return Err(backend("gate verdict column is not pass, fail, or error")),
        }
        add_cell(&mut rollup.duration_ms, &row[2])?;
        add_cell(&mut rollup.cost_usd_micros, &row[3])?;
    }
    Ok(rollup)
}

fn scan_projected(
    columns: &[ColumnDef],
    rows: &[Vec<ColumnValue>],
    filters: &[ColumnFilter],
) -> Result<ColumnScan, StoreError> {
    let groups = if rows.is_empty() {
        Vec::new()
    } else {
        vec![encode_row_group(columns, rows)?]
    };
    scan_groups(columns, &groups, filters)
}

fn attempt_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef {
            name: "project".to_string(),
            ty: ColumnType::Utf8,
        },
        ColumnDef {
            name: "cost_usd_micros".to_string(),
            ty: ColumnType::Int64,
        },
        ColumnDef {
            name: "tokens_in".to_string(),
            ty: ColumnType::Int64,
        },
        ColumnDef {
            name: "tokens_out".to_string(),
            ty: ColumnType::Int64,
        },
        ColumnDef {
            name: "duration_ms".to_string(),
            ty: ColumnType::Int64,
        },
    ]
}

fn gate_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef {
            name: "gate".to_string(),
            ty: ColumnType::Utf8,
        },
        ColumnDef {
            name: "verdict".to_string(),
            ty: ColumnType::Utf8,
        },
        ColumnDef {
            name: "duration_ms".to_string(),
            ty: ColumnType::Int64,
        },
        ColumnDef {
            name: "cost_usd_micros".to_string(),
            ty: ColumnType::Int64,
        },
    ]
}

fn verdict_name(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Pass { .. } => "pass",
        Verdict::Fail { .. } => "fail",
        Verdict::Error { .. } => "error",
    }
}

fn u_to_i(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| backend("ledger value does not fit in i64"))
}

fn add_cell(slot: &mut u64, value: &ColumnValue) -> Result<(), StoreError> {
    let ColumnValue::Int64(number) = value else {
        return Err(backend("rollup column is not an int64"));
    };
    let number = u64::try_from(*number).map_err(|_| backend("rollup value is negative"))?;
    *slot = slot
        .checked_add(number)
        .ok_or_else(|| backend("rollup overflow"))?;
    Ok(())
}

pub(super) fn type_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Int64 => "int64",
        ColumnType::Float64 => "float64",
        ColumnType::Bool => "bool",
        ColumnType::Utf8 => "utf8",
        ColumnType::Bytes => "bytes",
        ColumnType::TimestampMs => "timestamp_ms",
    }
}

pub(super) fn parse_type_name(name: &str) -> Result<ColumnType, StoreError> {
    match name {
        "int64" => Ok(ColumnType::Int64),
        "float64" => Ok(ColumnType::Float64),
        "bool" => Ok(ColumnType::Bool),
        "utf8" => Ok(ColumnType::Utf8),
        "bytes" => Ok(ColumnType::Bytes),
        "timestamp_ms" => Ok(ColumnType::TimestampMs),
        other => Err(backend(format!("unknown column type `{other}`"))),
    }
}

pub(super) fn column_def_json(column: &ColumnDef) -> Value {
    json!({
        "name": column.name,
        "ty": type_name(column.ty),
    })
}

pub(super) fn parse_columns(value: &Value) -> Result<Vec<ColumnDef>, StoreError> {
    let items = value
        .as_array()
        .ok_or_else(|| backend("columns is not an array"))?;
    items.iter().map(parse_column_def).collect()
}

fn parse_column_def(value: &Value) -> Result<ColumnDef, StoreError> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("column is missing name"))?
        .to_string();
    let ty = value
        .get("ty")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("column is missing ty"))?;
    Ok(ColumnDef {
        name,
        ty: parse_type_name(ty)?,
    })
}

pub(super) fn cell_json(cell: &ColumnValue) -> Result<Value, StoreError> {
    Ok(match cell {
        ColumnValue::Null => json!({"t": "null"}),
        ColumnValue::Int64(value) => json!({"t": "i64", "v": value}),
        ColumnValue::Float64(value) => {
            if value.is_finite() {
                let number = Number::from_f64(*value)
                    .ok_or_else(|| backend("f64 cell is not a finite json number"))?;
                json!({"t": "f64", "v": number})
            } else {
                json!({
                    "t": "f64",
                    "v": non_finite_f64_label(*value),
                    "bits": value.to_bits(),
                })
            }
        }
        ColumnValue::Bool(value) => json!({"t": "bool", "v": value}),
        ColumnValue::Utf8(value) => json!({"t": "utf8", "v": value}),
        ColumnValue::Bytes(value) => json!({"t": "bytes", "v": value}),
        ColumnValue::TimestampMs(value) => json!({"t": "ts", "v": value}),
    })
}

pub(super) fn parse_cell(value: &Value) -> Result<ColumnValue, StoreError> {
    let kind = value
        .get("t")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("cell is missing t"))?;
    if kind == "null" {
        return Ok(ColumnValue::Null);
    }
    let raw = value.get("v").ok_or_else(|| backend("cell is missing v"))?;
    match kind {
        "i64" => Ok(ColumnValue::Int64(json_i64(raw)?)),
        "f64" => Ok(ColumnValue::Float64(parse_f64_cell(value)?)),
        "bool" => Ok(ColumnValue::Bool(
            raw.as_bool()
                .ok_or_else(|| backend("bool cell is not a bool"))?,
        )),
        "utf8" => Ok(ColumnValue::Utf8(
            raw.as_str()
                .ok_or_else(|| backend("utf8 cell is not a string"))?
                .to_string(),
        )),
        "bytes" => Ok(ColumnValue::Bytes(parse_bytes(raw)?)),
        "ts" => Ok(ColumnValue::TimestampMs(json_i64(raw)?)),
        other => Err(backend(format!("unknown cell type `{other}`"))),
    }
}

fn non_finite_f64_label(value: f64) -> &'static str {
    if value.is_nan() {
        "NaN"
    } else if value.is_sign_negative() {
        "-Infinity"
    } else {
        "Infinity"
    }
}

fn f64_label_matches(label: &str, value: f64) -> bool {
    match label {
        "NaN" => value.is_nan(),
        "Infinity" => value.is_infinite() && value.is_sign_positive(),
        "-Infinity" => value.is_infinite() && value.is_sign_negative(),
        _ => false,
    }
}

fn parse_f64_cell(cell: &Value) -> Result<f64, StoreError> {
    let raw = cell.get("v").ok_or_else(|| backend("cell is missing v"))?;
    if let Some(label) = raw.as_str() {
        if !matches!(label, "NaN" | "Infinity" | "-Infinity") {
            return Err(backend("finite f64 cell must not use a string"));
        }
        let Some(bits) = cell.get("bits") else {
            return Ok(match label {
                "NaN" => f64::NAN,
                "Infinity" => f64::INFINITY,
                _ => f64::NEG_INFINITY,
            });
        };
        let bits = bits
            .as_u64()
            .ok_or_else(|| backend("f64 bits are not an integer in 0..=u64::MAX"))?;
        let value = f64::from_bits(bits);
        if !f64_label_matches(label, value) {
            return Err(backend("f64 bits do not match v"));
        }
        return Ok(value);
    }
    if cell.get("bits").is_some() {
        return Err(backend("finite f64 cell must not include bits"));
    }
    raw.as_f64()
        .ok_or_else(|| backend("f64 cell is not a number"))
}

fn json_i64(value: &Value) -> Result<i64, StoreError> {
    value.as_i64().ok_or_else(|| backend("cell is not an i64"))
}

pub(super) fn parse_rows(value: &Value) -> Result<Vec<Vec<ColumnValue>>, StoreError> {
    let items = value
        .as_array()
        .ok_or_else(|| backend("rows is not an array"))?;
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let cells = item
            .as_array()
            .ok_or_else(|| backend("row is not an array"))?;
        let mut row = Vec::with_capacity(cells.len());
        for cell in cells {
            row.push(parse_cell(cell)?);
        }
        rows.push(row);
    }
    Ok(rows)
}

pub(super) fn filter_json(filter: &ColumnFilter) -> Result<Value, StoreError> {
    let column = &filter.column;
    Ok(match &filter.predicate {
        ColumnPredicate::Eq(value) => {
            json!({"column": column, "op": "eq", "value": cell_json(value)?})
        }
        ColumnPredicate::Lt(value) => {
            json!({"column": column, "op": "lt", "value": cell_json(value)?})
        }
        ColumnPredicate::Le(value) => {
            json!({"column": column, "op": "le", "value": cell_json(value)?})
        }
        ColumnPredicate::Gt(value) => {
            json!({"column": column, "op": "gt", "value": cell_json(value)?})
        }
        ColumnPredicate::Ge(value) => {
            json!({"column": column, "op": "ge", "value": cell_json(value)?})
        }
        ColumnPredicate::Between { low, high } => json!({
            "column": column,
            "op": "between",
            "low": cell_json(low)?,
            "high": cell_json(high)?,
        }),
    })
}

pub(super) fn parse_filters(value: &Value) -> Result<Vec<ColumnFilter>, StoreError> {
    let items = value
        .as_array()
        .ok_or_else(|| backend("filters is not an array"))?;
    items.iter().map(parse_filter).collect()
}

fn parse_filter(value: &Value) -> Result<ColumnFilter, StoreError> {
    let column = value
        .get("column")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("filter is missing column"))?
        .to_string();
    let op = value
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| backend("filter is missing op"))?;
    let predicate = match op {
        "eq" => ColumnPredicate::Eq(parse_cell(required(value, "value")?)?),
        "lt" => ColumnPredicate::Lt(parse_cell(required(value, "value")?)?),
        "le" => ColumnPredicate::Le(parse_cell(required(value, "value")?)?),
        "gt" => ColumnPredicate::Gt(parse_cell(required(value, "value")?)?),
        "ge" => ColumnPredicate::Ge(parse_cell(required(value, "value")?)?),
        "between" => ColumnPredicate::Between {
            low: parse_cell(required(value, "low")?)?,
            high: parse_cell(required(value, "high")?)?,
        },
        other => return Err(backend(format!("unknown filter op `{other}`"))),
    };
    Ok(ColumnFilter { column, predicate })
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value, StoreError> {
    value
        .get(key)
        .ok_or_else(|| backend(format!("filter is missing {key}")))
}

pub(super) fn scan_json(scan: &ColumnScan) -> Result<Value, StoreError> {
    let mut rows = Vec::with_capacity(scan.rows.len());
    for row in &scan.rows {
        let mut cells = Vec::with_capacity(row.len());
        for cell in row {
            cells.push(cell_json(cell)?);
        }
        rows.push(Value::Array(cells));
    }
    Ok(json!({
        "rows": rows,
        "row_groups_total": scan.row_groups_total,
        "row_groups_skipped": scan.row_groups_skipped,
    }))
}

pub(super) fn parse_scan(value: &Value) -> Result<ColumnScan, StoreError> {
    let rows = parse_rows(
        value
            .get("rows")
            .ok_or_else(|| backend("scan is missing rows"))?,
    )?;
    Ok(ColumnScan {
        rows,
        row_groups_total: json_u64(value, "row_groups_total")?,
        row_groups_skipped: json_u64(value, "row_groups_skipped")?,
    })
}

pub(super) fn attempt_rollup_json(rollup: &AttemptRollup) -> Value {
    json!({
        "count": rollup.count,
        "cost_usd_micros": rollup.cost_usd_micros,
        "tokens_in": rollup.tokens_in,
        "tokens_out": rollup.tokens_out,
        "duration_ms": rollup.duration_ms,
    })
}

pub(super) fn parse_attempt_rollup(value: &Value) -> Result<AttemptRollup, StoreError> {
    Ok(AttemptRollup {
        count: json_u64(value, "count")?,
        cost_usd_micros: json_u64(value, "cost_usd_micros")?,
        tokens_in: json_u64(value, "tokens_in")?,
        tokens_out: json_u64(value, "tokens_out")?,
        duration_ms: json_u64(value, "duration_ms")?,
    })
}

pub(super) fn gate_rollup_json(rollup: &GateRunRollup) -> Value {
    json!({
        "pass": rollup.pass,
        "fail": rollup.fail,
        "error": rollup.error,
        "duration_ms": rollup.duration_ms,
        "cost_usd_micros": rollup.cost_usd_micros,
    })
}

pub(super) fn parse_gate_rollup(value: &Value) -> Result<GateRunRollup, StoreError> {
    Ok(GateRunRollup {
        pass: json_u64(value, "pass")?,
        fail: json_u64(value, "fail")?,
        error: json_u64(value, "error")?,
        duration_ms: json_u64(value, "duration_ms")?,
        cost_usd_micros: json_u64(value, "cost_usd_micros")?,
    })
}

fn json_u64(value: &Value, key: &str) -> Result<u64, StoreError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| backend(format!("missing integer field `{key}`")))
}

pub(super) fn tables_json(tables: &BTreeMap<String, ColumnTable>) -> Value {
    Value::Array(
        tables
            .iter()
            .map(|(name, table)| {
                json!({
                    "name": name,
                    "columns": table.columns.iter().map(column_def_json).collect::<Vec<_>>(),
                    "groups": table.groups.iter().map(|group| bytes_json(group)).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

pub(super) fn parse_tables(value: &Value) -> Result<BTreeMap<String, ColumnTable>, StoreError> {
    let items = value
        .as_array()
        .ok_or_else(|| backend("tables is not an array"))?;
    let mut tables = BTreeMap::new();
    for item in items {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| backend("table is missing name"))?
            .to_string();
        let columns = parse_columns(
            item.get("columns")
                .ok_or_else(|| backend("table is missing columns"))?,
        )?;
        let groups_json = item
            .get("groups")
            .and_then(Value::as_array)
            .ok_or_else(|| backend("table is missing groups"))?;
        let mut groups = Vec::with_capacity(groups_json.len());
        for group in groups_json {
            groups.push(parse_bytes(group)?);
        }
        if tables
            .insert(name.clone(), ColumnTable { columns, groups })
            .is_some()
        {
            return Err(backend(format!("duplicate column table `{name}`")));
        }
    }
    Ok(tables)
}

pub(super) fn parse_bytes(value: &Value) -> Result<Vec<u8>, StoreError> {
    let items = value
        .as_array()
        .ok_or_else(|| backend("bytes are not an array"))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let number = item
            .as_u64()
            .ok_or_else(|| backend("byte is not an integer"))?;
        let byte = u8::try_from(number).map_err(|_| backend("byte is outside 0..=255"))?;
        out.push(byte);
    }
    Ok(out)
}

fn bytes_json(bytes: &[u8]) -> Value {
    Value::Array(bytes.iter().copied().map(Value::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column_codecs(blob: &[u8], columns: &[ColumnDef]) -> Result<Vec<u8>, StoreError> {
        Ok(index_group(blob, columns)?
            .columns
            .into_iter()
            .map(|column| column.codec)
            .collect())
    }

    fn int_col() -> Vec<ColumnDef> {
        vec![ColumnDef {
            name: "n".to_string(),
            ty: ColumnType::Int64,
        }]
    }

    fn round_trip(columns: &[ColumnDef], rows: &[Vec<ColumnValue>]) -> Vec<u8> {
        let blob = encode_row_group(columns, rows).unwrap();
        let scan = scan_groups(columns, std::slice::from_ref(&blob), &[]).unwrap();
        assert_eq!(scan.rows, rows);
        assert_eq!(scan.row_groups_total, 1);
        assert_eq!(scan.row_groups_skipped, 0);
        blob
    }

    #[test]
    fn int_delta_and_delta_rle_round_trip() {
        let columns = int_col();
        let varied = vec![
            vec![ColumnValue::Int64(1)],
            vec![ColumnValue::Int64(3)],
            vec![ColumnValue::Int64(7)],
        ];
        let blob = round_trip(&columns, &varied);
        assert_eq!(column_codecs(&blob, &columns).unwrap(), vec![CODEC_DELTA]);

        let identical = vec![
            vec![ColumnValue::Int64(5)],
            vec![ColumnValue::Int64(5)],
            vec![ColumnValue::Int64(5)],
            vec![ColumnValue::Int64(5)],
        ];
        let blob = round_trip(&columns, &identical);
        assert_eq!(
            column_codecs(&blob, &columns).unwrap(),
            vec![CODEC_DELTA_RLE]
        );

        let stepped = vec![
            vec![ColumnValue::Int64(0)],
            vec![ColumnValue::Int64(10)],
            vec![ColumnValue::Int64(20)],
            vec![ColumnValue::Int64(30)],
        ];
        let blob = round_trip(&columns, &stepped);
        assert_eq!(
            column_codecs(&blob, &columns).unwrap(),
            vec![CODEC_DELTA_RLE]
        );

        let with_nulls = vec![
            vec![ColumnValue::Int64(i64::MIN)],
            vec![ColumnValue::Null],
            vec![ColumnValue::Int64(i64::MAX)],
            vec![ColumnValue::Int64(i64::MAX)],
        ];
        round_trip(&columns, &with_nulls);

        let ts = vec![ColumnDef {
            name: "t".to_string(),
            ty: ColumnType::TimestampMs,
        }];
        round_trip(
            &ts,
            &[
                vec![ColumnValue::TimestampMs(10)],
                vec![ColumnValue::TimestampMs(11)],
                vec![ColumnValue::TimestampMs(12)],
                vec![ColumnValue::TimestampMs(13)],
            ],
        );
    }

    #[test]
    fn utf8_dictionary_and_plain_round_trip() {
        let columns = vec![ColumnDef {
            name: "s".to_string(),
            ty: ColumnType::Utf8,
        }];
        let repeated = ["a", "b", "a", "b"]
            .into_iter()
            .map(|text| vec![ColumnValue::Utf8(text.to_string())])
            .collect::<Vec<_>>();
        let blob = round_trip(&columns, &repeated);
        assert_eq!(column_codecs(&blob, &columns).unwrap(), vec![CODEC_DICT]);

        let unique = ["a", "b", "c", "d"]
            .into_iter()
            .map(|text| vec![ColumnValue::Utf8(text.to_string())])
            .collect::<Vec<_>>();
        let blob = round_trip(&columns, &unique);
        assert_eq!(column_codecs(&blob, &columns).unwrap(), vec![CODEC_PLAIN]);

        let same = vec![
            vec![ColumnValue::Utf8("z".to_string())],
            vec![ColumnValue::Utf8("z".to_string())],
            vec![ColumnValue::Utf8("z".to_string())],
        ];
        let blob = round_trip(&columns, &same);
        assert_eq!(column_codecs(&blob, &columns).unwrap(), vec![CODEC_RLE]);
    }

    #[test]
    fn other_codecs_round_trip_and_nan_is_unordered() {
        let blobs = vec![ColumnDef {
            name: "b".to_string(),
            ty: ColumnType::Bytes,
        }];
        let same = vec![
            vec![ColumnValue::Bytes(b"xy".to_vec())],
            vec![ColumnValue::Bytes(b"xy".to_vec())],
            vec![ColumnValue::Bytes(b"xy".to_vec())],
        ];
        let blob = round_trip(&blobs, &same);
        assert_eq!(column_codecs(&blob, &blobs).unwrap(), vec![CODEC_RLE]);
        round_trip(
            &blobs,
            &[
                vec![ColumnValue::Bytes(b"a".to_vec())],
                vec![ColumnValue::Bytes(b"b".to_vec())],
            ],
        );

        let bits = vec![ColumnDef {
            name: "f".to_string(),
            ty: ColumnType::Float64,
        }];
        let blob = round_trip(
            &bits,
            &[
                vec![ColumnValue::Float64(1.5)],
                vec![ColumnValue::Float64(1.5)],
                vec![ColumnValue::Float64(1.5)],
            ],
        );
        assert_eq!(column_codecs(&blob, &bits).unwrap(), vec![CODEC_RLE]);

        let flags = vec![ColumnDef {
            name: "p".to_string(),
            ty: ColumnType::Bool,
        }];
        let blob = round_trip(
            &flags,
            &[
                vec![ColumnValue::Bool(true)],
                vec![ColumnValue::Bool(true)],
                vec![ColumnValue::Bool(false)],
            ],
        );
        assert_eq!(column_codecs(&blob, &flags).unwrap(), vec![CODEC_RLE]);

        let nan_rows = vec![
            vec![ColumnValue::Float64(f64::NAN)],
            vec![ColumnValue::Float64(f64::NAN)],
        ];
        let blob = encode_row_group(&bits, &nan_rows).unwrap();
        let scan = scan_groups(
            &bits,
            &[blob],
            &[ColumnFilter {
                column: "f".to_string(),
                predicate: ColumnPredicate::Eq(ColumnValue::Float64(1.0)),
            }],
        )
        .unwrap();
        assert_eq!(scan.row_groups_skipped, 1);
        assert!(scan.rows.is_empty());
    }

    #[test]
    fn zone_map_excludes_eq_lt_gt_between_and_all_null() {
        let zone = ZoneMap {
            null_count: 0,
            min: Some(ColumnValue::Int64(1)),
            max: Some(ColumnValue::Int64(10)),
        };
        assert!(!zone.excludes(&ColumnPredicate::Eq(ColumnValue::Int64(1))));
        assert!(!zone.excludes(&ColumnPredicate::Eq(ColumnValue::Int64(10))));
        assert!(zone.excludes(&ColumnPredicate::Eq(ColumnValue::Int64(0))));
        assert!(zone.excludes(&ColumnPredicate::Eq(ColumnValue::Int64(11))));
        assert!(zone.excludes(&ColumnPredicate::Lt(ColumnValue::Int64(1))));
        assert!(!zone.excludes(&ColumnPredicate::Lt(ColumnValue::Int64(2))));
        assert!(zone.excludes(&ColumnPredicate::Gt(ColumnValue::Int64(10))));
        assert!(!zone.excludes(&ColumnPredicate::Gt(ColumnValue::Int64(9))));
        assert!(zone.excludes(&ColumnPredicate::Between {
            low: ColumnValue::Int64(11),
            high: ColumnValue::Int64(12),
        }));
        assert!(zone.excludes(&ColumnPredicate::Between {
            low: ColumnValue::Int64(-5),
            high: ColumnValue::Int64(0),
        }));
        assert!(!zone.excludes(&ColumnPredicate::Between {
            low: ColumnValue::Int64(10),
            high: ColumnValue::Int64(10),
        }));

        let empty = ZoneMap {
            null_count: 4,
            min: None,
            max: None,
        };
        assert!(empty.excludes(&ColumnPredicate::Eq(ColumnValue::Int64(1))));
        assert!(empty.excludes(&ColumnPredicate::Lt(ColumnValue::Int64(1))));
        assert!(empty.excludes(&ColumnPredicate::Gt(ColumnValue::Int64(1))));
        assert!(empty.excludes(&ColumnPredicate::Between {
            low: ColumnValue::Int64(0),
            high: ColumnValue::Int64(5),
        }));

        let flags = ZoneMap {
            null_count: 0,
            min: Some(ColumnValue::Bool(false)),
            max: Some(ColumnValue::Bool(true)),
        };
        assert!(flags.excludes(&ColumnPredicate::Lt(ColumnValue::Bool(false))));
        assert!(!flags.excludes(&ColumnPredicate::Eq(ColumnValue::Bool(false))));

        let floats = ZoneMap {
            null_count: 0,
            min: Some(ColumnValue::Float64(1.0)),
            max: Some(ColumnValue::Float64(2.0)),
        };
        assert!(!floats.excludes(&ColumnPredicate::Le(ColumnValue::Float64(1.0))));
        assert!(floats.excludes(&ColumnPredicate::Le(ColumnValue::Float64(0.5))));
        assert!(!floats.excludes(&ColumnPredicate::Ge(ColumnValue::Float64(2.0))));
        assert!(floats.excludes(&ColumnPredicate::Ge(ColumnValue::Float64(3.0))));
        assert!(floats.excludes(&ColumnPredicate::Eq(ColumnValue::Float64(f64::NAN))));
        assert!(floats.excludes(&ColumnPredicate::Lt(ColumnValue::Float64(f64::NAN))));
        assert!(floats.excludes(&ColumnPredicate::Le(ColumnValue::Float64(f64::NAN))));
        assert!(floats.excludes(&ColumnPredicate::Gt(ColumnValue::Float64(f64::NAN))));
        assert!(floats.excludes(&ColumnPredicate::Ge(ColumnValue::Float64(f64::NAN))));
        assert!(floats.excludes(&ColumnPredicate::Between {
            low: ColumnValue::Float64(2.0),
            high: ColumnValue::Float64(1.0),
        }));
    }

    #[test]
    fn non_finite_f64_json_round_trips_bits_and_rejects_a_mismatch() {
        let payload = f64::from_bits(0x7ff8_0000_0000_0001);
        assert_ne!(payload.to_bits(), f64::NAN.to_bits());
        for value in [payload, f64::INFINITY, f64::NEG_INFINITY, -0.0] {
            let encoded = cell_json(&ColumnValue::Float64(value)).unwrap();
            let text = serde_json::to_string(&encoded).unwrap();
            let parsed: Value = serde_json::from_str(&text).unwrap();
            let ColumnValue::Float64(number) = parse_cell(&parsed).unwrap() else {
                panic!("expected f64");
            };
            assert_eq!(number.to_bits(), value.to_bits(), "{text}");
            if value.is_finite() {
                assert!(parsed.get("bits").is_none(), "{parsed}");
            }
        }
        let ColumnValue::Float64(canonical) = parse_cell(&json!({"t": "f64", "v": "NaN"})).unwrap()
        else {
            panic!("expected f64");
        };
        assert_eq!(canonical.to_bits(), f64::NAN.to_bits());
        let ColumnValue::Float64(inf) = parse_cell(&json!({"t": "f64", "v": "Infinity"})).unwrap()
        else {
            panic!("expected f64");
        };
        assert_eq!(inf.to_bits(), f64::INFINITY.to_bits());
        let err = parse_cell(&json!({
            "t": "f64",
            "v": "Infinity",
            "bits": payload.to_bits(),
        }))
        .unwrap_err();
        assert!(err.to_string().contains("do not match"), "{err}");
        let err = parse_cell(&json!({"t": "f64", "v": "1.5"})).unwrap_err();
        assert!(err.to_string().contains("string"), "{err}");
        let err = parse_cell(&json!({"t": "f64", "v": "NaN", "bits": -1})).unwrap_err();
        assert!(err.to_string().contains("0..=u64::MAX"), "{err}");
        let err = parse_cell(&json!({"t": "f64", "v": "NaN", "bits": 1.5})).unwrap_err();
        assert!(err.to_string().contains("0..=u64::MAX"), "{err}");
        let err = parse_cell(&json!({"t": "f64", "v": 1.5, "bits": 0})).unwrap_err();
        assert!(err.to_string().contains("bits"), "{err}");
    }

    #[test]
    fn a_zero_row_group_and_a_nan_zone_bound_are_corrupt() {
        let columns = int_col();
        let mut empty = Vec::new();
        write_u32(&mut empty, 0);
        write_u64(&mut empty, 0);
        empty.push(0);
        empty.push(CODEC_DELTA);
        write_u32(&mut empty, 0);
        let err = validate_row_group(&columns, &empty).unwrap_err();
        assert!(err.to_string().contains("no rows"), "{err}");

        let good = encode_row_group(&columns, &[vec![ColumnValue::Int64(4)]]).unwrap();
        validate_row_group(&columns, &good).unwrap();
        let mut bad = good;
        assert_eq!(bad[29], CODEC_DELTA);
        bad[29] = 0xff;
        let err = validate_row_group(&columns, &bad).unwrap_err();
        assert!(err.to_string().contains("codec"), "{err}");

        let mut blob = Vec::new();
        write_u32(&mut blob, 1);
        write_u64(&mut blob, 0);
        blob.push(1);
        write_i64(&mut blob, 1);
        write_i64(&mut blob, 1);
        blob.push(CODEC_DELTA);
        let mut body = vec![0u8];
        write_i64(&mut body, 1);
        write_u32(&mut blob, u32::try_from(body.len()).unwrap());
        blob.extend_from_slice(&body);
        write_u64(&mut blob, 0);
        blob.push(1);
        blob.extend_from_slice(&f64::NAN.to_le_bytes());
        blob.extend_from_slice(&f64::NAN.to_le_bytes());
        blob.push(CODEC_PLAIN);
        let mut body = vec![0u8];
        body.extend_from_slice(&1.0f64.to_le_bytes());
        write_u32(&mut blob, u32::try_from(body.len()).unwrap());
        blob.extend_from_slice(&body);
        let columns = vec![
            ColumnDef {
                name: "n".to_string(),
                ty: ColumnType::Int64,
            },
            ColumnDef {
                name: "f".to_string(),
                ty: ColumnType::Float64,
            },
        ];
        let err = scan_groups(
            &columns,
            &[blob],
            &[ColumnFilter {
                column: "n".to_string(),
                predicate: ColumnPredicate::Eq(ColumnValue::Int64(99)),
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains("not ordered"), "{err}");
    }
}

//! Arrow IPC stream bodies → [`Batch`].
//!
//! Arrow arrays are matched to columns by field name and converted by type:
//! any integer width into an integer column (range-checked), `Decimal128` of
//! any scale into a decimal column (rescaled exactly, or rejected if digits
//! would be lost), `Timestamp` of any unit into a timestamp column, and so on.
//! A timestamp column takes a timestamp without a zone; a timestamptz column
//! one with a zone.

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Date64Type, Decimal128Type, Float32Type, Float64Type, Int8Type, Int16Type,
    Int32Type, Int64Type, TimestampMicrosecondType, TimestampMillisecondType,
    TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_ipc::reader::StreamReader;
use arrow_schema::{DataType, TimeUnit};
use tidepool_config::types::ColumnType;
use tidepool_model::ir::Table;

use crate::batch::{
    Batch, Rejected, RowError, Shape, filled_message, new_batch, push_error, rejected_body,
    wanted_columns,
};
use crate::value::{Parsed, parse_decimal};

pub(crate) fn decode(table: &Table, shape: Shape, body: &[u8]) -> Result<Batch, Rejected> {
    let reader = StreamReader::try_new(body, None)
        .map_err(|e| rejected_body(format!("the body is not an Arrow IPC stream: {e}")))?;
    let columns = wanted_columns(table, shape);
    let mut batch = new_batch(&columns);
    let mut rejected = Rejected::default();
    let mut first_row = 0;

    for record in reader {
        let record = record.map_err(|e| rejected_body(format!("reading the Arrow stream: {e}")))?;
        decode_record(
            table,
            shape,
            &columns,
            &record,
            first_row,
            &mut batch,
            &mut rejected,
        );
        first_row += record.num_rows();
        batch.rows += record.num_rows();
    }

    if rejected.errors.is_empty() {
        Ok(batch)
    } else {
        Err(rejected)
    }
}

fn decode_record(
    table: &Table,
    shape: Shape,
    columns: &[(usize, &tidepool_model::ir::Column)],
    record: &RecordBatch,
    first_row: usize,
    batch: &mut Batch,
    rejected: &mut Rejected,
) {
    let schema = record.schema();
    for field in schema.fields() {
        if !columns.iter().any(|(_, c)| c.name == *field.name()) {
            push_error(
                rejected,
                RowError {
                    row: None,
                    column: Some(field.name().clone()),
                    message: format!("`{}` is not a column of `{}`", field.name(), table.name),
                },
            );
        }
    }
    for (slot, (_, column)) in columns.iter().enumerate() {
        let rows = record.num_rows();
        if column.filled.is_some() {
            if record.column_by_name(&column.name).is_some() {
                push_error(
                    rejected,
                    RowError {
                        row: None,
                        column: Some(column.name.clone()),
                        message: filled_message(column),
                    },
                );
            }
            batch.columns[slot].extend(std::iter::repeat_n(None, rows));
            continue;
        }
        let Some(array) = record.column_by_name(&column.name) else {
            if !(column.nullable && shape == Shape::Rows) {
                push_error(
                    rejected,
                    RowError {
                        row: None,
                        column: Some(column.name.clone()),
                        message: "a value is required, and the stream has no such field"
                            .to_string(),
                    },
                );
            }
            batch.columns[slot].extend(std::iter::repeat_n(None, rows));
            continue;
        };
        let values = convert(column.column_type, array.as_ref());
        match values {
            Err(message) => push_error(
                rejected,
                RowError {
                    row: None,
                    column: Some(column.name.clone()),
                    message,
                },
            ),
            Ok(values) => {
                for (i, value) in values.into_iter().enumerate() {
                    let value = match value {
                        Ok(value) => value,
                        Err(message) => {
                            push_error(
                                rejected,
                                RowError {
                                    row: Some(first_row + i),
                                    column: Some(column.name.clone()),
                                    message,
                                },
                            );
                            None
                        }
                    };
                    if value.is_none()
                        && !(column.nullable && shape == Shape::Rows)
                        && array.is_null(i)
                    {
                        push_error(
                            rejected,
                            RowError {
                                row: Some(first_row + i),
                                column: Some(column.name.clone()),
                                message: "a value is required".to_string(),
                            },
                        );
                    }
                    batch.columns[slot].push(value);
                }
            }
        }
    }
}

type Cell = Result<Option<Parsed>, String>;

/// Convert every non-null value with `f`, reading it with `value`.
fn each<T>(
    array: &dyn Array,
    value: impl Fn(usize) -> T,
    f: impl Fn(T) -> Result<Parsed, String>,
) -> Vec<Cell> {
    (0..array.len())
        .map(|i| {
            if array.is_null(i) {
                Ok(None)
            } else {
                f(value(i)).map(Some)
            }
        })
        .collect()
}

/// Every value of `array` as a column of type `target`, or why the array as a
/// whole can't be one.
fn convert(target: ColumnType, array: &dyn Array) -> Result<Vec<Cell>, String> {
    let mismatch = || {
        format!(
            "an Arrow {} can't be stored in a {target} column",
            array.data_type()
        )
    };
    let string = |v: &str| Ok(Parsed::Str(v.to_string()));
    match (target, array.data_type()) {
        (ColumnType::String, DataType::Utf8) => {
            let a = array.as_string::<i32>();
            Ok(each(array, |i| a.value(i), string))
        }
        (ColumnType::String, DataType::LargeUtf8) => {
            let a = array.as_string::<i64>();
            Ok(each(array, |i| a.value(i), string))
        }
        (ColumnType::String, DataType::Utf8View) => {
            let a = array.as_string_view();
            Ok(each(array, |i| a.value(i), string))
        }
        (ColumnType::Bool, DataType::Boolean) => {
            let a = array.as_boolean();
            Ok(each(array, |i| a.value(i), |v| Ok(Parsed::Bool(v))))
        }
        (ColumnType::Int32 | ColumnType::Int64, data_type) if data_type.is_integer() => {
            convert_integer(target, array).ok_or_else(mismatch)
        }
        (ColumnType::Float64, DataType::Float64) => {
            let a = array.as_primitive::<Float64Type>();
            Ok(each(array, |i| a.value(i), |v| Ok(Parsed::F64(v))))
        }
        (ColumnType::Float64, DataType::Float32) => {
            let a = array.as_primitive::<Float32Type>();
            Ok(each(
                array,
                |i| a.value(i),
                |v| Ok(Parsed::F64(f64::from(v))),
            ))
        }
        (ColumnType::Decimal { precision, scale }, DataType::Decimal128(_, from_scale)) => {
            let from_scale = u8::try_from((*from_scale).max(0)).unwrap_or(0);
            let a = array.as_primitive::<Decimal128Type>();
            // through text: exact, and reuses the one place that knows the rules
            Ok(each(
                array,
                |i| a.value(i),
                |v| {
                    let text = crate::value::format_decimal(v, from_scale);
                    parse_decimal(&text, precision, scale).map(|v| decimal(precision, v))
                },
            ))
        }
        (ColumnType::Decimal { precision, scale }, DataType::Utf8) => {
            let a = array.as_string::<i32>();
            Ok(each(
                array,
                |i| a.value(i),
                |v| parse_decimal(v, precision, scale).map(|v| decimal(precision, v)),
            ))
        }
        (ColumnType::Date, DataType::Date32) => {
            let a = array.as_primitive::<Date32Type>();
            Ok(each(
                array,
                |i| a.value(i),
                |v| Ok(Parsed::I64(i64::from(v))),
            ))
        }
        (ColumnType::Date, DataType::Date64) => {
            let a = array.as_primitive::<Date64Type>();
            Ok(each(
                array,
                |i| a.value(i),
                |v| Ok(Parsed::I64(v.div_euclid(86_400_000))),
            ))
        }
        (ColumnType::Timestamp | ColumnType::TimestampTz, DataType::Timestamp(unit, zone)) => {
            convert_timestamp(target, array, *unit, zone.is_some())
        }
        _ => Err(mismatch()),
    }
}

/// Any Arrow integer into an integer column, range-checked. `None` for a
/// non-integer array.
fn convert_integer(target: ColumnType, array: &dyn Array) -> Option<Vec<Cell>> {
    let fit = |v: i128| -> Result<Parsed, String> {
        match target {
            ColumnType::Int32 if i32::try_from(v).is_err() => {
                Err(format!("{v} doesn't fit in int32"))
            }
            _ => i64::try_from(v)
                .map(Parsed::I64)
                .map_err(|_| format!("{v} doesn't fit in int64")),
        }
    };
    macro_rules! ints {
        ($t:ty) => {{
            let a = array.as_primitive::<$t>();
            Some(each(array, |i| i128::from(a.value(i)), fit))
        }};
    }
    match array.data_type() {
        DataType::Int8 => ints!(Int8Type),
        DataType::Int16 => ints!(Int16Type),
        DataType::Int32 => ints!(Int32Type),
        DataType::Int64 => ints!(Int64Type),
        DataType::UInt8 => ints!(UInt8Type),
        DataType::UInt16 => ints!(UInt16Type),
        DataType::UInt32 => ints!(UInt32Type),
        DataType::UInt64 => ints!(UInt64Type),
        _ => None,
    }
}

/// An Arrow timestamp of any unit into microseconds. The zone must match the
/// column: none for `timestamp`, some for `timestamptz`.
fn convert_timestamp(
    target: ColumnType,
    array: &dyn Array,
    unit: TimeUnit,
    zoned: bool,
) -> Result<Vec<Cell>, String> {
    let wants_zone = target == ColumnType::TimestampTz;
    if zoned != wants_zone {
        return Err(if wants_zone {
            "a timestamptz column needs an Arrow timestamp with a time zone".to_string()
        } else {
            "a timestamp column needs an Arrow timestamp without a time zone".to_string()
        });
    }
    let micros = move |v: i64| -> Result<Parsed, String> {
        match unit {
            TimeUnit::Second => v.checked_mul(1_000_000),
            TimeUnit::Millisecond => v.checked_mul(1000),
            TimeUnit::Microsecond => Some(v),
            TimeUnit::Nanosecond => Some(v.div_euclid(1000)),
        }
        .map(Parsed::I64)
        .ok_or_else(|| format!("{v} is out of range"))
    };
    Ok(match unit {
        TimeUnit::Second => {
            let a = array.as_primitive::<TimestampSecondType>();
            each(array, |i| a.value(i), micros)
        }
        TimeUnit::Millisecond => {
            let a = array.as_primitive::<TimestampMillisecondType>();
            each(array, |i| a.value(i), micros)
        }
        TimeUnit::Microsecond => {
            let a = array.as_primitive::<TimestampMicrosecondType>();
            each(array, |i| a.value(i), micros)
        }
        TimeUnit::Nanosecond => {
            let a = array.as_primitive::<TimestampNanosecondType>();
            each(array, |i| a.value(i), micros)
        }
    })
}

fn decimal(precision: u8, value: i128) -> Parsed {
    if precision <= 18 {
        i64::try_from(value).map_or(Parsed::I128(value), Parsed::I64)
    } else {
        Parsed::I128(value)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Decimal128Array, Int64Array, StringArray, TimestampMicrosecondArray};
    use arrow_ipc::writer::StreamWriter;
    use arrow_schema::{Field, Schema};
    use tidepool_config::table::{Encoding, TableMode};
    use tidepool_model::ir::{Column, ColumnId, TableId};

    use super::*;

    fn table() -> Table {
        let column = |id, name: &str, column_type, nullable| Column {
            id: ColumnId(id),
            name: name.to_string(),
            column_type,
            nullable,
            encoding: Encoding::Dict,
            key_domain: None,
            filled: None,
        };
        Table {
            id: TableId(1),
            name: "orders".to_string(),
            mode: TableMode::Append,
            primary_key: vec![],
            event_time: None,
            retention: None,
            columns: vec![
                column(2, "id", ColumnType::String, false),
                column(
                    3,
                    "amount",
                    ColumnType::Decimal {
                        precision: 14,
                        scale: 2,
                    },
                    false,
                ),
                column(4, "quantity", ColumnType::Int32, true),
                column(5, "at", ColumnType::TimestampTz, true),
            ],
        }
    }

    fn stream(record: &RecordBatch) -> Vec<u8> {
        let mut out = Vec::new();
        let Ok(mut writer) = StreamWriter::try_new(&mut out, &record.schema()) else {
            panic!("writer");
        };
        assert!(
            writer.write(record).and_then(|()| writer.finish()).is_ok(),
            "writing the stream"
        );
        drop(writer);
        out
    }

    fn record(fields: Vec<(Field, Arc<dyn Array>)>) -> RecordBatch {
        let (fields, arrays): (Vec<Field>, Vec<Arc<dyn Array>>) = fields.into_iter().unzip();
        match RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays) {
            Ok(record) => record,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn arrow_columns_convert_by_type() {
        let amount = match Decimal128Array::from(vec![1250_i128, 7]).with_precision_and_scale(10, 2)
        {
            Ok(a) => a,
            Err(e) => panic!("{e}"),
        };
        let record = record(vec![
            (
                Field::new("id", DataType::Utf8, false),
                Arc::new(StringArray::from(vec!["a", "b"])),
            ),
            (
                Field::new("amount", DataType::Decimal128(10, 2), false),
                Arc::new(amount),
            ),
            (
                Field::new("quantity", DataType::Int64, true),
                Arc::new(Int64Array::from(vec![Some(3), None])),
            ),
            (
                Field::new(
                    "at",
                    DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                    true,
                ),
                Arc::new(TimestampMicrosecondArray::from(vec![5, 6]).with_timezone("UTC")),
            ),
        ]);
        let batch = match decode(&table(), Shape::Rows, &stream(&record)) {
            Ok(batch) => batch,
            Err(rejected) => panic!("{rejected:?}"),
        };
        assert_eq!(batch.rows, 2);
        assert_eq!(
            batch.columns[1],
            [Some(Parsed::I64(1250)), Some(Parsed::I64(7))]
        );
        assert_eq!(batch.columns[2], [Some(Parsed::I64(3)), None]);
        assert_eq!(
            batch.columns[3],
            [Some(Parsed::I64(5)), Some(Parsed::I64(6))]
        );
    }

    #[test]
    fn a_filled_column_in_the_stream_is_refused() {
        let mut table = table();
        if let Some(at) = table.columns.iter_mut().find(|c| c.name == "at") {
            at.filled = Some(tidepool_config::table::Filled::Received);
        }
        let ids = || {
            (
                Field::new("id", DataType::Utf8, false),
                Arc::new(StringArray::from(vec!["a"])) as Arc<dyn Array>,
            )
        };
        let amount = || {
            let amount = match Decimal128Array::from(vec![100_i128]).with_precision_and_scale(10, 2)
            {
                Ok(a) => a,
                Err(e) => panic!("{e}"),
            };
            (
                Field::new("amount", DataType::Decimal128(10, 2), false),
                Arc::new(amount) as Arc<dyn Array>,
            )
        };
        // without it: decoded, the column left for the server
        let batch = match decode(&table, Shape::Rows, &stream(&record(vec![ids(), amount()]))) {
            Ok(batch) => batch,
            Err(rejected) => panic!("{rejected:?}"),
        };
        assert_eq!(batch.columns[3], [None]);
        // with it: refused
        let at = (
            Field::new(
                "at",
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
                true,
            ),
            Arc::new(TimestampMicrosecondArray::from(vec![5]).with_timezone("UTC"))
                as Arc<dyn Array>,
        );
        let Err(rejected) = decode(
            &table,
            Shape::Rows,
            &stream(&record(vec![ids(), amount(), at])),
        ) else {
            panic!("a filled column must not be accepted from the producer");
        };
        assert_eq!(
            rejected.summary(),
            "`at` is filled by the server; leave it out"
        );
    }

    #[test]
    fn out_of_range_values_and_wrong_types_are_rejected() {
        let record = record(vec![
            (
                Field::new("id", DataType::Int64, false),
                Arc::new(Int64Array::from(vec![1])),
            ),
            (
                Field::new("amount", DataType::Utf8, false),
                Arc::new(StringArray::from(vec!["1.234"])),
            ),
            (
                Field::new("quantity", DataType::Int64, true),
                Arc::new(Int64Array::from(vec![5_000_000_000])),
            ),
        ]);
        let Err(rejected) = decode(&table(), Shape::Rows, &stream(&record)) else {
            panic!("should be rejected");
        };
        let messages: Vec<&str> = rejected.errors.iter().map(|e| e.message.as_str()).collect();
        assert!(
            messages[0].contains("can't be stored in a string column"),
            "{messages:?}"
        );
        assert!(messages[1].contains("more than 2 decimals"), "{messages:?}");
        assert!(messages[2].contains("doesn't fit in int32"), "{messages:?}");
    }

    #[test]
    fn garbage_is_not_a_stream() {
        let Err(rejected) = decode(&table(), Shape::Rows, b"nope") else {
            panic!("should be rejected");
        };
        assert!(rejected.summary().contains("not an Arrow IPC stream"));
    }
}

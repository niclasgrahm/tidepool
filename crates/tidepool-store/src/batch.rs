//! A batch of incoming rows, validated against a table but not yet encoded,
//! and the decoders that produce one from JSON, NDJSON and Arrow IPC.
//!
//! Decoding is strict, the same rule Kayak's database outputs follow: values
//! are checked, never coerced. A row with an unknown column, a missing
//! required column or a value of the wrong type rejects the whole batch, with
//! every problem (up to [`MAX_ERRORS`]) listed by row and column.
//!
//! Columns the server fills (`filled`, plan §9.2) must not be sent: a
//! decoder leaves them NULL and [`fill`] gives them their values.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::value::RawValue;
use tidepool_config::types::ColumnType;
use tidepool_model::ir::{ColumnId, Table};

use crate::value::{Parsed, parse_json};

/// Problems reported per rejected batch; the rest are counted, not listed.
pub const MAX_ERRORS: usize = 100;

/// Rows ready to apply: one vector per table column, in the table's column
/// order. `None` is NULL.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Batch {
    pub rows: usize,
    pub columns: Vec<Vec<Option<Parsed>>>,
    /// The column each vector is for, with its type when decoded. A batch
    /// decoded before a model change is fitted to the new columns by id
    /// when it is applied. Empty: the columns are the table's as it is.
    pub shape: Vec<(ColumnId, ColumnType)>,
}

impl Batch {
    fn new(columns: &[(usize, &tidepool_model::ir::Column)]) -> Self {
        Self {
            rows: 0,
            columns: vec![Vec::new(); columns.len()],
            shape: columns.iter().map(|(_, c)| (c.id, c.column_type)).collect(),
        }
    }
}

/// What is wrong with one row (or the body as a whole, when `row` is `None`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RowError {
    pub row: Option<usize>,
    pub column: Option<String>,
    pub message: String,
}

/// Why a batch was rejected.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub errors: Vec<RowError>,
    /// Problems beyond the listed ones.
    pub more: usize,
}

impl Rejected {
    fn body(message: impl Into<String>) -> Self {
        Self {
            errors: vec![RowError {
                row: None,
                column: None,
                message: message.into(),
            }],
            more: 0,
        }
    }

    fn push(&mut self, error: RowError) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(error);
        } else {
            self.more += 1;
        }
    }

    /// One line summing up the problems, for the error field of a response.
    #[must_use]
    pub fn summary(&self) -> String {
        let total = self.errors.len() + self.more;
        match self.errors.first() {
            Some(first) if total == 1 => first.message.clone(),
            _ => format!("{total} problems in the batch; nothing was written"),
        }
    }
}

/// Which columns a decoder must supply: all of them for writes, only the key
/// for deletes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Rows,
    Keys,
}

/// Decode a JSON array of row objects.
pub fn json_array(table: &Table, shape: Shape, body: &[u8]) -> Result<Batch, Rejected> {
    let rows: Vec<&RawValue> = serde_json::from_slice(body)
        .map_err(|e| Rejected::body(format!("the body is not a JSON array of rows: {e}")))?;
    decode_rows(table, shape, rows.into_iter().map(|r| Some(r.get())))
}

/// Decode newline-delimited JSON: one row object per line; blank lines are
/// skipped.
pub fn ndjson(table: &Table, shape: Shape, body: &[u8]) -> Result<Batch, Rejected> {
    let text = std::str::from_utf8(body).map_err(|_| Rejected::body("the body is not UTF-8"))?;
    decode_rows(
        table,
        shape,
        text.lines()
            .map(|line| (!line.trim().is_empty()).then_some(line)),
    )
}

/// The columns a shape decodes, as (index in the table, column).
fn wanted(table: &Table, shape: Shape) -> Vec<(usize, &tidepool_model::ir::Column)> {
    table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| shape == Shape::Rows || table.primary_key.contains(&c.id))
        .collect()
}

fn decode_rows<'a>(
    table: &Table,
    shape: Shape,
    rows: impl Iterator<Item = Option<&'a str>>,
) -> Result<Batch, Rejected> {
    let columns = wanted(table, shape);
    let mut batch = Batch::new(&columns);
    let mut rejected = Rejected::default();
    let mut row = 0;
    for text in rows {
        let Some(text) = text else { continue };
        let error = |column: Option<&str>, message: String| RowError {
            row: Some(row),
            column: column.map(String::from),
            message,
        };
        let fields: HashMap<&str, &RawValue> = match serde_json::from_str(text) {
            Ok(fields) => fields,
            Err(e) => {
                rejected.push(error(
                    None,
                    format!("not a JSON object of column values: {e}"),
                ));
                row += 1;
                continue;
            }
        };
        for name in fields.keys() {
            if !columns.iter().any(|(_, c)| c.name == *name) {
                let message = if shape == Shape::Keys {
                    format!("`{name}` is not a key column of `{}`", table.name)
                } else {
                    format!("`{name}` is not a column of `{}`", table.name)
                };
                rejected.push(error(Some(name), message));
            }
        }
        for (slot, (_, column)) in columns.iter().enumerate() {
            if column.filled.is_some() {
                if fields.contains_key(column.name.as_str()) {
                    rejected.push(error(Some(&column.name), filled_message(column)));
                }
                batch.columns[slot].push(None);
                continue;
            }
            let value = match fields.get(column.name.as_str()).map(|raw| raw.get().trim()) {
                None | Some("null") if column.nullable && shape == Shape::Rows => None,
                None | Some("null") => {
                    rejected.push(error(Some(&column.name), "a value is required".to_string()));
                    None
                }
                Some(raw) => match parse_json(column.column_type, raw) {
                    Ok(parsed) => Some(parsed),
                    Err(message) => {
                        rejected.push(error(Some(&column.name), message));
                        None
                    }
                },
            };
            batch.columns[slot].push(value);
        }
        batch.rows += 1;
        row += 1;
    }
    if rejected.errors.is_empty() {
        Ok(batch)
    } else {
        Err(rejected)
    }
}

/// Why a filled column can't be sent.
pub(crate) fn filled_message(column: &tidepool_model::ir::Column) -> String {
    format!("`{}` is filled by the server; leave it out", column.name)
}

/// Give the columns the server fills their values: the credential's name,
/// or the time the batch was received (microseconds since the Unix epoch).
/// Decoders leave them NULL.
pub fn fill(table: &Table, batch: &mut Batch, credential: &str, received_micros: i64) {
    for (slot, values) in batch.columns.iter_mut().enumerate() {
        let id = match batch.shape.get(slot) {
            Some((id, _)) => *id,
            None => match table.columns.get(slot) {
                Some(column) => column.id,
                None => continue,
            },
        };
        let value = match table.column(id).and_then(|c| c.filled) {
            Some(tidepool_config::table::Filled::Credential) => Parsed::Str(credential.to_string()),
            Some(tidepool_config::table::Filled::Received) => Parsed::I64(received_micros),
            None => continue,
        };
        for slot in values.iter_mut() {
            *slot = Some(value.clone());
        }
    }
}

/// Decode an Arrow IPC stream.
pub fn arrow(table: &Table, shape: Shape, body: &[u8]) -> Result<Batch, Rejected> {
    crate::arrow::decode(table, shape, body)
}

/// A rejection with one problem, for the body (or the request) as a whole.
pub fn rejected(message: impl Into<String>) -> Rejected {
    Rejected::body(message)
}

pub(crate) fn rejected_body(message: impl Into<String>) -> Rejected {
    Rejected::body(message)
}

pub(crate) fn new_batch(columns: &[(usize, &tidepool_model::ir::Column)]) -> Batch {
    Batch::new(columns)
}

pub(crate) fn wanted_columns(
    table: &Table,
    shape: Shape,
) -> Vec<(usize, &tidepool_model::ir::Column)> {
    wanted(table, shape)
}

pub(crate) fn push_error(rejected: &mut Rejected, error: RowError) {
    rejected.push(error);
}

#[cfg(test)]
mod tests {
    use tidepool_config::table::{Encoding, TableMode};
    use tidepool_config::types::ColumnType;
    use tidepool_model::ir::{Column, ColumnId, TableId};

    use super::*;

    fn orders() -> Table {
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
            mode: TableMode::Upsert,
            primary_key: vec![ColumnId(2)],
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
                column(4, "note", ColumnType::String, true),
            ],
        }
    }

    /// `orders` with a column filled with the credential, and one with the
    /// time received.
    fn stamped() -> Table {
        let mut table = orders();
        for (id, name, column_type, filled) in [
            (
                5,
                "written_by",
                ColumnType::String,
                tidepool_config::table::Filled::Credential,
            ),
            (
                6,
                "received_at",
                ColumnType::TimestampTz,
                tidepool_config::table::Filled::Received,
            ),
        ] {
            table.columns.push(Column {
                id: ColumnId(id),
                name: name.to_string(),
                column_type,
                nullable: false,
                encoding: Encoding::Plain,
                key_domain: None,
                filled: Some(filled),
            });
        }
        table
    }

    #[test]
    fn filled_columns_are_left_out_and_filled_by_the_server() {
        let table = stamped();
        let body = br#"[{"id": "a", "amount": "1.50"}, {"id": "b", "amount": "2"}]"#;
        let mut batch = json_array(&table, Shape::Rows, body).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(batch.columns[3], vec![None, None]);
        fill(&table, &mut batch, "sensors", 1_700_000_000_000_000);
        assert_eq!(
            batch.columns[3],
            vec![Some(Parsed::Str("sensors".into())); 2]
        );
        assert_eq!(
            batch.columns[4],
            vec![Some(Parsed::I64(1_700_000_000_000_000)); 2]
        );
        // deleting by key never touches them
        let keys = json_array(&table, Shape::Keys, br#"[{"id": "a"}]"#)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(keys.columns.len(), 1);
    }

    #[test]
    fn a_filled_column_sent_is_refused() {
        let body = br#"[{"id": "a", "amount": "1", "written_by": "admin"}]"#;
        let Err(rejected) = json_array(&stamped(), Shape::Rows, body) else {
            panic!("a filled column must not be accepted from the producer");
        };
        assert_eq!(
            rejected.summary(),
            "`written_by` is filled by the server; leave it out"
        );
    }

    #[test]
    fn a_json_array_decodes_into_columns() {
        let body = br#"[{"id": "a", "amount": 12.5}, {"id": "b", "amount": "3", "note": "hi"}]"#;
        let batch = match json_array(&orders(), Shape::Rows, body) {
            Ok(batch) => batch,
            Err(rejected) => panic!("{rejected:?}"),
        };
        assert_eq!(batch.rows, 2);
        assert_eq!(
            batch.columns[1],
            [Some(Parsed::I64(1250)), Some(Parsed::I64(300))]
        );
        assert_eq!(batch.columns[2], [None, Some(Parsed::Str("hi".into()))]);
    }

    #[test]
    fn ndjson_skips_blank_lines() {
        let body = b"{\"id\": \"a\", \"amount\": 1}\n\n{\"id\": \"b\", \"amount\": 2}\n";
        let batch = ndjson(&orders(), Shape::Rows, body).map(|b| b.rows);
        assert_eq!(batch, Ok(2));
    }

    #[test]
    fn every_problem_is_reported_by_row_and_column() {
        let body = br#"[{"id": "a", "amount": 1.234}, {"amount": 1, "colour": "red"}, 7]"#;
        let Err(rejected) = json_array(&orders(), Shape::Rows, body) else {
            panic!("should be rejected");
        };
        let found: Vec<(Option<usize>, Option<&str>)> = rejected
            .errors
            .iter()
            .map(|e| (e.row, e.column.as_deref()))
            .collect();
        assert_eq!(
            found,
            [
                (Some(0), Some("amount")),
                (Some(1), Some("colour")),
                (Some(1), Some("id")),
                (Some(2), None)
            ]
        );
        assert_eq!(
            rejected.summary(),
            "4 problems in the batch; nothing was written"
        );
    }

    #[test]
    fn keys_shape_wants_only_the_key() {
        let batch =
            json_array(&orders(), Shape::Keys, br#"[{"id": "a"}]"#).map(|b| b.columns.len());
        assert_eq!(batch, Ok(1));
        assert!(json_array(&orders(), Shape::Keys, br#"[{"id": "a", "amount": 1}]"#).is_err());
    }

    #[test]
    fn a_body_that_is_not_json_is_one_error() {
        let Err(rejected) = json_array(&orders(), Shape::Rows, b"{not json") else {
            panic!("should be rejected");
        };
        assert_eq!(rejected.errors.len(), 1);
        assert!(
            rejected
                .summary()
                .starts_with("the body is not a JSON array")
        );
    }
}

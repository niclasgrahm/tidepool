//! A pivot table's grid, without a DOM: which rows and columns there are,
//! what each cell holds, where the nested headers span, and the totals.
//!
//! A pivot's result is grouped by its dimensions down the side, then the
//! ones across, then one column per metric. Rows keep the order they come
//! in (the visual's sort); columns are ordered by their values, as a
//! heatmap's axes are: numbers as numbers, the rest as text. Past
//! [`COLUMNS`] columns, the rest are counted, not drawn.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::Value;
use tidepool_protocol::live::PivotTotals;

use crate::board::{display, number};

/// The most columns (values across) a pivot draws.
pub const COLUMNS: usize = 50;

/// One pivot's grid.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pivot {
    /// The values down the side, one key per row.
    pub rows: Vec<Vec<Value>>,
    /// The values across, one key per column (one empty key without
    /// `across`).
    pub columns: Vec<Vec<Value>>,
    /// Columns there are past [`COLUMNS`], not drawn.
    pub hidden: usize,
    /// `cells[row][column]`: the metrics' values, or `None` where the
    /// combination has no rows.
    pub cells: Vec<Vec<Option<Vec<Value>>>>,
    /// Each row's total, when there are totals across.
    pub row_totals: Option<Vec<Option<Vec<Value>>>>,
    /// Each column's total, when there are totals across.
    pub column_totals: Option<Vec<Option<Vec<Value>>>>,
    /// The whole, when there are totals.
    pub all: Option<Vec<Value>>,
}

/// A key's lookup form: values as text, which is how they're shown.
fn key(values: &[Value]) -> Vec<String> {
    values.iter().map(display).collect()
}

/// Rows of a totals result by their key (the first `width` columns).
fn by_key(rows: &[Vec<Value>], width: usize) -> HashMap<Vec<String>, Vec<Value>> {
    rows.iter()
        .filter(|r| r.len() >= width)
        .map(|r| (key(&r[..width]), r[width..].to_vec()))
        .collect()
}

/// The order of two column keys: level by level, by number where every
/// value at that level is one, by text otherwise.
fn compare(a: &[Value], b: &[Value], numeric: &[bool]) -> Ordering {
    for (level, numeric) in numeric.iter().enumerate() {
        let (x, y) = (a.get(level), b.get(level));
        let order = match (x, y) {
            (Some(x), Some(y)) if *numeric => number(x)
                .unwrap_or(0.0)
                .total_cmp(&number(y).unwrap_or(0.0)),
            (Some(x), Some(y)) => display(x).cmp(&display(y)),
            _ => Ordering::Equal,
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

impl Pivot {
    /// The grid of `result` (rows grouped by `down` dimensions, then
    /// `across`, then the metrics).
    #[must_use]
    pub fn new(
        result: &[Vec<Value>],
        down: usize,
        across: usize,
        totals: Option<&PivotTotals>,
    ) -> Self {
        let width = down + across;
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut row_at: HashMap<Vec<String>, usize> = HashMap::new();
        let mut columns: Vec<Vec<Value>> = Vec::new();
        let mut column_at: HashMap<Vec<String>, usize> = HashMap::new();
        for row in result.iter().filter(|r| r.len() >= width) {
            let (side, top) = (&row[..down], &row[down..width]);
            row_at.entry(key(side)).or_insert_with(|| {
                rows.push(side.to_vec());
                rows.len() - 1
            });
            column_at.entry(key(top)).or_insert_with(|| {
                columns.push(top.to_vec());
                columns.len() - 1
            });
        }
        if columns.is_empty() && across == 0 {
            columns.push(Vec::new());
        }
        let numeric: Vec<bool> = (0..across)
            .map(|level| {
                columns
                    .iter()
                    .all(|c| c.get(level).is_some_and(|v| number(v).is_some()))
            })
            .collect();
        columns.sort_by(|a, b| compare(a, b, &numeric));
        let hidden = columns.len().saturating_sub(COLUMNS);
        columns.truncate(COLUMNS);
        let column_at: HashMap<Vec<String>, usize> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (key(c), i))
            .collect();
        let mut cells = vec![vec![None; columns.len()]; rows.len()];
        for row in result.iter().filter(|r| r.len() >= width) {
            let (Some(&r), Some(&c)) = (
                row_at.get(&key(&row[..down])),
                column_at.get(&key(&row[down..width])),
            ) else {
                continue;
            };
            cells[r][c] = Some(row[width..].to_vec());
        }
        let row_totals = totals.and_then(|t| t.rows.as_ref()).map(|t| {
            let found = by_key(&t.rows, down);
            rows.iter().map(|r| found.get(&key(r)).cloned()).collect()
        });
        let column_totals = totals.and_then(|t| t.columns.as_ref()).map(|t| {
            let found = by_key(&t.rows, across);
            columns
                .iter()
                .map(|c| found.get(&key(c)).cloned())
                .collect()
        });
        let all = totals.and_then(|t| t.all.rows.first().cloned());
        Self {
            rows,
            columns,
            hidden,
            cells,
            row_totals,
            column_totals,
            all,
        }
    }

    /// Where row headers start and how many rows each spans, per level:
    /// `spans[row][level]` is `Some(n)` on the first row of a run of `n`
    /// rows with the same values up to that level, `None` below it.
    #[must_use]
    pub fn row_spans(&self) -> Vec<Vec<Option<usize>>> {
        spans(&self.rows)
    }

    /// The same for column headers: `spans[column][level]`.
    #[must_use]
    pub fn column_spans(&self) -> Vec<Vec<Option<usize>>> {
        spans(&self.columns)
    }
}

/// Runs of keys that agree up to each level.
fn spans(keys: &[Vec<Value>]) -> Vec<Vec<Option<usize>>> {
    let levels = keys.first().map_or(0, Vec::len);
    let mut out = vec![vec![None; levels]; keys.len()];
    for level in 0..levels {
        let mut start = 0;
        while start < keys.len() {
            let prefix = key(&keys[start][..=level]);
            let mut end = start + 1;
            while end < keys.len() && key(&keys[end][..=level]) == prefix {
                end += 1;
            }
            out[start][level] = Some(end - start);
            start = end;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tidepool_protocol::QueryResult;

    use super::*;

    fn result(rows: &Value) -> QueryResult {
        match serde_json::from_value(json!({"columns": [], "rows": rows, "epoch": 0})) {
            Ok(result) => result,
            Err(err) => panic!("a query result: {err}"),
        }
    }

    fn rows(value: Value) -> Vec<Vec<Value>> {
        match serde_json::from_value(value) {
            Ok(rows) => rows,
            Err(err) => panic!("rows: {err}"),
        }
    }

    #[test]
    fn cells_go_where_their_row_and_column_meet() {
        let pivot = Pivot::new(
            &rows(json!([
                ["south", "camping", "2.50", 1],
                ["north", "boats", "10.00", 1],
                ["south", "boats", "5.00", 1],
            ])),
            1,
            1,
            None,
        );
        // rows as they came; columns by their values
        assert_eq!(pivot.rows, rows(json!([["south"], ["north"]])));
        assert_eq!(pivot.columns, rows(json!([["boats"], ["camping"]])));
        assert_eq!(
            pivot.cells,
            vec![
                vec![
                    Some(vec![json!("5.00"), json!(1)]),
                    Some(vec![json!("2.50"), json!(1)])
                ],
                vec![Some(vec![json!("10.00"), json!(1)]), None],
            ]
        );
        assert_eq!(pivot.row_totals, None);
        assert_eq!(pivot.all, None);
    }

    #[test]
    fn totals_are_found_by_their_keys() {
        let totals = PivotTotals {
            rows: Some(result(&json!([
                ["south", "7.50", 1],
                ["north", "10.00", 1]
            ]))),
            columns: Some(result(&json!([
                ["camping", "2.50", 1],
                ["boats", "15.00", 2]
            ]))),
            all: result(&json!([["17.50", 2]])),
        };
        let pivot = Pivot::new(
            &rows(json!([
                ["north", "boats", "10.00", 1],
                ["south", "boats", "5.00", 1],
                ["south", "camping", "2.50", 1],
            ])),
            1,
            1,
            Some(&totals),
        );
        assert_eq!(
            pivot.row_totals,
            Some(vec![
                Some(vec![json!("10.00"), json!(1)]),
                Some(vec![json!("7.50"), json!(1)]),
            ])
        );
        // in column order, whatever order they came in
        assert_eq!(
            pivot.column_totals,
            Some(vec![
                Some(vec![json!("15.00"), json!(2)]),
                Some(vec![json!("2.50"), json!(1)]),
            ])
        );
        assert_eq!(pivot.all, Some(vec![json!("17.50"), json!(2)]));
    }

    #[test]
    fn without_across_each_row_has_one_column_and_the_whole() {
        let totals = PivotTotals {
            rows: None,
            columns: None,
            all: result(&json!([["15.00"]])),
        };
        let pivot = Pivot::new(
            &rows(json!([["north", "10.00"], ["south", "5.00"]])),
            1,
            0,
            Some(&totals),
        );
        assert_eq!(pivot.columns, vec![Vec::<Value>::new()]);
        assert_eq!(pivot.cells[1], vec![Some(vec![json!("5.00")])]);
        assert_eq!(pivot.row_totals, None);
        assert_eq!(pivot.all, Some(vec![json!("15.00")]));
    }

    #[test]
    fn numbers_across_sort_as_numbers_and_too_many_are_counted() {
        let years: Vec<Vec<Value>> = (0..60)
            .rev()
            .map(|i| vec![json!("x"), json!(1970 + i), json!(i)])
            .collect();
        let pivot = Pivot::new(&years, 1, 1, None);
        assert_eq!(pivot.columns.len(), COLUMNS);
        assert_eq!(pivot.hidden, 10);
        assert_eq!(pivot.columns[0], vec![json!(1970)]);
        assert_eq!(pivot.cells[0][0], Some(vec![json!(0)]));
        // 9 before 10, as numbers
        let pivot = Pivot::new(&rows(json!([["x", 10, 1], ["x", 9, 2]])), 1, 1, None);
        assert_eq!(pivot.columns, rows(json!([[9], [10]])));
    }

    #[test]
    fn nested_headers_span_their_runs() {
        let pivot = Pivot::new(
            &rows(json!([
                ["north", "paid", "a", 1],
                ["north", "open", "a", 2],
                ["south", "paid", "b", 3],
                ["north", "late", "b", 4],
            ])),
            2,
            1,
            None,
        );
        // north's three rows aren't together: the run is cut where they part
        assert_eq!(
            pivot.row_spans(),
            vec![
                vec![Some(2), Some(1)],
                vec![None, Some(1)],
                vec![Some(1), Some(1)],
                vec![Some(1), Some(1)],
            ]
        );
        let across = Pivot::new(
            &rows(json!([
                ["x", 2026, "q1", 1],
                ["x", 2026, "q2", 2],
                ["x", 2025, "q4", 3],
            ])),
            1,
            2,
            None,
        );
        assert_eq!(
            across.columns,
            rows(json!([[2025, "q4"], [2026, "q1"], [2026, "q2"]]))
        );
        assert_eq!(
            across.column_spans(),
            vec![
                vec![Some(1), Some(1)],
                vec![Some(2), Some(1)],
                vec![None, Some(1)],
            ]
        );
    }
}

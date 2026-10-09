//! The reference: the same data in DuckDB, the same query as plain SQL.
//!
//! The SQL is the textbook definition of what Tidepool computes: for each
//! anchor table (and set of selections it sees), `LEFT JOIN` the tables it
//! reaches many → one, filter, aggregate; then outer-join those results on
//! the dimension values and compute derived metrics on top. Nothing here
//! uses Tidepool's planner or evaluator.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use duckdb::Connection;
use duckdb::types::Value as Duck;
use serde_json::Value as Json;

use crate::generate::{Cond, Data, Gen, Ignore, MetricKind, Query, Relationship, Schema, Write};

/// A value both sides can be compared as.
#[derive(Clone, Debug, PartialEq)]
pub enum Canon {
    Null,
    Bool(bool),
    Num(f64),
    Text(String),
    Date(i32),
    Micros(i64),
}

impl Canon {
    /// Equal, allowing for float summation order.
    #[must_use]
    pub fn close(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Num(a), Self::Num(b)) => (a - b).abs() <= 1e-6 * a.abs().max(b.abs()).max(1.0),
            (a, b) => a == b,
        }
    }

    #[must_use]
    pub fn order(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let rank = |c: &Self| match c {
            Self::Bool(_) => 0,
            Self::Num(_) => 1,
            Self::Text(_) => 2,
            Self::Date(_) => 3,
            Self::Micros(_) => 4,
            Self::Null => 5,
        };
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            (Self::Num(a), Self::Num(b)) => a.total_cmp(b),
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            (Self::Date(a), Self::Date(b)) => a.cmp(b),
            (Self::Micros(a), Self::Micros(b)) => a.cmp(b),
            (a, b) => rank(a).cmp(&rank(b)).then(Ordering::Equal),
        }
    }
}

/// A DuckDB value as a canonical value.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn canon(value: Duck) -> Canon {
    match value {
        Duck::Null => Canon::Null,
        Duck::Boolean(b) => Canon::Bool(b),
        Duck::TinyInt(v) => Canon::Num(f64::from(v)),
        Duck::SmallInt(v) => Canon::Num(f64::from(v)),
        Duck::Int(v) => Canon::Num(f64::from(v)),
        Duck::BigInt(v) => Canon::Num(v as f64),
        Duck::HugeInt(v) => Canon::Num(v as f64),
        Duck::UTinyInt(v) => Canon::Num(f64::from(v)),
        Duck::USmallInt(v) => Canon::Num(f64::from(v)),
        Duck::UInt(v) => Canon::Num(f64::from(v)),
        Duck::UBigInt(v) => Canon::Num(v as f64),
        Duck::Float(v) => Canon::Num(f64::from(v)),
        Duck::Double(v) => Canon::Num(v),
        Duck::Decimal(d) => Canon::Num(d.to_string().parse().unwrap_or(f64::NAN)),
        Duck::Text(s) => Canon::Text(s),
        Duck::Date32(d) => Canon::Date(d),
        Duck::Timestamp(unit, v) => Canon::Micros(match unit {
            duckdb::types::TimeUnit::Second => v * 1_000_000,
            duckdb::types::TimeUnit::Millisecond => v * 1000,
            duckdb::types::TimeUnit::Microsecond => v,
            duckdb::types::TimeUnit::Nanosecond => v / 1000,
        }),
        other => Canon::Text(format!("{other:?}")),
    }
}

/// A JSON data value as a SQL literal of a column.
fn literal(value: &Json, values: &Gen) -> String {
    match (value, values) {
        (Json::Null, _) => "NULL".to_string(),
        (Json::String(s), Gen::Date(..)) => format!("DATE '{s}'"),
        (Json::String(s), Gen::Timestamp(..)) => {
            format!("TIMESTAMP '{}'", s.trim_end_matches('Z').replace('T', " "))
        }
        (Json::String(s), Gen::Decimal(..)) => s.clone(),
        (Json::String(s), _) => format!("'{}'", s.replace('\'', "''")),
        (other, _) => other.to_string(),
    }
}

/// Load the final data into a fresh in-memory DuckDB.
///
/// # Errors
/// DuckDB refuses a statement (a generator bug).
pub fn load(schema: &Schema, data: &Data) -> Result<Connection, String> {
    let db = Connection::open_in_memory().map_err(|e| e.to_string())?;
    for table in &schema.tables {
        let columns: Vec<String> = table
            .columns
            .iter()
            .map(|c| format!("{} {}", c.name, c.sql_ty))
            .collect();
        db.execute_batch(&format!(
            "CREATE TABLE {} ({});",
            table.name,
            columns.join(", ")
        ))
        .map_err(|e| e.to_string())?;
        let rows = data.tables.get(&table.name).cloned().unwrap_or_default();
        for chunk in rows.chunks(500) {
            let values: Vec<String> = chunk
                .iter()
                .map(|row| {
                    let cells: Vec<String> = table
                        .columns
                        .iter()
                        .map(|c| literal(row.get(&c.name).unwrap_or(&Json::Null), &c.values))
                        .collect();
                    format!("({})", cells.join(", "))
                })
                .collect();
            db.execute_batch(&format!(
                "INSERT INTO {} VALUES {};",
                table.name,
                values.join(", ")
            ))
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(db)
}

/// The SQL of a field (a column, or a derived field's expression).
fn field_sql(schema: &Schema, table: &str, field: &str) -> String {
    schema
        .derived
        .iter()
        .find(|d| d.table == table && d.name == field)
        .map_or_else(|| format!("{table}.{field}"), |d| d.sql_expr.clone())
}

fn gen_of<'a>(schema: &'a Schema, table: &str, column: &str) -> Option<&'a Gen> {
    schema
        .table(table)
        .and_then(|t| t.column(column))
        .map(|c| &c.values)
}

/// One selected field's condition, in SQL.
fn condition_sql(schema: &Schema, (table, column, cond): &(String, String, Cond)) -> String {
    let field = format!("{table}.{column}");
    let source = gen_of(schema, table, column).cloned().unwrap_or(Gen::Float);
    match cond {
        Cond::In(values) => {
            let list: Vec<String> = values.iter().map(|v| literal(v, &source)).collect();
            format!("{field} IN ({})", list.join(", "))
        }
        Cond::Range(from, to) => format!(
            "{field} >= {} AND {field} < {}",
            literal(from, &source),
            literal(to, &source)
        ),
        Cond::Relative {
            now,
            unit,
            last,
            ago,
        } => {
            // DuckDB's own calendar: its date_trunc and interval arithmetic
            let now = tidepool_expr::temporal::datetime_from_micros(*now)
                .map(|t| t.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
                .unwrap_or_default();
            let start = format!("date_trunc('{unit}', TIMESTAMP '{now}')");
            let (per, interval) = match *unit {
                "quarter" => (3, "MONTH"),
                "week" => (7, "DAY"),
                other => (1, other),
            };
            let (last, ago) = (i64::from(*last), i64::from(*ago));
            let step = |periods: i64| format!("{start} + INTERVAL ({}) {interval}", periods * per);
            format!(
                "{field} >= {} AND {field} < {}",
                step(-(ago + last - 1)),
                step(1 - ago)
            )
        }
    }
}

/// Tables `from` reaches many → one without crossing `cut`, each with the
/// relationship it's reached by.
fn reached(
    schema: &Schema,
    from: &str,
    cut: Option<&Relationship>,
) -> Vec<(String, Option<Relationship>)> {
    let mut out: Vec<(String, Option<Relationship>)> = vec![(from.to_string(), None)];
    let mut i = 0;
    while i < out.len() {
        let current = out[i].0.clone();
        for r in schema
            .relationships
            .iter()
            .filter(|r| r.from == current && Some(*r) != cut)
        {
            if !out.iter().any(|(t, _)| *t == r.to) {
                out.push((r.to.clone(), Some(r.clone())));
            }
        }
        i += 1;
    }
    out
}

/// `table`'s side of the relationship tree once `cut` is removed.
fn side(schema: &Schema, table: &str, cut: &Relationship) -> BTreeSet<String> {
    let mut seen = BTreeSet::from([table.to_string()]);
    let mut queue = vec![table.to_string()];
    while let Some(t) = queue.pop() {
        for r in schema.relationships.iter().filter(|r| *r != cut) {
            let next = if r.from == t {
                &r.to
            } else if r.to == t {
                &r.from
            } else {
                continue;
            };
            if seen.insert(next.clone()) {
                queue.push(next.clone());
            }
        }
    }
    seen
}

/// `FROM` for `anchor`: it and what it reaches, `LEFT JOIN`ed.
fn joined(schema: &Schema, anchor: &str, cut: Option<&Relationship>) -> String {
    let mut from = anchor.to_string();
    for (_, via) in reached(schema, anchor, cut) {
        if let Some(r) = via {
            let _ = write!(
                from,
                " LEFT JOIN {to} ON {from_t}.{fk} = {to}.key",
                to = r.to,
                from_t = r.from,
                fk = r.fk
            );
        }
    }
    from
}

/// With selections flowing both ways: the conditions a query on `anchor`
/// (on its side of `cut`) gets from selections across one → many
/// relationships. For each table it reaches, each other relationship into
/// that table whose many side has something selected restricts the table's
/// key to the foreign keys of that side's passing rows.
fn upward(
    schema: &Schema,
    anchor: &str,
    cut: Option<&Relationship>,
    visible: &[&(String, String, Cond)],
) -> Vec<String> {
    if !schema.both {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (table, by) in reached(schema, anchor, cut) {
        for edge in schema
            .relationships
            .iter()
            .filter(|r| r.to == table && Some(*r) != by.as_ref() && Some(*r) != cut)
        {
            let on_side = side(schema, &edge.from, edge);
            if !visible.iter().any(|(t, _, _)| on_side.contains(t)) {
                continue;
            }
            let near: Vec<String> = reached(schema, &edge.from, Some(edge))
                .into_iter()
                .map(|(t, _)| t)
                .collect();
            let mut conditions: Vec<String> = visible
                .iter()
                .filter(|(t, _, _)| near.contains(t))
                .map(|s| condition_sql(schema, s))
                .collect();
            let on_side_visible: Vec<&(String, String, Cond)> = visible
                .iter()
                .filter(|(t, _, _)| on_side.contains(t))
                .copied()
                .collect();
            conditions.extend(upward(schema, &edge.from, Some(edge), &on_side_visible));
            conditions.push(format!("{}.{} IS NOT NULL", edge.from, edge.fk));
            out.push(format!(
                "{table}.key IN (SELECT {}.{} FROM {} WHERE {})",
                edge.from,
                edge.fk,
                joined(schema, &edge.from, Some(edge)),
                conditions.join(" AND ")
            ));
        }
    }
    out
}

/// The SQL for `query`, and how many dimension columns it starts with.
#[must_use]
#[allow(clippy::too_many_lines, clippy::items_after_statements)]
pub fn sql(schema: &Schema, query: &Query) -> String {
    // base metrics needed, in first-use order
    fn expand(schema: &Schema, name: &str, out: &mut Vec<String>) {
        match schema.metric(name).map(|m| &m.kind) {
            Some(MetricKind::Base { .. }) if !out.iter().any(|n| n == name) => {
                out.push(name.to_string());
            }
            Some(MetricKind::Derived { inputs, .. }) => {
                for input in inputs {
                    expand(schema, input, out);
                }
            }
            Some(MetricKind::Base { .. }) | None => {}
        }
    }
    let mut bases = Vec::new();
    for m in &query.metrics {
        expand(schema, m, &mut bases);
    }

    let dims: Vec<String> = query
        .dims
        .iter()
        .map(|(table, field, grain)| {
            let sql = field_sql(schema, table, field);
            match grain {
                Some(grain) => format!("CAST(date_trunc('{grain}', {sql}) AS DATE)"),
                None => sql,
            }
        })
        .collect();

    // group base metrics by (anchor, selections they see, time shift)
    type Group = (String, Vec<usize>, Option<String>);
    let mut groups: BTreeMap<Group, Vec<String>> = BTreeMap::new();
    let mut shifts: BTreeMap<String, crate::generate::Shifted> = BTreeMap::new();
    for name in &bases {
        let Some(MetricKind::Base {
            anchor,
            ignore,
            shift,
            ..
        }) = schema.metric(name).map(|m| &m.kind)
        else {
            continue;
        };
        let shift_key = shift.as_ref().map(|s| {
            let key = format!("{}.{} {}", s.table, s.column, s.interval);
            shifts.insert(key.clone(), s.clone());
            key
        });
        let reachable: Vec<String> = schema
            .reachable(anchor)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let seen: Vec<usize> = query
            .selection
            .iter()
            .enumerate()
            .filter(|(_, (table, column, _))| {
                (schema.both || reachable.contains(table))
                    && match ignore {
                        Ignore::None => true,
                        Ignore::All => false,
                        Ignore::Fields(fields) => !fields.contains(&format!("{table}.{column}")),
                    }
            })
            .map(|(i, _)| i)
            .collect();
        groups
            .entry((anchor.clone(), seen, shift_key))
            .or_default()
            .push(name.clone());
    }

    let mut ctes = Vec::new();
    let mut column_of: BTreeMap<String, String> = BTreeMap::new();
    for (q, ((anchor, seen, shift), metrics)) in groups.iter().enumerate() {
        // a time-shifted metric reads its table with the column moved
        let shifted = shift.as_ref().and_then(|k| shifts.get(k));
        let source = |table: &str| match shifted.filter(|s| s.table == table) {
            Some(s) => {
                let moved = format!(
                    "{t}.{c} + INTERVAL {i}",
                    t = s.table,
                    c = s.column,
                    i = s.interval
                );
                let moved = if s.date {
                    format!("CAST({moved} AS DATE)")
                } else {
                    moved
                };
                format!(
                    "(SELECT * REPLACE ({moved} AS {c}) FROM {t}) AS {t}",
                    c = s.column,
                    t = s.table
                )
            }
            None => table.to_string(),
        };
        let mut from = source(anchor);
        for (_, via) in schema.reachable(anchor) {
            if let Some(r) = via {
                let _ = write!(
                    from,
                    " LEFT JOIN {joined} ON {from_t}.{fk} = {to}.key",
                    joined = source(&r.to),
                    to = r.to,
                    from_t = r.from,
                    fk = r.fk
                );
            }
        }
        let mut select: Vec<String> = dims
            .iter()
            .enumerate()
            .map(|(i, d)| format!("{d} AS d{i}"))
            .collect();
        for name in metrics {
            if let Some(MetricKind::Base { sql, .. }) = schema.metric(name).map(|m| &m.kind) {
                select.push(format!("{sql} AS m_{name}"));
                column_of.insert(name.clone(), format!("q{q}.m_{name}"));
            }
        }
        let near: Vec<String> = schema
            .reachable(anchor)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let visible: Vec<&(String, String, Cond)> =
            seen.iter().map(|i| &query.selection[*i]).collect();
        let mut conditions: Vec<String> = visible
            .iter()
            .filter(|(t, _, _)| near.contains(t))
            .map(|s| condition_sql(schema, s))
            .collect();
        conditions.extend(upward(schema, anchor, None, &visible));
        let mut cte = format!("q{q} AS (SELECT {} FROM {from}", select.join(", "));
        if !conditions.is_empty() {
            let _ = write!(cte, " WHERE {}", conditions.join(" AND "));
        }
        if !dims.is_empty() {
            let _ = write!(cte, " GROUP BY ALL");
        }
        cte.push(')');
        ctes.push(cte);
    }

    // a metric's value in the outer query: a column, or a derived expression
    fn value_of(schema: &Schema, name: &str, column_of: &BTreeMap<String, String>) -> String {
        match schema.metric(name).map(|m| &m.kind) {
            Some(MetricKind::Derived { inputs, sql }) => {
                let mut out = sql.clone();
                for input in inputs {
                    out = out.replace(
                        &format!("{{{input}}}"),
                        &format!("({})", value_of(schema, input, column_of)),
                    );
                }
                out
            }
            _ => column_of
                .get(name)
                .cloned()
                .unwrap_or_else(|| "NULL".into()),
        }
    }

    let n = groups.len();
    let outputs: Vec<String> = query
        .metrics
        .iter()
        .enumerate()
        .map(|(i, m)| format!("{} AS out{i}", value_of(schema, m, &column_of)))
        .collect();
    let mut sql = format!("WITH {}", ctes.join(", "));
    if dims.is_empty() {
        let from: Vec<String> = (0..n).map(|q| format!("q{q}")).collect();
        let _ = write!(
            sql,
            " SELECT {} FROM {}",
            outputs.join(", "),
            from.join(" CROSS JOIN ")
        );
    } else {
        let dim_names: Vec<String> = (0..dims.len()).map(|i| format!("d{i}")).collect();
        let keys: Vec<String> = (0..n)
            .map(|q| format!("SELECT {} FROM q{q}", dim_names.join(", ")))
            .collect();
        let _ = write!(sql, ", keys AS ({})", keys.join(" UNION "));
        let mut select: Vec<String> = dim_names.iter().map(|d| format!("keys.{d}")).collect();
        select.extend(outputs);
        let _ = write!(sql, " SELECT {} FROM keys", select.join(", "));
        for q in 0..n {
            let on: Vec<String> = dim_names
                .iter()
                .map(|d| format!("keys.{d} IS NOT DISTINCT FROM q{q}.{d}"))
                .collect();
            let _ = write!(sql, " LEFT JOIN q{q} ON {}", on.join(" AND "));
        }
    }
    sql
}

/// Run `sql` and return its rows, `width` columns each.
///
/// # Errors
/// DuckDB rejects the SQL.
/// One write applied to DuckDB the way the store applies it to Tidepool:
/// appends insert; upserts replace rows by key (within a batch, the last
/// row for a key wins); deletes remove by key.
///
/// # Errors
/// DuckDB refused a statement.
pub fn apply(db: &Connection, schema: &Schema, write: &Write) -> Result<(), String> {
    let insert = |table: &str, rows: &[&Json]| -> Result<(), String> {
        let Some(spec) = schema.table(table) else {
            return Err(format!("no table {table}"));
        };
        for chunk in rows.chunks(500) {
            let values: Vec<String> = chunk
                .iter()
                .map(|row| {
                    let cells: Vec<String> = spec
                        .columns
                        .iter()
                        .map(|c| literal(row.get(&c.name).unwrap_or(&Json::Null), &c.values))
                        .collect();
                    format!("({})", cells.join(", "))
                })
                .collect();
            db.execute_batch(&format!(
                "INSERT INTO {table} VALUES {};",
                values.join(", ")
            ))
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    let delete = |table: &str, keys: &[&Json]| -> Result<(), String> {
        if keys.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = keys
            .iter()
            .map(|k| literal(k, &Gen::Choice(Vec::new())))
            .collect();
        db.execute_batch(&format!(
            "DELETE FROM {table} WHERE key IN ({});",
            list.join(", ")
        ))
        .map_err(|e| e.to_string())
    };
    match write {
        Write::Publish => Ok(()),
        Write::Rows { table, rows } => {
            let upsert = schema.table(table).is_some_and(|t| t.upsert);
            if !upsert {
                return insert(table, &rows.iter().collect::<Vec<_>>());
            }
            // the last row per key
            let mut last: BTreeMap<String, &Json> = BTreeMap::new();
            for row in rows {
                if let Some(key) = row.get("key") {
                    last.insert(key.to_string(), row);
                }
            }
            let keys: Vec<&Json> = last.values().filter_map(|r| r.get("key")).collect();
            delete(table, &keys)?;
            insert(table, &last.into_values().collect::<Vec<_>>())
        }
        Write::Delete { table, keys } => {
            let keys: Vec<&Json> = keys.iter().filter_map(|k| k.get("key")).collect();
            delete(table, &keys)
        }
    }
}

/// An empty database with `schema`'s tables.
///
/// # Errors
/// DuckDB refused a statement.
pub fn empty(schema: &Schema) -> Result<Connection, String> {
    load(schema, &Data::default())
}

/// SQL for a filter list: the rows of `table` per value of `field`, under
/// the selected fields `table` reaches (as for a metric on `table`).
#[must_use]
pub fn values_sql(schema: &Schema, table: &str, field: &str, query: &Query) -> String {
    let reachable: Vec<String> = schema
        .reachable(table)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    let from = joined(schema, table, None);
    let visible: Vec<&(String, String, Cond)> = query.selection.iter().collect();
    let mut conditions: Vec<String> = visible
        .iter()
        .filter(|(t, _, _)| reachable.contains(t))
        .map(|s| condition_sql(schema, s))
        .collect();
    conditions.extend(upward(schema, table, None, &visible));
    let mut sql = format!(
        "SELECT {} AS d0, COUNT(*) AS n FROM {from}",
        field_sql(schema, table, field)
    );
    if !conditions.is_empty() {
        let _ = write!(sql, " WHERE {}", conditions.join(" AND "));
    }
    sql.push_str(" GROUP BY ALL");
    sql
}

pub fn run(db: &Connection, sql: &str, width: usize) -> Result<Vec<Vec<Canon>>, String> {
    let mut statement = db.prepare(sql).map_err(|e| format!("{e}\n{sql}"))?;
    let rows = statement
        .query_map([], |row| {
            (0..width)
                .map(|i| row.get::<_, Duck>(i).map(canon))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| format!("{e}\n{sql}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

//! Random projects: a schema (written out as real config files), data
//! (written to Tidepool the way clients would: batches, upserts, deletes,
//! epochs in between) and queries.
//!
//! Everything comes from one seed, so a failure is reproducible from the
//! seed alone. Every metric and derived field is generated twice: in
//! Tidepool's terms (the Ossie file) and as plain SQL for DuckDB, written
//! independently, so the oracle never trusts Tidepool's own translation.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Map, Value as Json, json};

/// `SplitMix64`: small, fast, good enough, and stable across versions.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % (n.max(1) as u64)).unwrap_or(0)
    }

    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + i64::try_from(self.next_u64() % u64::try_from(hi - lo + 1).unwrap_or(1)).unwrap_or(0)
    }

    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// How a column's values are made.
#[derive(Clone, Debug)]
pub enum Gen {
    /// Keys `{prefix}{i}` for `i` in `0..count`.
    Key(String, usize),
    /// A foreign key into a table's keys; some values point nowhere.
    Ref(String),
    /// A parent in the same table: the key of a row before this one (so
    /// the tree has no cycles), NULL for a root, or sometimes nowhere.
    Up,
    Choice(Vec<&'static str>),
    Int(i64, i64),
    /// Cents, i.e. a decimal with this many places.
    Decimal(i64, i64, u8),
    Float,
    /// Days since 1970-01-01.
    Date(i64, i64),
    /// Seconds since the epoch, UTC.
    Timestamp(i64, i64),
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    /// As written in the table file.
    pub ty: String,
    /// As DuckDB spells it.
    pub sql_ty: String,
    pub nullable: bool,
    pub values: Gen,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub upsert: bool,
    pub columns: Vec<Column>,
}

impl Table {
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relationship {
    pub from: String,
    pub fk: String,
    pub to: String,
}

/// A derived field: the expression in the model (bare column names) and in
/// SQL (qualified).
#[derive(Clone, Debug)]
pub struct DerivedField {
    pub table: String,
    pub name: String,
    pub model_expr: String,
    pub sql_expr: String,
    pub temporal: bool,
}

#[derive(Clone, Debug)]
pub enum MetricKind {
    /// `sql` is an aggregate expression over the anchor's (and reachable
    /// tables') columns.
    Base {
        anchor: String,
        sql: String,
        ignore: Ignore,
        /// A `time_offset`: the metric is evaluated with this column moved
        /// by the interval (`table`, `column`, the interval as DuckDB
        /// writes it, whether the column is a date).
        shift: Option<Shifted>,
    },
    /// `sql` combines other metrics, written `{name}`.
    Derived { inputs: Vec<String>, sql: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shifted {
    pub table: String,
    pub column: String,
    /// The move as a DuckDB interval: `-1w` reads as `INTERVAL 7 DAY`.
    pub interval: String,
    pub date: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ignore {
    None,
    All,
    Fields(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct Metric {
    pub name: String,
    pub model_expr: String,
    /// The TIDEPOOL extension's JSON, if any.
    pub extension: Option<String>,
    pub kind: MetricKind,
}

#[derive(Clone, Debug)]
pub struct Schema {
    pub tables: Vec<Table>,
    pub relationships: Vec<Relationship>,
    pub derived: Vec<DerivedField>,
    pub metrics: Vec<Metric>,
    /// `filter_direction: both`: selections also flow one → many.
    pub both: bool,
    /// `d0` is a tree: its `up` column points at a parent row, and its
    /// hierarchy field `tree` selects subtrees (plan §5.6).
    pub tree: bool,
}

/// `d0`'s hierarchy, as its dataset's extension in the model file.
pub const TREE_EXTENSION: &str = "    custom_extensions:\n      - vendor_name: TIDEPOOL\n        data: '{\"hierarchy\": {\"name\": \"tree\", \"parent\": \"up\", \"label\": \"name\"}}'\n";

impl Schema {
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// Tables reachable from `from` (including itself), with the relationship
    /// that leads to each (none for `from`).
    #[must_use]
    pub fn reachable(&self, from: &str) -> Vec<(String, Option<Relationship>)> {
        let mut out: Vec<(String, Option<Relationship>)> = vec![(from.to_string(), None)];
        let mut i = 0;
        while i < out.len() {
            let current = out[i].0.clone();
            for r in self.relationships.iter().filter(|r| r.from == current) {
                if !out.iter().any(|(t, _)| *t == r.to) {
                    out.push((r.to.clone(), Some(r.clone())));
                }
            }
            i += 1;
        }
        out
    }

    /// Whether `table.field` is the hierarchy field.
    #[must_use]
    pub fn is_tree(&self, table: &str, field: &str) -> bool {
        self.tree && table == "d0" && field == "tree"
    }

    /// How a field's values are made: a column's, or the keys for the
    /// hierarchy field.
    #[must_use]
    pub fn values_of(&self, table: &str, field: &str) -> Option<Gen> {
        if self.is_tree(table, field) {
            return Some(Gen::Key("k".into(), 0));
        }
        self.table(table)
            .and_then(|t| t.column(field))
            .map(|c| c.values.clone())
    }

    #[must_use]
    pub fn metric(&self, name: &str) -> Option<&Metric> {
        self.metrics.iter().find(|m| m.name == name)
    }

    /// The anchors a metric's value comes from.
    #[must_use]
    pub fn anchors(&self, name: &str) -> Vec<String> {
        match self.metric(name).map(|m| &m.kind) {
            Some(MetricKind::Base { anchor, .. }) => vec![anchor.clone()],
            Some(MetricKind::Derived { inputs, .. }) => {
                let mut all: Vec<String> = inputs.iter().flat_map(|i| self.anchors(i)).collect();
                all.sort();
                all.dedup();
                all
            }
            None => Vec::new(),
        }
    }
}

fn column(name: &str, ty: &str, sql_ty: &str, nullable: bool, values: Gen) -> Column {
    Column {
        name: name.to_string(),
        ty: ty.to_string(),
        sql_ty: sql_ty.to_string(),
        nullable,
        values,
    }
}

fn base(
    name: &str,
    model: &str,
    sql: &str,
    anchor: &str,
    extension: Option<String>,
    ignore: Ignore,
) -> Metric {
    Metric {
        name: name.to_string(),
        model_expr: model.to_string(),
        extension,
        kind: MetricKind::Base {
            anchor: anchor.to_string(),
            sql: sql.to_string(),
            ignore,
            shift: None,
        },
    }
}

fn derived(name: &str, inputs: &[&str], model: &str, sql: &str) -> Metric {
    Metric {
        name: name.to_string(),
        model_expr: model.to_string(),
        extension: Some(r#"{"kind": "derived"}"#.to_string()),
        kind: MetricKind::Derived {
            inputs: inputs.iter().map(|s| (*s).to_string()).collect(),
            sql: sql.to_string(),
        },
    }
}

/// A random schema: one or two fact tables, one to three dimensions, maybe a
/// snowflaked parent.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn schema(rng: &mut Rng) -> Schema {
    let mut tables = Vec::new();
    let mut relationships = Vec::new();
    let dims = 1 + rng.below(3);
    let snowflake = rng.chance(40);
    let second_fact = rng.chance(50);
    let tree = rng.chance(50);

    if snowflake {
        tables.push(Table {
            name: "p0".into(),
            upsert: true,
            columns: vec![
                column(
                    "key",
                    "string",
                    "VARCHAR",
                    false,
                    Gen::Key("p".into(), 4 + rng.below(4)),
                ),
                column(
                    "label",
                    "string",
                    "VARCHAR",
                    rng.chance(50),
                    Gen::Choice(vec!["x", "y", "z"]),
                ),
            ],
        });
    }
    for d in 0..dims {
        let name = format!("d{d}");
        let mut columns = vec![
            column(
                "key",
                "string",
                "VARCHAR",
                false,
                Gen::Key("k".into(), 5 + rng.below(25)),
            ),
            column(
                "name",
                "string",
                "VARCHAR",
                rng.chance(30),
                Gen::Choice(vec!["ann", "bo", "cy", "dee", "eve"]),
            ),
            column("tier", "int32", "INTEGER", rng.chance(30), Gen::Int(1, 4)),
        ];
        if rng.chance(50) {
            columns.push(column(
                "since",
                "date",
                "DATE",
                true,
                Gen::Date(20_000, 20_400),
            ));
        }
        if d == 0 && tree {
            columns.push(column("up", "string", "VARCHAR", true, Gen::Up));
        }
        if d == 0 && snowflake {
            columns.push(column(
                "parent",
                "string",
                "VARCHAR",
                true,
                Gen::Ref("p0".into()),
            ));
            relationships.push(Relationship {
                from: name.clone(),
                fk: "parent".into(),
                to: "p0".into(),
            });
        }
        tables.push(Table {
            name,
            upsert: true,
            columns,
        });
    }

    // sometimes a keyed fact: rows rewritten and deleted after they were
    // counted, which is what keeps incremental answers honest
    let keyed = rng.chance(50);
    let mut f0 = vec![];
    if keyed {
        f0.push(column(
            "key",
            "string",
            "VARCHAR",
            false,
            Gen::Key("o".into(), 10_000),
        ));
    }
    for d in 0..dims {
        f0.push(column(
            &format!("d{d}_key"),
            "string",
            "VARCHAR",
            rng.chance(20),
            Gen::Ref(format!("d{d}")),
        ));
        relationships.push(Relationship {
            from: "f0".into(),
            fk: format!("d{d}_key"),
            to: format!("d{d}"),
        });
    }
    f0.extend([
        column(
            "status",
            "string",
            "VARCHAR",
            rng.chance(50),
            Gen::Choice(vec!["a", "b", "c"]),
        ),
        column("qty", "int32", "INTEGER", rng.chance(40), Gen::Int(0, 9)),
        column(
            "amount",
            "\"decimal(10,2)\"",
            "DECIMAL(10,2)",
            false,
            Gen::Decimal(0, 10_000, 2),
        ),
        column("score", "float64", "DOUBLE", true, Gen::Float),
        column("day", "date", "DATE", false, Gen::Date(20_300, 20_400)),
        column(
            "seen_at",
            "timestamptz",
            "TIMESTAMP",
            rng.chance(30),
            Gen::Timestamp(1_750_000_000, 1_760_000_000),
        ),
    ]);
    tables.push(Table {
        name: "f0".into(),
        upsert: keyed,
        columns: f0,
    });
    if second_fact {
        tables.push(Table {
            name: "f1".into(),
            upsert: false,
            columns: vec![
                column(
                    "d0_key",
                    "string",
                    "VARCHAR",
                    rng.chance(40),
                    Gen::Ref("d0".into()),
                ),
                column("qty", "int32", "INTEGER", false, Gen::Int(0, 5)),
                column(
                    "amount",
                    "\"decimal(12,3)\"",
                    "DECIMAL(12,3)",
                    true,
                    Gen::Decimal(-5_000, 50_000, 3),
                ),
            ],
        });
        relationships.push(Relationship {
            from: "f1".into(),
            fk: "d0_key".into(),
            to: "d0".into(),
        });
    }

    let derived_fields = vec![
        DerivedField {
            table: "f0".into(),
            name: "big".into(),
            model_expr: "amount > 50".into(),
            sql_expr: "(f0.amount > 50)".into(),
            temporal: false,
        },
        DerivedField {
            table: "f0".into(),
            name: "month_num".into(),
            model_expr: "EXTRACT(MONTH FROM day)".into(),
            sql_expr: "EXTRACT(MONTH FROM f0.day)".into(),
            temporal: false,
        },
        DerivedField {
            table: "d0".into(),
            name: "upper_name".into(),
            model_expr: "UPPER(name)".into(),
            sql_expr: "UPPER(d0.name)".into(),
            temporal: false,
        },
        // non-NULL for a NULL input: a missing related row must still be
        // NULL, a present one with no name 'nobody'
        DerivedField {
            table: "d0".into(),
            name: "named".into(),
            model_expr: "COALESCE(name, 'nobody')".into(),
            sql_expr: "COALESCE(d0.name, 'nobody')".into(),
            temporal: false,
        },
        DerivedField {
            table: "f0".into(),
            name: "status_or_none".into(),
            model_expr: "COALESCE(status, 'none')".into(),
            sql_expr: "COALESCE(f0.status, 'none')".into(),
            temporal: false,
        },
    ];

    let table_ext = |t: &str| Some(format!(r#"{{"table": "{t}"}}"#));
    let mut metrics = vec![
        base(
            "f0_rows",
            "COUNT(*)",
            "COUNT(*)",
            "f0",
            table_ext("f0"),
            Ignore::None,
        ),
        base(
            "f0_amount",
            "SUM(f0.amount)",
            "SUM(f0.amount)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_qty",
            "SUM(f0.qty)",
            "SUM(f0.qty)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_avg_qty",
            "AVG(f0.qty)",
            "AVG(f0.qty)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_counted_qty",
            "COUNT(f0.qty)",
            "COUNT(f0.qty)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_counted_status",
            "COUNT(f0.status) + MIN(f0.qty)",
            "COUNT(f0.status) + MIN(f0.qty)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_score",
            "SUM(f0.score)",
            "SUM(f0.score)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_keys",
            "COUNT(DISTINCT f0.d0_key)",
            "COUNT(DISTINCT f0.d0_key)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_paid",
            "SUM(f0.amount) FILTER (WHERE f0.status = 'a')",
            "SUM(f0.amount) FILTER (WHERE f0.status = 'a')",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_first",
            "MIN(f0.day)",
            "MIN(f0.day)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_last_status",
            "MAX(f0.status)",
            "MAX(f0.status)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_per_row",
            "SUM(f0.amount) / COUNT(*)",
            "SUM(f0.amount) / NULLIF(COUNT(*), 0)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_mixed",
            "SUM(f0.amount) + COUNT(DISTINCT f0.status) * 2",
            "SUM(f0.amount) + COUNT(DISTINCT f0.status) * 2",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_tiered",
            "SUM(f0.qty * d0.tier)",
            "SUM(f0.qty * d0.tier)",
            "f0",
            None,
            Ignore::None,
        ),
        base(
            "f0_all",
            "SUM(f0.amount)",
            "SUM(f0.amount)",
            "f0",
            Some(r#"{"ignore_selection": ["*"]}"#.into()),
            Ignore::All,
        ),
        base(
            "f0_no_name",
            "SUM(f0.amount)",
            "SUM(f0.amount)",
            "f0",
            Some(r#"{"ignore_selection": ["d0.name"]}"#.into()),
            Ignore::Fields(vec!["d0.name".into()]),
        ),
        base(
            "d0_rows",
            "COUNT(*)",
            "COUNT(*)",
            "d0",
            table_ext("d0"),
            Ignore::None,
        ),
        base(
            "d0_tier",
            "SUM(d0.tier)",
            "SUM(d0.tier)",
            "d0",
            None,
            Ignore::None,
        ),
        {
            // the same sum a while ago: a few days, a week, a month or a
            // quarter, on the day or the timestamp (the data spans ~100 days)
            let (offset, interval) = *rng.pick(&[
                ("-3d", "3 DAY"),
                ("-1w", "7 DAY"),
                ("-1m", "1 MONTH"),
                ("-1q", "3 MONTH"),
                ("2d", "(-2) DAY"),
            ]);
            let (column, date) = *rng.pick(&[("day", true), ("seen_at", false)]);
            let mut metric = base(
                "f0_amount_before",
                "SUM(f0.amount)",
                "SUM(f0.amount)",
                "f0",
                Some(format!(
                    r#"{{"time_offset": "{offset}", "time_field": "f0.{column}"}}"#
                )),
                Ignore::None,
            );
            if let MetricKind::Base { shift, .. } = &mut metric.kind {
                *shift = Some(Shifted {
                    table: "f0".into(),
                    column: column.into(),
                    interval: interval.into(),
                    date,
                });
            }
            metric
        },
        derived(
            "f0_change",
            &["f0_amount", "f0_amount_before"],
            "f0_amount - f0_amount_before",
            "{f0_amount} - {f0_amount_before}",
        ),
        derived(
            "f0_per_qty",
            &["f0_amount", "f0_qty"],
            "f0_amount / f0_qty",
            "{f0_amount} / NULLIF({f0_qty}, 0)",
        ),
        derived(
            "f0_double",
            &["f0_per_qty"],
            "f0_per_qty * 2",
            "{f0_per_qty} * 2",
        ),
        derived(
            "amount_per_d0",
            &["f0_amount", "d0_rows"],
            "f0_amount / d0_rows",
            "{f0_amount} / NULLIF({d0_rows}, 0)",
        ),
    ];
    if second_fact {
        metrics.extend([
            base(
                "f1_rows",
                "COUNT(*)",
                "COUNT(*)",
                "f1",
                table_ext("f1"),
                Ignore::None,
            ),
            base(
                "f1_amount",
                "SUM(f1.amount)",
                "SUM(f1.amount)",
                "f1",
                None,
                Ignore::None,
            ),
            base(
                "f1_max",
                "MAX(f1.amount)",
                "MAX(f1.amount)",
                "f1",
                None,
                Ignore::None,
            ),
            derived(
                "f0_vs_f1",
                &["f0_amount", "f1_amount"],
                "f0_amount - f1_amount",
                "{f0_amount} - {f1_amount}",
            ),
        ]);
    }
    if tree {
        // a metric ignoring the tree: it must not see the subtrees either
        metrics.push(base(
            "f0_no_tree",
            "SUM(f0.amount)",
            "SUM(f0.amount)",
            "f0",
            Some(r#"{"ignore_selection": ["d0.tree"]}"#.into()),
            Ignore::Fields(vec!["d0.tree".into()]),
        ));
    }
    if snowflake {
        metrics.push(base(
            "p0_rows",
            "COUNT(*)",
            "COUNT(*)",
            "p0",
            table_ext("p0"),
            Ignore::None,
        ));
    }

    Schema {
        tables,
        relationships,
        derived: derived_fields,
        metrics,
        // relationships always form a tree here, so both ways is valid
        both: rng.chance(50),
        tree,
    }
}

/// The schema as config files: (relative path, text).
#[must_use]
pub fn files(schema: &Schema) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for table in &schema.tables {
        let mut text = format!(
            "name: {}\nmode: {}\n",
            table.name,
            if table.upsert { "upsert" } else { "append" }
        );
        if table.upsert {
            text.push_str("primary_key: [key]\n");
        }
        text.push_str("columns:\n");
        for c in &table.columns {
            let _ = writeln!(
                text,
                "  {}: {{ type: {}{} }}",
                c.name,
                c.ty,
                if c.nullable { ", nullable: true" } else { "" }
            );
        }
        files.push((format!("tables/{}.yaml", table.name), text));
    }

    let mut model = String::from("version: \"0.2.0\"\nname: oracle\ndatasets:\n");
    for table in &schema.tables {
        let _ = writeln!(model, "  - name: {0}\n    source: {0}", table.name);
        let fields: Vec<&DerivedField> = schema
            .derived
            .iter()
            .filter(|d| d.table == table.name)
            .collect();
        if !fields.is_empty() {
            model.push_str("    fields:\n");
            for field in fields {
                let _ = writeln!(
                    model,
                    "      - name: {}\n        expression: {{ dialects: [{{ dialect: OSSIE_SQL_2026, expression: \"{}\" }}] }}",
                    field.name, field.model_expr
                );
            }
        }
        if schema.tree && table.name == "d0" {
            model.push_str(TREE_EXTENSION);
        }
    }
    model.push_str("relationships:\n");
    for r in &schema.relationships {
        let _ = writeln!(
            model,
            "  - name: {from}_{fk}\n    from: {from}\n    to: {to}\n    from_columns: [{fk}]\n    to_columns: [key]",
            from = r.from,
            fk = r.fk,
            to = r.to
        );
    }
    if schema.relationships.is_empty() {
        model = model.replace("relationships:\n", "relationships: []\n");
    }
    model.push_str("metrics:\n");
    for m in &schema.metrics {
        let _ = writeln!(
            model,
            "  - name: {}\n    expression: {{ dialects: [{{ dialect: OSSIE_SQL_2026, expression: \"{}\" }}] }}",
            m.name, m.model_expr
        );
        if let Some(extension) = &m.extension {
            let _ = writeln!(
                model,
                "    custom_extensions:\n      - vendor_name: TIDEPOOL\n        data: '{extension}'"
            );
        }
    }
    if schema.both {
        model.push_str(
            "custom_extensions:\n  - vendor_name: TIDEPOOL\n    data: '{\"filter_direction\": \"both\"}'\n",
        );
    }
    files.push(("model.ossie.yaml".to_string(), model));
    files
}

/// One write, as a client would send it.
#[derive(Clone, Debug)]
pub enum Write {
    Rows {
        table: String,
        rows: Vec<Json>,
    },
    Delete {
        table: String,
        keys: Vec<Json>,
    },
    /// Publish an epoch here.
    Publish,
}

/// One generated row, by column.
type Row = Map<String, Json>;

/// The final contents of every table (what DuckDB loads), and the writes
/// that lead there (what Tidepool gets).
#[derive(Clone, Debug, Default)]
pub struct Data {
    pub tables: BTreeMap<String, Vec<Map<String, Json>>>,
    pub writes: Vec<Write>,
}

fn keys_of(schema: &Schema, table: &str) -> Vec<String> {
    schema
        .table(table)
        .and_then(|t| t.column("key"))
        .map(|c| match &c.values {
            Gen::Key(prefix, count) => (0..*count).map(|i| format!("{prefix}{i}")).collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

fn value(rng: &mut Rng, schema: &Schema, column: &Column, index: usize) -> Json {
    if column.nullable && rng.chance(15) {
        return Json::Null;
    }
    match &column.values {
        Gen::Key(prefix, _) => json!(format!("{prefix}{index}")),
        Gen::Ref(table) => {
            let keys = keys_of(schema, table);
            if keys.is_empty() || rng.chance(8) {
                json!("dangling")
            } else {
                json!(rng.pick(&keys))
            }
        }
        Gen::Up => {
            if index == 0 || rng.chance(15) {
                Json::Null
            } else if rng.chance(5) {
                json!("dangling")
            } else {
                json!(format!("k{}", rng.below(index)))
            }
        }
        Gen::Choice(choices) => json!(rng.pick(choices)),
        Gen::Int(lo, hi) => json!(rng.range(*lo, *hi)),
        Gen::Decimal(lo, hi, scale) => {
            let v = rng.range(*lo, *hi);
            let unit = 10i64.pow(u32::from(*scale));
            let sign = if v < 0 { "-" } else { "" };
            json!(format!(
                "{sign}{}.{:0width$}",
                (v / unit).abs(),
                (v % unit).abs(),
                width = usize::from(*scale)
            ))
        }
        #[allow(clippy::cast_precision_loss)]
        Gen::Float => json!(rng.range(-1000, 100_000) as f64 / 8.0),
        Gen::Date(lo, hi) => {
            let days = rng.range(*lo, *hi);
            json!(tidepool_expr::eval::text(&tidepool_expr::Scalar::Date(
                i32::try_from(days).unwrap_or(0)
            )))
        }
        Gen::Timestamp(lo, hi) => {
            let seconds = rng.range(*lo, *hi);
            json!(tidepool_expr::eval::text(
                &tidepool_expr::Scalar::TimestampTz(seconds * 1_000_000)
            ))
        }
    }
}

/// Random data for `schema`, written in a random order of batches with
/// upserts, deletes and publishes mixed in.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn data(rng: &mut Rng, schema: &Schema) -> Data {
    let mut data = Data::default();
    let mut tables: BTreeMap<String, BTreeMap<String, Map<String, Json>>> = BTreeMap::new();
    let mut facts: BTreeMap<String, Vec<Map<String, Json>>> = BTreeMap::new();

    let row = |rng: &mut Rng, table: &Table, index: usize| -> Map<String, Json> {
        table
            .columns
            .iter()
            .map(|c| (c.name.clone(), value(rng, schema, c, index)))
            .collect()
    };

    let is_fact = |t: &Table| t.name.starts_with('f');
    // dimensions first, then an update pass and some deletes
    for table in schema.tables.iter().filter(|t| t.upsert && !is_fact(t)) {
        let keys = keys_of(schema, &table.name);
        let rows: Vec<Map<String, Json>> = (0..keys.len()).map(|i| row(rng, table, i)).collect();
        for r in &rows {
            if let Some(Json::String(key)) = r.get("key") {
                tables
                    .entry(table.name.clone())
                    .or_default()
                    .insert(key.clone(), r.clone());
            }
        }
        data.writes.push(Write::Rows {
            table: table.name.clone(),
            rows: rows.into_iter().map(Json::Object).collect(),
        });
    }
    data.writes.push(Write::Publish);

    // a keyed fact's live rows by key, in insertion order
    let mut keyed: BTreeMap<String, Vec<(String, Row)>> = BTreeMap::new();
    for table in schema.tables.iter().filter(|t| is_fact(t)) {
        let total = 20 + rng.below(600);
        let batches = 1 + rng.below(4);
        for b in 0..batches {
            let rows: Vec<Map<String, Json>> = (0..total / batches)
                .map(|i| row(rng, table, b * total + i))
                .collect();
            if table.upsert {
                let live = keyed.entry(table.name.clone()).or_default();
                for r in &rows {
                    if let Some(Json::String(key)) = r.get("key") {
                        live.push((key.clone(), r.clone()));
                    }
                }
            } else {
                facts
                    .entry(table.name.clone())
                    .or_default()
                    .extend(rows.iter().cloned());
            }
            data.writes.push(Write::Rows {
                table: table.name.clone(),
                rows: rows.into_iter().map(Json::Object).collect(),
            });
            if rng.chance(60) {
                data.writes.push(Write::Publish);
            }
            // a keyed fact: rewrite and delete rows already written
            if table.upsert
                && rng.chance(70)
                && let Some(live) = keyed.get_mut(&table.name)
                && !live.is_empty()
            {
                let rewritten: Vec<Map<String, Json>> = (0..=rng.below(8))
                    .map(|_| {
                        let at = rng.below(live.len());
                        let mut fresh = row(rng, table, 0);
                        fresh.insert("key".into(), json!(live[at].0));
                        live[at].1.clone_from(&fresh);
                        fresh
                    })
                    .collect();
                data.writes.push(Write::Rows {
                    table: table.name.clone(),
                    rows: rewritten.into_iter().map(Json::Object).collect(),
                });
                let mut gone: Vec<Json> = Vec::new();
                for _ in 0..rng.below(4) {
                    if !live.is_empty() {
                        let at = rng.below(live.len());
                        gone.push(json!({ "key": live.remove(at).0 }));
                    }
                }
                if !gone.is_empty() {
                    data.writes.push(Write::Delete {
                        table: table.name.clone(),
                        keys: gone,
                    });
                }
                if rng.chance(60) {
                    data.writes.push(Write::Publish);
                }
            }
            // dimension changes in between fact batches
            if rng.chance(50)
                && let Some(dim) = schema
                    .tables
                    .iter()
                    .filter(|t| t.upsert && !is_fact(t))
                    .nth(rng.below(3))
            {
                let keys = keys_of(schema, &dim.name);
                let changed: Vec<Map<String, Json>> = (0..=rng.below(4))
                    .map(|_| {
                        let index = rng.below(keys.len().max(1));
                        row(rng, dim, index)
                    })
                    .collect();
                for r in &changed {
                    if let Some(Json::String(key)) = r.get("key") {
                        tables
                            .entry(dim.name.clone())
                            .or_default()
                            .insert(key.clone(), r.clone());
                    }
                }
                data.writes.push(Write::Rows {
                    table: dim.name.clone(),
                    rows: changed.into_iter().map(Json::Object).collect(),
                });
                if rng.chance(40) {
                    let victim = rng.pick(&keys).clone();
                    if let Some(rows) = tables.get_mut(&dim.name) {
                        rows.remove(&victim);
                    }
                    data.writes.push(Write::Delete {
                        table: dim.name.clone(),
                        keys: vec![json!({ "key": victim })],
                    });
                }
            }
        }
    }
    data.writes.push(Write::Publish);

    for (name, rows) in tables {
        data.tables.insert(name, rows.into_values().collect());
    }
    for (name, rows) in facts {
        data.tables.insert(name, rows);
    }
    for (name, live) in keyed {
        data.tables
            .insert(name, live.into_iter().map(|(_, row)| row).collect());
    }
    data
}

/// A query, in schema terms.
#[derive(Clone, Debug)]
pub struct Query {
    pub metrics: Vec<String>,
    /// `table.field`, a grain for temporal fields, and a level for the
    /// hierarchy field.
    pub dims: Vec<(String, String, Option<&'static str>, Option<u32>)>,
    /// `table.column` → condition.
    pub selection: Vec<(String, String, Cond)>,
}

/// A selection condition, values as JSON (as in the data).
#[derive(Clone, Debug)]
pub enum Cond {
    In(Vec<Json>),
    Range(Json, Json),
    /// The `last` whole `unit`s ending `ago` units before the one `now`
    /// (microseconds, UTC) is in: a relative window, as a dashboard keeps
    /// one.
    Relative {
        now: i64,
        unit: &'static str,
        last: u32,
        ago: u32,
    },
}

/// Fields of a table: columns and derived fields, with whether temporal.
fn fields(schema: &Schema, table: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = schema
        .table(table)
        .map(|t| {
            t.columns
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        matches!(c.values, Gen::Date(..) | Gen::Timestamp(..)),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    out.extend(
        schema
            .derived
            .iter()
            .filter(|d| d.table == table)
            .map(|d| (d.name.clone(), d.temporal)),
    );
    if schema.is_tree(table, "tree") {
        out.push(("tree".into(), false));
    }
    out
}

/// A query's metrics and the tables they're anchored on; sometimes no
/// metrics (the `bool`): only dimensions, all reached from one table.
fn metrics(rng: &mut Rng, schema: &Schema) -> (Vec<String>, Vec<String>, bool) {
    let names: Vec<String> = schema.metrics.iter().map(|m| m.name.clone()).collect();
    let mut metrics: Vec<String> = Vec::new();
    for _ in 0..=rng.below(3) {
        let name = rng.pick(&names).clone();
        if !metrics.contains(&name) {
            metrics.push(name);
        }
    }
    if rng.chance(15) {
        return (
            Vec::new(),
            vec![rng.pick(&schema.tables).name.clone()],
            true,
        );
    }
    let anchors = metrics.iter().flat_map(|m| schema.anchors(m)).collect();
    (metrics, anchors, false)
}

/// A random query over `schema`, with selection values taken from `data`.
#[must_use]
pub fn query(rng: &mut Rng, schema: &Schema, data: &Data) -> Query {
    let (metrics, anchors, only_dims) = metrics(rng, schema);
    // tables every anchor reaches
    let mut common: Vec<String> = schema.tables.iter().map(|t| t.name.clone()).collect();
    for anchor in &anchors {
        let reachable: Vec<String> = schema
            .reachable(anchor)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        common.retain(|t| reachable.contains(t));
    }
    let candidates: Vec<(String, String, bool)> = common
        .iter()
        .flat_map(|t| {
            fields(schema, t)
                .into_iter()
                .map(move |(f, temporal)| (t.clone(), f, temporal))
        })
        .collect();
    let mut dims = Vec::new();
    if !candidates.is_empty() {
        for _ in 0..rng.below(3) + usize::from(only_dims) {
            let (table, field, temporal) = rng.pick(&candidates).clone();
            if dims
                .iter()
                .any(|(t, f, _, _): &(String, String, _, _)| *t == table && *f == field)
            {
                continue;
            }
            let grain = (temporal && rng.chance(60))
                .then(|| *rng.pick(&["day", "week", "month", "quarter", "year"]));
            let level = (schema.is_tree(&table, &field) && rng.chance(70))
                .then(|| u32::try_from(rng.below(4)).unwrap_or(0));
            dims.push((table, field, grain, level));
        }
    }

    // selections on any column of any table (unreachable ones must be ignored)
    let mut selection = Vec::new();
    for _ in 0..rng.below(3) {
        let table = rng.pick(&schema.tables).clone();
        let column = rng.pick(&table.columns).clone();
        let rows = data.tables.get(&table.name).cloned().unwrap_or_default();
        let values: Vec<Json> = rows
            .iter()
            .filter_map(|r| r.get(&column.name).cloned())
            .filter(|v| !v.is_null())
            .collect();
        if values.is_empty()
            || selection
                .iter()
                .any(|(t, c, _)| *t == table.name && *c == column.name)
        {
            continue;
        }
        let ranged = matches!(
            column.values,
            Gen::Int(..) | Gen::Decimal(..) | Gen::Date(..) | Gen::Timestamp(..)
        );
        let temporal = matches!(column.values, Gen::Date(..) | Gen::Timestamp(..));
        let cond = if temporal && rng.chance(30) {
            let near = rng.pick(&values).clone();
            relative(rng, &column.values, &near)
        } else if ranged && rng.chance(40) {
            let (mut a, mut b) = (rng.pick(&values).clone(), rng.pick(&values).clone());
            // sometimes bounds between two stored values: one digit finer
            // than the column keeps
            if matches!(column.values, Gen::Decimal(..)) && rng.chance(50) {
                for bound in [&mut a, &mut b] {
                    if let Json::String(text) = bound {
                        text.push('5');
                    }
                }
            }
            Cond::Range(a, b)
        } else {
            let mut picked: Vec<Json> = (0..=rng.below(3))
                .map(|_| rng.pick(&values).clone())
                .collect();
            if rng.chance(10)
                && matches!(column.values, Gen::Choice(_) | Gen::Key(..) | Gen::Ref(_))
            {
                picked.push(json!("nothing"));
            }
            Cond::In(picked)
        };
        selection.push((table.name.clone(), column.name.clone(), cond));
    }
    if let Some(subtrees) = subtrees(rng, schema, data) {
        selection.push(subtrees);
    }
    Query {
        metrics,
        dims,
        selection,
    }
}

/// Sometimes a selection of subtrees of the hierarchy, sometimes naming a
/// node that isn't there.
fn subtrees(rng: &mut Rng, schema: &Schema, data: &Data) -> Option<(String, String, Cond)> {
    if !schema.tree || !rng.chance(40) {
        return None;
    }
    let keys: Vec<Json> = data
        .tables
        .get("d0")
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("key").cloned())
        .collect();
    if keys.is_empty() {
        return None;
    }
    let mut picked: Vec<Json> = (0..=rng.below(2))
        .map(|_| rng.pick(&keys).clone())
        .collect();
    if rng.chance(10) {
        picked.push(json!("nothing"));
    }
    Some(("d0".into(), "tree".into(), Cond::In(picked)))
}

/// A relative window around `near` (a stored value), so it usually holds
/// some rows: units a date has, or any for a timestamp, and sometimes ending
/// before the current period.
fn relative(rng: &mut Rng, values: &Gen, near: &Json) -> Cond {
    const DAY: i64 = 86_400_000_000;
    let text = near.as_str().unwrap_or_default();
    let at = match values {
        Gen::Date(..) => tidepool_expr::temporal::parse_date(text).map(|d| i64::from(d) * DAY),
        _ => tidepool_expr::temporal::parse_timestamptz(text),
    }
    .unwrap_or_default();
    // now: a few days either side of the value, at any time of day
    let now = at + rng.range(-3 * DAY, 3 * DAY);
    let units: &[(&'static str, usize)] = match values {
        Gen::Date(..) => &[
            ("day", 60),
            ("week", 10),
            ("month", 4),
            ("quarter", 2),
            ("year", 1),
        ],
        _ => &[
            ("minute", 3000),
            ("hour", 300),
            ("day", 60),
            ("week", 10),
            ("month", 4),
            ("quarter", 2),
            ("year", 1),
        ],
    };
    let (unit, most) = *rng.pick(units);
    Cond::Relative {
        now,
        unit,
        last: u32::try_from(1 + rng.below(most)).unwrap_or(1),
        ago: if rng.chance(30) {
            u32::try_from(1 + rng.below(3)).unwrap_or(1)
        } else {
            0
        },
    }
}

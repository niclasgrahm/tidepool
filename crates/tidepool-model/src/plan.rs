//! Comparing two compiled models: what changed, and what applying it costs
//! (plan §8.2).
//!
//! Objects are compared by stable id, so a rename (declared with
//! `renamed_from`) shows as a rename rather than a drop and an add. Each change
//! gets a class: **safe** (applies instantly), **rebuild** (existing data is
//! rewritten in the background) or **destructive** (existing data would be
//! lost; needs explicit approval). The plan's class is the worst of its
//! changes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::ir::{Column, ColumnId, Model, Table, TableId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeClass {
    Safe,
    Rebuild,
    Destructive,
}

impl ChangeClass {
    fn label(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Rebuild => "rebuild",
            Self::Destructive => "destructive",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Add,
    Remove,
    Change,
    Rename,
}

impl Op {
    fn sigil(self) -> char {
        match self {
            Self::Add => '+',
            Self::Remove => '-',
            Self::Change => '~',
            Self::Rename => '>',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Change {
    pub op: Op,
    /// `table`, `column`, `relationship`, `dataset`, `field`, `metric`,
    /// `dashboard` or `setting`.
    pub kind: &'static str,
    /// What changed, e.g. `orders.quantity`.
    pub subject: String,
    /// How, e.g. `int32 → int64`.
    pub detail: Option<String>,
    pub class: ChangeClass,
    /// Why it is classed as it is, when that isn't obvious.
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub warnings: Vec<String>,
}

impl Plan {
    /// The worst class among the changes, or `None` for an empty plan.
    #[must_use]
    pub fn class(&self) -> Option<ChangeClass> {
        self.changes.iter().map(|c| c.class).max()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The plan as `tidepool plan` prints it.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.changes.is_empty() {
            out.push_str("no changes\n");
        } else {
            let count = |class| self.changes.iter().filter(|c| c.class == class).count();
            let n = self.changes.len();
            let _ = writeln!(
                out,
                "{n} change{} ({} safe, {} rebuild, {} destructive)\n",
                if n == 1 { "" } else { "s" },
                count(ChangeClass::Safe),
                count(ChangeClass::Rebuild),
                count(ChangeClass::Destructive)
            );
            let rows: Vec<(String, &Change)> = self
                .changes
                .iter()
                .map(|c| {
                    let what = match &c.detail {
                        Some(detail) => format!("{}  {detail}", c.subject),
                        None => c.subject.clone(),
                    };
                    (what, c)
                })
                .collect();
            let width = rows
                .iter()
                .map(|(what, _)| what.chars().count())
                .max()
                .unwrap_or(0);
            for (what, change) in rows {
                let _ = write!(
                    out,
                    "  {} {:<12} {what:<width$}  {}",
                    change.op.sigil(),
                    change.kind,
                    change.class.label()
                );
                if let Some(note) = &change.note {
                    let _ = write!(out, "  ({note})");
                }
                out.push('\n');
            }
        }
        for warning in &self.warnings {
            let _ = writeln!(out, "\nwarning: {warning}");
        }
        out
    }

    fn push(
        &mut self,
        op: Op,
        kind: &'static str,
        subject: impl Into<String>,
        class: ChangeClass,
    ) -> &mut Change {
        self.changes.push(Change {
            op,
            kind,
            subject: subject.into(),
            detail: None,
            class,
            note: None,
        });
        let last = self.changes.len() - 1;
        &mut self.changes[last]
    }
}

impl Change {
    fn detail(&mut self, detail: impl Into<String>) -> &mut Self {
        self.detail = Some(detail.into());
        self
    }

    fn note(&mut self, note: impl Into<String>) -> &mut Self {
        self.note = Some(note.into());
        self
    }
}

/// What applying `new` over `old` changes. Both must come from the same
/// catalog lineage (`new` compiled against `old`'s catalog), so ids match.
#[must_use]
pub fn diff(old: &Model, new: &Model) -> Plan {
    let mut plan = Plan::default();
    tables(&mut plan, old, new);
    relationships(&mut plan, old, new);
    datasets(&mut plan, old, new);
    metrics(&mut plan, old, new);
    dashboards(&mut plan, old, new);
    settings(&mut plan, old, new);
    plan
}

fn tables(plan: &mut Plan, old: &Model, new: &Model) {
    let removed: Vec<&Table> = old
        .tables
        .iter()
        .filter(|t| new.table(t.id).is_none())
        .collect();
    let added: Vec<&Table> = new
        .tables
        .iter()
        .filter(|t| old.table(t.id).is_none())
        .collect();

    for table in &new.tables {
        let Some(before) = old.table(table.id) else {
            plan.push(Op::Add, "table", &table.name, ChangeClass::Safe)
                .detail(format!(
                    "{}, {} column{}",
                    format!("{:?}", table.mode).to_lowercase(),
                    table.columns.len(),
                    if table.columns.len() == 1 { "" } else { "s" }
                ));
            continue;
        };
        if before.name != table.name {
            plan.push(
                Op::Rename,
                "table",
                format!("{} → {}", before.name, table.name),
                ChangeClass::Safe,
            );
        }
        if before.mode != table.mode {
            plan.push(Op::Change, "table", &table.name, ChangeClass::Destructive)
                .detail(format!("mode {:?} → {:?}", before.mode, table.mode).to_lowercase())
                .note("rows are kept differently in each mode");
        }
        if before.primary_key != table.primary_key {
            let names = |t: &Table| {
                t.primary_key
                    .iter()
                    .filter_map(|id| t.column(*id).map(|c| c.name.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            // a key with fewer columns can make distinct rows share a key
            let narrower = before
                .primary_key
                .iter()
                .any(|c| !table.primary_key.contains(c));
            let change = plan.push(Op::Change, "table", &table.name, ChangeClass::Rebuild);
            change.detail(format!(
                "primary key ({}) → ({})",
                names(before),
                names(table)
            ));
            change.note(if narrower {
                "the key index is rebuilt; rows that now share a key collapse to the latest"
            } else {
                "the key index is rebuilt"
            });
        }
        if before.event_time != table.event_time {
            let name = |t: &Table| {
                t.event_time
                    .and_then(|id| t.column(id))
                    .map_or("none", |c| c.name.as_str())
                    .to_string()
            };
            plan.push(Op::Change, "table", &table.name, ChangeClass::Safe)
                .detail(format!("event_time {} → {}", name(before), name(table)));
        }
        if before.retention != table.retention {
            let show = |r: Option<tidepool_config::types::HumanDuration>| {
                r.map_or("none".to_string(), |r| r.to_string())
            };
            plan.push(Op::Change, "table", &table.name, ChangeClass::Safe)
                .detail(format!(
                    "retention {} → {}",
                    show(before.retention),
                    show(table.retention)
                ));
        }
        columns(plan, before, table);
    }

    for table in &removed {
        plan.push(Op::Remove, "table", &table.name, ChangeClass::Destructive)
            .note("its data is deleted");
    }
    for gone in &removed {
        let shape = |t: &Table| {
            t.columns
                .iter()
                .map(|c| (c.name.clone(), c.column_type.to_string()))
                .collect::<BTreeSet<_>>()
        };
        if let Some(arrived) = added.iter().find(|t| shape(t) == shape(gone)) {
            plan.warnings.push(format!(
                "table `{}` was removed and `{}` added with the same columns: if this is a rename, add `renamed_from: {}` to tables/{}.yaml to keep its data",
                gone.name, arrived.name, gone.name, arrived.name
            ));
        }
    }
}

fn columns(plan: &mut Plan, before: &Table, after: &Table) {
    let label = |column: &Column| format!("{}.{}", after.name, column.name);
    let removed: Vec<&Column> = before
        .columns
        .iter()
        .filter(|c| after.column(c.id).is_none())
        .collect();
    let added: Vec<&Column> = after
        .columns
        .iter()
        .filter(|c| before.column(c.id).is_none())
        .collect();

    for column in &after.columns {
        let Some(old) = before.column(column.id) else {
            if column.nullable {
                plan.push(Op::Add, "column", label(column), ChangeClass::Safe)
                    .detail(column.column_type.to_string());
            } else {
                plan.push(Op::Add, "column", label(column), ChangeClass::Destructive)
                    .detail(column.column_type.to_string())
                    .note("existing rows have no value for it; add it as nullable to keep them");
            }
            continue;
        };
        if old.name != column.name {
            plan.push(
                Op::Rename,
                "column",
                format!(
                    "{}.{} → {}.{}",
                    after.name, old.name, after.name, column.name
                ),
                ChangeClass::Safe,
            );
        }
        if old.column_type != column.column_type {
            let widening = old.column_type.widens_to(column.column_type);
            let change = plan.push(
                Op::Change,
                "column",
                label(column),
                if widening {
                    ChangeClass::Rebuild
                } else {
                    ChangeClass::Destructive
                },
            );
            change.detail(format!("{} → {}", old.column_type, column.column_type));
            if !widening {
                change.note("not every existing value fits the new type");
            }
        }
        if old.nullable != column.nullable {
            if column.nullable {
                plan.push(Op::Change, "column", label(column), ChangeClass::Safe)
                    .detail("now nullable");
            } else {
                plan.push(
                    Op::Change,
                    "column",
                    label(column),
                    ChangeClass::Destructive,
                )
                .detail("no longer nullable")
                .note("existing NULLs would be invalid");
            }
        }
        if old.encoding != column.encoding {
            plan.push(Op::Change, "column", label(column), ChangeClass::Rebuild)
                .detail(
                    format!("encoding {:?} → {:?}", old.encoding, column.encoding).to_lowercase(),
                );
        }
    }
    for column in &removed {
        plan.push(
            Op::Remove,
            "column",
            format!("{}.{}", after.name, column.name),
            ChangeClass::Destructive,
        )
        .note("its data is deleted");
    }
    for gone in &removed {
        if let Some(arrived) = added.iter().find(|c| c.column_type == gone.column_type) {
            plan.warnings.push(format!(
                "`{table}.{}` was removed and `{table}.{}` added with the same type: if this is a rename, add `renamed_from: {}` to the column to keep its data",
                gone.name,
                arrived.name,
                gone.name,
                table = after.name
            ));
        }
    }
}

/// The set of (table, column) pairs sharing a dictionary with `column`.
fn domain_of(model: &Model, table: TableId, column: ColumnId) -> BTreeSet<(TableId, ColumnId)> {
    model
        .key_domains
        .iter()
        .find(|d| d.columns.contains(&(table, column)))
        .map_or_else(
            || BTreeSet::from([(table, column)]),
            |d| d.columns.iter().copied().collect(),
        )
}

fn relationships(plan: &mut Plan, old: &Model, new: &Model) {
    let table_name =
        |model: &Model, id: TableId| model.table(id).map_or("?".to_string(), |t| t.name.clone());
    for relationship in &new.relationships {
        let before = old.relationships.iter().find(|r| r.id == relationship.id);
        let subject = format!(
            "{} ({} → {})",
            relationship.name,
            table_name(new, relationship.from),
            table_name(new, relationship.to)
        );
        if before.is_some_and(|b| {
            b.from == relationship.from
                && b.to == relationship.to
                && b.from_columns == relationship.from_columns
                && b.to_columns == relationship.to_columns
        }) {
            if before.is_some_and(|b| b.role != relationship.role) {
                plan.push(Op::Change, "relationship", subject, ChangeClass::Safe)
                    .detail("role");
            }
            continue;
        }
        // Joining two columns that hold data and didn't share a dictionary
        // means re-encoding one side into the other's codes.
        let merges_existing = relationship
            .from_columns
            .iter()
            .zip(&relationship.to_columns)
            .any(|(f, t)| {
                let both_existed = old
                    .table(relationship.from)
                    .and_then(|tb| tb.column(*f))
                    .is_some()
                    && old
                        .table(relationship.to)
                        .and_then(|tb| tb.column(*t))
                        .is_some();
                both_existed
                    && !domain_of(old, relationship.from, *f).contains(&(relationship.to, *t))
            });
        let op = if before.is_some() {
            Op::Change
        } else {
            Op::Add
        };
        if merges_existing {
            plan.push(op, "relationship", subject, ChangeClass::Rebuild)
                .note("joins existing data: the key dictionaries are merged");
        } else {
            plan.push(op, "relationship", subject, ChangeClass::Safe);
        }
    }
    for relationship in &old.relationships {
        if !new.relationships.iter().any(|r| r.id == relationship.id) {
            plan.push(
                Op::Remove,
                "relationship",
                &relationship.name,
                ChangeClass::Safe,
            );
        }
    }
}

fn datasets(plan: &mut Plan, old: &Model, new: &Model) {
    for dataset in &new.datasets {
        let Some(before) = old.dataset(dataset.table) else {
            plan.push(Op::Add, "dataset", &dataset.name, ChangeClass::Safe);
            continue;
        };
        if before.name != dataset.name {
            plan.push(
                Op::Rename,
                "dataset",
                format!("{} → {}", before.name, dataset.name),
                ChangeClass::Safe,
            );
        }
        // fields are compared by name, and only declared changes matter;
        // implicit column fields follow their columns, reported above
        let declared = |d: &crate::ir::Dataset| -> BTreeMap<String, crate::ir::Field> {
            d.fields
                .iter()
                .map(|f| (f.name.clone(), f.clone()))
                .collect()
        };
        let (was, now) = (declared(before), declared(dataset));
        for (name, field) in &now {
            let subject = format!("{}.{name}", dataset.name);
            match was.get(name) {
                None if matches!(field.source, crate::ir::FieldSource::Derived { .. }) => {
                    plan.push(Op::Add, "field", subject, ChangeClass::Safe);
                }
                Some(old_field) if old_field != field && !column_only_change(old_field, field) => {
                    plan.push(Op::Change, "field", subject, ChangeClass::Safe);
                }
                _ => {}
            }
        }
        for (name, field) in &was {
            if !now.contains_key(name)
                && matches!(field.source, crate::ir::FieldSource::Derived { .. })
            {
                plan.push(
                    Op::Remove,
                    "field",
                    format!("{}.{name}", dataset.name),
                    ChangeClass::Safe,
                );
            }
        }
    }
    for dataset in &old.datasets {
        if new.dataset(dataset.table).is_none() {
            plan.push(Op::Remove, "dataset", &dataset.name, ChangeClass::Safe);
        }
    }
}

/// A field that only differs because its column was renamed: already reported.
fn column_only_change(a: &crate::ir::Field, b: &crate::ir::Field) -> bool {
    matches!((&a.source, &b.source), (crate::ir::FieldSource::Column(x), crate::ir::FieldSource::Column(y)) if x == y)
        && a.is_time == b.is_time
        && a.grains == b.grains
        && a.label == b.label
        && a.description == b.description
}

fn metrics(plan: &mut Plan, old: &Model, new: &Model) {
    for metric in &new.metrics {
        match old.metric(metric.id) {
            None => {
                plan.push(Op::Add, "metric", &metric.name, ChangeClass::Safe);
            }
            Some(before) if before != metric => {
                let what = if before.body == metric.body {
                    "settings"
                } else {
                    "expression"
                };
                plan.push(Op::Change, "metric", &metric.name, ChangeClass::Safe)
                    .detail(what);
            }
            Some(_) => {}
        }
    }
    for metric in &old.metrics {
        if new.metric(metric.id).is_none() {
            plan.push(Op::Remove, "metric", &metric.name, ChangeClass::Safe);
        }
    }
}

fn dashboards(plan: &mut Plan, old: &Model, new: &Model) {
    for dashboard in &new.dashboards {
        match old.dashboards.iter().find(|d| d.id == dashboard.id) {
            None => {
                plan.push(Op::Add, "dashboard", &dashboard.slug, ChangeClass::Safe);
            }
            Some(before) if before != dashboard => {
                plan.push(Op::Change, "dashboard", &dashboard.slug, ChangeClass::Safe);
            }
            Some(_) => {}
        }
    }
    for dashboard in &old.dashboards {
        if !new.dashboards.iter().any(|d| d.id == dashboard.id) {
            plan.push(Op::Remove, "dashboard", &dashboard.slug, ChangeClass::Safe);
        }
    }
}

fn settings(plan: &mut Plan, old: &Model, new: &Model) {
    let (a, b) = (&old.server, &new.server);
    let mut setting = |name: &str, before: String, after: String, restart: bool| {
        if before != after {
            let change = plan.push(Op::Change, "setting", name, ChangeClass::Safe);
            change.detail(format!("{before} → {after}"));
            if restart {
                change.note("takes effect on restart");
            }
        }
    };
    let show = |v: Option<String>| v.unwrap_or_else(|| "default".to_string());
    setting(
        "listen",
        show(a.listen.map(|l| l.to_string())),
        show(b.listen.map(|l| l.to_string())),
        true,
    );
    setting(
        "data_dir",
        show(a.data_dir.clone()),
        show(b.data_dir.clone()),
        true,
    );
    setting(
        "epoch_interval",
        a.epoch_interval.to_string(),
        b.epoch_interval.to_string(),
        false,
    );
    setting(
        "memory_budget",
        a.memory_budget.to_string(),
        b.memory_budget.to_string(),
        false,
    );
    setting(
        "durability",
        format!("{:?}", a.durability).to_lowercase(),
        format!("{:?}", b.durability).to_lowercase(),
        false,
    );
}

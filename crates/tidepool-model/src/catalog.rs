//! Stable identity for everything a model names (plan §8.4).
//!
//! Names are labels; ids are identity. The catalog remembers which id each
//! name had in the previous version of the model, so compiling a new version
//! against it keeps ids for things that kept their name, reuses the old id for
//! things declared `renamed_from`, and gives new things fresh ids. Ids are
//! never reused, so a dropped-and-re-added table is a different table.
//!
//! The catalog is persisted with the data (M5); until then it lives in memory
//! and `tidepool plan` builds it by compiling the base config first.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ir::{ColumnId, DashboardId, MetricId, RelationshipId, TableId};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    next: u32,
    pub tables: BTreeMap<String, TableId>,
    pub columns: BTreeMap<TableId, BTreeMap<String, ColumnId>>,
    pub relationships: BTreeMap<String, RelationshipId>,
    pub metrics: BTreeMap<String, MetricId>,
    pub dashboards: BTreeMap<String, DashboardId>,
}

impl Catalog {
    /// A fresh id. One counter for every kind keeps ids unique across kinds,
    /// which makes logs and test output unambiguous.
    fn fresh(&mut self) -> u32 {
        self.next += 1;
        self.next
    }
}

/// Assigns ids while a new model is compiled: reads the previous catalog,
/// writes the next one.
#[derive(Debug)]
pub struct Assigner<'a> {
    previous: &'a Catalog,
    pub next: Catalog,
}

impl<'a> Assigner<'a> {
    #[must_use]
    pub fn new(previous: &'a Catalog) -> Self {
        Self {
            previous,
            next: Catalog {
                next: previous.next,
                ..Catalog::default()
            },
        }
    }

    /// The id for table `name`, which may have been called `renamed_from`.
    pub fn table(&mut self, name: &str, renamed_from: Option<&str>) -> TableId {
        let id = self
            .previous
            .tables
            .get(name)
            .or_else(|| renamed_from.and_then(|old| self.previous.tables.get(old)))
            .copied()
            .unwrap_or_else(|| TableId(self.next.fresh()));
        self.next.tables.insert(name.to_string(), id);
        id
    }

    /// The id for column `name` of `table`.
    pub fn column(&mut self, table: TableId, name: &str, renamed_from: Option<&str>) -> ColumnId {
        let previous = self.previous.columns.get(&table);
        let id = previous
            .and_then(|columns| {
                columns
                    .get(name)
                    .or_else(|| renamed_from.and_then(|old| columns.get(old)))
            })
            .copied()
            .unwrap_or_else(|| ColumnId(self.next.fresh()));
        self.next
            .columns
            .entry(table)
            .or_default()
            .insert(name.to_string(), id);
        id
    }

    pub fn relationship(&mut self, name: &str) -> RelationshipId {
        let id = self
            .previous
            .relationships
            .get(name)
            .copied()
            .unwrap_or_else(|| RelationshipId(self.next.fresh()));
        self.next.relationships.insert(name.to_string(), id);
        id
    }

    pub fn metric(&mut self, name: &str) -> MetricId {
        let id = self
            .previous
            .metrics
            .get(name)
            .copied()
            .unwrap_or_else(|| MetricId(self.next.fresh()));
        self.next.metrics.insert(name.to_string(), id);
        id
    }

    pub fn dashboard(&mut self, slug: &str) -> DashboardId {
        let id = self
            .previous
            .dashboards
            .get(slug)
            .copied()
            .unwrap_or_else(|| DashboardId(self.next.fresh()));
        self.next.dashboards.insert(slug.to_string(), id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_their_ids_across_versions() {
        let empty = Catalog::default();
        let mut first = Assigner::new(&empty);
        let orders = first.table("orders", None);
        let amount = first.column(orders, "amount", None);
        let v1 = first.next;

        let mut second = Assigner::new(&v1);
        assert_eq!(second.table("orders", None), orders);
        assert_eq!(second.column(orders, "amount", None), amount);
    }

    #[test]
    fn renamed_from_keeps_the_old_id_and_new_names_get_fresh_ones() {
        let empty = Catalog::default();
        let mut first = Assigner::new(&empty);
        let orders = first.table("orders", None);
        let amount = first.column(orders, "amount", None);
        let v1 = first.next;

        let mut second = Assigner::new(&v1);
        let sales = second.table("sales", Some("orders"));
        assert_eq!(sales, orders);
        assert_eq!(second.column(sales, "total", Some("amount")), amount);
        let fresh = second.column(sales, "discount", None);
        assert_ne!(fresh, amount);
    }

    #[test]
    fn ids_are_never_reused() {
        let empty = Catalog::default();
        let mut first = Assigner::new(&empty);
        let old = first.table("orders", None);
        let v1 = first.next;

        // orders dropped, then a table of the same name added in a later version
        let v2 = Assigner::new(&v1).next;
        let mut third = Assigner::new(&v2);
        assert_ne!(third.table("orders", None), old);
    }
}

//! Selections flowing both ways (plan §5.4), for models with
//! `filter_direction: both`.
//!
//! The one-directional engine already applies a selection to every table
//! that reaches the selected field through many→one relationships. What
//! flowing both ways adds is the other direction: a selection on orders
//! narrows customers to those with such orders, and through them every
//! other table related to customers. That is a *message* along a
//! relationship from its many side to its one side: the keys present in the
//! many side's rows that pass whatever is selected on that side. It is
//! handed to the one-directional engine as one more condition, on the one
//! side's key field ([`Condition::Set`]), and from there flows to everything
//! that reaches that key, as any selection does.
//!
//! Relationships form a tree (the compiler refuses loops under
//! `filter_direction: both`), so this is exact message passing: a side of
//! the tree only sends a message when something on it is selected (a
//! customer without orders stays possible when only a region is selected,
//! as in Qlik), and the message into a table never includes what came from
//! the table itself. Relationships are single-column.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use tidepool_model::ir::{Direction, FieldRef, Model, RelationshipId, TableId};
use tidepool_store::Snapshot;

use crate::cache::Caches;
use crate::query::{Condition, QueryError, Selection, ValueSet};

/// One relationship, as a tree edge.
struct Edge {
    id: RelationshipId,
    many: TableId,
    one: TableId,
    /// The many side's foreign key, and the one side's key, as fields.
    fk: FieldRef,
    key: FieldRef,
}

fn edges(model: &Model) -> Vec<Edge> {
    let field = |table: TableId, column| {
        let name = model
            .table(table)
            .and_then(|t| t.column(column))
            .map(|c| c.name.clone())?;
        Some(FieldRef { table, field: name })
    };
    model
        .relationships
        .iter()
        .filter_map(|r| {
            let ([fk], [key]) = (r.from_columns.as_slice(), r.to_columns.as_slice()) else {
                return None;
            };
            Some(Edge {
                id: r.id,
                many: r.from,
                one: r.to,
                fk: field(r.from, *fk)?,
                key: field(r.to, *key)?,
            })
        })
        .collect()
}

/// The tables on `table`'s side of the tree once `cut` is removed.
fn side(edges: &[Edge], table: TableId, cut: Option<RelationshipId>) -> BTreeSet<TableId> {
    let mut seen = BTreeSet::from([table]);
    let mut queue = VecDeque::from([table]);
    while let Some(t) = queue.pop_front() {
        for e in edges.iter().filter(|e| Some(e.id) != cut) {
            let next = if e.many == t {
                e.one
            } else if e.one == t {
                e.many
            } else {
                continue;
            };
            if seen.insert(next) {
                queue.push_back(next);
            }
        }
    }
    seen
}

/// Tables `from` reaches through many→one relationships (not `cut`), each
/// with the relationship it was reached by; `from` itself first.
fn reached(
    edges: &[Edge],
    from: TableId,
    cut: Option<RelationshipId>,
) -> Vec<(TableId, Option<RelationshipId>)> {
    let mut out = vec![(from, None)];
    let mut i = 0;
    while i < out.len() {
        let t = out[i].0;
        for e in edges.iter().filter(|e| e.many == t && Some(e.id) != cut) {
            if !out.iter().any(|(x, _)| *x == e.one) {
                out.push((e.one, Some(e.id)));
            }
        }
        i += 1;
    }
    out
}

/// Works out messages for one selection, remembering them per epoch.
pub(crate) struct Associations<'a> {
    snapshot: &'a Snapshot,
    caches: Option<&'a Caches>,
    edges: Vec<Edge>,
}

impl<'a> Associations<'a> {
    pub(crate) fn new(snapshot: &'a Snapshot, caches: Option<&'a Caches>) -> Self {
        Self {
            snapshot,
            caches,
            edges: edges(&snapshot.model),
        }
    }

    /// The selection a query anchored on `anchor` sees, given the part of
    /// the user's selection it doesn't ignore (`visible`): that selection,
    /// plus what selections on the other side of each relationship leave of
    /// the keys of the tables it reaches.
    pub(crate) fn expand(
        &self,
        anchor: TableId,
        visible: &Selection,
    ) -> Result<Selection, QueryError> {
        if self.snapshot.model.direction != Direction::Both || visible.0.is_empty() {
            return Ok(visible.clone());
        }
        let mut out = visible.clone();
        for (field, set) in self.restrictions(anchor, None, visible)? {
            add(&mut out, field, &set);
        }
        Ok(out)
    }

    /// The key conditions for a query on `anchor` (on its side of `cut`):
    /// for every table it reaches through many→one relationships, the
    /// messages from the relationships into that table other than the one
    /// it was reached by.
    fn restrictions(
        &self,
        anchor: TableId,
        cut: Option<RelationshipId>,
        visible: &Selection,
    ) -> Result<Vec<(FieldRef, Arc<ValueSet>)>, QueryError> {
        let selected: BTreeSet<TableId> = visible.0.keys().map(|f| f.table).collect();
        let mut out = Vec::new();
        for (table, by) in reached(&self.edges, anchor, cut) {
            for edge in self
                .edges
                .iter()
                .filter(|e| e.one == table && Some(e.id) != by && Some(e.id) != cut)
            {
                // a side with nothing selected says nothing
                if side(&self.edges, edge.many, Some(edge.id)).is_disjoint(&selected) {
                    continue;
                }
                out.push((edge.key.clone(), self.message(edge, visible)?));
            }
        }
        Ok(out)
    }

    /// The keys present in the passing rows of `edge`'s many side, with
    /// what's selected on that side of it.
    fn message(&self, edge: &Edge, visible: &Selection) -> Result<Arc<ValueSet>, QueryError> {
        let on_side = side(&self.edges, edge.many, Some(edge.id));
        let mut selection = Selection(
            visible
                .0
                .iter()
                .filter(|(f, _)| on_side.contains(&f.table))
                .map(|(f, c)| (f.clone(), c.clone()))
                .collect(),
        );
        let key = format!("{} {:?}", edge.id, selection);
        if let Some(known) = self
            .caches
            .and_then(|c| c.message(self.snapshot.epoch, &key))
        {
            return Ok(known);
        }
        for (field, set) in self.restrictions(edge.many, Some(edge.id), visible)? {
            add(&mut selection, field, &set);
        }
        let values = crate::raw_values(self.snapshot, &edge.fk, &selection, self.caches)?;
        // a NULL foreign key relates to nothing
        let set = Arc::new(ValueSet::new(
            values.into_iter().map(|(v, _)| v).filter(|v| !v.is_null()),
        ));
        if let Some(caches) = self.caches {
            caches.keep_message(self.snapshot.epoch, key, Arc::clone(&set));
        }
        Ok(set)
    }
}

/// `field` must also be in `set`: added to `selection`, or combined with
/// what it holds for `field` already (both must hold).
pub(crate) fn add(selection: &mut Selection, field: FieldRef, set: &Arc<ValueSet>) {
    let combined = match selection.0.get(&field) {
        None => Condition::Set(Arc::clone(set)),
        Some(Condition::Set(other)) => Condition::Set(Arc::new(ValueSet::new(
            other.values().iter().filter(|v| set.contains(v)).cloned(),
        ))),
        Some(condition) => Condition::Set(Arc::new(ValueSet::new(
            set.values()
                .iter()
                .filter(|v| condition.matches(v))
                .cloned(),
        ))),
    };
    selection.0.insert(field, combined);
}

//! Parent–child hierarchies (plan §5.6).
//!
//! A hierarchy field's values are node keys, and selecting a node selects it
//! and everything below it. The selection keeps the keys a viewer picked;
//! [`resolve`] turns them into the keys of their subtrees against the
//! snapshot a query runs on, as a [`Condition::Set`] on the key field. So a
//! node moved under another one is filtered with its new parent from the
//! next epoch, without anyone selecting anything again, and neither
//! executor knows hierarchies exist.
//!
//! The tree is built from the table's live rows: nodes in depth-first
//! order, so a subtree is a range ([`Hierarchy::subtree`]). A row whose
//! parent is NULL, isn't there, or is itself is a root (an orphan, when it
//! names a parent). Parents that go round in a cycle have no root above
//! them: the node with the smallest key of each cycle becomes one. Children
//! are ordered by label, then key.
//!
//! Grouping by the ancestor at some depth (a filter's measure per tree
//! level, where a ratio can't be added up from the level below) is a
//! derived dictionary over the key column ([`Hierarchy::ancestors`]): key
//! code → its ancestor's key code. It is built from the tree, never kept in
//! the engine's derived dictionaries, whose values never change once a code
//! has one: a move changes them here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tidepool_expr::Scalar;
use tidepool_model::ir::{FieldRef, Hierarchy as Columns, Model, TableId};
use tidepool_store::Snapshot;
use tidepool_store::column::Raw;
use tidepool_store::dict::DictSnapshot;

use crate::cache::Caches;
use crate::derived::DerivedDict;
use crate::exec::{Key, key, scalar};
use crate::query::{Condition, QueryError, Selection, ValueSet};

const NONE: u32 = u32::MAX;

/// One node: a live row of the hierarchy's table.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub key: Scalar,
    /// The key's dictionary code.
    pub code: u32,
    /// What it's shown as: the label column's value, or the key.
    pub label: Scalar,
    pub kind: Scalar,
    /// The parent's index in [`Hierarchy::nodes`]; `None` for a root.
    pub parent: Option<u32>,
    /// 0 for a root.
    pub depth: u32,
}

/// A table's rows as a tree.
pub struct Hierarchy {
    /// Depth-first, roots and children in order.
    nodes: Vec<Node>,
    /// Where each node's subtree ends: it is `i..end[i]`.
    end: Vec<u32>,
    /// Key code → node, `NONE` for a code that isn't one.
    by_code: Vec<u32>,
    by_key: HashMap<Key, u32>,
    /// Rows naming a parent that isn't there (or themselves).
    pub orphans: usize,
    /// Cycles broken by making a node a root.
    pub cycles: usize,
    /// Per depth: key code → ancestor's key code (see the module docs).
    ancestors: Mutex<HashMap<u32, Arc<DerivedDict>>>,
}

/// One live row, before the tree is made.
struct Row {
    code: u32,
    parent: Option<u32>,
    key: Scalar,
    label: Scalar,
    kind: Scalar,
}

impl Hierarchy {
    /// The tree of `table`'s live rows. `None` if the table or its columns
    /// aren't in the snapshot.
    #[must_use]
    pub fn build(snapshot: &Snapshot, table: TableId, columns: &Columns) -> Option<Self> {
        let spec = snapshot.model.table(table)?;
        let stored = snapshot.table(table)?;
        let position = |id| spec.columns.iter().position(|c| c.id == id);
        let key = position(columns.key)?;
        let parent = position(columns.parent)?;
        let label = columns.label.and_then(position);
        let kind = columns.kind.and_then(position);

        // the last live row of each key wins (an upsert table has one)
        let mut rows: HashMap<u32, Row> = HashMap::new();
        for segment in stored.segments.iter() {
            let data = &segment.data;
            for offset in 0..data.rows as usize {
                if segment
                    .deletes
                    .contains(u32::try_from(offset).unwrap_or(u32::MAX))
                {
                    continue;
                }
                let Raw::Code(code) = data.columns[key].get(offset) else {
                    continue;
                };
                let value = |column: usize| {
                    scalar(
                        snapshot.decode(&stored.layouts[column], data.columns[column].get(offset)),
                    )
                };
                let key_value = value(key);
                rows.insert(
                    code,
                    Row {
                        code,
                        parent: match data.columns[parent].get(offset) {
                            Raw::Code(p) => Some(p),
                            _ => None,
                        },
                        label: label.map_or_else(|| key_value.clone(), value),
                        kind: kind.map_or(Scalar::Null, value),
                        key: key_value,
                    },
                );
            }
        }
        let mut rows: Vec<Row> = rows.into_values().collect();
        rows.sort_by(|a, b| a.key.sort_cmp(&b.key));
        Some(Self::from_rows(&rows))
    }

    fn from_rows(rows: &[Row]) -> Self {
        let (mut up, orphans) = parents(rows);
        let cycles = break_cycles(&mut up);

        let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
        let mut roots = Vec::new();
        for (i, parent) in up.iter().enumerate() {
            match parent {
                Some(p) => children[*p].push(i),
                None => roots.push(i),
            }
        }
        let order = |a: &usize, b: &usize| {
            rows[*a]
                .label
                .sort_cmp(&rows[*b].label)
                .then_with(|| rows[*a].key.sort_cmp(&rows[*b].key))
        };
        roots.sort_by(order);
        for list in &mut children {
            list.sort_by(order);
        }

        // depth first, keeping where each subtree ends
        let mut nodes = Vec::with_capacity(rows.len());
        // by node index: every row becomes exactly one node
        let mut end = vec![0u32; rows.len()];
        let mut placed = vec![NONE; rows.len()];
        let mut stack: Vec<(usize, Option<u32>, u32, bool)> =
            roots.iter().rev().map(|r| (*r, None, 0, false)).collect();
        while let Some((row, parent, depth, done)) = stack.pop() {
            if done {
                end[placed[row] as usize] = u32::try_from(nodes.len()).unwrap_or(NONE);
                continue;
            }
            let at = u32::try_from(nodes.len()).unwrap_or(NONE);
            placed[row] = at;
            let r = &rows[row];
            nodes.push(Node {
                key: r.key.clone(),
                code: r.code,
                label: r.label.clone(),
                kind: r.kind.clone(),
                parent,
                depth,
            });
            stack.push((row, parent, depth, true));
            for child in children[row].iter().rev() {
                stack.push((*child, Some(at), depth + 1, false));
            }
        }
        let size = nodes.iter().map(|n| n.code as usize + 1).max().unwrap_or(0);
        let mut by_code = vec![NONE; size];
        let mut by_key = HashMap::new();
        for (i, node) in nodes.iter().enumerate() {
            let i = u32::try_from(i).unwrap_or(NONE);
            by_code[node.code as usize] = i;
            by_key.insert(key(&node.key), i);
        }
        Self {
            nodes,
            end,
            by_code,
            by_key,
            orphans,
            cycles,
            ancestors: Mutex::new(HashMap::new()),
        }
    }

    /// Every node, depth first.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Node `i` and everything below it, as indexes into [`Self::nodes`].
    #[must_use]
    pub fn subtree(&self, i: usize) -> std::ops::Range<usize> {
        i..self.end.get(i).map_or(i, |e| *e as usize)
    }

    /// The node with this key.
    #[must_use]
    pub fn find(&self, value: &Scalar) -> Option<usize> {
        self.by_key.get(&key(value)).map(|i| *i as usize)
    }

    /// The node of a key code.
    #[must_use]
    pub fn of_code(&self, code: u32) -> Option<usize> {
        self.by_code
            .get(code as usize)
            .filter(|i| **i != NONE)
            .map(|i| *i as usize)
    }

    /// The deepest depth there is, plus one: 0 for an empty tree.
    #[must_use]
    pub fn levels(&self) -> u32 {
        self.nodes.iter().map(|n| n.depth + 1).max().unwrap_or(0)
    }

    /// Node `i`'s ancestor at `depth` (itself at its own depth); `None`
    /// when it isn't that deep.
    #[must_use]
    pub fn ancestor(&self, mut i: usize, depth: u32) -> Option<usize> {
        let node = self.nodes.get(i)?;
        if node.depth < depth {
            return None;
        }
        for _ in depth..node.depth {
            i = self.nodes[i].parent? as usize;
        }
        Some(i)
    }

    /// The keys of every node matching `condition`, and of everything below
    /// them.
    #[must_use]
    pub fn below(&self, condition: &Condition) -> ValueSet {
        let mut keys = Vec::new();
        let mut i = 0;
        while i < self.nodes.len() {
            if condition.matches(&self.nodes[i].key) {
                let range = self.subtree(i);
                keys.extend(self.nodes[range.clone()].iter().map(|n| n.key.clone()));
                i = range.end;
            } else {
                i += 1;
            }
        }
        ValueSet::new(keys)
    }

    /// Key code → the key code of its node's ancestor at `depth` (0 when
    /// it isn't a node or isn't that deep), as a derived dictionary over
    /// `dict` whose values are keys.
    pub(crate) fn ancestors(&self, depth: u32, dict: &DictSnapshot) -> Arc<DerivedDict> {
        let mut kept = self
            .ancestors
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(known) = kept.get(&depth)
            && known.covers(dict)
        {
            return Arc::clone(known);
        }
        let codes = dict.len() as usize + 1;
        let mut map = vec![0u32; codes];
        let mut values = vec![Scalar::Null; codes];
        for node in &self.nodes {
            if let Some(slot) = values.get_mut(node.code as usize) {
                *slot = node.key.clone();
            }
        }
        for (code, slot) in map.iter_mut().enumerate() {
            let found = u32::try_from(code)
                .ok()
                .and_then(|c| self.of_code(c))
                .and_then(|i| self.ancestor(i, depth));
            if let Some(a) = found {
                *slot = self.nodes[a].code;
            }
        }
        let built = Arc::new(DerivedDict::from_parts(map, values));
        kept.insert(depth, Arc::clone(&built));
        built
    }
}

/// Each row's parent row, if it's there and isn't the row itself, and how
/// many rows name a parent that isn't.
fn parents(rows: &[Row]) -> (Vec<Option<usize>>, usize) {
    let index: HashMap<u32, usize> = rows.iter().enumerate().map(|(i, r)| (r.code, i)).collect();
    let mut orphans = 0;
    let up = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let found = row
                .parent
                .and_then(|p| index.get(&p).copied())
                .filter(|p| *p != i);
            if row.parent.is_some() && found.is_none() {
                orphans += 1;
            }
            found
        })
        .collect();
    (up, orphans)
}

/// Rows that don't lead up to a root are on (or under) a cycle: the
/// smallest of each cycle (rows are in key order) becomes a root. How many
/// cycles there were.
fn break_cycles(up: &mut [Option<usize>]) -> usize {
    let mut cycles = 0;
    let mut state = vec![0u8; up.len()]; // 0 unknown, 1 on this walk, 2 leads to a root
    for start in 0..up.len() {
        let mut walk = Vec::new();
        let mut at = Some(start);
        while let Some(i) = at {
            match state[i] {
                2 => break,
                1 => {
                    // a cycle: from `i`'s first place in this walk on
                    let from = walk.iter().position(|w| *w == i).unwrap_or(0);
                    if let Some(smallest) = walk[from..].iter().copied().min() {
                        up[smallest] = None;
                        cycles += 1;
                    }
                    break;
                }
                _ => {
                    state[i] = 1;
                    walk.push(i);
                    at = up[i];
                }
            }
        }
        for w in walk {
            state[w] = 2;
        }
    }
    cycles
}

/// The hierarchy of a field, if it is one.
#[must_use]
pub fn columns(model: &Model, field: &FieldRef) -> Option<Columns> {
    model
        .dataset(field.table)
        .and_then(|d| d.field(&field.field))
        .and_then(tidepool_model::ir::Field::hierarchy)
        .copied()
}

/// The tree of `table` in `snapshot`, kept while the table is unchanged.
pub(crate) fn get(
    snapshot: &Snapshot,
    caches: Option<&Caches>,
    table: TableId,
    columns: &Columns,
) -> Result<Arc<Hierarchy>, QueryError> {
    let missing = || QueryError("the hierarchy's table isn't in the snapshot".into());
    match (caches, snapshot.tables.get(&table)) {
        (Some(caches), Some(stored)) => caches
            .hierarchy(stored, columns, || {
                Hierarchy::build(snapshot, table, columns)
            })
            .ok_or_else(missing),
        _ => Hierarchy::build(snapshot, table, columns)
            .map(Arc::new)
            .ok_or_else(missing),
    }
}

/// `selection` with every condition on a hierarchy field replaced by the
/// subtrees it selects, as a condition on the key (combined with any the
/// key has already).
///
/// # Errors
/// A hierarchy's table isn't in the snapshot.
pub(crate) fn resolve(
    snapshot: &Snapshot,
    caches: Option<&Caches>,
    selection: &Selection,
) -> Result<Selection, QueryError> {
    let model = &snapshot.model;
    let trees: Vec<(&FieldRef, &Condition, Columns)> = selection
        .0
        .iter()
        .filter_map(|(f, c)| columns(model, f).map(|h| (f, c, h)))
        .collect();
    if trees.is_empty() {
        return Ok(selection.clone());
    }
    let mut out = selection.clone();
    for (field, condition, columns) in trees {
        out.0.remove(field);
        let tree = get(snapshot, caches, field.table, &columns)?;
        let Some(name) = model
            .table(field.table)
            .and_then(|t| t.column(columns.key))
            .map(|c| c.name.clone())
        else {
            continue;
        };
        let key_field = FieldRef {
            table: field.table,
            field: name,
        };
        crate::assoc::add(&mut out, key_field, &Arc::new(tree.below(condition)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Scalar {
        Scalar::String(text.into())
    }

    /// Rows of (code, parent code); keys are `k<code>`, labels the keys.
    fn tree(rows: &[(u32, Option<u32>)]) -> Hierarchy {
        let mut rows: Vec<Row> = rows
            .iter()
            .map(|(code, parent)| Row {
                code: *code,
                parent: *parent,
                key: s(&format!("k{code}")),
                label: s(&format!("k{code}")),
                kind: Scalar::Null,
            })
            .collect();
        rows.sort_by(|a, b| a.key.sort_cmp(&b.key));
        Hierarchy::from_rows(&rows)
    }

    fn shape(h: &Hierarchy) -> Vec<String> {
        h.nodes()
            .iter()
            .enumerate()
            .map(|(i, n)| {
                format!(
                    "{}{} ({})",
                    "  ".repeat(n.depth as usize),
                    match &n.key {
                        Scalar::String(k) => k.clone(),
                        other => format!("{other:?}"),
                    },
                    h.subtree(i).len()
                )
            })
            .collect()
    }

    #[test]
    fn nodes_are_depth_first_with_their_subtrees() {
        // 1 ─┬─ 2 ── 4
        //    └─ 3
        // 5
        let h = tree(&[
            (4, Some(2)),
            (1, None),
            (3, Some(1)),
            (2, Some(1)),
            (5, None),
        ]);
        assert_eq!(
            shape(&h),
            ["k1 (4)", "  k2 (2)", "    k4 (1)", "  k3 (1)", "k5 (1)"]
        );
        assert_eq!((h.orphans, h.cycles), (0, 0));
        assert_eq!(h.levels(), 3);
        let Some(four) = h.find(&s("k4")) else {
            panic!("k4 is a node");
        };
        assert_eq!(h.ancestor(four, 0), h.find(&s("k1")));
        assert_eq!(h.ancestor(four, 1), h.find(&s("k2")));
        assert_eq!(h.ancestor(four, 2), Some(four));
        assert_eq!(h.ancestor(four, 3), None);
    }

    #[test]
    fn a_subtree_selection_includes_the_node_and_everything_below() {
        let h = tree(&[
            (1, None),
            (2, Some(1)),
            (3, Some(1)),
            (4, Some(2)),
            (5, None),
        ]);
        let set = h.below(&Condition::In(vec![s("k2"), s("k5")]));
        let mut keys: Vec<String> = set.values().iter().map(|v| format!("{v:?}")).collect();
        keys.sort();
        assert_eq!(
            keys,
            [r#"String("k2")"#, r#"String("k4")"#, r#"String("k5")"#]
        );
        // a node inside a selected subtree adds nothing
        assert_eq!(h.below(&Condition::In(vec![s("k1"), s("k4")])).len(), 4);
    }

    #[test]
    fn missing_and_own_parents_make_orphan_roots() {
        let h = tree(&[(1, Some(9)), (2, Some(2)), (3, Some(1))]);
        assert_eq!(shape(&h), ["k1 (2)", "  k3 (1)", "k2 (1)"]);
        assert_eq!((h.orphans, h.cycles), (2, 0));
    }

    #[test]
    fn a_cycle_is_broken_at_its_smallest_key() {
        // 3 → 2 → 4 → 3 is a cycle, 5 hangs under it, 1 is a root
        let h = tree(&[
            (1, None),
            (3, Some(4)),
            (2, Some(3)),
            (4, Some(2)),
            (5, Some(4)),
        ]);
        assert_eq!(h.cycles, 1);
        assert_eq!(
            shape(&h),
            ["k1 (1)", "k2 (4)", "  k4 (3)", "    k3 (1)", "    k5 (1)"]
        );
        // two separate cycles, one root each
        let h = tree(&[(1, Some(2)), (2, Some(1)), (3, Some(4)), (4, Some(3))]);
        assert_eq!(h.cycles, 2);
        assert_eq!(shape(&h), ["k1 (2)", "  k2 (1)", "k3 (2)", "  k4 (1)"]);
    }
}

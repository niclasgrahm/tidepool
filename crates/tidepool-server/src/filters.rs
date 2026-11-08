//! A dashboard's filters, as a session sees them (plan §5.3, §5.6): each
//! value's or tree node's state under the selection, its rows or measure,
//! and the window of them the viewer has scrolled or searched to.
//!
//! Qlik's rule throughout: a field's own selection doesn't narrow its
//! alternatives, so states, rows and measures are computed under every
//! *other* field's selection.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value as Json;
use tidepool_engine::hierarchy::Hierarchy;
use tidepool_engine::{Condition, Dim, Query, QueryError, Selection};
use tidepool_expr::Scalar;
use tidepool_model::ir::{FieldRef, Filter};
use tidepool_protocol::live::{ChosenNode, FilterFrame, FilterValue, TreeNode, ValueState};
use tidepool_store::Snapshot;

use crate::AppState;
use crate::query::json;

/// One filter's window: where a list is scrolled to, and what it's
/// searched for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Window {
    pub offset: usize,
    pub limit: usize,
    pub search: Option<String>,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 100,
            search: None,
        }
    }
}

/// How a value is told apart in maps and sets: the same in every frame.
pub(crate) fn token(value: &Scalar) -> String {
    format!("{value:?}")
}

/// A list filter's window.
///
/// # Errors
/// The field or its measure can't be computed.
pub(crate) fn list(
    state: &AppState,
    snapshot: &Snapshot,
    filter: &Filter,
    name: String,
    selection: &Selection,
    window: &Window,
) -> Result<FilterFrame, QueryError> {
    let at = &filter.field;
    let all = state.engine.values(snapshot, at, &Selection::default())?;
    let (others, own) = without(selection, at);
    let possible = state.engine.values(snapshot, at, &others)?;
    let rows: HashMap<String, i64> = possible.iter().map(|(v, n)| (token(v), *n)).collect();
    let measures = measures(state, snapshot, filter, &others, None)?;
    let selected: BTreeSet<String> = match &own {
        Some(Condition::In(values)) => values.iter().map(token).collect(),
        _ => BTreeSet::new(),
    };
    let in_range = |value: &Scalar| match &own {
        Some(condition @ Condition::Range { .. }) => condition.matches(value),
        _ => false,
    };
    let search = window.search.as_ref().map(|s| s.to_lowercase());
    let values: Vec<FilterValue> = all
        .iter()
        .map(|(value, _)| {
            let key = token(value);
            let state = if selected.contains(&key) || in_range(value) {
                ValueState::Selected
            } else if rows.contains_key(&key) {
                ValueState::Possible
            } else {
                ValueState::Excluded
            };
            FilterValue {
                value: json(value),
                state,
                rows: rows.get(&key).copied().unwrap_or(0),
                measure: measures
                    .as_ref()
                    .map(|m| m.get(&key).cloned().unwrap_or(Json::Null)),
            }
        })
        .filter(|v| matches(search.as_deref(), &v.value))
        .collect();
    Ok(FilterFrame {
        field: name,
        total: values.len(),
        offset: window.offset,
        values: values
            .into_iter()
            .skip(window.offset)
            .take(window.limit)
            .collect(),
        nodes: Vec::new(),
        chosen: Vec::new(),
    })
}

/// A tree filter's window: the visible nodes, depth first. A node's
/// children are visible when it is `open`; while searching, every match
/// and the nodes above it are.
///
/// # Errors
/// The field isn't a hierarchy, or its measure can't be computed.
pub(crate) fn tree(
    state: &AppState,
    snapshot: &Snapshot,
    filter: &Filter,
    name: String,
    selection: &Selection,
    window: &Window,
    open: &BTreeSet<String>,
) -> Result<FilterFrame, QueryError> {
    let at = &filter.field;
    let tree = state
        .engine
        .hierarchy(snapshot, at)?
        .ok_or_else(|| QueryError(format!("`{name}` isn't a hierarchy")))?;
    let nodes = tree.nodes();
    let (others, own) = without(selection, at);

    // the nodes that pass the other fields' selections, rolled up: a node
    // is possible when it or anything below it passes
    let passing: BTreeSet<String> = state
        .engine
        .values(snapshot, at, &others)?
        .iter()
        .map(|(v, _)| token(v))
        .collect();
    let mut rows: Vec<i64> = nodes
        .iter()
        .map(|n| i64::from(passing.contains(&token(&n.key))))
        .collect();
    let selected: Vec<bool> = nodes
        .iter()
        .map(|n| own.as_ref().is_some_and(|c| c.matches(&n.key)))
        .collect();
    let mut holds = vec![false; nodes.len()];
    for i in (0..nodes.len()).rev() {
        if let Some(p) = nodes[i].parent {
            let p = p as usize;
            rows[p] += rows[i];
            holds[p] |= selected[i] || holds[i];
        }
    }
    let mut inside = vec![false; nodes.len()];
    for i in 0..nodes.len() {
        if let Some(p) = nodes[i].parent {
            let p = p as usize;
            inside[i] = selected[p] || inside[p];
        }
    }

    let search = window.search.as_ref().map(|s| s.to_lowercase());
    let visible = visible(&tree, open, search.as_deref());
    let shown: Vec<usize> = visible
        .iter()
        .copied()
        .skip(window.offset)
        .take(window.limit)
        .collect();

    // the measure per depth on screen: a ratio isn't the sum of the level
    // below, so each level is grouped by itself
    let mut measure: HashMap<String, Json> = HashMap::new();
    let depths: BTreeSet<u32> = shown.iter().map(|i| nodes[*i].depth).collect();
    let mut measured = false;
    for depth in depths {
        if let Some(values) = measures(state, snapshot, filter, &others, Some(depth))? {
            measured = true;
            measure.extend(values);
        }
    }

    let tree_nodes = shown
        .iter()
        .map(|&i| {
            let node = &nodes[i];
            let key = token(&node.key);
            TreeNode {
                key: json(&node.key),
                label: json(&node.label),
                kind: (!node.kind.is_null()).then(|| json(&node.kind)),
                depth: node.depth,
                has_children: tree.subtree(i).len() > 1,
                open: search.is_some() || open.contains(&key),
                state: if selected[i] {
                    ValueState::Selected
                } else if rows[i] > 0 {
                    ValueState::Possible
                } else {
                    ValueState::Excluded
                },
                inside: inside[i],
                holds: holds[i],
                rows: rows[i],
                measure: measured.then(|| measure.get(&key).cloned().unwrap_or(Json::Null)),
            }
        })
        .collect();
    Ok(FilterFrame {
        field: name,
        total: visible.len(),
        offset: window.offset,
        values: Vec::new(),
        nodes: tree_nodes,
        chosen: chosen(&tree, own.as_ref()),
    })
}

/// The visible nodes, depth first: everything under open nodes, or every
/// match of `search` and the nodes above it.
fn visible(tree: &Hierarchy, open: &BTreeSet<String>, search: Option<&str>) -> Vec<usize> {
    let nodes = tree.nodes();
    let Some(search) = search else {
        let mut out = Vec::new();
        let mut i = 0;
        while i < nodes.len() {
            out.push(i);
            i = if open.contains(&token(&nodes[i].key)) {
                i + 1
            } else {
                tree.subtree(i).end
            };
        }
        return out;
    };
    let mut hit: Vec<bool> = nodes
        .iter()
        .map(|n| matches(Some(search), &json(&n.label)) || matches(Some(search), &json(&n.key)))
        .collect();
    for i in (0..nodes.len()).rev() {
        if hit[i]
            && let Some(p) = nodes[i].parent
        {
            hit[p as usize] = true;
        }
    }
    (0..nodes.len()).filter(|i| hit[*i]).collect()
}

/// The selected nodes, with the labels from their root down to them.
fn chosen(tree: &Hierarchy, own: Option<&Condition>) -> Vec<ChosenNode> {
    let Some(Condition::In(values)) = own else {
        return Vec::new();
    };
    values
        .iter()
        .map(|value| {
            let mut path = Vec::new();
            let mut at = tree.find(value);
            while let Some(i) = at {
                let node = &tree.nodes()[i];
                path.push(json(&node.label));
                at = node.parent.map(|p| p as usize);
            }
            if path.is_empty() {
                path.push(json(value));
            }
            path.reverse();
            ChosenNode {
                key: json(value),
                path,
            }
        })
        .collect()
}

/// `selection` without `at`'s own condition, and that condition.
fn without(selection: &Selection, at: &FieldRef) -> (Selection, Option<Condition>) {
    let mut others = selection.clone();
    let own = others.0.remove(at);
    (others, own)
}

/// Whether a value's text contains `search` (lowercase), or there's none.
fn matches(search: Option<&str>, value: &Json) -> bool {
    search.is_none_or(|s| {
        let text = match value {
            Json::String(text) => text.to_lowercase(),
            other => other.to_string(),
        };
        text.contains(s)
    })
}

/// The filter's measure per value (by [`token`]) under `others`, grouped
/// by the field or, for a tree, by each node's ancestor at `level`.
/// `None` without a measure.
fn measures(
    state: &AppState,
    snapshot: &Snapshot,
    filter: &Filter,
    others: &Selection,
    level: Option<u32>,
) -> Result<Option<HashMap<String, Json>>, QueryError> {
    let Some(metric) = filter.measure else {
        return Ok(None);
    };
    let query = Query {
        dimensions: vec![Dim {
            level,
            ..Dim::of(filter.field.clone())
        }],
        metrics: vec![metric],
        selection: others.clone(),
        now: Some(state.clock.now()),
        ..Query::default()
    };
    // only what can change the answer: sessions that differ elsewhere
    // share it
    let relevant = tidepool_engine::relevant(snapshot, &query)?;
    let answer = state.engine.run(
        snapshot,
        &Query {
            selection: relevant,
            ..query
        },
    )?;
    Ok(Some(
        answer
            .rows
            .iter()
            .filter_map(|row| match row.as_slice() {
                // a level's NULL group is the nodes not that deep: no node's;
                // a list's NULL is one of its values
                [value, _] if value.is_null() && level.is_some() => None,
                [value, measure] => Some((token(value), json(measure))),
                _ => None,
            })
            .collect(),
    ))
}

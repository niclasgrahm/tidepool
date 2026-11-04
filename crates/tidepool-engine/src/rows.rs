//! The newest rows of a table that pass a selection (plan §5.7): what a
//! `rows` visual shows, a log tail for instance.
//!
//! Rows come newest first, in the order they arrived (row ids never change,
//! so that order is the table's own). The anchor's conditions run the fast
//! way when they can (text searches included) morsel by morsel from the
//! newest, a few morsels per thread at a time, until the limit is reached;
//! only the rows kept are decoded. With an [`crate::Engine`] the answer is
//! kept, and the next epoch scans only the rows that arrived since.

use std::ops::Range;
use std::sync::Arc;

use rayon::prelude::*;
use tidepool_expr::Scalar;
use tidepool_model::ir::FieldRef;
use tidepool_store::Snapshot;
use tidepool_store::store::TableSnapshot;

use crate::cache::Caches;
use crate::derived::DerivedDict;
use crate::exec;
use crate::fast::{self, MORSEL, Map, Prepared, Scratch, Task};
use crate::plan::{self, AnchorPlan};
use crate::query::{QueryError, ResultColumn, ResultSet, Role, Selection};

/// Which rows to show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowsQuery {
    /// The columns: fields of the table whose rows these are, or of
    /// tables it reaches.
    pub fields: Vec<FieldRef>,
    pub selection: Selection,
    pub limit: usize,
}

/// Rows at most, whatever a query asks for.
pub const MAX_ROWS: usize = 1000;

/// An answer kept to be brought up to date: the rows found below a
/// high-water mark, and whether they are every row there that passes.
pub(crate) struct Kept {
    epoch: u64,
    /// Rows below this row id were scanned.
    high: u64,
    /// Row ids, newest first.
    rows: Vec<u64>,
    /// The scan reached the oldest row: no passing row below `high` is
    /// left out.
    exhausted: bool,
    maps: Vec<Option<Arc<Map>>>,
    derived: Vec<Option<Arc<DerivedDict>>>,
}

impl Kept {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// The newest rows passing `query`'s selection.
///
/// # Errors
/// No table reaches every field, or a field doesn't exist.
pub fn rows(snapshot: &Snapshot, query: &RowsQuery) -> Result<ResultSet, QueryError> {
    answer(snapshot, query, true, None)
}

/// [`rows`] without the fast path.
///
/// # Errors
/// As [`rows`].
pub fn rows_general(snapshot: &Snapshot, query: &RowsQuery) -> Result<ResultSet, QueryError> {
    answer(snapshot, query, false, None)
}

/// The part of `query`'s selection that can change its rows: conditions
/// on fields its table reaches (all of them, when selections flow both
/// ways). Sessions whose selections differ only elsewhere share the rows.
///
/// # Errors
/// As [`rows`].
pub fn relevant(snapshot: &Snapshot, query: &RowsQuery) -> Result<Selection, QueryError> {
    let model = &snapshot.model;
    if model.direction == tidepool_model::ir::Direction::Both {
        return Ok(query.selection.clone());
    }
    let tables: Vec<_> = query.fields.iter().map(|f| f.table).collect();
    let anchor = plan::rows_anchor(model, &tables)?;
    Ok(Selection(
        query
            .selection
            .0
            .iter()
            .filter(|(field, _)| plan::reaches(model, anchor, field.table))
            .map(|(f, c)| (f.clone(), c.clone()))
            .collect(),
    ))
}

/// Where row ids are: (first row id, segment) by start, oldest first.
fn starts(table: &TableSnapshot) -> Vec<(u64, usize)> {
    let mut starts: Vec<(u64, usize)> = table
        .segments
        .iter()
        .enumerate()
        .map(|(s, view)| (view.data.start, s))
        .collect();
    starts.sort_unstable();
    starts
}

/// The live row with id `id`: (segment, offset).
fn locate(table: &TableSnapshot, starts: &[(u64, usize)], id: u64) -> Option<(usize, u32)> {
    let at = starts
        .partition_point(|(start, _)| *start <= id)
        .checked_sub(1)?;
    let segment = starts[at].1;
    let view = &table.segments[segment];
    let offset = u32::try_from(id - view.data.start).ok()?;
    (offset < view.data.rows && !view.deletes.contains(offset)).then_some((segment, offset))
}

/// How rows are tested: the fast way, or the general way.
enum Scan<'a> {
    Fast(Prepared<'a>),
    General(exec::Rows<'a>),
}

/// The newest rows with ids from `from` on that pass, newest first, up to
/// `limit`; and whether every morsel was scanned.
fn newest(scan: &Scan, table: &TableSnapshot, from: u64, limit: usize) -> (Vec<u64>, bool) {
    // morsels holding rows from `from` on, newest first
    let mut morsels: Vec<(usize, Range<usize>, usize)> = Vec::new();
    for (start, s) in starts(table).into_iter().rev() {
        let view = &table.segments[s];
        let rows = view.data.rows as usize;
        let skip = usize::try_from(from.saturating_sub(start)).unwrap_or(usize::MAX);
        let mut morsel: Vec<(usize, Range<usize>, usize)> = (0..rows)
            .step_by(MORSEL)
            .map(|m| (s, m..(m + MORSEL).min(rows), skip))
            .filter(|(_, r, skip)| r.end > *skip)
            .collect();
        morsel.reverse();
        morsels.extend(morsel);
    }
    // in waves of a few morsels per thread: the first usually suffices
    let wave = rayon::current_num_threads().max(1) * 2;
    let mut found: Vec<u64> = Vec::new();
    for chunk in morsels.chunks(wave) {
        let passing: Vec<Vec<u64>> = chunk
            .par_iter()
            .map_init(Scratch::default, |scratch, (s, rows, skip)| {
                let view = &table.segments[*s];
                let start = view.data.start;
                let mut ids: Vec<u64> = match scan {
                    Scan::Fast(prepared) => {
                        prepared.select(
                            table,
                            *s,
                            rows.clone(),
                            &Task::Add { from: *skip },
                            scratch,
                        );
                        scratch
                            .selected()
                            .iter()
                            .map(|r| start + (rows.start + *r as usize) as u64)
                            .collect()
                    }
                    Scan::General(general) => (rows.start.max(*skip)..rows.end)
                        .filter_map(|o| u32::try_from(o).ok())
                        .filter(|o| !view.deletes.contains(*o) && general.passes(*s, *o))
                        .map(|o| start + u64::from(o))
                        .collect(),
                };
                ids.reverse();
                ids
            })
            .collect();
        for ids in passing {
            found.extend(ids);
            if found.len() >= limit {
                found.truncate(limit);
                return (found, false);
            }
        }
    }
    (found, true)
}

pub(crate) fn answer(
    snapshot: &Snapshot,
    query: &RowsQuery,
    fast: bool,
    caches: Option<&Caches>,
) -> Result<ResultSet, QueryError> {
    let model = &snapshot.model;
    if query.fields.is_empty() {
        return Err(QueryError("rows need at least one field to show".into()));
    }
    let tables: Vec<_> = query.fields.iter().map(|f| f.table).collect();
    let anchor = plan::rows_anchor(model, &tables)?;
    let selection = crate::assoc::Associations::new(snapshot, caches).expand(
        anchor,
        &crate::hierarchy::resolve(snapshot, caches, &query.selection)?,
    )?;
    let (scanned, decoded, accesses) = plan::rows(model, anchor, &selection, &query.fields)?;
    let columns: Vec<ResultColumn> = query
        .fields
        .iter()
        .zip(&accesses)
        .map(|(field, access)| ResultColumn {
            name: plan::label(model, field),
            ty: decoded.accesses[*access].ty,
            role: Role::Dimension,
        })
        .collect();
    let Some(table) = snapshot.table(anchor) else {
        return Ok(ResultSet {
            columns,
            rows: Vec::new(),
        });
    };
    let limit = query.limit.min(MAX_ROWS);
    let ids = if limit == 0 {
        Vec::new()
    } else {
        found(snapshot, table, &scanned, fast, caches, limit)
            .ok_or_else(|| QueryError("the table isn't in the snapshot".into()))?
    };

    let reader = exec::Rows::new(model, snapshot, &decoded, caches)
        .ok_or_else(|| QueryError("the table isn't in the snapshot".into()))?;
    let starts = starts(table);
    let rows: Vec<Vec<Scalar>> = ids
        .par_iter()
        .filter_map(|id| locate(table, &starts, *id))
        .map(|(segment, offset)| reader.values(segment, offset, &accesses))
        .collect();
    Ok(ResultSet { columns, rows })
}

/// The ids of the newest `limit` rows passing `scanned`'s conditions: with
/// caches, from what was kept for the same plan when that still applies.
fn found(
    snapshot: &Snapshot,
    table: &TableSnapshot,
    scanned: &AnchorPlan,
    fast: bool,
    caches: Option<&Caches>,
    limit: usize,
) -> Option<Vec<u64>> {
    let model = &snapshot.model;
    let prepared = if fast {
        fast::prepare(model, snapshot, scanned, caches)
    } else {
        None
    };
    let high = table
        .segments
        .iter()
        .map(|v| v.data.start + u64::from(v.data.rows))
        .max()
        .unwrap_or(0);
    let inputs = prepared.as_ref().map(Prepared::inputs);
    let scan = match prepared {
        Some(prepared) => Scan::Fast(prepared),
        None => Scan::General(exec::Rows::new(model, snapshot, scanned, caches)?),
    };
    let (Some(caches), Scan::Fast(prepared), Some((maps, derived))) = (caches, &scan, inputs)
    else {
        return Some(newest(&scan, table, 0, limit).0);
    };

    let key = format!("{:p} {limit} {scanned:?}", Arc::as_ptr(&snapshot.model));
    let starts = starts(table);
    // brought up to date: new rows in front, kept ones that are gone left
    // out, as long as that leaves the limit or every row there is
    let updated = caches.take_rows(&key).and_then(|kept| {
        if kept.epoch > snapshot.epoch
            || kept.high > high
            || !prepared.same_reads(&kept.maps, &kept.derived)
        {
            return None;
        }
        let (mut rows, _) = newest(&scan, table, kept.high, limit);
        let before = kept.rows.len();
        let alive: Vec<u64> = kept
            .rows
            .into_iter()
            .filter(|id| locate(table, &starts, *id).is_some())
            .collect();
        let lost = alive.len() < before;
        rows.extend(alive);
        let exhausted = kept.exhausted && rows.len() < limit;
        rows.truncate(limit);
        (rows.len() == limit || !lost || kept.exhausted).then_some((rows, exhausted))
    });
    let (rows, exhausted) = updated.unwrap_or_else(|| newest(&scan, table, 0, limit));
    caches.keep_rows(
        key,
        Kept {
            epoch: snapshot.epoch,
            high,
            rows: rows.clone(),
            exhausted,
            maps,
            derived,
        },
    );
    Some(rows)
}

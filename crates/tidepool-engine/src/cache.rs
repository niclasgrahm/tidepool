//! What the engine keeps between queries (plan §6.5).
//!
//! Everything here is keyed by the *identity* of immutable store data: an
//! unchanged table keeps its `Arc<TableSnapshot>` from epoch to epoch, and a
//! segment keeps its `Arc<SegmentData>` until it is sealed into a new one. An
//! entry holds `Weak` references to what it was built from, which both
//! proves it still applies (`ptr_eq`) and keeps the address from being
//! reused while the entry exists. Nothing is invalidated explicitly: an
//! entry whose source is gone is dead, and dead entries are swept.
//!
//! - **Relationship maps**: a related table's values by its key (`Map`),
//!   per path and column. Rebuilt only when a table on the path changes.
//! - **Join columns**: those values materialized per anchor row, in
//!   morsel-sized chunks filled the first time a scan reads them. Reading a
//!   related field then costs one sequential byte (or two) per row, instead
//!   of a key decode and a random lookup.
//! - **Text hits** (plan §5.7): which rows of a plain string column of one
//!   segment contain a needle, a bitset per morsel filled the first time a
//!   scan reads it. A needle that contains an earlier one searches only the
//!   rows that one found.
//! - **Results**, per epoch: a selection toggled back is answered at once.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use tidepool_expr::Expr;
use tidepool_store::dict::DictSnapshot;
use tidepool_store::store::{ColumnLayout, DictId, SegmentData, TableSnapshot};

use crate::derived::DerivedDict;
use crate::text::Needle;

use crate::fast::{Incremental, MORSEL, Map};
use crate::query::ResultSet;

/// Codes of one morsel of a join column, as narrow as the map's.
pub(crate) enum Narrow {
    U8(Box<[u8]>),
    U16(Box<[u16]>),
    U32(Box<[u32]>),
}

impl Narrow {
    fn bytes(&self) -> usize {
        match self {
            Self::U8(v) => v.len(),
            Self::U16(v) => v.len() * 2,
            Self::U32(v) => v.len() * 4,
        }
    }
}

/// A related field's codes for every row of one anchor segment, by morsel.
pub(crate) struct JoinColumn {
    chunks: Box<[OnceLock<Narrow>]>,
}

impl JoinColumn {
    fn new(rows: usize) -> Self {
        Self {
            chunks: (0..rows.div_ceil(MORSEL))
                .map(|_| OnceLock::new())
                .collect(),
        }
    }

    /// The chunk holding rows from `start` (a multiple of [`MORSEL`]),
    /// built by `build` the first time.
    pub(crate) fn chunk(&self, start: usize, build: impl FnOnce() -> Narrow) -> Option<&Narrow> {
        self.chunks
            .get(start / MORSEL)
            .map(|c| c.get_or_init(build))
    }

    fn bytes(&self) -> usize {
        self.chunks
            .iter()
            .filter_map(OnceLock::get)
            .map(Narrow::bytes)
            .sum()
    }
}

/// The rows of one segment's string column containing a needle: a bitset
/// per morsel (bit `r` for the morsel's row `r`), filled on first read.
pub(crate) struct TextHits {
    pub needle: Arc<Needle>,
    chunks: Box<[OnceLock<Box<[u64]>>]>,
    /// Hits of a needle this one contains, when it was cached: in a morsel
    /// it has, only its hits need searching.
    pub wider: Option<Arc<TextHits>>,
}

impl TextHits {
    fn new(rows: usize, needle: Arc<Needle>, wider: Option<Arc<TextHits>>) -> Self {
        Self {
            needle,
            chunks: (0..rows.div_ceil(MORSEL))
                .map(|_| OnceLock::new())
                .collect(),
            wider,
        }
    }

    /// The bits of the morsel from `start` (a multiple of [`MORSEL`]),
    /// built by `build` the first time.
    pub(crate) fn chunk(&self, start: usize, build: impl FnOnce() -> Box<[u64]>) -> Option<&[u64]> {
        self.chunks
            .get(start / MORSEL)
            .map(|c| &**c.get_or_init(build))
    }

    /// The bits of the morsel from `start`, if they were built.
    pub(crate) fn built(&self, start: usize) -> Option<&[u64]> {
        self.chunks
            .get(start / MORSEL)
            .and_then(OnceLock::get)
            .map(|b| &**b)
    }

    fn bytes(&self) -> usize {
        self.chunks
            .iter()
            .filter_map(OnceLock::get)
            .map(|b| b.len() * 8)
            .sum()
    }
}

/// Identifies a relationship map: the tables along the path (by identity),
/// the foreign-key column of each hop, and the target column.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct MapKey {
    pub tables: Vec<usize>,
    pub hops: Vec<usize>,
    pub column: usize,
    /// The derived field the values are mapped through, if any.
    pub variant: String,
}

struct MapEntry {
    tables: Vec<Weak<TableSnapshot>>,
    map: Arc<Map>,
}

struct HierarchyEntry {
    table: Weak<TableSnapshot>,
    columns: tidepool_model::ir::Hierarchy,
    tree: Arc<crate::hierarchy::Hierarchy>,
}

struct JoinEntry {
    segment: Weak<SegmentData>,
    map: Weak<Map>,
    column: Arc<JoinColumn>,
}

/// A text hits entry's key: (segment, column, folded needle).
type TextKey = (usize, usize, Vec<u8>);

struct TextEntry {
    segment: Weak<SegmentData>,
    hits: Arc<TextHits>,
    tick: u64,
}

#[derive(Default)]
struct Inner {
    /// Text hits by (segment, column, folded needle).
    texts: HashMap<TextKey, TextEntry>,
    maps: HashMap<MapKey, MapEntry>,
    joins: HashMap<(usize, usize), JoinEntry>,
    /// Hierarchies' trees, by their table's identity.
    hierarchies: HashMap<usize, HierarchyEntry>,
    /// The general path's key indexes, by their table's identity.
    key_indexes: HashMap<usize, (Weak<TableSnapshot>, Arc<crate::exec::KeyIndex>)>,
    /// Derived dictionaries per (base dictionary, expression). Codes never
    /// change, so an entry stays valid; it is extended as its base grows.
    /// Keyed by the dictionary's generation too: a model change can build
    /// a new dictionary under an old id.
    derived: HashMap<(DictId, u32, String), Arc<DerivedDict>>,
    /// Anchor answers kept to be brought up to date, by plan, with the
    /// tick they were last used at.
    anchors: HashMap<String, (u64, Incremental)>,
    /// `rows` answers kept the same way.
    rows: HashMap<String, (u64, crate::rows::Kept)>,
    tick: u64,
    /// (epoch, query) → answer, oldest first; only the newest epoch's.
    results: VecDeque<((u64, String), ResultSet)>,
    /// Messages along relationships for selections flowing both ways
    /// (`assoc`), by (relationship, selection); only the newest epoch's.
    messages: (u64, HashMap<String, Arc<crate::query::ValueSet>>),
}

/// The engine's caches. Shared by every query; cheap to look up once per
/// query, never touched per row.
pub(crate) struct Caches {
    inner: Mutex<Inner>,
    /// Join columns are dropped (all of them, for now) past this many bytes.
    join_budget: usize,
}

/// Anchor answers kept to be brought up to date, at most.
const ANCHORS: usize = 256;

/// Results kept per epoch.
const RESULTS: usize = 256;

/// Bytes of text hits kept, at most: 64 MB is a search over 512M rows.
const TEXT_BYTES: usize = 64 << 20;

fn address<T: ?Sized>(arc: &Arc<T>) -> usize {
    Arc::as_ptr(arc).cast::<()>() as usize
}

impl Caches {
    pub(crate) fn new(join_budget: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            join_budget,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // a panic while holding the lock leaves nothing half-written that
        // matters: entries are inserted whole
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The map for `tables` (every table on the path, anchor excluded),
    /// built by `build` unless a live one is cached.
    pub(crate) fn map(
        &self,
        tables: &[&Arc<TableSnapshot>],
        hops: Vec<usize>,
        column: usize,
        variant: &str,
        build: impl FnOnce() -> Option<Map>,
    ) -> Option<Arc<Map>> {
        let key = MapKey {
            tables: tables.iter().map(|t| address(t)).collect(),
            hops,
            column,
            variant: variant.to_string(),
        };
        if let Some(entry) = self.lock().maps.get(&key)
            && entry
                .tables
                .iter()
                .zip(tables)
                .all(|(w, t)| w.upgrade().is_some_and(|w| Arc::ptr_eq(&w, t)))
        {
            return Some(Arc::clone(&entry.map));
        }
        // built outside the lock: other queries go on meanwhile
        let map = Arc::new(build()?);
        self.lock().maps.insert(
            key,
            MapEntry {
                tables: tables.iter().map(|t| Arc::downgrade(t)).collect(),
                map: Arc::clone(&map),
            },
        );
        Some(map)
    }

    /// The tree of a hierarchy over `table`, built by `build` unless the
    /// table is unchanged since it last was.
    pub(crate) fn hierarchy(
        &self,
        table: &Arc<TableSnapshot>,
        columns: &tidepool_model::ir::Hierarchy,
        build: impl FnOnce() -> Option<crate::hierarchy::Hierarchy>,
    ) -> Option<Arc<crate::hierarchy::Hierarchy>> {
        if let Some(entry) = self.lock().hierarchies.get(&address(table))
            && entry.columns == *columns
            && entry
                .table
                .upgrade()
                .is_some_and(|t| Arc::ptr_eq(&t, table))
        {
            return Some(Arc::clone(&entry.tree));
        }
        let tree = Arc::new(build()?);
        self.lock().hierarchies.insert(
            address(table),
            HierarchyEntry {
                table: Arc::downgrade(table),
                columns: *columns,
                tree: Arc::clone(&tree),
            },
        );
        Some(tree)
    }

    /// The key index of `table`, built by `build` unless the table is
    /// unchanged since it last was.
    pub(crate) fn key_index(
        &self,
        table: &Arc<TableSnapshot>,
        build: impl FnOnce() -> crate::exec::KeyIndex,
    ) -> Arc<crate::exec::KeyIndex> {
        if let Some((known, index)) = self.lock().key_indexes.get(&address(table))
            && known.upgrade().is_some_and(|t| Arc::ptr_eq(&t, table))
        {
            return Arc::clone(index);
        }
        let index = Arc::new(build());
        self.lock()
            .key_indexes
            .insert(address(table), (Arc::downgrade(table), Arc::clone(&index)));
        index
    }

    /// The derived dictionary of `expr` over `dict`, covering every code.
    pub(crate) fn derived(
        &self,
        id: DictId,
        expr: &Expr,
        slot: u32,
        layout: &ColumnLayout,
        dict: &DictSnapshot,
    ) -> Arc<DerivedDict> {
        let key = (id, dict.generation(), format!("{expr:?}"));
        let known = self.lock().derived.get(&key).cloned();
        if let Some(known) = &known
            && known.covers(dict)
        {
            return Arc::clone(known);
        }
        // extended outside the lock; the newest one wins
        let next = Arc::new(match known {
            Some(known) => known.extend(expr, slot, layout, dict),
            None => DerivedDict::empty().extend(expr, slot, layout, dict),
        });
        let mut inner = self.lock();
        let keep = inner
            .derived
            .get(&key)
            .is_none_or(|k| k.map.len() < next.map.len());
        if keep {
            inner.derived.insert(key, Arc::clone(&next));
        }
        next
    }

    /// The join column of `segment` through `map`, empty until scanned.
    pub(crate) fn join(&self, segment: &Arc<SegmentData>, map: &Arc<Map>) -> Arc<JoinColumn> {
        let key = (address(segment), address(map));
        let mut inner = self.lock();
        if let Some(entry) = inner.joins.get(&key)
            && entry
                .segment
                .upgrade()
                .is_some_and(|s| Arc::ptr_eq(&s, segment))
            && entry.map.upgrade().is_some_and(|m| Arc::ptr_eq(&m, map))
        {
            return Arc::clone(&entry.column);
        }
        let column = Arc::new(JoinColumn::new(segment.rows as usize));
        inner.joins.insert(
            key,
            JoinEntry {
                segment: Arc::downgrade(segment),
                map: Arc::downgrade(map),
                column: Arc::clone(&column),
            },
        );
        column
    }

    /// The hits of `needle` in column `column` of `segment`, empty until
    /// scanned; with the hits of the longest cached needle it contains.
    pub(crate) fn text(
        &self,
        segment: &Arc<SegmentData>,
        column: usize,
        needle: &Arc<Needle>,
    ) -> Arc<TextHits> {
        let at = address(segment);
        let key = (at, column, needle.folded().to_vec());
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(entry) = inner.texts.get_mut(&key)
            && entry
                .segment
                .upgrade()
                .is_some_and(|s| Arc::ptr_eq(&s, segment))
        {
            entry.tick = tick;
            return Arc::clone(&entry.hits);
        }
        let wider = inner
            .texts
            .iter()
            .filter(|((s, c, _), e)| {
                *s == at
                    && *c == column
                    && e.segment
                        .upgrade()
                        .is_some_and(|s| Arc::ptr_eq(&s, segment))
                    && needle.narrows(&e.hits.needle)
            })
            .max_by_key(|(_, e)| e.hits.needle.folded().len())
            .map(|(_, e)| Arc::clone(&e.hits));
        let hits = Arc::new(TextHits::new(
            segment.rows as usize,
            Arc::clone(needle),
            wider,
        ));
        inner.texts.insert(
            key,
            TextEntry {
                segment: Arc::downgrade(segment),
                hits: Arc::clone(&hits),
                tick,
            },
        );
        hits
    }

    /// Drop entries whose sources are gone, and join columns past budget.
    pub(crate) fn sweep(&self) {
        let mut inner = self.lock();
        inner
            .maps
            .retain(|_, e| e.tables.iter().all(|t| t.strong_count() > 0));
        inner
            .joins
            .retain(|_, e| e.segment.strong_count() > 0 && e.map.strong_count() > 0);
        inner.hierarchies.retain(|_, e| e.table.strong_count() > 0);
        inner.key_indexes.retain(|_, (t, _)| t.strong_count() > 0);
        inner.texts.retain(|_, e| e.segment.strong_count() > 0);
        // the least recently used hits past their budget (a narrower
        // needle's `wider` keeps what it uses alive meanwhile)
        let mut held: usize = inner.texts.values().map(|e| e.hits.bytes()).sum();
        if held > TEXT_BYTES {
            let mut ticks: Vec<(u64, TextKey)> = inner
                .texts
                .iter()
                .map(|(k, e)| (e.tick, k.clone()))
                .collect();
            ticks.sort_unstable();
            for (_, key) in ticks {
                if held <= TEXT_BYTES {
                    break;
                }
                if let Some(entry) = inner.texts.remove(&key) {
                    held = held.saturating_sub(entry.hits.bytes());
                }
            }
        }
        let bytes: usize = inner.joins.values().map(|e| e.column.bytes()).sum();
        if bytes > self.join_budget {
            inner.joins.clear();
        }
    }

    pub(crate) fn result(&self, epoch: u64, query: &str) -> Option<ResultSet> {
        self.lock()
            .results
            .iter()
            .find(|((e, q), _)| *e == epoch && q == query)
            .map(|(_, r)| r.clone())
    }

    pub(crate) fn keep_result(&self, epoch: u64, query: String, result: ResultSet) {
        let mut inner = self.lock();
        // a newer epoch makes every older answer stale
        inner.results.retain(|((e, _), _)| *e >= epoch);
        if inner.results.iter().any(|((e, _), _)| *e > epoch) {
            return;
        }
        if inner.results.len() >= RESULTS {
            inner.results.pop_front();
        }
        inner.results.push_back(((epoch, query), result));
    }

    /// The kept answer for an anchor plan, taken out while it's brought up
    /// to date (a query running meanwhile computes its own).
    pub(crate) fn take_anchor(&self, key: &str) -> Option<Incremental> {
        self.lock().anchors.remove(key).map(|(_, kept)| kept)
    }

    /// Keep an anchor answer; the least recently used go past the budget.
    pub(crate) fn keep_anchor(&self, key: String, kept: Incremental) {
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        // a newer one kept meanwhile wins
        if inner
            .anchors
            .get(&key)
            .is_some_and(|(_, k)| k.epoch() > kept.epoch())
        {
            return;
        }
        inner.anchors.insert(key, (tick, kept));
        while inner.anchors.len() > ANCHORS
            || inner
                .anchors
                .values()
                .map(|(_, k)| k.bytes())
                .sum::<usize>()
                > self.join_budget
        {
            let Some(oldest) = inner
                .anchors
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            inner.anchors.remove(&oldest);
        }
    }

    /// The kept answer of a `rows` query, taken out while it's brought up
    /// to date.
    pub(crate) fn take_rows(&self, key: &str) -> Option<crate::rows::Kept> {
        self.lock().rows.remove(key).map(|(_, kept)| kept)
    }

    /// Keep a `rows` answer; the least recently used go past [`ANCHORS`].
    pub(crate) fn keep_rows(&self, key: String, kept: crate::rows::Kept) {
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        if inner
            .rows
            .get(&key)
            .is_some_and(|(_, k)| k.epoch() > kept.epoch())
        {
            return;
        }
        inner.rows.insert(key, (tick, kept));
        if inner.rows.len() > ANCHORS
            && let Some(oldest) = inner
                .rows
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone())
        {
            inner.rows.remove(&oldest);
        }
    }

    /// A message computed earlier in `epoch`.
    pub(crate) fn message(&self, epoch: u64, key: &str) -> Option<Arc<crate::query::ValueSet>> {
        let inner = self.lock();
        (inner.messages.0 == epoch)
            .then(|| inner.messages.1.get(key).cloned())
            .flatten()
    }

    /// Keep a message for the rest of `epoch`.
    pub(crate) fn keep_message(&self, epoch: u64, key: String, set: Arc<crate::query::ValueSet>) {
        let mut inner = self.lock();
        if inner.messages.0 != epoch {
            inner.messages = (epoch, HashMap::new());
        }
        inner.messages.1.insert(key, set);
    }

    /// Bytes held by every cache: join columns and kept answers.
    pub(crate) fn bytes(&self) -> usize {
        let inner = self.lock();
        inner
            .joins
            .values()
            .map(|e| e.column.bytes())
            .sum::<usize>()
            + inner
                .anchors
                .values()
                .map(|(_, k)| k.bytes())
                .sum::<usize>()
            + inner.texts.values().map(|e| e.hits.bytes()).sum::<usize>()
    }

    /// Bytes held by join columns, for status pages and tests.
    pub(crate) fn join_bytes(&self) -> usize {
        self.lock().joins.values().map(|e| e.column.bytes()).sum()
    }
}

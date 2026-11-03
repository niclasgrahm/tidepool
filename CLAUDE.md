# CLAUDE.md

Tidepool is a live, associative analytics server: data pushed in over HTTP,
held in a dictionary-encoded in-memory column store, a semantic model declared
as code (Apache Ossie + extensions), and dashboards that update continuously and
respond instantly to selections. One Rust binary with the UI built in.

**Read [docs/plan.md](docs/plan.md) before working on anything.** It is the
spec: decisions (§1), config formats (§3), each subsystem (§4–10), testing
(§11) and the milestones with their exit criteria (§12). When the code and the
plan disagree, either the code is wrong or the plan needs updating in the same
change. Don't leave them disagreeing silently.

Sister project: [Kayak](../../rust/kayak), the stream processor that is
Tidepool's connector layer (plan §15). Many conventions here are Kayak's
(plan D14). When in doubt about style, look at how Kayak does it.

## Commands

```bash
just                 # list recipes
just ci              # fmt check + clippy -D warnings + all tests: what CI runs
just test            # cargo test --workspace --all-targets
just lint            # cargo clippy --workspace --all-targets -- -D warnings
just fmt             # cargo fmt --all
just deny            # cargo deny check (licences, advisories, sources)
just miri            # the store's unsafe code under Miri (needs nightly + miri)
just frontend        # build the UI into frontend/dist (trunk)
just build           # just frontend, then a release binary with the UI embedded
just test-embed      # tests of the embedded UI; needs `just build` first
just e2e             # the browser end to end: headless Chrome against the built UI
just docs            # regenerate the doc site's reference (website/reference/generated)
just docs-dev        # the doc site on :5173 with hot reload (npm; website/ only)
just docs-build      # production build of the doc site
just docker          # the container image, UI included (tidepool:dev)
just demo            # the Kayak demo: MQTT → Kayak → Tidepool (docker compose)
just northwind       # Northwind live: Postgres dims + order events → Kayak → Tidepool
just pglogs          # Postgres logs live: app → Postgres jsonlog → shipper → Tidepool, searched
just run             # debug server on :7070, UI served from frontend/dist
just dev             # server on :7070 + trunk on :7080 (hot reload); open :7080
just bench           # ingest throughput per format, release build (args: --rows N --batch N --clients N)
just bench-query     # query latency on the reference dataset (args: --orders N --runs N --metrics a,b,c)
just bench-live      # freshness: 50 WebSocket sessions while ingesting 100k rows/s
just bench-search    # text search and rows on synthetic log lines (args: --lines N --runs N)
just feed            # keep a running server (`just run`) fed with retail data, to watch it live
just oracle 1000     # the DuckDB oracle over 1000 seeds, release build (`just test` runs 40)
just crash           # 1,000 crash points and 25 kill -9 cycles (`just test` runs fewer)
cargo run -p tidepool -- init shop && cargo run -p tidepool -- serve --config shop   # the quick-start
cargo run -p tidepool -- feed                                 # sample data for an `init` project
cargo run -p tidepool -- validate --config examples/retail    # check a config directory
cargo run -p tidepool -- plan --config new/ --base old/       # what changes, and what it costs
cargo insta review   # accept or reject changed golden snapshots (diagnostics, plans)
```

Tooling the frontend needs: `rustup target add wasm32-unknown-unknown` and
`cargo install trunk --locked`. Nothing else: the product has no node
toolchain. Only the doc site (`website/`) uses npm, for VitePress.

## Definition of done

Not negotiable, for every change however small:

1. **New behaviour ships with tests.** Any new or changed behaviour (an
   endpoint, a config field, an engine path, a bug fix) needs a test that
   fails without the change. A bug fix without a regression test is not a fix.
   If something can't be tested offline, say so and why.
2. **`just ci` is green before a task is called done.** The whole suite, not
   "it compiles" or "the new test passes". Never disable, `#[ignore]` or weaken
   an existing test to get to green; a test that encodes the wrong behaviour is
   a conversation first.
3. **Engine results are checked against the oracle** (from M3): any change to
   how results are computed or maintained runs the oracle suites, and new
   query features extend the generators so the oracle covers them.

Lints are strict by design: clippy `all` + `pedantic`, plus `unwrap_used` and
`expect_used`, which `clippy.toml` applies to tests as well. In tests, use
`match`/`let ... else` with `panic!` and a message that says what was expected.

## Architecture

Cargo workspace under `crates/`. Dependencies flow one way:
`config → expr → model → store → engine → server → tidepool`, and `protocol`
depends only on serde/schemars. (`expr` before `model`: the model compiler
resolves names in expressions.)

| crate | licence | what | status |
|---|---|---|---|
| `tidepool-protocol` | Apache-2.0 | wire types and the **endpoint table** (`api_docs.rs`) | M0–M6 ✓ |
| `tidepool-config` | Apache-2.0 | serde types for every config file, JSON Schemas, diagnostics with `file:line:col` | M1 ✓ |
| `tidepool-expr` | AGPL | SQL expressions: syntax + reference analysis (M1 ✓); type-check, kernels (M3) | M1/M3 |
| `tidepool-model` | AGPL | compiled Model IR, cross-file validation, catalog of stable ids, diff → plan | M1 ✓ |
| `tidepool-store` | AGPL | values, dictionaries, segments, PK index, epochs/snapshots, the writer thread, ingest decoders, WAL and checkpoints, model changes over data | M2 ✓ / M5 ✓ / M7 ✓ |
| `tidepool-engine` | AGPL | selection engine, aggregation, caches, incremental answers | M3 ✓ / M4 ✓ |
| `tidepool-server` | AGPL | axum: router from the endpoint table, OpenAPI, UI serving, live sessions, model/status, the config pipeline | M0–M7 ✓ |
| `tidepool-oracle` | AGPL | test-only DuckDB reference + random generators | M3 ✓ / M4 ✓ |
| `tidepool` | AGPL | the binary: CLI and wiring; crash and browser e2e tests | M0–M6 ✓ |
| `tidepool-bench` | AGPL | load/latency harness (`just bench`, `bench-search`…, not in CI) | M2 ✓ (ingest) |
| `tidepool-docsgen` | AGPL | the doc site's generated reference (`just docs`), and the tests that keep `website/` true | ✓ |
| `frontend` (`tidepool-frontend`) | AGPL | the web UI: Leptos, client-side rendered, built by trunk | M6 ✓ / M7 ✓ |

The licence split is enforced by `crates/tidepool/tests/licensing.rs`; see
[licensing.md](licensing.md). A permissive crate must never depend on a
copyleft one.

### The endpoint table

`tidepool_protocol::api_docs::endpoints()` is the single description of the
HTTP surface (Kayak's pattern). `tidepool_server::api_router` is a fold over
it, `handler_for` matches exhaustively on `Operation` (so a documented endpoint
without a handler doesn't compile), and `openapi::document()` renders the same
entries. To add an endpoint: add an `Operation` variant (and to `OPERATIONS`),
add its `ApiDoc` entry with prose and an `Access`, add any body schema to
`schemas()`, then the handler. The tests in `tests/api.rs` walk the table, so a
new entry is covered by them automatically.

`Access` is declared per endpoint and enforced by `auth::authorize` from the
same entry (see "Ingest over HTTP" below).

Probes, metrics and MCP live at the root (`/healthz`, `/readyz`, `/metrics`,
`/mcp`), where their clients look; everything else is under `/v1`.

### Config files and diagnostics (`tidepool-config`)

`ConfigSet::from_files` takes (relative path, text) pairs, so a directory, a
test's inline files and (later) an API change all load the same way; which
file is which is decided by path (`tables/<name>.yaml`, …). Types are
deserialized with `serde_norway` and `deny_unknown_fields` everywhere, so a
typo is an error with serde's location.

Cross-file errors need locations serde can't give, so every file also gets a
`PathIndex` (built from `saphyr-parser` events) mapping each YAML path
(`relationships[1].to`) to line/column/length. **Report problems by path,
never by line number**: `Diagnostic::error(..).at(file, path)` for a value,
`.at_key(..)` for the key that introduces it. `diag::render` turns that into
rustc-style output; `suggest`/`closest` give the "did you mean" help.

Ossie: the flat 0.2 shape, with 0.1.1's `semantic_model:` wrapper accepted
(D8). Tidepool's own semantics are `custom_extensions` entries with
`vendor_name: TIDEPOOL` and a JSON string `data`; their shapes are
`MetricExtension` etc. in `ossie.rs`, also `deny_unknown_fields`.

### The model compiler (`tidepool-model`)

`compile(&ConfigSet, &previous_catalog)` runs four stages (tables → datasets
and relationships → metrics → dashboards) into one `Diagnostics`, and only
returns a `Model` when there are no errors. Rules that keep the output
readable, and that every new check must follow:

- **Never stop at the first problem**, but **never cascade**. An object that
  failed is recorded as broken (`Tables::broken`, `Tables::key_broken`,
  `Semantic::broken`), and later references to it are skipped silently.
  A failed relationship marks its many side as `uncertain_from`;
  "can't reach" errors use `Semantic::maybe_reaches` so they only appear when
  even a fixed relationship couldn't help.
- **Point at the token that is wrong**: an expression's string, an
  extension's `data`, a relationship's `name`. Never the enclosing object.
- **Every new check gets a golden case** in `tests/diagnostics.rs`: the retail
  example plus one edit, snapshotted with insta. A case must produce exactly
  the error it is about and nothing else. Review snapshot diffs like UI copy
  (`cargo insta review`).
- Names are labels, ids are identity: the `Catalog` keeps ids across
  versions, `renamed_from` carries an id over a rename, and ids are never
  reused.

`plan::diff(old, new)` compares two models by id and classes each change
safe / rebuild / destructive (plan §8.2). New change kinds get a scenario in
`tests/plans.rs`; scenarios can edit the base config too (`base: [...]`),
which is how "joins two tables that already hold data" is tested.

### The store (`tidepool-store`)

One writer thread owns the `Store`; everything else reads immutable
`Snapshot`s. There is no shared mutable state and no `unsafe`:

- **Writes** go through `WriterHandle::apply` into a bounded queue (64
  batches). A full queue is `Busy`, which HTTP turns into `503 Retry-After`.
  Never make the queue unbounded: backpressure is the point.
- **Epochs**: the writer publishes on its own clock (`epoch_interval`) when
  something changed: it freezes each table's builder into an immutable chunk,
  snapshots dictionaries, and swaps in a new `Snapshot` (`ArcSwap`).
  `?wait=visible` waits for that; it never forces an epoch, so clients can't
  drive the epoch rate (every epoch will cost work per live dashboard).
- **Segments** are immutable once published; deletes are a copy-on-write
  `RoaringBitmap` beside each one, and `version` changes whenever it does.
  Anything cached about a segment (M4) is keyed by its version.
- **Row ids never change** (sealing keeps them), so the primary-key index
  points at rows forever. Primary-key columns are dictionary-encoded, which
  the model enforces, so a single-column key is a dense array by code.
- **An upsert equal to the stored row is skipped**: not written, not
  counted, the table not dirtied, so re-sending a whole dimension on a timer
  publishes no epoch and keeps every cache keyed by its snapshot.
  `Store::visible_epoch` is then the current epoch (`?wait=visible` must
  not wait for one that never comes). Upserts that do change something
  track which columns did (`TableSnapshot::changed`).
- **`Op::Replace`** (`PUT …/rows`): the batch is the table's whole
  contents; it's written as upserts and every live key it didn't list is
  deleted, in the same batch (WAL op codes 5/6, replayed as is).
- **Decoding** (JSON, NDJSON, Arrow) is strict: values are checked, never
  coerced; a batch is all or nothing; every problem is listed by row and
  column. Decimals are parsed from their text, never through `f64`.

**Durability** (`persist.rs`, `wal.rs`, plan §4.7): with a data directory
the writer owns a `Durable` (store + WAL) instead of a bare `Store`. Every
batch is logged as it's applied (with its idempotency key and response,
when it has a key), and acknowledged only after the burst it came in is
synced (group commit). Checkpoints are published snapshots
written down on their own thread: immutable segments go to files once,
dictionaries are appended, and the checkpoint file is written last,
atomically. Recovery re-encodes dictionary values in code order, so the
log replays to the same codes and row ids. Anything that changes what's
written or how it's replayed must keep `tests/durability.rs` (1,000 power
losses, including mid-checkpoint) and the `kill -9` loop
(`crates/tidepool/tests/crash.rs`) green, and add a planted-bug check.

**Model changes** (`migrate.rs`, plan §8.2): `Store::plan_migration`
works out what a new `ModelVersion` does to the data; tables are
re-shaped by column id (unchanged columns shared via `Arc`, new nullable
columns as `ColumnData::Null`, which holds nothing). Anything that
rewrites rows (joined dictionaries merged into a new one, encoding or type
changes) is `migrate::build`, run from an immutable snapshot on its own
thread; `Store::migrate` catches up with what arrived meanwhile and swaps
on the writer thread. Rebuilt dictionaries and rewritten segments get a
new **generation**, which is in their file names and in the engine's
derived-dictionary cache key: a dictionary under an old id is never taken
for the old one. Batches carry the column ids and types they were decoded
for (`Batch::shape`), and `Store::fit` re-shapes a batch decoded before a
change. A model change is a WAL record and part of every checkpoint.
Every new kind of change needs a case in `tests/migrate.rs`, and anything
that changes results must keep the migration oracle (`check_migration`)
green.

Store tests run against the retail example's model (`tests/store.rs`); use
`Store::with_seal_rows` with a small number to exercise sealing.

### Ingest over HTTP (`tidepool-server::tables`)

Rows endpoints exist once in the endpoint table (`/v1/tables/{table}/rows`)
and dispatch by name at request time, so a table added to the model works
without touching the router. The OpenAPI document replaces the templated path
with one typed path per table (`Row_<table>` / `Key_<table>` schemas).
Decoding runs in `spawn_blocking`. `Idempotency-Key` is per credential and
table (`keys::Scoped`) and kept for 24 h; with a data directory it's
durable: the writer logs the key, its credential and the response in the
batch's WAL record, checkpoints keep the live keys, and
`AppState::with_writer` restores them (`tidepool_store::keys`).

Access is enforced from the endpoint table: `auth::authorize` wraps every
route with its declared `Access`, checking the bearer token configured for
that level (`TIDEPOOL_{READ,ADMIN}_TOKEN`, names configurable under
`auth:`; unset = open; the admin token is good for every level) or a
browser session (`POST /v1/auth/session` trades a token for an `HttpOnly`
cookie; in memory, like Kayak's). `every_protected_endpoint_wants_its_token`
walks the table to prove it.

**Exposure** (plan §9.2, D15). Ingest is done by `auth::Credential`s
(`auth.ingest`, or the single `ingest_token_env` as one named `ingest`),
each scoped to tables and operations. For ingest routes `authorize` only
works out the `Caller` and puts it in the request; the handler decides
with `Caller::writer` once it knows the table and operation, together with
the `Listener` the request came in on. Each listener (`listen.public`,
`.ingest`, `.admin`) gets its own fold of the endpoint table
(`api_router_on`, `endpoints::placed`); only `public` credentials write
through a public-only listener, and there anything else is a 404 that
looks like a missing table. Limits (`limits.rs`: rate and new-value token
buckets per credential, a quarter of the writer queue for public batches)
and `filled` columns (`batch::fill`) are applied in `tables.rs`; the writer
refuses a batch over its new-values budget (`Store::new_values`).
`tidepool_server::serve` refuses to start with ingest or admin open off loopback
(`exposure`, `--insecure-open`). `tests/exposure.rs` covers all of it; a
change to `auth`, `listen` or `agents` is class `security`
(`plan::ChangeClass::needs_approval`), never auto-applied.

### The query engine (`tidepool-engine`)

`run(snapshot, query)` is the whole surface: plan, execute, combine, sort.

- **Planning** (`plan.rs`): every value a query reads is an *access*: a
  relationship path from the anchor table plus a column or derived
  expression. Selection is one-directional: a metric is filtered by the
  selected fields its anchor reaches through many→one paths; the others are
  ignored for it. One `AnchorPlan` per (anchor table, selection set), so
  metrics with `ignore_selection` get their own scan.
- **Two executors, same answers.** `exec.rs` is the general path: decodes
  every value into a `Scalar` and evaluates expressions; slow, always right.
  `fast.rs` takes any anchor plan whose values are plain columns
  (dictionary codes, i64, f64) or derived fields over one dictionary column,
  and whose aggregates are simple. It works on 16K-row morsels in parallel
  with branch-free selection vectors (about half the rows pass a typical
  selection, the worst case for a predicted branch), follows relationships
  through arrays indexed by foreign-key code (`scatter`/`follow`), tests
  dictionary conditions once per code, and uses dense group ids when every
  dimension is a code. `COUNT(DISTINCT code)` sets bits in a group × code
  bitset: per thread when small, shared and bucketed by range when not, and
  with any dimension the counted key itself determines folded out
  (`Folded`). `prepare` returning `None` means "use the general path";
  never make the fast path answer something it can't do exactly.
- **Derived dictionaries** (`derived.rs`): a derived field over one
  dictionary column is evaluated once per dictionary value; its results get
  codes of their own, so it reads, groups and filters like a stored column.
  They only grow, as their base dictionaries do.
- **Semantics** follow DuckDB, except dividing by zero is NULL. No
  dimensions means exactly one row, as in SQL. A related row that doesn't
  exist reads as all NULLs, as in a `LEFT JOIN`: a derived field over it is
  still evaluated (`COALESCE(name, 'n/a')` is 'n/a'). A relationship map
  therefore carries the code of a missing row (`Map::missing`), which isn't
  always NULL.
- **Caches** (`cache.rs`) live in an `Engine`, which the server keeps for
  its lifetime; the free `run` is uncached. Entries are keyed by the
  *identity* of immutable store data (an unchanged table keeps its
  `Arc<TableSnapshot>`, a segment its `Arc<SegmentData>`) and hold `Weak`s
  to it, so nothing is invalidated by hand: a changed table simply doesn't
  match. Relationship maps per path, join columns (related codes
  materialized per anchor segment, in morsel chunks filled by the first scan
  that reads them), and answers per epoch. A new cache must key the same
  way, and get a planted-bug check against the oracle's history replay.

- **Both ways** (`assoc.rs`, `filter_direction: both`): before planning,
  each anchor's selection gets one `Condition::Set` per relationship into a
  table it reaches (other than the way it got there) whose many side has
  something selected: the foreign keys of that side's passing rows,
  computed recursively and cached per epoch. Relationships form a tree
  (the compiler checks), so this is exact message passing; the executors
  never know.

- **Hierarchies** (`hierarchy.rs`, plan §5.6): a dataset's
  `{"hierarchy": …}` extension adds a field whose values are node keys
  (`FieldSource::Hierarchy`); the parent column joins the key's
  dictionary through `key_domains`, never as a `Relationship` (paths,
  `assoc` and cycle checks would loop on a self-edge). `resolve` turns a
  condition on it into a `Condition::Set` of subtree keys on the key
  field, per anchor after `ignore_selection` and before `assoc`, against
  the query's snapshot (so moves apply live). The tree is cached by the
  table's identity like the other caches; `Dim.level` groups by ancestor
  at a depth via `Target::Ancestor`, a derived dictionary built from the
  tree (map-cache variant `ancestor:d`) and never put in
  `Caches::derived`, whose values must not change.

- **Text search** (`text.rs`, plan §5.7): `Condition::Contains(Needle)`,
  ASCII case folded. On a plain string column the fast path reads it as
  `Kind::Text` (searched, never decoded: a plain string in a dimension or
  argument still means the general path), searching a morsel's arena
  slice at once (fold into scratch + `memchr::memmem`, hits mapped to
  rows by the offsets, a hit across a row boundary is none). Hits are
  cached per segment and morsel (`Caches::text`, keyed by segment
  identity, column and folded needle; a needle that contains a cached
  one searches only its hits). Text tests run after the other
  conditions; when few rows are left they're searched one at a time.
  On a dictionary column it's an ordinary per-code test.
- **Rows** (`rows.rs`): `Engine::rows` is the `rows` visual and
  `POST /v1/rows`: the newest rows passing the anchor's conditions, by
  row id, scanned in waves of morsels from the newest (`Prepared::select`,
  shared with `scan`), only the kept rows decoded (`exec::Rows`). The
  answer is kept per plan and brought up to date with the rows appended
  since (`Kept`); deletes of kept rows that leave it short start over.
  The oracle compares rows with DuckDB as a set, and order and limit
  across paths (`check_rows`, also live in `check_live`).

- **Queries without metrics** (`plan::rows_anchor`): dimensions only are
  the combinations there are. They're counted (`ROWS`) on the table that
  reaches every dimension and reaches fewest tables (ties by name), and
  groups without rows are dropped. The oracle's `combinations_sql` picks
  the same table independently; keep the two rules equal.

- **Incremental answers** (`Incremental` in `fast.rs`): with an `Engine`,
  each anchor's aggregate state is kept with a row-id high-water mark and
  the deleted rows it accounted for. The next epoch scans only appended
  rows, and takes deleted ones back out when every aggregate can
  (count/sum/avg); otherwise, or when a related table changed, it
  recomputes. Row ids never change, so sealing costs nothing here.

Over HTTP, `POST /v1/query` (`tidepool-server::query`) resolves the semantic
layer's names (`dataset.field`, metric names, with did-you-mean errors),
converts JSON filter values by the field's type, and runs the query in
`spawn_blocking`.

### Live dashboards (`tidepool-server::live`)

`/v1/ws` speaks the protocol in `tidepool-protocol::live` (JSON messages
tagged by `type`). The `Hub` holds sessions and knows nothing of sockets;
tests drive it through a real socket (`tests/live.rs`). A session has a
selection with back/forward history, an open dashboard, filter-list
windows, and hashes of what it last sent, so a frame carries only what
changed. Every visual is run through the shared `Engine` with its selection
cut to the fields that can change it (`tidepool_engine::relevant`), which
is what makes sessions on the same dashboard share work. One epoch loop
refreshes every session from the newest snapshot (coalescing); selection
changes are answered at once, outside it. Never hold a session's lock
while calling `Session::send`: it takes the lock itself. A `filter` visual
has no query frame: it's shown from the filter frames of its field
(`Dashboard::lists` adds one list per filter visual whose field the rail
doesn't have). Filter frames are
built in `filters.rs`: lists, and trees (visible nodes depth first through
the session's `open` set, states rolled up, `inside`/`holds`, the chosen
nodes' paths); a filter's `measure` is one grouped query, per depth on
screen for a tree. A selection can
hold windows relative to now (`Chosen::windows`, "Last 7 days"): they are
resolved into ranges with `AppState::clock` (tests stop it with
`set_clock`) whenever a query runs, so the engine never sees one, and the
loop's one-second tick refreshes sessions whose windows the clock moved
(plan §5.5).

### The config pipeline (`tidepool-server::config`)

Every change to what the server runs goes through `config::submit`: a
`ConfigChange` (files to set or remove, against a `base_version`) is
compiled against the catalog, planned, checked against policy
(destructive needs `approve`; `propose` queues it), handed to the writer
(`WriterHandle::change_model`), recorded as a version, written back to the
config directory with `renamed_from` hints stripped, and committed when
the directory is a git repository. One async lock serialises changes; the
directory watcher (`config::look`, every second) reads the directory under
that lock and compares it with what the server last wrote, so write-backs
are never mistaken for edits. `AppState::open` is startup: it recovers the
data's model and applies the directory's difference as a change
(destructive only with `--allow-destructive`). History and the pending
queue live in `<data>/config/`. Sessions learn of a new model in the epoch
loop (`live::follow`). Tests: `tests/config.rs`, which includes the M7 exit
criterion.

### The oracle (`tidepool-oracle`)

Random schemas, data (written in random batches with upserts, deletes and
epochs) and queries from a seed; every query runs on the fast path, the
general path and a cached `Engine`, and is compared with DuckDB. A few
queries are also replayed over every intermediate snapshot through one
cached engine and compared with uncached answers, which is what catches a
cache that outlives its data. The incremental oracle (`check_live`) goes
further: writes go to Tidepool and DuckDB one at a time, and after every
epoch a few live queries on one cached engine must equal DuckDB's cold
answer as of that epoch. `f0` is sometimes keyed, so fact rows get
rewritten and deleted after they were counted. A failure names the
seed and the query: `TIDEPOOL_ORACLE_SEED=<n> cargo test -p tidepool-oracle`
reproduces it. Debug builds use 13-row morsels so small data still splits
segments. When you add a query feature, extend the generator so the oracle
covers it, and check it would catch a planted bug in the new code.
`check_migration` writes each project's data under an earlier version of
it (no relationships, other encodings, an extra column), changes the
model midway with a background build and writes going on, then checks
every query.

### Agents, metrics, the quick-start

- **MCP** (`mcp.rs`): JSON-RPC at `POST /mcp`, hand-written (no `rmcp`).
  Every tool is an in-process call of the documented API with the caller's
  credentials (`tower::ServiceExt::oneshot` on `api_router`), so a tool
  can't do or answer anything its endpoint wouldn't. The one policy of its
  own: `apply_change` applies what `agents.auto_apply` allows and proposes
  the rest. A new tool wraps an endpoint the same way and gets a case in
  `tests/mcp.rs`.
- **`/metrics`** (`metrics.rs`): gauges read from the snapshot when
  scraped, counters (`Counters` in `AppState`) kept as requests come in,
  timings as `Timing` (`_sum`/`_count`). Names: `tidepool_…`, `_total`,
  `_seconds`, `_bytes`.
- **`tidepool init` / `tidepool feed`** (`crates/tidepool/src/quickstart.rs`):
  the retail example compiled into the binary, and a dependency-free
  feeder. `tests/quickstart.rs` runs init → validate → serve → feed →
  query, timed.
- **`time_offset` metrics** move their time field's *column* (every read of
  it, including derived fields over it) by the offset, in their own anchor
  plan (`ShiftOn`, `AccessShift` in `plan.rs`); the fast path shifts values
  as they load (`move_in_time`). The oracle generates them.

User documentation is the doc site in `website/` (VitePress, published to
propell.dev/tidepool); keep it true when behaviour changes (it's what a user
reads, unlike the plan). The prose is written by hand in the copy style of
`landing/copy-style.md` (Simplified Technical English, lowercase headings);
the reference (config files, HTTP API, schemas, MCP tools, `openapi.json`)
is generated by `tidepool-docsgen` (`just docs`) and committed.
`crates/tidepool-docsgen/tests/site.rs` fails `just ci` when the committed
reference drifts from the source, when a generated partial isn't included
by a page, or when a YAML sample on the site that names its file
(`# tables/orders.yaml`) doesn't compile in place of that file in the
retail example (other samples start with `# fragment`). So: a changed doc
comment, endpoint, tool or retail file needs `just docs`; a new config kind
needs a page that includes its partial. `website/readme.md` says where each
kind of change goes. The landing page is `website/.vitepress/theme/Landing.vue`,
written from `landing/`.

### UI serving

`tidepool_server::ui`. Every path the API doesn't match falls through to
`ui::serve`: unknown `/v1/...` is a JSON 404 (never the app), existing files
are served with their content type, extension-less paths are client routes and
get `index.html`, the rest is 404. Files under `assets/` are cached as
immutable (bundlers hash those names); `index.html` is `no-cache`.

The UI comes from one of three places: compiled in (`embed-assets` feature,
off by default so `cargo check` never needs a frontend build), a directory
(`--ui-dir`, for development), or neither (a placeholder page).

### The frontend

`frontend/` is a Leptos 0.8 client-side-rendered app (decision D10, from
`spikes/leptos-charts`). trunk builds it into `frontend/dist`, with every hashed
output under `assets/` (`data-target-path` in `index.html`) so the server's
immutable-cache rule applies to them. It depends on `tidepool-protocol`
directly: request and response types are shared, never generated or copied.

The look is Radix Themes' (slate, teal accent, ruby for the selection;
the system's fonts, fetched from nowhere): `style.css` defines Radix's colour steps once,
light and dark, and the rules use only its named tokens (`--ink`, `--line`,
`--tide`, `--anemone`…). Every page has the top bar (sections, the ⌘K
palette, the pulse) and a status bar (`/v1/status` every 5 s); a dashboard
adds the context bar (where you are, the selection, "answered in", timed by
`InFlight`).

Every metric value on screen goes through `board::formatted` with the
format that applies to it (`VisualInfo::formats`, `FilterInfo::measure_format`):
the grammar is `tidepool_config::format` (one parser, for the compiler and the
UI), and a format is display only, never applied to what's queried or exported.

Pages (hash routes, `route.rs`): the dashboard list, dashboards, the
dashboard editor (`designer.rs`), the model browser (tables with an ingest `curl`, a relationship diagram, the metric
catalog; from `GET /v1/model`), the config pages (`configui.rs`: the files
edited with the plan checked as you type, history with diffs and revert,
the approval queue) and a status page (`GET /v1/status`, polled).
`api.rs` sends a `?token=` the page was opened with as the bearer token;
the config pages need the admin one.
One `Live` (`live.rs`, in context) owns the WebSocket to `/v1/ws` (keeping
a `?token=` from the page URL) and the signals its messages feed; frames
are applied to a `Board`. Rules the dashboard follows:

- **Nothing that receives clicks is rebuilt per frame.** Each filter and
  visual reads its own entry through a `Memo`; a chart is one `BarChart`
  or `LineChart` for the visual's life, fed a data memo. State that must
  outlive a frame (a table's sort) lives in `Live`, not in the rebuilt view.
- **Charts are our own** (no chart library). `scale.rs` (linear and band
  scales, nice ticks) and `charts.rs` (`BarLayout`, `LineLayout`: where
  every mark and label goes for a size, `hit`/`nearest` for which category
  a position means, `span` for a brush, `blend` for the frames between two
  layouts) have no DOM and get plain tests. `plot.rs` draws them as SVG,
  coloured only by classes in `style.css` (so the theme applies), and
  handles pointer events once on the chart's box, turning positions into
  categories through the layout: the marks carry no handlers, so a frame
  may rebuild them mid-click. A click or a drag selects (a drag across
  bars chooses several; across a line, a range), and the keyboard does the
  same (arrows, Shift to extend, Enter; `aria-activedescendant` names the
  place). Text is measured with a canvas (`plot::measure`).
- Logic without a DOM goes in `board.rs`, `charts.rs`, `scale.rs`, `route.rs`,
  `calendar.rs` (the date range picker's days, months, typed bounds and
  half-open ranges; `datepick.rs` draws it, no native date inputs),
  `commands.rs` (what the ⌘K palette offers and how a query ranks it;
  `palette.rs` draws it),
  `textdiff.rs`, `highlight.rs` (the config editor's YAML colours, drawn
  by a `<pre>` behind a transparent textarea), `layout.rs` (the editor's grid: snapping, moving,
  resizing, never overlapping), `diagram.rs` (the relationship diagram's
  cards, tree layout and Kayak's orthogonal routing) or `design.rs` (which visual types fit
  which fields, ids, what fields can go together) and gets a plain test.
- **The dashboard editor edits the file.** It reads and writes
  `dashboards/<id>.yaml` with `tidepool-config`'s own types
  (`DashboardFile::from_yaml`/`to_yaml`), plans each draft on the server,
  and saves through `/v1/config/apply`, like every other change. Its drag
  is pointer events with pointer capture on the tile, followed on the
  canvas; nothing moves until a drop on free cells. Future editors for
  other objects should work the same way. The browser path is covered by `just e2e`
  (`crates/tidepool/tests/e2e.rs`): headless Chrome over the DevTools
  protocol against the built UI (on a copy of the example: config
  changes are written back), clicking a bar, watching a live update,
  going back, brushing the trend, dragging across bars, choosing with the keys, adding a table in the config editor, and making a dashboard
  in the dashboard editor (dragging and resizing with real mouse events). It needs Chrome, so it isn't in `just ci`; run it after UI
  changes.

It is a workspace member, so `just ci` lints and tests it natively.

## Notes

- Spikes live in `spikes/`, outside the workspace, and are kept for reference
  once their decision is recorded in the plan.
- `examples/indud-oee/` is an OEE project over indud's namespace (a
  hierarchy fed by Kayak's `indu_namespace` input with `write: replace`;
  `simulate.sh --sample` runs it without indud).
- `examples/northwind/` (`just northwind`) is the realtime-BI test bed:
  Northwind's dimensions in Postgres, read whole by Kayak every 5 s
  (`snapshot`) into upsert tables (unchanged rows skipped), and a publisher
  replaying its orders as events through Kayak's `http` input;
  `freshness.py` measures event → queryable.
- `examples/pglogs/` (`just pglogs`) is the logs test bed: Postgres
  logging everything as jsonlog, a Python shop app that makes errors on
  purpose, a stdlib shipper tailing the log into Tidepool (positions
  saved, idempotency keys from where a batch starts). `sample.ndjson` is
  a real run's lines, used by `tidepool-server/tests/pglogs.rs`; refresh
  it from a running stack when the shipper's mapping changes.
- `examples/retail/` is the example project. Keep it covering every config
  feature as features land. A server run on it writes applied changes back
  into it (`git_commit: false` keeps them out of history); the demo in
  `.claude/launch.json` runs on a copy in `target/demo-config` instead.

# Tidepool: implementation plan

Status: draft v2 · 2026-10-07 (v2: Kayak integration, licensing, build tooling, frontend spike)
Background and the alternatives we rejected: [architecture-options.md](architecture-options.md) (Tidepool is "Architecture B" there).

Tidepool is a live, associative analytics server. Data is pushed in over HTTP and held in a compressed, dictionary-encoded in-memory column store. A semantic model (tables, relationships, metrics) is declared as code. Dashboards update continuously as data arrives and respond instantly to selections. It ships as one Rust binary with the web UI built in.

---

## 1. Decisions so far

| # | Decision | Notes |
|---|---|---|
| D1 | Build the engine from scratch in Rust | Use ecosystem building blocks (arrow-rs, sqlparser-rs, roaring, rayon, tokio/axum), but no external query engine on the hot path. |
| D2 | Single node, moderate scale | Target up to ~500M fact rows on one machine. No sharding or distribution in scope. |
| D3 | Single tenant | One model per server process. |
| D4 | Single binary | Server, ingest, UI assets, MCP endpoint and CLI subcommands all in `tidepool`. |
| D5 | One-directional filtering (v1) | Selections flow from the one-side to the many-side of a relationship (dimension → fact). Bidirectional "possible values" is a planned later addition; the selection engine is designed so it can be added. |
| D6 | Ingest mode declared per table | `append` (default for facts, cheapest) or `upsert` (delete the old version + append the new one; default for dimensions). |
| D7 | Config as code, with live apply | Config files are the source of truth. Every change, whether from files, the UI, the API or an agent, goes through one validate → plan → apply pipeline and is written back to the files. |
| D8 | Semantic layer in Apache Ossie | Written in the **0.2** shape (flat at the document root, as on Ossie's main branch); 0.1.1's wrapped `semantic_model:` shape is accepted and unwrapped. Expressions are read in `OSSIE_SQL_2026` (Ossie's SQL:2003 subset, its default dialect from 0.2), falling back to `ANSI_SQL`. Tidepool-specific semantics live in `custom_extensions` with `vendor_name: TIDEPOOL` and JSON `data`, as the spec defines. Internally everything compiles to our own Model IR. (Decided in M1, 2026-10-07; the earlier "pin 0.1.1" assumed a format 0.1.1 doesn't have.) |
| D9 | Expressions are SQL | Row and aggregate expressions use SQL syntax (parsed with sqlparser-rs). Metric modifiers are structured YAML, not a new language. |
| D10 | Frontend is **Leptos (client-side rendered, built with trunk)**, embedded in the binary | Decided by the M0 spike (2026-10-07, `spikes/leptos-charts/README.md`): the whole ECharts wrapper is 120 lines; searching 50k rows takes 4–5 ms; a selection click re-renders everything in 5–14 ms. One stack with Kayak, and the UI uses `tidepool-protocol` types directly. Client-side only rather than Kayak's SSR + hydrate: a logged-in dashboard app gains nothing from server rendering, and a static `dist/` keeps embedding trivial. |
| D11 | Every update batch ("epoch") is consistent | Every result the server sends is tagged with an epoch. A client never shows a mix of results from different epochs on one screen. |
| D12 | Kayak is the connector layer | Tidepool only ingests over HTTP, permanently. Kafka, MQTT, NATS, OPC UA, databases and so on are reached through Kayak pipelines with a `tidepool` output (§15). Tidepool never grows transforms, and Kayak never grows dashboards. |
| D13 | Licence split, same as Kayak | `tidepool-protocol` and `tidepool-config` (the types any client needs) are **Apache-2.0**. Everything else (server, engine, frontend) is **AGPL-3.0-or-later**, with a commercial licence available. Every crate declares `license`, and a test enforces it. |
| D14 | Reuse Kayak's conventions and solved problems | `just` for tasks, `serde_norway` for YAML, the `embed-assets` feature with rust-embed, one endpoint table that generates routes + auth roles + OpenAPI + docs, Kayak's auth stack (basic / session cookie / JWT with the iframe embedding handshake), `${NAME}` secrets, a bench crate, clippy pedantic with `unwrap_used`/`expect_used`, and a "definition of done" in CLAUDE.md. Copy first; extract a shared crate only if a third project needs it. |

---

## 2. Goals, non-goals, targets

### 2.1 Goals for v0.1

- Declare tables in YAML and get a typed HTTP ingest API for them immediately.
- Declare relationships and metrics in Ossie YAML. Declare dashboards in YAML.
- Dashboards in the browser: KPI, bar, line, table and filter widgets. Clicking a chart selects; selections apply to the whole dashboard; back/forward through past selections.
- Results update continuously (default refresh every 200 ms), stay consistent across the screen, and are shared across sessions with the same view.
- Durable: survives `kill -9` without losing acknowledged data and restarts in seconds.
- Live config changes (add a table, a metric or a dashboard) without a restart, written back to the config files.
- An agent can inspect the model, query metrics, and propose/plan/apply changes over MCP.

### 2.2 Non-goals for v0.1

Distribution or sharding · multi-tenancy · row-level security · bidirectional filtering · drag-and-drop dashboard builder · visual relationship editor · any ingest connector other than HTTP (that's Kayak's job, D12) · full SQL query surface · mobile layouts · user management.

### 2.3 Performance targets

Reference machine: 16 cores, 64 GB RAM, NVMe SSD. Reference dataset: retail star schema with 100M `orders` rows, 1M customers, 100k products.

| Metric | Target |
|---|---|
| Sustained ingest (Arrow IPC, append) | ≥ 500k rows/s |
| Sustained ingest (NDJSON, append) | ≥ 100k rows/s |
| Freshness: ingest ack → visible on all live dashboards | p95 < 1 s (at 50 sessions, 10 distinct views) |
| Selection change → all visuals updated (cold, 100M rows) | p95 < 100 ms |
| Selection change (cached / toggled back) | p95 < 20 ms |
| Per-epoch refresh cost per live view | proportional to new rows, not to total rows |
| Restart (100M rows persisted) | < 10 s until queries can run |
| Memory for reference dataset | < 8 GB resident |

---

## 3. The project as code

### 3.1 Config directory layout

```
my-project/
├── tidepool.yaml             # server settings (not part of the data model)
├── tables/
│   ├── orders.yaml           # one file per table: adding a table = adding a file
│   ├── customers.yaml
│   └── products.yaml
├── model.ossie.yaml          # semantic layer (Apache Ossie + tidepool extensions)
└── dashboards/
    └── sales.yaml
```

The data directory is separate and is the server's internal state (see §4.7). Running `tidepool serve --config ./my-project --data ./data` is the whole deployment.

### 3.2 `tidepool.yaml`

As of M1 the server config accepts `listen`, `data_dir`, `epoch_interval`, `memory_budget` and `durability`; `auth`, `config_writeback` and `agents` are added with the milestones that implement them (M7, M8), since unknown keys are rejected.

```yaml
listen: 0.0.0.0:7070
data_dir: ./data                 # overridable with --data
epoch_interval: 200ms
memory_budget: 75%               # of system RAM, for data + caches
cache_budget: 15%                # share of memory_budget reserved for result/partial caches
durability: fsync                # fsync | async (ack before fsync)
auth:
  admin_token_env: TIDEPOOL_ADMIN_TOKEN    # config changes, MCP apply
  ingest_token_env: TIDEPOOL_INGEST_TOKEN  # ingest endpoints
  viewer: open                             # open | token
config_writeback:
  enabled: true
  git_commit: true               # commit applied changes if config dir is a git repo
agents:
  auto_apply: [safe]             # change classes an agent may apply without human approval
```

### 3.3 Table file

```yaml
# tables/orders.yaml
name: orders
mode: append                     # append | upsert
event_time: ordered_at           # optional; used for zone maps / time pruning
retention: 730d                  # optional; based on event_time
columns:
  order_id:    { type: string, encoding: plain }     # high-cardinality id: don't dictionary-encode
  customer_id: { type: string }
  product_id:  { type: string }
  status:      { type: string }
  ordered_at:  { type: timestamptz }
  quantity:    { type: int32 }
  amount:      { type: "decimal(14,2)" }
  note:        { type: string, nullable: true, encoding: plain }
```

```yaml
# tables/customers.yaml
name: customers
mode: upsert
primary_key: [customer_id]
columns:
  customer_id: { type: string }
  name:        { type: string, encoding: plain }
  region:      { type: string }
  segment:     { type: string, nullable: true }
  # renamed_from: old_name   ← an explicit rename hint (see §8.4)
```

**Types (v1):** `bool`, `int32`, `int64`, `float64`, `decimal(p,s)` (p ≤ 38, stored as a scaled i128; i64 when p ≤ 18), `string`, `date`, `timestamp`, `timestamptz` (stored as UTC microseconds).

**Encoding defaults:**
- `string`, `bool`, `date` → `dict`
- numeric and timestamp types → `plain`
- override per column with `encoding: dict | plain`
- columns used as relationship keys are always `dict` with a shared dictionary (enforced by the compiler)

Columns are non-nullable unless `nullable: true` is set. Ingest rejects rows that violate types or nullability, and returns per-row errors.

### 3.4 Semantic model (`model.ossie.yaml`)

The structure follows the Ossie core spec, 0.2 shape (D8). Every column of a dataset's table is implicitly a field of the dataset under its own name; declaring a field adds a label, a description, a time role, or a derived expression. The file below is `examples/retail/model.ossie.yaml`.

```yaml
version: "0.2.0"
name: retail
datasets:
  - name: orders
    source: orders                       # table name from tables/*.yaml
    fields:
      - name: ordered_at
        expression: { dialects: [{ dialect: ANSI_SQL, expression: ordered_at }] }
        datatype: DateTimeTz
        dimension: { is_time: true }
      - name: order_year                 # derived field: a cheap dimension (see §6.2)
        expression: { dialects: [{ dialect: ANSI_SQL, expression: "EXTRACT(YEAR FROM ordered_at)" }] }
        datatype: Integer
      - name: status
        expression: { dialects: [{ dialect: ANSI_SQL, expression: status }] }
  - name: customers
    source: customers
    primary_key: [customer_id]
    fields:
      - name: region
        expression: { dialects: [{ dialect: ANSI_SQL, expression: region }] }
  - name: products
    source: products
    primary_key: [product_id]
    fields:
      - name: category
        expression: { dialects: [{ dialect: ANSI_SQL, expression: category }] }

relationships:
  - name: order_customer
    from: orders                         # many side
    to: customers                        # one side
    from_columns: [customer_id]
    to_columns: [customer_id]
  - name: order_product
    from: orders
    to: products
    from_columns: [product_id]
    to_columns: [product_id]

metrics:
  - name: revenue
    description: Gross order value
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
  - name: paid_revenue                   # SQL's standard FILTER clause instead of set analysis
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount) FILTER (WHERE orders.status = 'paid')" }] }
  - name: order_count
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "COUNT(*)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"table": "orders"}'      # anchor table for COUNT(*)
  - name: customer_count
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "COUNT(DISTINCT orders.customer_id)" }] }
  - name: avg_order_value
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "revenue / order_count" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"kind": "derived"}'      # expression over other metrics, computed after aggregation
  - name: revenue_all_regions
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"ignore_selection": ["customers.region"]}'
  - name: revenue_prev_year
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"time_offset": "-1y", "time_field": "orders.ordered_at"}'

custom_extensions:
  - vendor_name: TIDEPOOL
    data: '{"filter_direction": "one_to_many"}'
```

**Tidepool extension vocabulary (v1):**

| Where | Key | Meaning |
|---|---|---|
| metric | `kind: derived` | expression references other metrics; evaluated after aggregation |
| metric | `table` | anchor table for expressions that don't name one (`COUNT(*)`) |
| metric | `ignore_selection: [field…]` | evaluate as if these fields had no selection (`"*"` = all) |
| metric | `time_offset`, `time_field` | evaluate with the time selection shifted (M8) |
| metric | `format` | display hint: `currency:EUR`, `percent`, `int`, … |
| field | `grain` | for time fields: the grains a visual may group by, as a list (`day`, `week`, `month`, `quarter`, `year`) |
| relationship | `role` | role name, for when two relationships connect the same pair of tables (e.g. order date vs ship date) |
| model | `filter_direction` | `one_to_many` (v1 only value) |

Only `OSSIE_SQL_2026` and `ANSI_SQL` are read (in that order of preference). Other dialects in the file are kept but ignored, so a model can be shared with other Ossie tools. A YAML gotcha the validator explains: in a `{ … }` flow mapping a comma ends the value, so write `type: "decimal(14,2)"` quoted.

### 3.5 Dashboard file

```yaml
# dashboards/sales.yaml
name: Sales overview
layout: { columns: 12, row_height: 80 }
filters:
  - field: customers.region
  - field: products.category
  - field: orders.ordered_at
    widget: date_range
visuals:
  - id: kpi_revenue
    type: kpi
    metrics: [revenue]
    compare: revenue_prev_year
    at: { x: 0, y: 0, w: 3, h: 2 }
  - id: by_region
    type: bar
    dimensions: [customers.region]
    metrics: [revenue]
    sort: { by: revenue, desc: true }
    on_click: select
    at: { x: 3, y: 0, w: 5, h: 4 }
  - id: trend
    type: line
    dimensions: [{ field: orders.ordered_at, grain: day }]
    metrics: [revenue, order_count]
    on_brush: select_range
    at: { x: 0, y: 4, w: 12, h: 4 }
  - id: top_products
    type: table
    dimensions: [products.name, products.category]
    metrics: [revenue, order_count, avg_order_value]
    sort: { by: revenue, desc: true }
    limit: 50
    at: { x: 8, y: 0, w: 4, h: 4 }
```

### 3.6 Machine-readable schemas

Every config file kind has a JSON Schema, generated from the Rust types with `schemars` and served at `/v1/schemas/*.json`. The UI's YAML editor and external editors (via the `yaml-language-server` modeline) use them for completion and validation. Agents use them to write valid changes.

---

## 4. System architecture

### 4.1 Crate layout (cargo workspace)

```
tidepool/
├── Cargo.toml                    # workspace
├── crates/
│   ├── tidepool-config/          # [Apache-2.0] serde types for all YAML files (serde_norway), parsing, JSON Schema, error spans (miette)
│   ├── tidepool-model/           # Model IR, compiler (config → IR), validation, diff/plan, catalog (stable IDs)
│   ├── tidepool-expr/            # SQL expressions: syntax + reference analysis (M1), type checking, IR, vectorized kernels (M3)
│   ├── tidepool-store/           # types, dictionaries, segments, tail, PK index, delete bitmaps, epochs/snapshots, WAL, persistence
│   ├── tidepool-engine/          # selection engine, aggregate states, executor, caches, subscription manager
│   ├── tidepool-protocol/        # [Apache-2.0] wire types (shared with the wasm frontend), the endpoint table, Arrow frame encoding
│   ├── tidepool-server/          # axum: ingest, query, admin/config, WebSocket, MCP, static assets, auth
│   ├── tidepool-oracle/          # test-only: DuckDB reference evaluator + random model/data/query generators
│   └── tidepool/                 # the binary: CLI (serve, validate, plan, init) wiring it all together
├── frontend/                     # Leptos CSR app (tidepool-frontend), built by trunk into frontend/dist
├── tidepool-bench/               # load generator + benchmark scenarios (like kayak-bench)
├── justfile                      # dev, run, build, frontend, ci, test, lint, deny, miri, bench (same verbs as Kayak)
├── examples/retail/              # example project + synthetic data generator config
└── docs/
```

Dependencies flow strictly one way: `config → expr → model → store → engine → server → tidepool`. `expr` comes before `model` because the model compiler resolves names in expressions; type checking (M3) takes the schema through a trait the model implements. `protocol` depends only on serde/schemars.

### 4.2 Runtime overview

```
                       ┌────────────────────────────── tidepool (single process) ───────────────────────────────┐
 POST /v1/tables/*/rows│  ingest handlers ──► decode+validate ──► WAL append (group fsync) ──► writer thread     │
 (JSON/NDJSON/Arrow)   │                                                                       │ encode,dict,    │
                       │                                                                       │ PK index, tail  │
                       │                          epoch ticker (200 ms, when dirty) ◄──────────┘                 │
                       │                                   │ publishes Snapshot{epoch, tables…} via ArcSwap      │
                       │                                   ▼                                                     │
 WebSocket /v1/ws ────►│  sessions (selection state, history) ──► subscription manager ──► executor (rayon)      │
   ◄── epoch frames ───│        ▲                                   dedupe · caches · priorities                 │
                       │        │ model version change                                                          │
 Admin/MCP/files ─────►│  config pipeline: parse → compile → plan → apply (at epoch boundary) → write back       │
                       │                                                                                        │
 GET /                 │  embedded SPA (rust-embed)        background: sealing · checkpoint · retention · rebuilds│
                       └────────────────────────────────────────────────────────────────────────────────────────┘
```

**Threads:**
- tokio runtime for I/O (HTTP, WS, file watching)
- **one writer thread** that owns all mutation of the store (keeps v1 simple and deterministic, which matters for WAL replay)
- an epoch ticker task
- a rayon pool for query execution and background jobs (sealing, rebuilds)

### 4.3 Model IR (`tidepool-model`)

The compiled, validated, immutable model. Swapped atomically (`ArcSwap<Model>`) when config changes.

```rust
struct Model {
    version: ConfigVersion,            // monotonically increasing
    tables: IdMap<TableId, Table>,     // physical tables, columns with stable ColumnIds
    fields: IdMap<FieldId, Field>,     // semantic fields: column ref or derived expression
    relationships: Vec<Relationship>,  // many(table, cols) → one(table, cols), role
    graph: RelGraph,                   // DAG; precomputed reachability & unique paths
    metrics: IdMap<MetricId, Metric>,  // compiled MetricPlan
    dashboards: IdMap<DashboardId, Dashboard>,
    key_domains: Vec<KeyDomain>,       // groups of columns sharing one dictionary
}
```

The compiler validates:
- every `source` exists
- field expressions type-check
- relationships point many → one, the one-side columns are the table's PK, and FK/PK types match
- the relationship graph has no cycles
- between any two tables there is at most one path, unless the paths have distinct `role`s
- metric expressions are valid aggregate expressions over tables reachable through many → one paths from one anchor table
- derived metrics have no reference cycles
- every dimension used in a dashboard visual is reachable from every measure's anchor table in that visual

Errors point to file, line and column (miette + YAML spans).

**Key domains:** the columns on both ends of a relationship (and transitively connected ones) form a *key domain* that shares one dictionary. Each domain gets a stable `DomainId` that is stored in the catalog.

### 4.4 Store (`tidepool-store`)

#### Values and dictionaries

- A **dictionary** maps value ↔ `u32` code. Code `0` is reserved for NULL.
  - Codes are assigned in arrival order and never change while the dictionary lives, so dictionaries are append-only.
  - Strings are kept in a byte arena with an offsets array. Lookups use a hashbrown table keyed by hash into the arena (no duplicate string storage).
- Every dict-encoded column has its own dictionary, except key-domain columns, which share their domain's dictionary.
- **Sort permutation:** maintained lazily per dictionary (`code → rank`). It's rebuilt incrementally, by sorting only the new values and merging, the first time sorted access is needed after the dictionary grows. It's used for ordered filter lists, range filters on dict columns, and sorted output.
- **Derived dictionaries** for single-column derived fields (e.g. `EXTRACT(YEAR FROM ordered_at)` on a dict column, or a date grain): a `code → derived_code` mapping array, extended incrementally as the base dictionary grows. This makes derived dimensions as cheap as stored ones.
- **High-cardinality guard:** if a dict column's dictionary passes a configured limit (default 50M entries), log a warning and show it in the model browser. The plan suggests `encoding: plain`.

#### Segments

```
Table
 ├── segments: Vec<SegmentView>      immutable: sealed (bit-packed) and chunks (plain), each with a
 │                                    copy-on-write delete bitmap and a version; consecutive row ids
 ├── builder                          rows applied since the last epoch; the writer's alone
 └── pk_index (upsert tables only):  key codes → row id (dense array for a single dict key)
```

**As built in M2 (changed from the original design):** there is no shared,
concurrently-written tail. At each epoch the writer *freezes* its builder into
an immutable chunk; a snapshot is a list of `Arc`s to immutable chunks and
sealed segments. No `unsafe`, no loom, and readers can never observe a
half-written row. Chunks are sealed together once they hold `seal_rows` rows
or there are more than 64 of them (a slow trickle would otherwise leave many
tiny chunks). Row ids never change, so the primary-key index survives sealing
untouched. Dictionaries work the same way: values in fixed 4096-value blocks,
full blocks shared by `Arc`, only the partial block copied per epoch. Sealing
runs on the writer thread for now (≈ hundreds of ms per million rows); moving it
to a background job is a later optimisation.

- **Segment size:** seal at 1,048,576 rows (configurable per table; smaller for dimensions). A segment holds the rows of one table for one contiguous row range.
- **Sealed segment column formats:**
  - dict columns: bit-packed codes at ⌈log₂(dict len at seal time)⌉ bits
  - integers, decimals, timestamps: frame-of-reference + bit-packing
  - floats: raw
  - strings with plain encoding: arena + offsets
  - NULL bitmaps for nullable plain columns
- **Zone maps per sealed segment:** min/max per numeric/time column, and min/max code plus a small code bitset (when cardinality is small) per dict column. Used to skip whole segments for selections.
- **Sealing:** when the chunks are full (see above), they are concatenated and compressed into one segment with min/max per integer and code column; in M5 the segment is also written to its file (§4.7).
- **Upsert:** look up the key in `pk_index`.
  - If found, set the delete bit for the old row: in the tail via a tail delete bitmap, in a sealed segment via copy-on-write of that segment's delete bitmap, bumping its `version`.
  - Append the new row and update the index.
  - The index key is the PK code (single dict column) or a packed tuple of codes (composite key).
- **Column-change detection on upsert:** compare old and new codes per column and record the changed `ColumnId`s per table per epoch. Cache invalidation (§6.5) uses this to ignore upserts that don't touch any column a view depends on, e.g. a `last_seen` timestamp that nobody groups by.
- **Delete-by-key** (`DELETE /v1/tables/{t}/rows` with keys) is supported for upsert tables only.

#### Epochs and snapshots

- The writer applies batches to the tail and dirty state. The epoch ticker fires every `epoch_interval`. If anything changed, it publishes a new `Snapshot` (via `ArcSwap`) that contains, per table:
  - the `Arc` list of sealed segments with their delete-bitmap versions
  - the tail watermark
  - the tail delete bitmap
  - dictionary lengths
  - the changed-columns set since the previous epoch
- A reader holds an `Arc<Snapshot>` for the whole evaluation, which gives snapshot isolation without locks.
- Epochs are global, across all tables. A single number makes cross-visual consistency trivial.

### 4.5 Ingest

```
POST   /v1/tables/{table}/rows        body: application/json (array) | application/x-ndjson | application/vnd.apache.arrow.stream
DELETE /v1/tables/{table}/rows        body: keys (upsert tables only)
headers: Authorization: Bearer <ingest token>, Idempotency-Key: <string> (optional)
query:   ?wait=visible                (respond only after the rows' epoch is published)
→ 200 { "accepted": 5000, "epoch": 81723, "config_version": 12 }
→ 400 { "errors": [ { "row": 17, "column": "amount", "message": "expected decimal(14,2)" } ] }   # whole batch rejected
```

- **One route for all tables.** The handler looks up the table in the current `Model`, so new tables work immediately without rebuilding the router.
- **Decoding:**
  - JSON/NDJSON: `simd-json` or `serde_json` into column builders, validated against the table's column types.
  - Arrow IPC: schema checked for compatibility (names, types castable), with zero-copy where possible.
  - Output in every case: a normalized Arrow `RecordBatch`.
- **Batches are atomic:** either the whole batch is accepted or none of it.
- **Durability:** the normalized batch is appended to the WAL. The response is sent after group-commit fsync (`durability: fsync`) or immediately (`async`). Then the batch is queued for the writer thread.
- **Idempotency:** a recent-keys map (table, key) → response, with a 24 h TTL. It's persisted inside the WAL records, so it survives restarts.
- **Backpressure:** a bounded writer queue. When it's full, respond `503` with `Retry-After`.
- **Discovery:**
  - `GET /v1/tables`: list tables
  - `GET /v1/tables/{t}`: schema, row count, ingest rate
  - `GET /v1/openapi.json`: the static endpoints come from the endpoint table (the Kayak pattern: one table drives routes, auth roles, OpenAPI and `/docs`); per-table ingest schemas are added from the current model on each request

### 4.6 Retention

A background job runs per table that has `retention` and `event_time`:
- **Sealed segments** whose `event_time` zone-map max is older than the retention window are dropped whole. Cheap: drop the file and remove the segment from the snapshot at the next epoch.
- **Tail rows** are never dropped individually in v1.

Dictionaries are not compacted in v1, so codes of expired values stay. Dictionary compaction (rebuild + remap at sealing time) is a later item.

### 4.7 Durability and recovery

**Data directory layout:**

```
data/
├── catalog/
│   ├── catalog.json                 # stable IDs: tables, columns, key domains; current config version
│   └── versions/000012/…            # snapshot of each applied config (files + compiled summary)
├── wal/
│   └── 00000000000081723.wal        # rolling WAL files
├── dict/
│   └── {domain_or_column_id}.dict   # append-only value logs, in code order
├── segments/
│   └── {table_id}/{segment_id}.seg  # immutable segment files (mmap'd)
└── checkpoints/
    └── 000000417.ckpt               # LSN, per-table segment lists, delete bitmaps, dict lengths, PK index snapshot ref
```

- **WAL records:** `(lsn, config_version, kind, table_id, payload)`. Kinds are `Append(batch)`, `Upsert(batch)`, `Delete(keys)`, `ConfigApplied(version)`, `SegmentSealed(table, segment_id, row_range)`.
  - Payload is Arrow IPC of the **raw normalized values**, not codes.
  - Each record carries a CRC32C. Group commit: one fsync per flush interval or batch.
- **Determinism:** replaying the WAL from a checkpoint re-encodes values. Dictionaries are truncated to the checkpoint's recorded lengths before replay, so the same values get the same codes in the same order.
- **Checkpoint:** runs periodically (e.g. every 60 s, or after N sealed segments).
  1. Flush dictionaries up to the current lengths.
  2. Write the delete bitmaps.
  3. Write a PK-index snapshot, or rebuild it on recovery from PK columns (for v1, rebuild — simpler).
  4. Record the checkpoint LSN.
  5. Truncate WAL files that are entirely below the oldest kept checkpoint.
- **Recovery:**
  1. Load the catalog.
  2. Load the latest valid checkpoint.
  3. mmap segment files.
  4. Load dictionaries, truncated to the checkpoint lengths.
  5. Rebuild PK indexes.
  6. Replay WAL records after the checkpoint LSN into a fresh tail.
  7. Publish the epoch.
- **Tiering:** sealed segments are mmap'd, so the OS pages cold segments out under memory pressure. Zone-map pruning avoids touching them for selective queries. Explicit eviction policies come later.

---

## 5. Selection engine

### 5.1 Selection state

```rust
struct Selection {                         // per session; immutable values, cheap to clone (Arc)
    fields: BTreeMap<FieldId, FieldSelection>,
}
enum FieldSelection {
    Codes(RoaringBitmap),                  // dict fields: selected values
    Range { lo: Bound<Scalar>, hi: Bound<Scalar> },   // plain numeric/time fields, date_range widget
    Excluded(RoaringBitmap),               // "select excluded" / NOT IN
}
```

Selections are canonicalized and hashed (`SelectionHash`) for caching and dedupe. Each session keeps a history stack for back/forward.

### 5.2 Evaluation (one-directional)

For a snapshot and selection, the *effective filter* of a table T is:

```
filter(T) = local_predicates(T)  ∧  ⋀ over relationships R where T is the many side:
                                       T.fk_R ∈ passing_keys(R.one_side)
passing_keys(D) = { key codes of rows of D satisfying filter(D) }
```

1. **Local predicates on dict fields** are evaluated **on the dictionary, not the rows**. The predicate becomes a code bitset of size `dict_len`, so the row test is one bit lookup per row. Range predicates on plain columns are SIMD compares.
2. **Dimension tables** (small) are evaluated first, top-down through the DAG, producing `passing_keys` as code bitsets in the key domain. This is where the shared dictionary pays off: crossing a relationship is a bitset, not a hash join.
3. **Fact rows:** the row mask per segment is the AND of local predicates and `fk ∈ passing_keys` tests. Zone maps skip segments that can't match, or that match fully.
4. **Masks** are cached per `(table, filter_hash, segment_id, segment_version)` (§6.5). Sealed segments' masks never change unless their delete bitmap changes. *(M3: measured and not built. Branch-free filtering makes a condition about as cheap as reading its mask; see the M3 status. Worth revisiting for expensive conditions such as derived fields or long `IN` lists.)*

When a table has no effective filter, no mask is built; the scan just honours the delete bitmap.

### 5.3 Filter widget states

For a filter on field F of table T, the widget shows each value as one of:
- **selected**
- **possible**: occurs in rows of T passing `filter(T)` computed *without F's own selection* (Qlik semantics: alternatives in the same field stay visible)
- **excluded**: everything else

The possible set is computed as a code bitset by scanning T's passing rows for F's codes. With one-directional filtering, a filter on a dimension field reflects selections on that dimension and on anything upstream of it, but not selections on facts. This is a known limitation of D5, and the UI should show it honestly.

Large filter lists are served windowed: the client requests `(offset, limit, search, sort)` and the server answers from the bitsets and the dictionary sort permutation.

### 5.4 Prepared for bidirectional (later)

Bidirectional propagation adds a reverse pass: `fact passing rows → present key codes → filter dimension rows`. This needs a fixpoint over the relationship tree (one pass up, one pass down). Steps 1–4 above stay the same. The extension point is the `passing_keys` computation; nothing else changes.

---

## 6. Query execution

### 6.1 Visual query (canonical form)

```rust
struct ViewQuery {                         // what a visual (or API query) asks for
    dimensions: Vec<DimRef>,               // field, optional time grain
    metrics: Vec<MetricId>,
    sort: Vec<SortKey>,
    limit: Option<u32>,
}
```

A `ViewQuery` plus the relevant selection makes a subscription. "Relevant" means the selection restricted to fields that can affect the query's anchor tables (§6.6). Restricting first means more sessions share each subscription.

### 6.2 Expressions (`tidepool-expr`)

- Parse with `sqlparser-rs` (generic dialect), then resolve names against the model, type-check, and lower to an `Expr` IR.
- **Row expressions (v1):** arithmetic, comparisons, `AND/OR/NOT`, `CASE`, `COALESCE`, `NULLIF`, `CAST`, `IN`, `BETWEEN`, `LIKE`, `LOWER/UPPER/TRIM/LENGTH/SUBSTRING`, `DATE_TRUNC`, `EXTRACT`, `ABS/ROUND/FLOOR/CEIL`.
- **Aggregates (v1):** `SUM`, `COUNT`, `COUNT(*)`, `COUNT(DISTINCT x)`, `MIN`, `MAX`, `AVG`, plus the standard `FILTER (WHERE …)` clause. Later: `MEDIAN`, `PERCENTILE_CONT`, `APPROX_COUNT_DISTINCT`.
- **Execution:** kernels work on 4096-row chunks of the column formats above. Bit-packed codes are unpacked in chunks into a scratch buffer.
- **Expressions that depend on one dict column** are evaluated once over the dictionary, giving a value array or derived dictionary, and then gathered by code. This is how derived fields and `CASE` on categories stay nearly free.
- **References to one-side columns** in a fact-anchored expression (e.g. `SUM(orders.quantity * products.list_price)`) gather through the FK. The fact's FK code gives the dimension row through the dimension's `code → current row` map (§6.3).

### 6.3 Following relationships

For each one-side table, a dense `Vec<u32>` maps `key code → current row` (or NONE), updated on upsert. A fact row reaches a dimension attribute in two array lookups: `fk_code → dim_row → attr_code`. Fact segments never store row pointers, so dimension upserts never rewrite fact data.

### 6.4 Aggregation

- **Group keys:**
  - *Dense path:* all dimensions are dict-backed (stored, derived or gathered) and the product of their cardinalities is ≤ 2²⁰. The group id is computed arithmetically and the states live in flat arrays.
  - *Hash path:* otherwise, a hashbrown map keyed by packed codes, or by hashed values for plain-column dimensions.
- **Mergeable aggregate states:**

  | Aggregate | State | Merge |
  |---|---|---|
  | SUM / COUNT / AVG | sum, count (decimals: i128) | add |
  | MIN / MAX | value (or code + rank for dict) | min/max |
  | COUNT DISTINCT on dict column | roaring bitmap of codes | OR (exact) |
  | COUNT DISTINCT on plain column | hash set (exact) up to a limit, then HLL | union |
  | MEDIAN / PERCENTILE (later) | dict: code → count histogram; plain: t-digest | add / merge |

- **Each base metric is aggregated on its own anchor table's rows.** Metrics in one view with different anchors are aggregated separately and joined on the group key afterwards. Row-level joins between two fact tables never happen, so fan traps can't occur.
- **Derived metrics** are evaluated over the finalized states.
- `sort` and `limit` are applied after the final merge.

### 6.5 Caches and incremental maintenance

The key idea: **sealed segments are immutable, so their partial results can be cached and reused forever.** Each epoch only has to compute the tail and merge.

```
result(view, selection, epoch) =
    finalize( merge( partial(seg_0), …, partial(seg_k),  partial(tail) ) )
                    └──── cached, keyed by segment version ────┘   └─ incremental ─┘
```

**Cache tiers** (all in one memory-budgeted, cost-aware LRU):
1. **Mask cache:** `(table, filter_hash, segment_id, seg_version) → RowMask`
2. **Partial cache:** `(agg_plan_hash, filter_hash, segment_id, seg_version, dim_version_vector) → Arc<PartialAgg>`
3. **Result cache:** `(view_hash, selection_hash, epoch) → Arc<ResultFrame>`. This is shared by every session showing the same thing.

**The tail partial is incremental** for append-only tails: `partial(tail @ N) = merge(partial(tail @ N−1), aggregate(rows appended since N−1))`. When the tail has deletes since N−1 (upserts that replaced tail rows), recompute the tail partial. That costs at most one segment's worth of rows.

**Invalidation rules:**

| Event in epoch | What becomes stale |
|---|---|
| Rows appended to tail | tail partial (updated incrementally) |
| Tail sealed into segment k | nothing; tail partial is re-keyed as partial(seg_k) |
| Upsert/delete hits sealed segment s | entries with `seg_version` of s (automatic, since the key changes) |
| Dimension table D has changed columns C | `dim_version_vector` bumps for (D, C). Partials that gather or filter through any column in C are stale. Partials that don't depend on C stay valid. |
| Config change affecting a metric/field | `agg_plan_hash` changes, so it's naturally a miss |
| Selection change | new `filter_hash`, so it's a miss. Cold compute, then incremental. Old entries stay in the LRU, so toggling back is instant. |

**Known cost:** a dimension upsert that changes a column a view groups or filters by makes that view rescan all fact segments. That's fine for slowly changing dimensions. The later optimization ("late binding": partials keyed by FK code for low-cardinality keys, mapped to attributes at merge time) removes it; see §12.

### 6.6 Subscription manager

- A **subscription** = `(ViewQuery, relevant Selection)`, keyed by hash and refcounted by sessions. The refcount is why several users on one dashboard cost about the same as one.
- **Per epoch:** find subscriptions whose dependency set (tables, plus changed columns via `dim_version_vector`) intersects the epoch's changes. Re-evaluate them in parallel and produce new result frames.
- **Priorities:**
  1. interactive (selection just changed in some session)
  2. epoch refresh for visible dashboards
  3. background warming
- **Coalescing:** if evaluation for epoch N is still running when N+1 is published, skip ahead to the latest epoch. Clients always get a consistent, newer frame.
- **Frames:** for each session, after all of its subscriptions are evaluated for epoch N, the server sends **one frame** containing every changed visual's result for N. The client applies a frame atomically. That's how D11 is enforced.
- **Payloads:** full results in v1 (visual results are small after grouping and `limit`). Row-level deltas are a later optimization for large tables.

### 6.7 One-shot query API

`POST /v1/query` takes `{metrics, dimensions, filters, sort, limit}` and returns JSON or Arrow. It uses the same engine without a subscription. It's used by agents, tests (the oracle), CSV export and scripts. It's built before the live path (M3).

---

## 7. Client protocol (WebSocket `/v1/ws`)

Binary WS messages with a framed format: `u32 header_len | header (JSON) | payloads (Arrow IPC streams)`. JSON-only messages are allowed when there's no payload. Types are defined in `tidepool-protocol` and used directly by the server and the wasm frontend.

**Client → server:**

| Message | Purpose |
|---|---|
| `hello { session_id?, token? }` | resume or create session (session id stored in localStorage) |
| `open_dashboard { id }` | subscribe all its visuals and filters |
| `select { field, values, mode: replace\|toggle\|add }` | change selection (click, list box) |
| `select_range { field, lo, hi }` | brush on time axis / date range |
| `clear { field? }` / `back` / `forward` | selection management |
| `filter_window { filter_id, offset, limit, search?, sort? }` | page/search a filter list |
| `pause` / `resume` | freeze live updates for this session (the server keeps the last frame) |

**Server → client:**

| Message | Purpose |
|---|---|
| `welcome { session_id, model_version, epoch }` | |
| `model { version, dashboards… }` | sent on connect and whenever the config version changes |
| `selection { state, can_back, can_forward }` | authoritative selection state after each change |
| `frame { epoch, visuals: [{ id, payload_idx \| error }], filters: [{ id, counts, window }] }` + Arrow payloads | atomic update unit |
| `status { epoch, ingest_rate, lag_ms }` | for the live indicator, ~1 Hz |

---

## 8. Config pipeline (code and a live server, reconciled)

### 8.1 One pipeline for every change

```
source: startup │ file watcher │ PUT /v1/config │ UI editor │ MCP propose_change
            ▼
   ConfigSet (full set of files) + base_version
            ▼
   parse → compile (Model IR) → diff vs current → Plan { changes[], class, warnings[] }
            ▼
   class = safe | rebuild | destructive  →  policy check (who, auto-apply rules, approvals)
            ▼
   apply: WAL ConfigApplied(v+1) → catalog update → ArcSwap<Model> at next epoch → background jobs (if rebuild)
            ▼
   write back to config dir (atomic file writes; optional git commit "tidepool: <summary> [by agent:x | user]")
            ▼
   notify sessions: model { version: v+1 }
```

- **Optimistic concurrency:** a change whose `base_version` isn't current is rejected with `409` plus the fresh plan input.
- **File watcher** (`notify`): the server ignores its own writes by comparing content hashes. External edits become a proposed change from source `files`. If the change is safe it's auto-applied. Otherwise it goes to the approval queue and the UI shows "config dir has unapplied changes".
- **Startup** is the same path. The source is `startup`, and it's compared with the last applied version in the catalog. Destructive changes refuse to start without `--allow-destructive`.

### 8.2 Change classes

| Class | Examples | Apply |
|---|---|---|
| **safe** | add table · add nullable column (old segments read it as virtual all-NULL) · add/edit/remove metric · add/edit field expression · add/edit/remove dashboard · change retention · change epoch interval | instant, at next epoch |
| **rebuild** | add relationship between existing populated tables (dictionaries must be merged into one key domain, and one side's key codes remapped) · change encoding · widen type (int32→int64, decimal precision up) · add non-nullable column with default · change PK of upsert table | background job builds new segment versions, swapped at an epoch; progress shown in the UI; old version keeps serving until the swap |
| **destructive** | drop table/column · narrow type · change table mode · remove a relationship used by dashboards | needs explicit approval (human) |

### 8.3 Plans are first-class

A plan is a structured document (JSON) and is rendered in the UI and the CLI:

```
$ tidepool plan --config ./my-project --data ./data
config v12 → v13 (3 changes)
  + table   returns (append, 6 columns)              safe
  + rel     return_order: returns → orders            safe (returns is empty)
  ~ metric  avg_order_value: expression changed       safe   affects 2 dashboards
  ~ column  orders.quantity int32 → int64             rebuild (100M rows, est. 40 s)
```

### 8.4 Stable identity and renames

The catalog assigns stable IDs to tables, columns, key domains, metrics and dashboards. Names are labels. A rename in YAML looks like drop + add, so:
- the planner detects likely renames (same type, the old name disappeared, a new name appeared in the same table) and shows them as warnings, and
- an explicit `renamed_from: <old>` on a table or column makes it a safe rename. After it's applied, the server's write-back removes the hint.

### 8.5 Approval queue

Changes that policy doesn't auto-apply (agent changes above `auto_apply`, or destructive external file edits) wait in a pending queue (`GET /v1/changes`). A human approves or rejects them in the UI or the CLI. The queue is persisted in the catalog.

### 8.6 Version history

Every applied version is kept under `catalog/versions/`. `GET /v1/config/versions` lists them with author and summary. "Revert to vN" is just a new change whose `ConfigSet` is vN's files, so it goes through the same plan/apply.

---

## 9. HTTP API summary

| Area | Endpoints |
|---|---|
| Ingest | `POST/DELETE /v1/tables/{t}/rows` |
| Catalog | `GET /v1/tables`, `GET /v1/tables/{t}`, `GET /v1/tables/{t}/preview?limit=100`, `GET /v1/model`, `GET /v1/openapi.json`, `GET /v1/schemas/{kind}.json` |
| Query | `POST /v1/query` |
| Config | `GET /v1/config` (files + version), `POST /v1/config/plan`, `POST /v1/config/apply`, `GET /v1/config/versions`, `GET/POST /v1/changes[/{id}/approve\|reject]` |
| Live | `GET /v1/ws` (upgrade) |
| Agents | `/mcp` (MCP streamable HTTP, via the `rmcp` crate) |
| Ops | `GET /healthz`, `GET /readyz`, `GET /metrics` (Prometheus) |
| UI | `GET /*` → embedded SPA |

**Auth (v1):**
Since M2: a bearer token per access level (`TIDEPOOL_INGEST_TOKEN`, `TIDEPOOL_READ_TOKEN`, `TIDEPOOL_ADMIN_TOKEN`), enforced by middleware built from each endpoint's declared `Access`; a level without a token is open. M8 then ports Kayak's auth stack instead of designing a new one:
- **Ingest:** bearer token (`TIDEPOOL_INGEST_TOKEN`), constant-time comparison. This is what Kayak's `tidepool` output sends.
- **Roles:** `viewer` and `admin` (config apply, MCP write tools), declared per endpoint in the endpoint table.
- **Humans:** HTTP Basic + session cookie, or **JWT** validated against an identity provider's JWKS.
- **Embedding:** Kayak's iframe handshake (`?auth_token=` exchanged at `POST /v1/auth/token` for a session cookie), so Tidepool dashboards can be embedded in other apps.
- **Off by default on loopback.** The server warns at startup when it's bound to anything else without auth.

### 9.1 MCP tools

| Tool | Notes |
|---|---|
| `describe_model` | tables, columns, relationships, metrics, dashboards, plus `ai_context` from Ossie |
| `preview_table` | latest N rows |
| `query_metrics` | wraps `/v1/query` |
| `get_config` | current files + version |
| `plan_change` | takes full or patched files, returns a plan |
| `apply_change` | applies if policy allows; otherwise queues for approval and returns the change id |
| `ingest_rows` | for agents that generate or forward data (ingest token scope) |

---

## 10. Frontend (v1)

### 10.1 Stack and build

- **Stack:**
  - Leptos 0.8, client-side rendering, built by trunk (D10)
  - Apache ECharts through a small wrapper component (`EChart`, see `spikes/leptos-charts/src/echarts.rs`): an option signal in, click and x-brush callbacks out, `ResizeObserver` sizing. ECharts is vendored with its licence (like Kayak's Scalar).
  - a virtualised list component for filter lists (spike: `vlist.rs`)
  - a small dashboard grid (CSS grid from the YAML layout, read-only in v1)
  - Monaco + monaco-yaml (with the generated JSON Schemas) for the config editor, through the same kind of thin wrapper as ECharts
  - Arrow frames decoded with arrow-rs compiled to wasm, or a lighter frame format if bundle size says so (decide in M4)
- **State:**
  - signals hold `{model, selection, frames by epoch}`; a frame is applied to all of them at once (D11)
  - charts are updated through their option signals, so ECharts merges the new data and animates the change itself
- **Build:**
  - `just frontend` = `trunk build --release` in `frontend/` → `frontend/dist` (hashed files under `assets/`, which the server caches as immutable)
  - `just build` = `just frontend`, then `cargo build --release -p tidepool --features embed-assets`
  - the `tidepool-server` crate embeds `frontend/dist` with `rust-embed` behind the `embed-assets` feature. It's off by default so `cargo check` never needs a frontend build (same reasoning as Kayak).
  - dev: `just dev` runs the server on :7070 and `trunk serve` on :7080 with hot reload, proxying `/v1` to the server
- **Types:** shared directly: the frontend depends on `tidepool-protocol`. There is no codegen step to keep in sync.

### 10.2 Features

**Dashboards**
- Dashboard list. Dashboard view on a 12-column grid from the YAML layout.
- Visuals:
  - KPI tile (value, comparison metric, delta %)
  - bar (vertical/horizontal)
  - line/area with a time axis and grain
  - table (sortable, uses the server-side sort)
- Filters:
  - list box with selected/possible/excluded styling, search and a virtualized window
  - date range picker
- Click on a bar, table row or legend item selects (replace); Ctrl/Cmd-click toggles; brushing on time axes selects a range.
- **Selection bar:** chips per field, clear one or all, back/forward.
- **Live:**
  - "updated 0.3 s ago" with epoch and ingest rate
  - a pause toggle (frozen at the current frame, with a "N updates waiting" badge)
  - a brief highlight on values that changed
- Error tiles per visual (e.g. a metric was removed by a config change).

**Model browser**
- Tables: columns, types, encodings, row counts, ingest rate, preview of the latest rows.
- Relationship diagram: auto layout (elkjs), read-only.
- Metric catalog: description, SQL, anchor table, which dashboards use it.
- Ingest docs per table: endpoint, example `curl` with sample JSON, link to the OpenAPI spec.

**Config**
- YAML editor over the config files, with validation as you type.
- "Plan" shows the structured plan; "Apply" applies (admin token).
- Version history with diffs and revert.
- Pending changes queue (approve/reject).
- Banner when the config dir has unapplied external edits.

**System**
- Status page: memory use by table/cache, epoch timing, WAL/checkpoint status, background jobs (sealing, rebuilds) with progress.

---

## 11. Testing strategy

Correctness is the main risk of an incremental engine. A bug shows up as **plausible but wrong numbers**. The test strategy is built around that.

1. **Oracle tests (`tidepool-oracle`)**:
   - A generator creates random models (star/snowflake, 1–4 dimensions, random types and encodings), random data, random metrics from the supported vocabulary, random view queries and random selections.
   - Each query runs on Tidepool and, translated to plain SQL, on DuckDB (`duckdb` crate, bundled, dev-dependency only).
   - Results are compared exactly (decimals) or within ε (floats).
2. **Incremental oracle:** the same, but the test interleaves appends, upserts, deletes, seals, selection changes and config changes. It checks every live subscription after every epoch against a cold DuckDB evaluation. This is the most important test in the repo, and it runs in CI with a fixed seed budget plus a nightly long run.
3. **Shadow verification in production:** an optional setting (`verify_sample: 0.01`) occasionally recomputes a live result cold, with no caches, and compares it. Mismatches are logged and counted as metrics.
4. **Crash/recovery:** fault injection in the WAL and the segment writer (crash at random points), restart, compare with the oracle. Also a `kill -9` loop test against the real binary.
5. **Concurrency:** `loom` for the tail watermark protocol and snapshot publication. Miri on the store's unsafe code.
6. **Fuzzing (`cargo-fuzz`):** JSON/NDJSON/Arrow decoders, YAML config parsing and compilation, expression parser.
7. **Property tests (`proptest`):** dictionary encode/decode, bit-packing, aggregate-state merge laws (associativity, commutativity, identity).
8. **Performance:**
   - `criterion` microbenchmarks for kernels, packing and merge.
   - `tidepool-bench` (a workspace crate, like `kayak-bench`; `just bench`, not part of `just ci`): a synthetic retail generator at a configurable scale and ingest rate, plus N simulated sessions clicking randomly. It reports the §2.3 metrics.
   - Benchmark results are tracked per commit on a fixed machine (later).
9. **Frontend:** plain `cargo test` for the frontend's pure logic (state transitions, frame application, formatting); `wasm-bindgen-test` where the DOM is needed. Playwright e2e for: open dashboard → click bar → selection applied → live update arrives → back.

---

## 12. Milestones

Effort is in focused engineer-weeks and is rough. The frontend track can run in parallel from M2 against a mocked protocol.

### M0: Skeleton + frontend spike (1–2 wks)
**Status: done 2026-10-07.**

- Cargo workspace with the crates above. `justfile` with `dev`, `run`, `build`, `frontend`, `ci`, `test`, `lint`, `deny`, `miri`, `bench`.
- Lints as in Kayak: clippy `all` + `pedantic`, `unwrap_used`/`expect_used`, applied to tests too (`clippy.toml`).
- CI (GitHub Actions running `just ci`): fmt, clippy `-D warnings`, test, Miri on `tidepool-store`, frontend lint and test, a licence-declaration test, `cargo deny`.
- `CLAUDE.md` with commands, the definition of done (new behaviour ships with a test that fails without it; `just ci` green before anything is called done), and an architecture tour that grows with the code.
- Licence files per D13.
- Endpoint-table skeleton (routes + roles + OpenAPI) ported from Kayak.
- Frontend scaffold embedded behind `embed-assets` and served by `tidepool serve`.
- **Frontend spike (2 days, decides D10):** in Leptos, build an ECharts bar chart with click-to-select and a line chart with brush-select through JS interop, plus a 50k-row virtualized list with search. Pass if the interop is a thin, reusable wrapper (< ~300 lines) and interaction feels native. Fail if every chart feature needs new glue. Write the outcome into D10.
- `examples/retail` started.
- **Exit:** `just build` produces one binary that serves a hello-world UI; `just ci` green; D10 decided.

### M1: Config and model compiler (2–3 wks)

**Status: done 2026-10-07.** 44 malformed-config golden cases (each yields exactly one error, at the offending token, with no cascades) and 19 plan scenarios; `tidepool validate`, `tidepool plan`, `GET /v1/schemas/{kind}`. Ossie format settled as D8.

- Serde types for `tidepool.yaml`, tables, Ossie subset + extensions, dashboards. YAML parsing with spans.
- **Before starting:** verify the Ossie extension envelope against the spec. YAML parsing uses `serde_norway` (as in Kayak).
- Model IR, full validation (§4.3), key domain assignment.
- Catalog with stable IDs. Diff → Plan with change classes.
- JSON Schemas.
- CLI: `tidepool validate`, `tidepool plan` (without data, the plan treats everything as empty).
- **Exit:** the example project validates. Good errors for at least 20 malformed-config test cases. Plan output for a set of change scenarios matches golden files.

### M2: Store and ingest, in memory (3–4 wks)

**Status: done 2026-10-07.** Measured with `just bench` on an M1 Max (10 cores), 8 clients, 10k-row batches, timed until the last write is visible, all rows verified: **NDJSON ~2.2M rows/s, Arrow ~2.4M rows/s** (targets 100k / 500k); **28 bytes per stored `orders` row** (≈ 2.8 GB for the 100M-row reference). Changes from the plan: frozen chunks instead of a shared tail (no `unsafe`, so no loom; see §4.4); derived dictionaries move to M3, where expressions are evaluated; primary-key columns must be dictionary-encoded (a validation rule); bearer-token auth per access level arrived with the first protected endpoints.

- Types, dictionaries (with sort permutation and derived dictionaries), tail with watermark, sealing (in memory), zone maps.
- PK index, upsert, delete bitmaps, column-change detection.
- Writer thread, epochs, snapshots.
- HTTP ingest (JSON, NDJSON, Arrow), the single dynamic route, idempotency, backpressure.
- `/v1/tables*`, OpenAPI generation, table preview.
- **Exit:** ingest targets from §2.3 met in the in-memory configuration; `loom` tests pass.

### M3: Query engine (5–6 wks)

**Status: in progress.** Done: expression parser, type checker and interpreter; planner with one-directional selection, `ignore_selection` and derived metrics; a general executor and a columnar fast path (morsels, branch-free selection vectors, relationships as arrays by foreign-key code, dense grouping, bucketed distinct bitsets); caches in an `Engine` (relationship maps, join columns materialized per anchor segment, answers per epoch), keyed by the identity of immutable store data; every path checked against DuckDB on every oracle query, plus a replay of every intermediate snapshot through a cached engine; `POST /v1/query`. Measured with `just bench-query` on the M1 Max (10 cores, with other load: load average ~18 during runs, so ±15 ms) at 100M orders, region × category, 200 selections each a new combination of regions and statuses: revenue, order count and paid revenue **fresh selection p50 76–80 ms / p95 118–134 ms**, toggled back < 1 ms, first query ever 165–175 ms (builds the join columns); with `COUNT(DISTINCT customer)` instead of paid revenue, fresh p50 ~113–127 ms / p95 ~195 ms. 1.9–2.3 GB resident. Measured and dropped: selection-mask caches (§5.2 step 4). With branch-free filtering a condition costs about as much as reading a mask, and its columns are usually read anyway for dimensions or filters, so warm masks gained nothing and building them cost ~70%. Left: fresh-selection p95 under 100 ms (likely on the 16-core reference machine for the non-distinct set, not yet for distinct counts); derived dictionaries.

- Expression parser, type checker, kernels; aggregate states; dense and hash grouping.
- Derived dictionaries (moved from M2: they need expression evaluation).
- Selection engine (§5.2) with mask and partial caches. Derived metrics. `ignore_selection`.
- `POST /v1/query`.
- Oracle framework (static data).
- **Exit:**
  - oracle green on 10k random cases
  - 100M-row reference: cold group-by with 2 dimensions and 3 metrics < 100 ms, selection change p95 < 100 ms

### M4: Live (3–4 wks)
- Incremental tail partials, invalidation rules, result cache.
- Subscription manager with dedupe, priorities and coalescing.
- WebSocket protocol, sessions, selection history, filter windows, epoch frames.
- Incremental oracle.
- **Exit:**
  - incremental oracle green
  - freshness p95 < 1 s at 50 simulated sessions while ingesting 100k rows/s
  - per-epoch cost shown to be independent of total rows (benchmark at 10M vs 100M)

### M5: Durability (3–4 wks)
- WAL with group commit, segment file format, dictionary files, checkpoints, recovery, retention job, mmap.
- Crash-injection tests and the `kill -9` loop.
- **Exit:** no acknowledged row is ever lost across 1,000 random crash points; restart of the 100M-row reference in < 10 s.

### M6: Frontend v1 (4–6 wks; parallel from M2)
- Everything in §10.2 except the config section.
- Playwright e2e.
- **Exit:** the retail demo works end to end in the browser, with a live data generator running.

### M7: Live config (3–4 wks)
- Apply pipeline in the running server; background rebuild jobs (dictionary merge for new relationships, type widening).
- Write-back with git commits; file watcher; approval queue; version history and revert.
- Config UI (editor, plan, apply, history, pending queue). Clients follow model version changes.
- **Exit:**
  - an agent or human adds a table, posts data to its new endpoint, adds a relationship and a metric, and sees it on a dashboard without a restart
  - the config dir reflects every change, and a fresh `tidepool serve` from it reproduces the same model

### M8: Agents and v0.1 polish (2–3 wks)
- MCP endpoint and tools, auth tokens, `/metrics`, status page.
- `time_offset` metrics, CSV export, documentation, and a quick-start (`tidepool init` creates an example project).
- **Kayak demo:** `docker compose` with an MQTT/OPC UA source → Kayak → Tidepool → a live dashboard (§15.4).
- **Exit:** v0.1 tagged; the quick-start goes from zero to a live dashboard in < 5 minutes; the Kayak demo runs from one `docker compose up`.

### K1: Kayak `tidepool` output (in the Kayak repo, 1–2 wks; any time after M2)
- Built per §15.2, against `tidepool-protocol` (Apache-2.0) for the request/response types.
- **Exit:**
  - Kayak's usual test bar: an offline test with a fake Tidepool (axum in process), plus an entry in `example_config/`
  - a `docker compose` integration test where Kayak's dummy/MQTT input fills a Tidepool table, and `POST /v1/query` row counts match what Kayak's history says it sent
  - retries after a forced 503 produce no duplicates (idempotency)

**Total:** roughly 26–35 engineer-weeks to v0.1.

### After v0.1 (ordered by expected value)

1. Bidirectional filtering for filter widgets and selections (§5.4)
2. Late binding of dimension attributes (no fact rescans on dimension upserts)
3. Percentiles/median, approximate distinct
4. Segment compaction and dictionary compaction
5. Row-level deltas in frames for large tables
6. Bookmarks and shareable selection URLs
7. Drag-and-drop dashboard builder, visual relationship editor (both writing YAML through the pipeline)
8. Row-level security (mandatory per-user selection applied before everything else)
9. Kayak `tidepool` **input**: subscribe to a live metric query and turn updates into messages (alerting, webhooks), see §15.3
10. Perspective-based pivot visual
11. Engine self-metrics exposed as Tidepool tables (`_system.*`), i.e. a dashboard of Tidepool in Tidepool

---

## 13. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Incremental results silently wrong | medium | severe | oracle + incremental oracle in CI, nightly long runs, production shadow verification (§11) |
| Tail concurrency bug (unsafe) | retired in M2 | — | the store has no `unsafe`: immutable chunks frozen per epoch instead of a shared tail |
| Dictionary growth from high-cardinality columns | high | memory blow-up | `plain` encoding defaults for numerics, warnings and plan hints, dictionary compaction after v0.1 |
| Dimension upserts cause frequent full rescans | medium | latency | column-change detection (v1); late binding (post v0.1) |
| Ossie spec churn (0.1 → 0.2 → 1.0) | high | low–medium | pin a version; all Ossie handling in `tidepool-config`; an internal IR; migrate with a converter |
| YAML crate ecosystem churn (serde_yaml deprecated) | medium | low | `serde_norway` (already proven in Kayak), isolated behind `tidepool-config` |
| Kayak and Tidepool overlapping (aggregation in both, query features in Kayak) | medium | product confusion | D12 boundary: Kayak computes over messages in motion, Tidepool over accumulated history with a selection applied |
| Scope creep in the UI | high | schedule | v1 UI is read-only except the YAML editor; builders after v0.1 |
| SQL expectations beyond the v1 expression set | medium | user friction | clear errors listing supported functions; add functions on demand; `/v1/query` for ad-hoc work |

---

## 14. Open questions

1. ~~License~~: decided, D13.
2. **Viewer identity:** Kayak's auth stack (D14) gives us named users via Basic/JWT. Should sessions and selection history be tied to the user (resume on another device) or only to the browser in v0.1?
3. **Time zones:** dashboard-level time zone for grains (`day` in which zone)? Proposal: a `timezone` setting in `tidepool.yaml`, overridable per dashboard.
4. **Late-arriving data with `event_time` far in the past:** accept always (affects zone maps only), or reject outside a window?
5. **Ossie upgrade:** we write the 0.2 shape already (D8). When 0.2 is released, check whether it adds any of our extension concepts (derived metrics, filters, time grains) natively, and read those too.
6. **Name check:** does `tidepool` collide on crates.io/npm or with trademarks in our space? (Rename cost is lowest now.)

---

## 15. Kayak integration

[Kayak](../../../rust/kayak) is a config-driven, graph-based stream processor for untyped JSON with many connectors and a live pipeline canvas. Kayak **moves** data; Tidepool is where it **settles** so you can look into it (D12).

### 15.1 Boundary

| | Kayak | Tidepool |
|---|---|---|
| Data | untyped JSON messages in motion | typed tables, accumulated, in memory |
| Computes | per message / per window: filter, map, enrich, buffer, windowed stats | queries over history with a user's selection applied: metrics, filters, dashboards |
| Connectors | all of them | HTTP ingest only |
| UI | pipeline graph canvas | dashboards, model browser, config editor |

When a feature request could land on either side, this table decides. They stay **separate binaries** with separate lifecycles. The coupling is just the `tidepool` output and, later, the `tidepool` input.

### 15.2 The Kayak `tidepool` output

- **Config:**
  ```yaml
  outputs:
    - type: tidepool
      connection: analytics          # url + ingest token (${TIDEPOOL_INGEST_TOKEN}) in Kayak's connections file
      table: orders
      columns: { ... }               # optional: Kayak's column mapping; omitted → map message fields to columns by name
  ```
- **`init`** fetches `GET /v1/tables/{table}` and checks the mapping against Tidepool's real column types and nullability. A mismatch fails at startup, not on the first batch (the same rule as Kayak's ClickHouse output).
- **Mapping** reuses `kayak-core/src/columns.rs` (`ColumnPlan`). Tidepool becomes its third consumer after postgres and ClickHouse, and adds no new mapping concept. Kayak's logical types map one-to-one onto Tidepool's (§3.3). Anything that doesn't map is a build-time error.
- **One batch = one request**, NDJSON first and Arrow IPC later. A `buffer` in front is recommended, as with ClickHouse.
- **Idempotency:** `Idempotency-Key = {pipeline_id}:{run_id}:{batch_seq}`, so Kayak's retry/backoff can't double-insert.
- **Errors:**
  - a `400` with per-row errors fails the batch, and Tidepool's message is quoted on the Kayak card
  - `503` + `Retry-After` (backpressure) goes through Kayak's backoff
  - the epoch in the `200` response is kept for diagnostics
- **Schema drift:** if Tidepool's config version in the response changes, the output re-fetches the table schema and re-validates the plan. When the plan no longer fits, the output stops with a clear error instead of sending rows that will be rejected.

### 15.3 Later: the Kayak `tidepool` input

Subscribes to a live metric query over the WebSocket protocol (§7), or a lightweight SSE variant, and emits a message per update. That makes "a metric as a stream" possible for alerting, webhooks or writing KPIs back to other systems. It needs no new Tidepool concepts, only a non-browser client of the existing protocol.

### 15.4 The demo

`examples/kayak-demo/docker-compose.yaml`: an MQTT broker with a simulated plant publisher → Kayak (map, buffer, `tidepool` output) → Tidepool with a `machines` / `readings` / `stoppages` model → a live plant-floor dashboard. It's the showcase for both products and the integration test for K1.

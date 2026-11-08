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
| D10 | Frontend is **Leptos (client-side rendered, built with trunk)**, embedded in the binary | Decided by the M0 spike (2026-10-07, `spikes/leptos-charts/README.md`): the whole ECharts wrapper is 120 lines; searching 50k rows takes 4–5 ms; a selection click re-renders everything in 5–14 ms. One stack with Kayak, and the UI uses `tidepool-protocol` types directly. Client-side only rather than Kayak's SSR + hydrate: a logged-in dashboard app gains nothing from server rendering, and a static `dist/` keeps embedding trivial. Charts are our own since 2026-11-01 (§10.1): SVG drawn by Leptos from DOM-free layouts, ECharts removed. |
| D11 | Every update batch ("epoch") is consistent | Every result the server sends is tagged with an epoch. A client never shows a mix of results from different epochs on one screen. |
| D12 | Kayak is the connector layer | Tidepool only ingests over HTTP, permanently. Kafka, MQTT, NATS, OPC UA, databases and so on are reached through Kayak pipelines with a `tidepool` output (§15). Tidepool never grows transforms, and Kayak never grows dashboards. |
| D13 | Licence split, same as Kayak | `tidepool-protocol` and `tidepool-config` (the types any client needs) are **Apache-2.0**. Everything else (server, engine, frontend) is **AGPL-3.0-or-later**, with a commercial licence available. Every crate declares `license`, and a test enforces it. |
| D14 | Reuse Kayak's conventions and solved problems | `just` for tasks, `serde_norway` for YAML, the `embed-assets` feature with rust-embed, one endpoint table that generates routes + auth roles + OpenAPI + docs, Kayak's auth stack (basic / session cookie / JWT with the iframe embedding handshake), `${NAME}` secrets, a bench crate, clippy pedantic with `unwrap_used`/`expect_used`, and a "definition of done" in CLAUDE.md. Copy first; extract a shared crate only if a third project needs it. |
| D15 | Ingest is private by default; public ingest is a grant | The UI being public says nothing about where producers are. Listeners are split by surface (public / ingest / admin), placed from the endpoint table's `Access`. Ingest credentials are named and scoped to tables and operations, and a table becomes writable from the internet only through a credential marked `public`, with mandatory limits. Off loopback, an ingest or admin level without a token refuses to start. Changes to `auth`, `listen` and `agents` are their own change class, never auto-applied. (Decided 2026-10-17; §9.2.) |
| D16 | Measures grow by structured modifiers, not a formula language | D9 extended to the measures DAX's `CALCULATE` and Qlik's set analysis are used for (shares of totals, fixed selections, level of detail, time windows, sets from sets). The core of a metric stays one SQL aggregate expression. Each new modifier is a key in its `TIDEPOOL` extension, with a plain-SQL translation the oracle checks, a stated place in one order of application, and a plan the engine already knows how to run. Inter-row functions go on visuals, not metrics. (Proposed 2026-11-02; §6.8.) |
| D17 | Time zoom is a visual's grain, not a selection | How wide a visual's time buckets are is view state of that visual in one session, like its sort: no other visual changes and the selection's history doesn't record it. Narrowing the time shown is a selection on the time field, as before, so every visual answers for it. An `auto` grain is resolved from that range and the visual's width in grid columns (never the browser's), before the engine sees the query, so sessions keep sharing answers. (Proposed 2026-11-03; §5.8.) |

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

Distribution or sharding · multi-tenancy · row-level security · bidirectional filtering · visual relationship editor · any ingest connector other than HTTP (that's Kayak's job, D12) · full SQL query surface · mobile layouts · user management.

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

As of M8 the server config accepts every key below.

```yaml
listen: 0.0.0.0:7070
data_dir: ./data                 # overridable with --data
epoch_interval: 200ms
memory_budget: 75%               # of system RAM, for data + caches
cache_budget: 15%                # share of memory_budget reserved for result/partial caches
durability: fsync                # fsync | async (ack before fsync)
auth:
  admin_token_env: TIDEPOOL_ADMIN_TOKEN    # config changes, MCP apply; good for every level
  ingest_token_env: TIDEPOOL_INGEST_TOKEN  # ingest endpoints
  read_token_env: TIDEPOOL_READ_TOKEN      # dashboards and queries; unset: open
config_writeback:
  enabled: true
  git_commit: true               # commit applied changes if config dir is a git repo
agents:
  auto_apply: [safe]             # change classes an agent may apply without human approval
```

Since §9.2 (D15): `listen` can be a map of listeners per surface (`public`, `ingest`, `admin`), and `auth.ingest` a list of named credentials scoped to tables and operations, some of them `public`. The single address and `ingest_token_env` stay accepted.

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
| metric | `format` | how the value reads (display only): a style (`int`, `decimal`, `percent`, `currency:XXX`), `compact` (167.8M) and `decimals:N`, as words in any order: `currency:EUR compact`. A visual can say it again per metric (§3.5) |
| field | `grain` | for time fields: the grains a visual may group by, as a list (`day`, `week`, `month`, `quarter`, `year`, and for timestamps `hour` and `minute`, §5.8) |
| dataset | `hierarchy` | `{name, parent, label?, kind?}`: a parent–child tree over the dataset's rows, as a field of its own (§5.6) |
| relationship | `role` | role name, for when two relationships connect the same pair of tables (e.g. order date vs ship date) |
| model | `filter_direction` | `one_to_many` (default) or `both`: selections also flow from the many side to the one side (§5.4) |

The next metric modifiers (`total`, `set`, `per`/`then`, `time_window`) are proposed in §6.8.

Only `OSSIE_SQL_2026` and `ANSI_SQL` are read (in that order of preference). Other dialects in the file are kept but ignored, so a model can be shared with other Ossie tools. A YAML gotcha the validator explains: in a `{ … }` flow mapping a comma ends the value, so write `type: "decimal(14,2)"` quoted.

### 3.5 Dashboard file

```yaml
# dashboards/sales.yaml
name: Sales overview
layout: { columns: 12, row_height: 80 }
filters:
  - field: customers.region
    measure: revenue                   # shown beside each value instead of its row count
  - field: categories.tree             # a hierarchy field: shown as a tree (§5.6)
  - field: products.category
  - field: orders.ordered_at
    widget: date_range
    default: { last: 30, unit: day }   # opens on a window relative to now (§5.5)
visuals:
  - id: kpi_revenue
    type: kpi
    metrics: [revenue]
    compare: revenue_prev_year
    format: { revenue: compact }   # how its metrics read here, over each metric's own format
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

A visual's `metrics` may be left out: a `filter` visual has none (one dimension, its values as a list on the grid), and a `table` without metrics lists the combinations of its dimensions there are. KPIs, bars and lines need at least one metric.

A visual's `format` maps metrics it shows (its `metrics` and `compare`) to formats, in the grammar of the metric `format` (§3.4; `tidepool_config::format`); the compiler checks both, and refuses a format for a metric the visual doesn't show. The live protocol sends each visual the format that applies per metric (`VisualInfo::formats`: the visual's, else the metric's), and the UI formats every value of that metric through it (KPI, table cells, tooltips, filter measures). Formats are display only: queries, CSV and the API answer the full value. They're for values that change every epoch: 167,791,795.92 moving in its last digits can't be read; 167.8M can.

*Proposed (§5.8):* a time dimension's `grain` may be `auto`, and a visual may say `zoom: [grain…]` (the steps a viewer may zoom between) or `zoom: false`.

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
├── justfile                      # dev, run, build, frontend, ci, test, lint, deny, bench (same verbs as Kayak)
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
- **Unchanged upserts are skipped** (2026-10-13): a row equal to the stored row under its key is not written, not counted and doesn't dirty the table, so a dimension re-sent whole on a timer (Kayak's `postgres` input in `snapshot` mode) costs a comparison per row and nothing downstream: no epoch, no tombstones, the same `TableSnapshot` and so every cache keyed by it. The receipt's epoch is then the current one (`Store::visible_epoch`), so `?wait=visible` doesn't wait for an epoch that never comes. A replace keeps such rows rather than sweeping them.
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
PUT    /v1/tables/{table}/rows        body: the table's whole contents (upsert tables only)
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
- **Idempotency:** a recent-keys map (table, key) → response, with a 24 h TTL (and at most 100k keys, oldest dropped first). With a data directory it's durable with the write it acknowledges: the key, the response and the wall-clock time are in that batch's WAL record, checkpoints carry the keys still live, and recovery rebuilds the map from the checkpoint plus the replayed records (§4.7). A retry of a batch acknowledged just before a restart, whose reply was lost, gets the first response and writes nothing. The TTL runs from the recorded time, across restarts. Without a data directory the keys are in memory, like the data. Since §9.2 keys are scoped by credential as well, `(credential, table, key)`, with the 100k capacity per credential, so one producer can neither claim nor evict another's keys.
- **Replace** (`PUT`, `Op::Replace`, done 2026-10-10): for a dimension kept in step with a system of record that can only be read whole (indud's namespace, a catalogue). The batch's rows are written as upserts, then every live row whose key it didn't list is deleted, in one writer batch, so one epoch and one WAL record (op codes 5/6). Replay applies it to the same state, so it needs no expanded deletes in the log; the crash suite mixes replaces into its workload and catches one logged as a plain write. `replaced` counts rows replaced and rows deleted (the receipt keeps its three counts). An empty body empties the table.
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
  - A write sent with an `Idempotency-Key` carries the key, the time it was written and the response it was acknowledged with (rows written/replaced/missing, epoch). The batch is applied before it's logged so the record can say what it did; neither is visible or acknowledged before the commit. (As built: write and delete records, with or without a key, and model changes; old records without keys are still read.)
  - Payload is Arrow IPC of the **raw normalized values**, not codes.
  - Each record carries a CRC32C. Group commit: one fsync per flush interval or batch.
- **Determinism:** replaying the WAL from a checkpoint re-encodes values. Dictionaries are truncated to the checkpoint's recorded lengths before replay, so the same values get the same codes in the same order.
- **Checkpoint:** runs periodically (e.g. every 60 s, or after N sealed segments).
  1. Flush dictionaries up to the current lengths.
  2. Write the delete bitmaps.
  3. Write a PK-index snapshot, or rebuild it on recovery from PK columns (for v1, rebuild — simpler).
  4. Record the checkpoint LSN, and the idempotency keys live as of it.
  5. Truncate WAL files that are entirely below the oldest kept checkpoint.
- **Recovery:**
  1. Load the catalog.
  2. Load the latest valid checkpoint.
  3. mmap segment files.
  4. Load dictionaries, truncated to the checkpoint lengths.
  5. Rebuild PK indexes.
  6. Replay WAL records after the checkpoint LSN into a fresh tail, adding their idempotency keys to the checkpoint's (expired ones dropped).
  7. Publish the epoch; the server answers retries from the recovered keys.
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

The possible set is computed as a code bitset by scanning T's passing rows for F's codes. With one-directional filtering, a filter on a dimension field reflects selections on that dimension and on anything upstream of it, but not selections on facts. This is a known limitation of D5, and the UI should show it honestly. `filter_direction: both` lifts it (§5.4).

Large filter lists are served windowed: the client requests `(offset, limit, search, sort)` and the server answers from the bitsets and the dictionary sort permutation.

### 5.4 Both ways (`filter_direction: both`)

**Status: done 2026-10-08, after v0.1.** Opt in per model with the model-level extension `{"filter_direction": "both"}`. Selecting a product category then narrows customers to those who bought from it, and through them everything else related to customers, as in Qlik.

It's message passing on the relationship tree (`tidepool-engine::assoc`). The message along a relationship from its many side to its one side is the set of foreign keys present in the many side's rows that pass what's selected on that side, itself including the messages into that side from further away. A query anchored on T gets, for every table T reaches many → one, the messages from every *other* relationship into that table, as one more condition on the table's key (`Condition::Set`). From there the one-directional engine (steps 1–4 above) applies them like any selection, on both executors. So nothing below the planner changed.

- Relationships must form a tree: the compiler refuses a loop under `both` (it would make the answer depend on the order messages go round). Role-playing relationships (two between the same tables) are loops.
- A side of the tree with nothing selected sends no message, so a customer without orders stays possible while only a region is selected (Qlik does the same).
- A NULL foreign key relates to nothing. Relationships over more than one column send no message.
- Messages are kept per epoch in the `Engine`, keyed by relationship and selection, so the visuals and filter lists of one dashboard compute each message once. Under `both`, a query's relevant selection (§6.6) is the whole selection: anything selected anywhere connected can change it, so fewer sessions share subscriptions.
- `ignore_selection` applies first: an ignored field sends nothing either.
- The oracle generates `both` for half its schemas (a second fact, a snowflake parent and nullable foreign keys give messages to carry) and builds the same thing in SQL as `key IN (SELECT fk FROM … WHERE …)`, recursively. It catches a planted bug that sends messages from empty sides, or that leaves out the messages a message itself carries.

### 5.5 Relative time windows

"Today", "Last 7 days", "Last month": a selection on a time field that moves
as time passes. A window is `{unit, last, ago}`: the `last` whole periods of
`unit` (minute … year) ending with the current one, or `ago` periods before
it. Periods start where `DATE_TRUNC` puts them (weeks on Monday), in UTC
(dashboard time zones are §14's open question); a date takes day units or
longer. Like Looker's "in the last 7 days", the current period counts, so
"Last 7 days" is today and the six days before it.

- **The engine never sees a window.** A session keeps it as a window
  (`live::Chosen`) and resolves it into a `Condition::Range` with the
  server's clock (`AppState::clock`, which tests stop) every time it runs a
  query. Caches therefore key on the resolved bounds, which change once per
  period: a "Last 7 days" dashboard costs a recompute at midnight, not per
  query. Incremental answers restart when the bounds move (rows leave the
  window, which they can only do by a rescan), so windows of minutes
  recompute every minute.
- **The clock moves frames, not only epochs.** Once a second the epoch loop
  checks whether a session's windows resolve differently from its last
  frame, and if so sends `selection` (where the window is now) and a frame,
  without new data.
- **Defaults:** a `date_range` can declare `default: {last, unit, ago}`,
  applied the first time a session opens the dashboard, to fields the
  viewer hasn't chosen anything for, as part of the selection it opens with
  (not a step `back` undoes).
- **`POST /v1/query`** takes `{"field", "relative": {unit, last, ago}}` too.
- **Oracle:** the generator makes windows around stored values with a
  random `now`; Tidepool resolves them itself and DuckDB gets its own
  `date_trunc` and interval arithmetic, so the resolution is checked
  independently (a window one period too long fails all three suites).

---

### 5.6 Hierarchies (parent–child, ragged)

**Status: done 2026-10-10, after v0.1.** A dataset declares a tree over its rows with the extension `{"hierarchy": {"name": "asset", "parent": "parent_id", "label": "label", "kind": "kind"}}`: each row is a node keyed by the table's (single-column) primary key, and `parent` holds its parent's key. Depth is whatever the data says, so a plant tree (site → hall → line → cell → machine), an org chart or indud's untyped org tree all fit. The extension adds one field, `dataset.asset`, whose values are node keys.

- **Not a relationship.** The compiler still refuses a self-relationship (and its help points here): `plan::path`, `assoc` and the cycle checks walk relationships, and a self-edge would loop them. The parent column joins the key's dictionary through the key-domain union-find instead (so a parent code *is* a key code); adding a hierarchy over a populated parent column is a **rebuild** (its codes are re-encoded, as for a new relationship), removing one is safe.
- **Selection keeps node keys.** `Select { field: "dataset.asset", values: [keys] }` is the ordinary message. Per query, `engine::hierarchy::resolve` swaps the condition for a `Condition::Set` of every key in the selected subtrees on the key field, against the snapshot the query runs on, so a node moved under another parent is filtered with its new one at the next epoch without anyone selecting again. It runs inside the planner's per-anchor closure, after `ignore_selection` (a metric ignoring the hierarchy must not see its subtrees), and before `assoc`. Neither executor knows hierarchies exist.
- **The tree** (`engine::hierarchy::Hierarchy`) is built from the table's live rows and cached by the table snapshot's identity (§6.5): nodes depth first, so a subtree is a range. A parent that is NULL, absent or the node itself makes a root (an *orphan*, when it named a parent); a cycle is broken at its smallest key. Children are ordered by label, then key.
- **Levels.** A query dimension can carry `level: d`: grouped by each node's ancestor at depth `d` (NULL for nodes not that deep). On the fast path that is a derived dictionary over the key column (key code → ancestor's key code), built from the tree and keyed in the map cache as `ancestor:d`; never in the engine's own derived dictionaries, whose "a code's value never changes" doesn't hold for a tree that moves. A move changes the map, so an incremental answer over it recomputes (the prefix check fails).
- **Tree filters** (`widget: tree`, the default for a hierarchy field): the frame carries the *visible* nodes, depth first through the nodes a session has opened (`FilterOpen`), or every match of a search with the nodes above it, windowed like a list. Each node: `selected`, or `possible` when it or anything below it passes every *other* field's selection (Qlik's rule), else `excluded`; `inside` (an ancestor is selected) and `holds` (something below is), so a selection stays findable with its branch closed; and `rows`, the passing nodes in its subtree. The selected nodes' paths come with every frame (`chosen`) for the chips.
- **Measures on filters** (`measure: metric`, any list or tree): each value shows the metric under the rest of the selection instead of its row count. For a tree, one query per depth on screen (grouped by `level`), so a ratio (OEE, an average) is exact at every level rather than added up from the one below.
- **Tested** by the oracle: a generated dimension sometimes points at an earlier row of itself (`up`), rewritten (moves), deleted (orphans), with subtree selections and level dimensions checked against DuckDB's recursive CTEs. Planted bugs it catches: a subtree without its own node, a tree cached by anything but the table's identity, a level off by one on the fast path, an incremental answer kept across a move. Cycle breaking is unit-tested (DuckDB can't express it).
- **Limits.** Placement is current-state: past facts roll up to where a node is now. Under one-directional filtering fact selections don't narrow the tree (D5); `both` does. A hierarchy field as a visual dimension groups by key.

### 5.7 Text search, and rows as they arrive

**Status: done 2026-11-03, after v0.1.** For logs and other text: a
table of log lines (`message` stored `encoding: plain`, plan §3.3) is
searched live from the dashboard, and the search is a selection like any
other, so every visual answers for the matching rows. Type `timeout` and
the per-level bars, the per-service counts and the rate line show the
lines that contain it; click a service and the matches narrow. That is
what a log store's grep can't do. Loki narrows by labels, then greps; here
the labels are dictionary columns (service, level, pod: cheap
filters), and the search runs only on what they leave.

- **The condition.** `Condition::Contains(Needle)`: the value contains the
  text, ignoring the case of ASCII letters (`A`–`Z` match `a`–`z`; other
  characters must match exactly). One phrase, no operators: `lock
  timeout` is that phrase. An empty text selects nothing (it's removed).
  NULL never contains anything. It applies to string fields (columns of
  either encoding, and derived string fields). On the wire:
  `search { field, text }` in the live protocol (empty text clears it),
  `{"field", "contains": "…"}` in `POST /v1/query` filters, and
  `selection` reports it as `contains`. It goes into history, `back`
  undoes it.
- **On a dictionary column** it's evaluated once per value, like every
  other condition (§5.2): searching `service` costs the dictionary, not
  the rows.
- **On a plain column** the fast path searches the text where it's
  stored. A segment's strings are one byte arena with offsets
  (`ColumnData::Str`), so a morsel's rows are one contiguous slice: it is
  lowercased (ASCII only, a branch-free pass the compiler vectorizes) into
  a scratch buffer and searched with `memchr::memmem` (SIMD), and each hit
  is mapped to its row by walking the offsets. After a hit the search
  jumps to the next row, so each row is found at most once, and a hit
  that crosses into the next row is no hit. Needles without letters skip
  the lowercasing. Measured on 157 MB of synthetic Postgres log lines, one
  core of an M1 Max: ~16 GB/s with lowercasing and ~27 GB/s without,
  against 1.5–9 GB/s for a case-insensitive `regex` and Aho-Corasick. So
  100M lines of ~150 bytes are a ~1 s scan on one core, ~100 ms on ten.
  When other conditions have already left few rows (fewer than one in
  four), only those rows are searched, one at a time.
- **Hits are cached per segment** (§6.5): a bitset per morsel, keyed by
  the segment's identity (`Arc<SegmentData>`), the column and the needle,
  filled the first time a scan reads it, like join columns. Segments are
  immutable, so a live dashboard with a search open searches each new
  chunk once and then never again, every other visual with the same
  search reuses the bits, and a session toggling back is answered from
  them. **Typing narrows:** a needle that contains an earlier needle (for
  `time` then `timeo`) only searches the rows the earlier one matched in
  a morsel, one at a time, when that morsel's bits are there and hold
  fewer than one row in four (otherwise searching the morsel whole is
  faster: measured, `d` → `de` row by row was 2.5× slower). Entries go
  when their segment does, and past 64 MB the least recently used go.
- **The general path** tests the same `Needle` on each decoded value.
  Both paths, cached and uncached, are checked against DuckDB:
  `contains(translate(x, 'A…Z', 'a…z'), translate(text, 'A…Z', 'a…z'))`
  folds the same way (ASCII only, as `lower` would not).
- **Rows, newest first: the `rows` visual.** `type: rows` with
  `dimensions` (the columns to show) and `limit` (default 100, at most
  1000) shows the newest rows of one table that pass the selection, most
  recent arrival first (row id order: ingest order, which for a log tail
  is what you want; it isn't sorted by a time column). The table is the
  one the columns' tables are reached from, by the rule of a table
  without metrics (`rows_anchor`). Columns may be fields of related
  tables (a log line's service owner). The engine
  (`Engine::rows`) runs the anchor's conditions the fast way (a plain
  column can be searched but not otherwise read by the fast path), morsel
  by morsel from the newest, in parallel waves of a few morsels per
  thread, and stops when it has the limit. Only the rows it keeps are
  decoded. With an `Engine` the answer is kept: the next epoch scans only
  rows appended since, puts their hits in front and drops kept rows that
  were deleted; it starts over when it's short of the limit after that,
  or when a related table changed (the same check as for incremental
  answers, §6.5). Over HTTP: `POST /v1/rows`.
- **The search box: `widget: search`** is a filter on a string field: a
  text box that sends `search` as you type (debounced, 150 ms). The UI
  marks the matches in `rows` visuals.
- **Measured** with `just bench-search` on the M1 Max (10 cores), 10M
  synthetic Postgres lines (1.2 GB of text): a new needle is counted in
  ~20 ms (~60 GB/s over the text; a needle without letters, which skips
  lowercasing, ~11 ms); the same needle under another selection, its hits
  cached, 2–4 ms; typing `deadlock` costs ~20 ms for each of the first
  three keys and ~2 ms for each after (narrowing), 80 ms in all; the
  newest 200 matching rows ~1 ms (a needle found nowhere is a scan of
  everything, 24 ms); after 100k more lines, the kept views 0.3–0.7 ms.
  By the same rate a new needle over 100M lines is ~200 ms: over the
  fresh-selection target of §2.3, which wasn't written for text. The skip
  indexes below are what would bring it down.
- **Also built on the way:** the general path's key indexes (row by
  primary-key code, for following relationships) are cached by the
  table's identity, as the other caches are: a `rows` visual showing a
  related column read one every epoch. `POST /v1/rows`, the MCP tool
  `read_rows`, and `contains` filters in `POST /v1/query` and
  `query_metrics`.
- **The example** (`examples/pglogs`, `just pglogs`): Postgres logging
  everything as JSON (statements with durations, connections, lock waits,
  checkpoints, vacuum, its `DEBUG` lines), a Python shop app that makes
  the usual mistakes on purpose (unique and check violations, syntax
  errors, deadlocks, lock and statement timeouts, wrong passwords, a
  missing database), and a standard-library shipper tailing the log into
  Tidepool. The shipper sends the minute as its own dictionary column (the
  trend's buckets), saves where it read each file to, and keys each batch
  by where it starts, so restarts lose and repeat nothing (checked: line
  counts equal across restarts of the shipper and of Tidepool). Kayak has
  no file input, which is why the shipper is a script; a Kayak `file`
  input would replace it.
- **Out of scope here:** token or n-gram skip indexes per sealed segment
  (ClickHouse's `tokenbf_v1`), so a search can skip segments that can't
  match; several terms, `NOT`, regular expressions; sorting `rows` by a
  column; a count of matching rows in the `rows` visual (a KPI over
  `COUNT(*)` shows it).
- **Tested** by the oracle (a plain-encoded `note` column on the fact,
  `Contains` on it, on dictionary columns and on derived fields, with
  needles cut from stored values, sometimes in other case or absent; the
  rows of a `rows` query compared with DuckDB's as a set, and its order
  and limit by unit tests), planted bugs (a hit crossing into the next row
  counted, hits cached by the needle only, a narrowed search that keeps
  rows the longer needle doesn't match), and `just bench-search`.

### 5.8 Time zoom

**Status: proposed 2026-11-03.** D17 is the decision; this is the design.
Sub-day grains are built (2026-11-04); the rest isn't.

A visual that groups by a time field (revenue per hour) can be zoomed out
(per day, per month) and in again. Two things are called zooming, and they
are kept apart:

- **The grain** is how wide a bucket is. It is view state of one visual
  in one session, like a sort: it changes no other visual and is not a
  step in the selection's history.
- **The range** is which time is shown. Narrowing it is a selection on the
  time field (a brush, a click on a bucket, the date range picker, a
  relative window), so every visual answers for it, as for any other
  selection.

With an `auto` grain the first follows from the second: brush a week on a
monthly trend and it shows days; `back` and it shows months again.

**Where it applies.** A visual can be zoomed when exactly one of its
dimensions is a time field with a grain: `line`, `bar` and `table`
(`kpi`, `filter` and `rows` have no grains already). The steps are the
field's `grain` list (§3.4), or all grains when it has none. A date field
has no `hour` or `minute`. A dashboard can narrow the steps per visual,
or turn zoom off:

```yaml
# fragment
  - id: trend
    type: line
    dimensions: [{ field: orders.ordered_at, grain: auto }]
    zoom: [hour, day, week, month]   # or `zoom: false`
```

A step is offered only when it gives a readable number of buckets over
the range on screen: between 2 and the visual's bucket budget (below). So
`minute` isn't offered over a year, nor `year` over three weeks. Which
steps are open changes with the range, and every frame says so.

Visuals with a `time_window` metric (§6.8) offer no grain coarser than
the window until its spike has settled how the two combine.

**Sub-day grains** (done 2026-11-04). `TimeGrain` has `hour` and `minute`,
for timestamp fields only; the compiler refuses them on a date, in a
visual and in a field's `grain` list, and the planner refuses them in
`POST /v1/query`. A sub-day grain's buckets are timestamps of the field's
own type, not dates (`plan::dim_types`, `dim_value` in `fast.rs`). The fast
path truncates by integer division, with no day memo, a dictionary column
through each code's value as the day grains do. Brushes still select whole
buckets as half-open ranges (`board::brush_range`). `time_offset` stays
in days or longer (`ir::OffsetUnit`): its `Shift` moves a value by calendar
days and months, and an offset of hours would need a third unit in both
paths and a date check, which nothing has asked for yet.

**The `auto` grain.** A dimension may say `grain: auto` (the dashboard
editor writes it for new time dimensions; `grain: day` and the other
fixed grains stay as they are). It resolves to the finest open step,
every time a query runs, from:

- the range: what the session selects on the field, with windows resolved
  (`AppState::clock`, as in §5.5); without a selection, the extent of the
  field's live values (from the segments' zone maps, §4.4, once they
  exist; until then a min/max over the column, cached by the table
  snapshot's identity like the other caches);
- the bucket budget: 40 buckets per grid column of the visual's width
  (`at.w`, so 480 across a 12-column dashboard). The budget is from the
  dashboard file, not the browser's size, so every session on a dashboard
  gets the same grain for the same selection and they still share
  answers (§6.6). Maximizing a visual (§10.2) is the browser's alone and
  keeps the grain; a finer one is a pin away.

Like a window, `auto` is resolved before the engine sees the query: the
engine only ever gets a fixed grain. A grain that changes as data arrives
(the extent grows past a step) is a new query and starts its incremental
answer over; that happens rarely and only at a step.

**The session.** `ClientMessage::Grain { visual, grain }` pins a grain
for the visual in this session; `grain: None` goes back to the
dashboard's (`auto` or fixed). The session keeps pins next to its sorts
(`inner.grains`) and `visual_frame` puts them in the query, as it does a
sort. A pin outside the open steps (the range moved) falls back to the
nearest open step for that frame and stays pinned. Each visual's frame
carries the grain its answer is at, whether it's `auto`, and the open
steps.

**The engine** doesn't change: another grain is another query. Sessions on
the same grain share it through the `Engine`'s caches, and switching back
within an epoch is an answer-cache hit. Rolling coarse buckets up from a
cached finer answer (exact for `SUM`, `COUNT`, `MIN`, `MAX`, not for
`COUNT(DISTINCT …)` or medians) is left out until a measurement asks for
it: a rescan is milliseconds, and it would be a cache of its own.

**The UI.** The tile's header gets the steps as a small segmented control
(closed steps greyed out, `auto` marked), `+` and `−` on the focused
chart, and later Ctrl+wheel (a plain wheel scrolls the page). Two
things zooming makes visible, which the time axis needs first:

- **Gaps.** The line chart's axis is categories (`scale::Points`) and a
  bucket with no rows isn't a group, so at `hour` or `minute` an empty
  bucket would be joined over silently. A grained axis becomes a time
  scale: buckets are placed by time and the client fills the missing ones
  from the grain and range (`next_period`), drawn as a break in the line
  (or a missing bar).
- **The current bucket.** At `month` on a live dashboard the last bucket
  is this month so far and reads as a drop. The bucket that holds the
  server's now is marked (a dashed last segment, a lighter bar).

**Out of scope here:** a zoom of one visual's range that doesn't select
(a viewport only that visual sees); pinned grains in bookmarks and URLs
(with bookmarks, §12); dashboard time zones (§14, question 3), which zoom
makes more pressing: at `day` the buckets are UTC days.

**Tests:** for sub-day grains, golden diagnostics (`hour` on a date, in a
visual and in a field's list), `tidepool-engine/tests/grains.rs` (every
path, plain and dictionary columns, before 1970, a date refused), the
oracle (`hour` and `minute` on timestamps, DuckDB's `date_trunc` without
the `DATE` cast; a planted bug truncating them to days fails all three
suites). Still to come: golden diagnostics for `zoom` (a visual without a
grained dimension, a step the field doesn't allow);
plain tests of `auto`'s choice and the open steps (budgets, ranges,
windows, an empty table); `tests/live.rs` (a pin, a brush that refines an
`auto` grain, `back` that coarsens it, two sessions sharing a grain, a pin
that falls back); `charts.rs` tests for the time
scale and filled gaps; `just e2e` zooms a trend out and in.

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

### 6.8 Measure vocabulary (after v0.1)

**Status: proposed 2026-11-02.** D16 is the decision; this is the design. Nothing here is built.

**Where we are.** A metric is one SQL aggregate expression (`SUM`, `COUNT`, `COUNT(DISTINCT …)`, `MIN`, `MAX`, `AVG`, with `FILTER (WHERE …)`), or a `derived` expression over other metrics. Four modifiers change what it sees: `table`, `ignore_selection`, `time_offset` and the model's `filter_direction`. That covers the simple half of what DAX's `CALCULATE` and Qlik's set analysis are used for. These are the gaps users will meet first:

| Gap | DAX | Qlik | Example |
|---|---|---|---|
| share of a total over the visual's own dimensions | `ALL(dim)` in `CALCULATE` | `TOTAL <dim>` | revenue share per region |
| a fixed value that replaces the selection on a field | `CALCULATE(…, field = v)` | `{<field={v}>}` | paid revenue, whatever status is selected |
| more aggregates | `MEDIAN`, `PERCENTILEX`, `STDEV`, `LASTNONBLANK` | `Median`, `Fractile`, `Stdev`, `FirstSortedValue` | median order value, the latest state of an asset |
| aggregation in two steps (level of detail) | `AVERAGEX(VALUES(c), …)` | `Aggr()` | average revenue per customer; customers with revenue over 1,000 |
| time windows other than a shift | `DATESYTD`, `DATESINPERIOD` | set analysis with `$(=…)` dates | year to date, the last 12 months |
| sets from other sets | `CALCULATETABLE`, `TREATAS` | `P()`, `E()` | revenue on all products from customers who bought bikes |

Apache Ossie doesn't limit any of this. Its metric is a name, an expression and `custom_extensions`, and our extension's `data` is ours to define. The limits are D9's (no formula language) and the engine's (every path exact, incremental where possible, checked by the oracle).

**The rule for new modifiers (D16).** The core stays a SQL aggregate expression. Every way to change what a metric sees or how it rolls up is a key in the metric's `TIDEPOOL` extension, and it must:

- have a translation to plain SQL that the oracle can generate (`combinations_sql` and its neighbours), so DuckDB checks it;
- compose with the other modifiers in a stated order (below), with no special cases between pairs;
- compile to anchor plans the engine already has (one per anchor table, selection and shape), joined on the group key as §6.4 does today. A modifier may start on the general path only (`prepare` returns `None`) and get fast-path support later;
- get golden diagnostics for its mistakes, a metric in the retail example, a generator in the oracle, and a page on the doc site.

**Order of application,** for each base metric in a visual: the selection (with `filter_direction`), then `ignore_selection`, then `set`, then `time_offset` or `time_window`, then `FILTER (WHERE …)`, then grouping (the visual's dimensions without `total`, plus `per`), then `then`. Derived metrics come last, as now.

#### The modifiers, in order of work

1. **`total: [field…]`** (or `"*"`). The metric is computed without these of the visual's dimensions, and its value is repeated across them. Selections still apply, as with Qlik's `TOTAL` (to get DAX's `ALL`, combine it with `ignore_selection`). A total field the visual doesn't group by does nothing. Share of total is then a derived metric: `revenue / revenue_total`. It is one more anchor plan with fewer dimensions, joined back on the dimensions it kept. Incremental like any other plan. Oracle: the same aggregate grouped by the kept dimensions, joined.

2. **`set: {field: [values…]}`.** The metric sees these values on these fields *instead of* the session's selection on them. This is shorthand for `ignore_selection` plus a condition, but a condition on a dimension's field (`customers.region`), not only on the anchor's columns, as `FILTER` requires. A `set` field is never in `relevant` (§6.6), so changing it in the session costs nothing.

3. **More aggregates.** `MEDIAN(x)`, `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY x)`, `STDDEV_SAMP`, `STDDEV_POP`, `VAR_SAMP`, `VAR_POP`, and `ARG_MAX(x, t)` / `ARG_MIN(x, t)` (the value of `x` on the row with the largest `t`: the latest reading, the closing balance). All exact. States: a code → count histogram for dictionary columns, a value list otherwise (§6.4's t-digest only behind an explicit `APPROX_` name, like `APPROX_COUNT_DISTINCT`). Deletes recompute, as for `MIN` and `MAX`. These take over item 4 of "After v0.1".

4. **`per: [field…]` with `then`.** Aggregation in two steps. The expression is aggregated per combination of the visual's dimensions and the `per` fields (inner). Then `then`, a SQL aggregate over the inner value named `value`, aggregates those to the visual's dimensions (outer):

   ```json
   {"per": ["orders.customer_id"], "then": "AVG(value)"}
   {"per": ["orders.customer_id"], "then": "COUNT(*) FILTER (WHERE value > 1000)"}
   ```

   With `total: "*"` as well, the inner groups are the `per` fields alone (Tableau's `FIXED`). `per` fields must be on the anchor or reachable from it through many → one paths, like every other read. The inner state is kept per epoch like any grouped answer, and the outer one is recomputed from it, which costs the number of inner groups, not rows. A `per` field with very many values (one row each) is the expensive case. `tidepool plan` reports its cardinality, and the compiler warns over a limit. Oracle: a `GROUP BY` subquery inside a `GROUP BY`.

5. **`time_window`** with `time_field`: `ytd`, `qtd`, `mtd`, `wtd`, or `last:<n><unit>` (`last:12mo`, `last:7d`). Without a time dimension in the visual, the window ends where the time selection ends (or now, §5.5). With one, each group gets its own window, ending where the group ends (a running year to date per month, a rolling 12 months per month). The per-group case is a range join of groups against groups. It's exact for every aggregate when each window is computed from the rows, and cheap for sums and counts when it's computed from finer groups. Start with the exact version on the general path. Needs a short design spike before it's built: the interaction with a `grain` coarser than the window (which time zoom, §5.8, makes a viewer's choice), and with `time_offset` on the same metric.

6. **Sets from sets.** A condition that is the possible values of a field under another selection: `{"in": {"field": "orders.customer_id", "under": {"products.category": ["bikes"]}, "keep_selection": true}}` (shape not final). It's Qlik's `P()`. The engine already computes such sets: `assoc.rs` turns the passing rows of one side into a `Condition::Set` on another. So this is mostly syntax, planning and the oracle. `E()` is the complement. Design after items 1–4 are in, when we know how they read in practice.

Smaller items to fold in on the way: `via: <relationship>` to choose a relationship by its `role` per metric (DAX's `USERELATIONSHIP`), and `time_offset` with `time_window` on the same metric.

**Not planned:**

- **A formula language** (DAX, Qlik expressions). Its power comes with an evaluation model (context transition, implicit iteration) that is hard to learn, hard to plan and can't be checked against SQL. If the JSON gets heavy, a short surface syntax that compiles to the same keys is a separate, later decision.
- **Inter-row functions** (rank, running total, difference to the previous row; Qlik's `Above`, DAX's `RANKX`). They work on a visual's result rows, not on data, so they belong on the visual (a `rank` or `running` option in the dashboard file), computed after `sort`. Plan them with the visuals.
- **Alternate states** (two selections side by side). That's a session and protocol feature. It belongs with bookmarks (§12).

**Portability.** Another Ossie tool reads `revenue_prev_year` as plain `SUM(orders.amount)`, and gets a different number without an error. The more of a metric's meaning moves into the extension, the more this is true. See §14.

---

## 7. Client protocol (WebSocket `/v1/ws`)

Binary WS messages with a framed format: `u32 header_len | header (JSON) | payloads (Arrow IPC streams)`. JSON-only messages are allowed when there's no payload. *(M4: every message is JSON for now, tagged by `type`; see `tidepool-protocol::live`. Arrow payloads come back when a result is large enough to matter.)* Types are defined in `tidepool-protocol` and used directly by the server and the wasm frontend.

**Client → server:**

| Message | Purpose |
|---|---|
| `hello { session_id?, token? }` | resume or create session (session id stored in localStorage) |
| `open_dashboard { id }` | subscribe all its visuals and filters; a view's id (`~…`) opens the view |
| `select { field, values, mode: replace\|toggle\|add }` | change selection (click, list box) |
| `select_range { field, lo, hi }` | brush on time axis / date range |
| `select_relative { field, window: { unit, last, ago? } }` | a period relative to now ("Last 7 days", §5.5) |
| `clear { field? }` / `back` / `forward` | selection management |
| `filter_window { filter_id, offset, limit, search?, sort? }` | page/search a filter list |
| `grain { visual, grain? }` | pin a visual's time grain for this session, or go back to the dashboard's (proposed, §5.8) |
| `pause` / `resume` | freeze live updates for this session (the server keeps the last frame) |

**Server → client:**

| Message | Purpose |
|---|---|
| `welcome { session_id, model_version, epoch }` | |
| `model { version, dashboards… }` | sent on connect and whenever the config version changes |
| `view { dashboard }` | a view the session opened (views aren't in `model`); again when a config change changes it |
| `selection { state, can_back, can_forward }` | authoritative selection state after each change |
| `frame { epoch, visuals: [{ id, payload_idx \| error }], filters: [{ id, counts, window }] }` + Arrow payloads | atomic update unit |
| `status { epoch, ingest_rate, lag_ms }` | for the live indicator, ~1 Hz |
| `done` | one per client message, after everything it caused: the UI shows a selection as under way until its `done` arrives |

**Views (2026-11-08).** A view is a dashboard that lives only in memory, for answers an agent shows once (`POST /v1/views`, MCP `show_view`). It is compiled as one more dashboard file of the applied config (`views.rs`: the config's files plus its YAML, through the one compiler, so every rule and located diagnostic applies), opened by its id (`~` and 128 random bits; no file name can start with `~`), and never written, versioned or committed. It is recompiled for a new config version the first time it is looked at after one; one that no longer compiles is an `error` to its sessions, once. Kept 24 h from last use, 1000 at most (least recently used first). Saving it (`save_view`, the UI's **Save as dashboard**) is an ordinary config change of its YAML. A server with no config files (a model given in code) can't make views. `ConfigStore::published` holds the applied version's files and catalog for this, readable without the config lock.

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
- **File watcher** (since M7 a one-second poll of the directory, read under the pipeline's lock, so the server's own writes are never mistaken for edits). External edits become a proposed change from source `files`. If the change is safe it's auto-applied. Otherwise it goes to the approval queue and the UI shows "config dir has unapplied changes".
- **Startup** is the same path. The source is `startup`, and it's compared with the last applied version in the catalog. Destructive changes refuse to start without `--allow-destructive`.

### 8.2 Change classes

| Class | Examples | Apply |
|---|---|---|
| **safe** | add table · add nullable column (old segments read it as virtual all-NULL) · add/edit/remove metric · add/edit field expression · add/edit/remove dashboard · change retention · change epoch interval | instant, at next epoch |
| **rebuild** | add relationship between existing populated tables (dictionaries must be merged into one key domain, and one side's key codes remapped) · change encoding · widen type (int32→int64, decimal precision up) · add non-nullable column with default · change PK of upsert table | background job builds new segment versions, swapped at an epoch; progress shown in the UI; old version keeps serving until the swap |
| **destructive** | drop table/column · narrow type · change table mode · remove a relationship used by dashboards | needs explicit approval (human) |
| **security** (§9.2) | any change to `auth`, `listen` or `agents` in `tidepool.yaml`: a credential added, a table granted publicly, an operation widened, `auto_apply` changed | needs explicit approval (human); never in `auto_apply` |

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
Since M2: a bearer token per access level (`TIDEPOOL_INGEST_TOKEN`, `TIDEPOOL_READ_TOKEN`, `TIDEPOOL_ADMIN_TOKEN`), enforced by middleware built from each endpoint's declared `Access`; a level without a token is open. Since M8 the admin token is good for every level, the variables are named in `tidepool.yaml`'s `auth`, and a browser trades a token for an in-memory session (`POST /v1/auth/session`, an `HttpOnly` cookie; the UI does it and drops `?token=` from the address bar). What follows ports Kayak's auth stack instead of designing a new one; of it, M8 did the session cookie, and the rest comes after v0.1 (§12):
- **Ingest:** bearer token (`TIDEPOOL_INGEST_TOKEN`), constant-time comparison. This is what Kayak's `tidepool` output sends.
- **Roles:** `viewer` and `admin` (config apply, MCP write tools), declared per endpoint in the endpoint table.
- **Humans:** HTTP Basic + session cookie, or **JWT** validated against an identity provider's JWKS.
- **Embedding:** Kayak's iframe handshake (`?auth_token=` exchanged at `POST /v1/auth/token` for a session cookie), so Tidepool dashboards can be embedded in other apps. *(2026-11-08, for AI apps: embed tokens instead, `POST /v1/auth/embed`: good on `/v1/ws` only, for the views named, an hour idle; see §9.1, MCP App.)*
- **Off by default on loopback.** Since §9.2 the server refuses to start when ingest or admin is open off loopback (`--insecure-open` for demos), and warns for an open read level.
- **Exposure** (§9.2, D15): listeners per surface, named and scoped ingest credentials, public grants with limits.

### 9.1 MCP tools

| Tool | Notes |
|---|---|
| `describe_model` | tables, columns, datasets and fields (labels, grains), relationships, metrics, dashboards with their visuals, the visual types (`VisualType::shape`), `semantics` (how answers are computed), plus `ai_context` from Ossie at every level it's written |
| `preview_table` | latest N rows |
| `query_metrics` | wraps `/v1/query` |
| `get_config` | current files + version |
| `plan_change` | takes full or patched files, returns a plan |
| `apply_change` | applies if policy allows; otherwise queues for approval and returns the change id |
| `ingest_rows` | for agents that generate or forward data (ingest token scope) |

**MCP App (2026-11-08).** `show_view` names `ui://tidepool/view` (`_meta.ui.resourceUri`), an MCP App: hosts that show them (Claude, ChatGPT, VS Code, …) draw the view live in the conversation. The resource is the UI's `index.html` with every asset loaded from the server's public address (`public_url`, else the MCP request's host; a `security` setting) and a CSP of that origin as resource and connect domain (`apps.rs`); hashed assets are served with `Access-Control-Allow-Origin: *` for it. A small script in the page speaks the bridge (JSON-RPC over `postMessage`: `ui/initialize`, the tool result, `ui/update-model-context`) and keeps the tool result for the UI; the result's `_meta` (which the model doesn't see) has the view, the origin and an embed token for that view alone. The UI (`apps.rs`, `McpApp` in `main.rs`) connects with it, shows the view without the shell, and tells the host's model every selection, in words and as data. Without WebAssembly or a connection, the script shows the tool's numbers as tables. Also: live session ids are 128 random bits and a session can be resumed for an hour after its connection goes, by a connection with the same scope; a cookie opens the socket only with a same-origin `Origin`; UI pages send `frame-ancestors 'self'`; `?token=` on `/mcp` is passed on to the tools. The e2e (`an_ai_app_…`) plays the host: the resource in a sandboxed frame under its declared CSP, the bridge, a live update, a click and the model context it sends; and with WebAssembly refused, the tables. Real hosts can't be tested offline: whether each allows the connection and WebAssembly, and how long it keeps a view, is theirs.

**Grounding (2026-11-08).** Ossie's `description`, `label` and `ai_context` reach the IR, `GET /v1/model` and `describe_model` as written; editing them is a safe change that plans report. Every query answer carries `as_of` (when its epoch was published) next to `epoch`. MCP also serves resources: each config kind's JSON Schema at `tidepool://schemas/<kind>`, read through `GET /v1/schemas/{kind}`; `query_metrics`, `read_rows` and `plan_change` declare an `outputSchema`. `evals/questions.yaml` is a set of questions about the retail example with the call that answers each one; `tidepool-server/tests/evals.rs` keeps its answers true, and it's what an agent is scored against (no LLM runs in CI).

### 9.2 Exposure: listeners and ingest credentials

**Status: done 2026-10-17.** D15 is the decision; this is how it works. Changes from the design, as built: the idempotency capacity is 100k keys per credential, the same for public ones (a flood evicts only its own keys); the credential is in the WAL records of keyed writes (op codes 7/8/9; checkpoints `TPC4`), which is what recovery needs, and per batch only in metrics; new values are counted on the writer thread against its own dictionaries (`Store::new_values`) and a batch over the budget is refused there whole, which is exact per batch where a snapshot count wasn't; the CORS preflight (`OPTIONS` on the rows path) is the one route outside the endpoint table; tokens, credentials and listeners are read at startup, so a `security` change takes effect on restart; a token for something else where some credential needs none counts as no token, as at any open level; `tidepool_ingest_requests_total` labels a table that doesn't exist `(none)`, so the public listener can't grow it with made-up names; the bare Docker image and the compose demos start with `--insecure-open ingest,admin`. Tests: `tidepool-server/tests/exposure.rs` (placement per listener, the credential matrix, limits, the queue share, CORS, filled columns, OpenAPI and MCP per listener, fail closed; six planted bugs each fail it), `config.rs` and `mcp.rs` for the `security` class, 18 golden diagnostics and 7 plan scenarios, the crash suite with two credentials sharing keys, and migration, decoder and writer cases.

**The problem.** Today one address (`listen`) serves everything, so putting the UI on the internet also puts ingest, config, `/metrics` and `/mcp` there. An access level without a token is open, and away from loopback that only gets a warning. A deployment that sets only `TIDEPOOL_READ_TOKEN` has a protected dashboard and an ingest API anyone can write to, `PUT` (an empty body empties a table) and `DELETE` included. There is one ingest token, good for every table and every operation. Idempotency keys are scoped by `(table, key)` with one shared capacity, so any ingest caller can claim a key another producer will use (its batch is then answered "already done" and never written), or evict other producers' live keys by sending fresh ones. And `tidepool.yaml` goes through the config pipeline, but `auth` and `agents` changes don't appear in plans, and every setting change is `safe`, so `agents.auto_apply: [safe]` lets an agent rewrite either.

**Who calls what.** Viewers (the UI, `/v1/query`, `/v1/ws`) are people, anywhere. Producers are machines at addresses we know: Kayak in the same network, an edge site, a CI job. Under D12 the usual reasons for public ingest (third-party webhooks, browser beacons) land in Kayak, not here. So by default ingest is private, and making a table writable from the internet is a narrow grant you can see, not a side effect of a public UI. That grant is still supported, because a sensor or a script that can `curl` a table directly is a real convenience.

#### Listeners

```yaml
listen:
  public: 0.0.0.0:7070      # the UI, read endpoints, /v1/ws, and public ingest grants
  ingest: 10.0.0.5:7071     # read + all ingest
  admin: 127.0.0.1:7072     # everything, including config, changes and /metrics
```

- A plain address (`listen: 0.0.0.0:7070`) is still accepted and means one listener serving everything, as today (subject to "fail closed" below). A surface left out of the map is served by `admin`, or by `public` when `admin` is also left out.
- **The endpoint table decides placement.** Each `ApiDoc`'s `Access` maps to the listeners that serve it (Read → all; Ingest → `ingest`, `admin`, and `public` only for public grants; Admin → `admin`). `api_router` folds the table once per listener, as it folds it into `authorize` today, so a documented endpoint can't end up on the wrong listener. Probes are on every listener. `/metrics` is on `admin`.
- **Listener and credential must both allow a request.** Cookies are per host, not per port, so an admin session cookie also reaches the public port. It still can't reach config there, because the route doesn't exist on that listener. On the public listener, a table nobody granted publicly is a 404 (as if it didn't exist), and its OpenAPI document lists only the granted tables, which makes it the page you give to whoever runs the producer.
- **`/mcp` is on every listener**, and the listener is passed into its in-process `oneshot` calls, so a tool can't do anything over the public port that its endpoint wouldn't do there.

#### Ingest credentials

```yaml
auth:
  admin_token_env: TIDEPOOL_ADMIN_TOKEN
  read_token_env: TIDEPOOL_READ_TOKEN
  ingest:
    - name: kayak
      token_env: KAYAK_INGEST_TOKEN
      tables: ["*"]
      operations: [append, upsert, delete, replace]
    - name: field-sensors
      token_env: [SENSOR_TOKEN, SENSOR_TOKEN_NEXT]   # any of them; rotation without downtime
      tables: [readings]
      operations: [append]
      public: true
      limits: { rate: 200/s, batch_rows: 1000, body: 1MiB, new_values: 1000/min }
      origins: ["https://app.example.com"]           # CORS, for browser producers
```

- `operations`: `append` is `POST` to an append table, `upsert` is `POST` to an upsert table, `delete` is `DELETE`, `replace` is `PUT`.
- `ingest_token_env` stays accepted and means one private credential named `ingest` with every table and operation, so existing deployments and Kayak's `tidepool` output keep working unchanged.
- Grants live in `tidepool.yaml` (the deployment), not in table files (the model): the same model runs in dev and in production with different exposure, and secrets don't belong in files that go into git. Tokens are named by environment variable, as now.
- The credential's name is in the WAL record of each keyed write and in metrics (`tidepool_rows_written_total{credential="…"}`, `tidepool_ingest_refused_total{credential, limit}`). Records and checkpoints written before the change read as the `ingest` credential's.

#### Guardrails for public grants

What's acceptable from Kayak isn't acceptable from the internet:

- **Append only by default.** A public grant with `upsert`, `delete` or `replace` is allowed but gets a warning at startup and in its plan: anyone with the key can overwrite rows by key or empty the table.
- **Limits are mandatory**, with defaults where a public grant leaves them out. Over the rate it's `429 Too Many Requests` with `Retry-After` (`503` stays "the server is busy"). Too many rows or too large a body is `413`.
- **Reserved writer capacity.** Public batches together may hold at most a quarter of the writer queue (16 of 64), so a flood gets that caller 429s while private producers still get through.
- **Caps on new dictionary values.** Dictionaries never shrink (risk, §13), so a public caller sending unique strings is a memory leak that lasts until compaction. The writer counts a batch's values missing from its dictionaries before applying it and refuses the batch whole when they exceed what the credential has left (a token bucket per credential), naming the column with the most. The budget is approximate under concurrency (requests in flight can each see the same budget), which is enough for a limit.
- **Server-filled columns** (table file, model side): `ingested_by: { type: string, filled: credential }` and `received_at: { type: timestamp, filled: received }`. A producer that sends one is refused with a 400. When a public key is abused, you can find the bad rows with a filter and remove them, instead of guessing.
- **Anonymous grants** (`token: none`) are possible only when written out in full, and only for `append`. A browser beacon's key is public by nature, so there the limits are the only protection.

#### Idempotency keys per credential

Keys are scoped by `(credential, table, key)`, and the capacity (§4.5) is per credential. One producer can then neither claim nor evict another's keys. The key's credential is in the WAL record, so recovery rebuilds the same scoping. This fixes the eviction and claiming problems for a leaked ingest token too, so it comes first.

#### Fail closed

`serve` refuses to start when a listener away from loopback serves ingest or admin and the level has no token. `--insecure-open ingest,admin` allows it, by name. A public grant always has a token unless it says `token: none`. An open read level on a public listener stays a warning, because public read-only dashboards are a legitimate deployment.

#### The `security` change class

A change to `auth`, `listen` or `agents` in `tidepool.yaml` is class **security** (§8.2): it's shown in the plan item by item (a credential added, a table granted publicly, an operation widened), it is never auto-applied, and `agents.auto_apply` can't include it (a compile error). Today these changes are invisible in plans; this closes that.

#### Order of work

1. Credential-scoped idempotency keys, with the one legacy credential (a fix that's useful before anything else exists).
2. The `security` change class for `auth`, `listen` and `agents`.
3. Named ingest credentials with `tables` and `operations`.
4. Listeners per surface, and fail closed.
5. Public grants: limits, reserved queue share, caps on new values, `origins`.
6. Server-filled columns.

#### Tests

- `tests/api.rs` walks the endpoint table: every endpoint is on exactly the listeners its `Access` maps to, and the public listener 404s ungranted tables and every admin path, also with an admin session cookie.
- A credential matrix over HTTP: table × operation × listener, each allowed or refused as granted.
- Idempotency: a second credential can't claim or evict the first one's keys. The crash suite with credentials in the stream: recovered keys keep their credential (with a planted bug that drops it).
- Limits: 429s at the rate, 413s past the size, private writes succeeding under a public flood (with a small queue), a new-values cap with the column named.
- Fail closed: each refusing startup and each allowed one.
- Plan scenarios in `tidepool-model/tests/plans.rs` for each `security` item, and a config test that `auto_apply` can't apply one.
- Filled columns: stamped on every write path (`POST`, `PUT`), refused when sent, kept through replay.
- The doc site gets a deployment page (public UI, private ingest, a public grant); the config reference is regenerated.

---

## 10. Frontend (v1)

### 10.1 Stack and build

- **Stack:**
  - Leptos 0.8, client-side rendering, built by trunk (D10)
  - charts of our own, in Rust (decided 2026-11-01, replacing the ECharts wrapper the M0 spike chose): `scale.rs` and `charts.rs` lay a chart out for its size (nice ticks, bands, label fitting, which bar a position means, the frames between two states) without a DOM, and `plot.rs` draws that as SVG coloured by `style.css`. Why: the UI ships no JavaScript library (1.1 MB of ECharts was a quarter of the download; the UI is 667 KB with brotli instead of 914 KB), charts look like the rest of the page and follow its theme, every mark is a DOM element the e2e can find, and selection works the same way from the pointer and the keyboard. What we gave up: every new chart type is ours to build.
  - a virtualised list component for filter lists (spike: `vlist.rs`)
  - a small dashboard grid (CSS grid from the YAML layout), and the dashboard editor on the same grid: written from scratch with pointer events and pointer capture (no grid library); the geometry (`layout.rs`) and the visual rules (`design.rs`) are DOM-free and unit-tested (decided 2026-10-13)
  - Monaco + monaco-yaml (with the generated JSON Schemas) for the config editor, through a thin wrapper (as built: a highlighted `<pre>` behind a textarea, `highlight.rs`)
  - frames are JSON (decided in M4: small, and serde on both sides); Arrow if frame size ever says so
- **State:**
  - signals hold `{model, selection, frames by epoch}`; a frame is applied to all of them at once (D11)
  - charts are fed a data memo; the layout for the new data is reached over 300 ms from wherever the drawing is (bars matched by category, points by period), and drawn at once on a resize or under `prefers-reduced-motion`
- **Build:**
  - `just frontend` = `trunk build --release` in `frontend/` → `frontend/dist` (hashed files under `assets/`, which the server caches as immutable)
  - `just build` = `just frontend`, then `cargo build --release -p tidepool --features embed-assets`
  - the `tidepool-server` crate embeds `frontend/dist` with `rust-embed` behind the `embed-assets` feature. It's off by default so `cargo check` never needs a frontend build (same reasoning as Kayak).
  - dev: `just dev` runs the server on :7070 and `trunk serve` on :7080 with hot reload, proxying `/v1` to the server
- **Types:** shared directly: the frontend depends on `tidepool-protocol`. There is no codegen step to keep in sync.

### 10.2 Features

**Dashboards**
- Dashboard list. Dashboard view on a 12-column grid from the YAML layout.
- **Dashboard editor** (`#/new`, `#/d/<id>/edit`; since 2026-10-13): visuals on the dashboard's grid, moved by dragging and resized by a corner handle (or arrow keys, Shift to resize), snapped to whole cells, never overlapping (a blocked drop shows red and doesn't move; the compiler enforces the same rule). A new visual starts from 0+ dimensions and 0+ metrics picked from the model (fields no relationship connects to the choice are greyed out); one dimension alone defaults to a `filter` visual, anything else to a `table`; the type can then be changed among those that fit. Each visual previews its own `/v1/query`. The editor reads and writes `dashboards/<id>.yaml` with `tidepool-config`'s types (`DashboardFile::from_yaml` / `to_yaml`, in the examples' compact style), plans every draft on the server as it changes, and saves through `/v1/config/apply` (admin). Comments in an edited file aren't kept (it says so); rail filters are left as they are.
- Visuals:
  - KPI tile (value, comparison metric, delta %)
  - bar (vertical/horizontal)
  - line/area with a time axis and grain; zoomed between grains (proposed, §5.8)
  - table (sortable, uses the server-side sort). Without metrics (2026-10-13): the distinct combinations of its dimensions, counted on one anchor (of the tables reaching every dimension, the one reaching fewest, ties by name); a group with no rows under the selection isn't one. Covered by the oracle (`SELECT DISTINCT` over the same anchor).
  - filter (2026-10-13): one dimension's list on the grid, no metrics, no grain; served from the same filter frames as a rail filter of that field (`Dashboard::lists`), so a field shown in both is one list
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
- **Maximize** (2026-11-03): a button in each visual's header (not on `filter` visuals) shows it alone, as large as the grid; Esc or the button puts it back. The rail and the selection bar stay, so the selection is still shown and still made there or in the visual. It's page state, not in the route: the other visuals stay mounted and live, hidden, so nothing is re-opened or re-asked of the server, and a chart only resizes.

**Model browser**
- Tables: columns, types, encodings, row counts, ingest rate, preview of the latest rows.
- Relationship diagram: a card per table (rows, write rate with a sparkline, segments, memory, retention, and its key, foreign key, event time and hierarchy rows), linked from each key row to the one it refers to. Lines are Kayak's orthogonal routes (grid-snapped channels, moved apart when they would overlap), with a crow's foot on the many side, a bar on the one side, and chevrons for which way selections flow (one, or two for `filter_direction: both`). Laid out as a tree on its side, facts on the right (`frontend/src/diagram.rs`); elkjs if models outgrow it. Read-only: a card opens its table, a line says what selecting either side does.
- Metric catalog: description, SQL, anchor table, which dashboards use it.
- Ingest docs per table: endpoint, example `curl` with sample JSON, link to the OpenAPI spec.

**Config**
- YAML editor over the config files, with validation as you type.
- "Plan" shows the structured plan; "Apply" applies (admin token).
- Version history with diffs and revert.
- Pending changes queue (approve/reject).
- Banner when the config dir has unapplied external edits.
- (M7: a text editor with the server checking as you type, rather than Monaco.)

**System**
- Status page: memory use by table/cache, epoch timing, WAL/checkpoint status, background jobs (sealing, rebuilds) with progress.

---

## 11. Testing strategy

Correctness is the main risk of an incremental engine. A bug shows up as **plausible but wrong numbers**. The test strategy is built around that.

1. **Oracle tests (`tidepool-oracle`)**:
   - A generator creates random models (star/snowflake, 1–4 dimensions, random types and encodings), random data, random metrics from the supported vocabulary, random view queries and random selections.
   - Each query runs on Tidepool and, translated to plain SQL, on DuckDB (`duckdb` crate, test-only, linked against DuckDB's release of its version: `.cargo/config.toml`).
   - Results are compared exactly (decimals) or within ε (floats).
2. **Incremental oracle:** the same, but the test interleaves appends, upserts, deletes, seals, selection changes and config changes. It checks every live subscription after every epoch against a cold DuckDB evaluation. This is the most important test in the repo, and it runs in CI with a fixed seed budget plus a nightly long run.
3. **Shadow verification in production:** an optional setting (`verify_sample: 0.01`) occasionally recomputes a live result cold, with no caches, and compares it. Mismatches are logged and counted as metrics.
4. **Crash/recovery:** fault injection in the WAL and the segment writer (crash at random points), restart, compare with the oracle. Also a `kill -9` loop test against the real binary.
5. **Concurrency:** `loom` for the tail watermark protocol and snapshot publication. No Miri: the workspace has no `unsafe` (`unsafe_code = "deny"`), so the job went on 2026-11-04. It comes back if that changes.
6. **Fuzzing (`cargo-fuzz`):** JSON/NDJSON/Arrow decoders, YAML config parsing and compilation, expression parser.
7. **Property tests (`proptest`):** dictionary encode/decode, bit-packing, aggregate-state merge laws (associativity, commutativity, identity).
8. **Performance:**
   - `criterion` microbenchmarks for kernels, packing and merge.
   - `tidepool-bench` (a workspace crate, like `kayak-bench`; `just bench`, not part of `just ci`): a synthetic retail generator at a configurable scale and ingest rate, plus N simulated sessions clicking randomly. It reports the §2.3 metrics.
   - Benchmark results are tracked per commit on a fixed machine (later).
9. **Frontend:** plain `cargo test` for the frontend's pure logic (state transitions, frame application, formatting); `wasm-bindgen-test` where the DOM is needed. A browser e2e (`just e2e`: headless Chrome over its debugging protocol, from a Rust test) for: open dashboard → click bar → selection applied → live update arrives → back.

---

## 12. Milestones

Effort is in focused engineer-weeks and is rough. The frontend track can run in parallel from M2 against a mocked protocol.

### M0: Skeleton + frontend spike (1–2 wks)
**Status: done 2026-10-07.**

- Cargo workspace with the crates above. `justfile` with `dev`, `run`, `build`, `frontend`, `ci`, `test`, `lint`, `deny`, `bench`.
- Lints as in Kayak: clippy `all` + `pedantic`, `unwrap_used`/`expect_used`, applied to tests too (`clippy.toml`).
- CI (GitHub Actions running `just ci`): fmt, clippy `-D warnings`, test, frontend lint and test, a licence-declaration test, `cargo deny`; beside it the release build, `just e2e` and the doc site. `deep.yml` runs the oracle and the crash suites at depth nightly.
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

**Status: in progress.** Done: expression parser, type checker and interpreter; planner with one-directional selection, `ignore_selection` and derived metrics; a general executor and a columnar fast path (morsels, branch-free selection vectors, width-specialized unpacking, relationships as arrays by foreign-key code, dense grouping, distinct-count bitsets that fold out dimensions the counted key determines); derived dictionaries (a derived field over one dictionary column groups and filters like a stored one); caches in an `Engine` (relationship maps, join columns materialized per anchor segment, derived dictionaries, answers per epoch), keyed by the identity of immutable store data; every path checked against DuckDB on every oracle query, plus a replay of every intermediate snapshot through a cached engine; `POST /v1/query`. Semantics settled on the way: a missing related row reads as all NULLs, as in a SQL `LEFT JOIN`, so a derived field over it is still evaluated. Measured with `just bench-query` on the M1 Max (10 cores, with other load: load average 15–30 during runs, so ±15–20 ms) at 100M orders, region × category, 200 selections each a new combination of regions and statuses: revenue, order count and paid revenue **fresh selection p50 ~68 ms / p95 ~113 ms**; grouped by a derived field (`customers.coast`) instead of `region`, p50 61 / p95 103 ms; with `COUNT(DISTINCT customer)` in place of paid revenue, p50 79–109 / p95 130–174 ms over two runs; toggled back < 1 ms; first query ever 170–290 ms (builds join columns and maps); 2–2.6 GB resident. Measured and dropped: selection-mask caches (§5.2 step 4). With branch-free filtering a condition costs about as much as reading a mask, and its columns are usually read anyway for dimensions or filters. Left: fresh-selection p95 under 100 ms on this machine (the target is for a 16-core reference machine); derived fields over non-dictionary columns (e.g. `EXTRACT(YEAR FROM ordered_at)`) still take the general path.

- Expression parser, type checker, kernels; aggregate states; dense and hash grouping.
- Derived dictionaries (moved from M2: they need expression evaluation).
- Selection engine (§5.2) with mask and partial caches. Derived metrics. `ignore_selection`.
- `POST /v1/query`.
- Oracle framework (static data).
- **Exit:**
  - oracle green on 10k random cases
  - 100M-row reference: cold group-by with 2 dimensions and 3 metrics < 100 ms, selection change p95 < 100 ms

### M4: Live (3–4 wks)

**Status: done 2026-10-07.** Incremental answers: the engine keeps each anchor's aggregate state with a row-id high-water mark and the deleted rows already accounted for, scans only rows appended since, and subtracts rows deleted since (count/sum/avg; MIN/MAX and distinct counts recompute when rows are deleted; a changed related table recomputes, the known cost of §6.5). Distinct counts keep running per-group counts as bits go 0→1. Subscriptions aren't a separate structure: every visual runs through the one shared engine with its selection cut to the fields that can change it, so sessions showing the same thing share one answer per epoch and one incremental state. Sessions with selection history, filter lists with selected/possible/excluded states, one frame per session per epoch carrying only what changed, coalescing (always the newest snapshot), pause/resume. Incremental oracle: live queries checked against DuckDB after every epoch while writes (appends, keyed-fact rewrites and deletes, dimension upserts) are applied to both. Measured on the M1 Max: an epoch of 100k new orders costs 0.6–0.9 ms (1.6–2 ms with `COUNT(DISTINCT customer)`) at 10M and at 100M orders; 50 WebSocket sessions clicking regions while ingesting 100k rows/s see new data p50 128–180 ms / p95 220–228 ms after the ingest ack (1M and 10M orders loaded; 200 ms epochs). A thin dashboard page (M6's first slice) shows the retail dashboard live. Changes from the plan: frames are JSON, not Arrow-framed (§7); no separate subscription refcounting or priority queue (the engine's caches give the sharing, and selection changes are answered outside the epoch loop); `compare` metrics (time shifts) aren't shown yet.

- Incremental tail partials, invalidation rules, result cache.
- Subscription manager with dedupe, priorities and coalescing.
- WebSocket protocol, sessions, selection history, filter windows, epoch frames.
- Incremental oracle.
- **Exit:**
  - incremental oracle green
  - freshness p95 < 1 s at 50 simulated sessions while ingesting 100k rows/s
  - per-epoch cost shown to be independent of total rows (benchmark at 10M vs 100M)

### M5: Durability (3–4 wks)

**Status: done 2026-10-07.** WAL with group commit (the writer drains a burst of batches, logs and applies them, syncs once, then acknowledges; `durability: async` acknowledges before the fsync and syncs on the epoch clock), CRC-framed records of parsed values, rolling files. Checkpoints on their own thread from a published snapshot: segment files written once each, dictionaries appended, the checkpoint file written last and atomically; two kept, unreferenced files and covered log files removed. Recovery loads the newest checkpoint that reads back, re-encodes dictionaries in code order, rebuilds primary-key indexes and replays the log; a changed table layout is refused with a pointer to M7. Retention deletes sealed segments past the window (not logged: rows expired since the last checkpoint come back until it runs again). Crash injection: 1,000 crash points (power loss: unsynced work dropped, the log cut to its last fsync plus a random torn tail, some crashes inside checkpoints, the log sometimes forgotten late) recover exactly the acknowledged writes, and planted bugs in the log, dictionary files and log truncation are caught; a `kill -9` loop against the binary (25 kills) loses no acknowledged order. Restart of the 100M-order reference: 1.2 s (first checkpoint of it: 10 s, in the background). Changes from the plan: segments are read into memory, not mmap'd (mapping needs `unsafe`, and the store has none), and WAL payloads are a small binary codec rather than Arrow IPC.

- WAL with group commit, segment file format, dictionary files, checkpoints, recovery, retention job, mmap.
- Crash-injection tests and the `kill -9` loop.
- **Exit:** no acknowledged row is ever lost across 1,000 random crash points; restart of the 100M-row reference in < 10 s.

### M6: Frontend v1 (4–6 wks; parallel from M2)

**Status: done 2026-10-07.** Dashboard list and dashboards on the YAML grid: KPI tiles with a comparison and delta, bar charts (horizontal past six bars) and line/area charts with a second axis, both on one ECharts instance per visual fed a derived option (vendored ECharts 6.1 with its licence); tables sorted by the server per session, with the sort kept across frames. Filter lists with the three states, search and a window the server fills as you scroll; a date range. Click or Ctrl/Cmd-click on a bar, table row or list value selects; brushing the time axis selects a half-open range; chips, back, forward, clear. Live strip ("updated … ago", epoch, ingest rate), pause with an "N updates waiting" badge, a brief highlight on changed cells. Model browser from the new `GET /v1/model`: tables (columns, encodings, rows, an ingest `curl` with a sample row, a preview), a relationship diagram, the metric catalog. Status page from the new `GET /v1/status` (epoch, uptime, sessions, memory by table, dictionaries and caches, log and checkpoint). Hash routes, light and dark, usable at phone width. The e2e (`just e2e`) opens the retail dashboard in headless Chrome, clicks a bar, checks the chip, the list and the KPI, posts orders and sees the KPI follow under the selection, and goes back; a click handler broken on purpose fails it. Changes from the plan: the e2e drives Chrome over its debugging protocol from a Rust test instead of Playwright (no node toolchain, CLAUDE.md), and isn't in `just ci` because it needs Chrome; the relationship diagram is a small layered layout, not elkjs; legends hold metrics, not values, so they don't select; KPI comparisons with `time_offset` metrics are left out until M8 schedules them; the status page has no epoch timings or background jobs yet (jobs arrive with M7's rebuilds); no `wasm-bindgen-test`, as the DOM paths are what the e2e covers.

- Everything in §10.2 except the config section.
- Browser e2e.
- **Exit:** the retail demo works end to end in the browser, with a live data generator running.

### M7: Live config (3–4 wks)

**Status: done 2026-10-08.** One pipeline for every change (`tidepool-server::config`): a change is files to set or remove against a `base_version`, compiled against the catalog, planned, checked against policy (destructive needs `approve`; `propose` and unsafe edits in the config directory go to the approval queue), handed to the writer, recorded as a version, written back to the config directory with `renamed_from` hints removed, and committed when the directory is in a git repository. Endpoints: `GET /v1/config`, `POST /v1/config/plan`, `POST /v1/config/apply`, `GET /v1/config/versions[/{v}]`, `GET /v1/changes`, `POST /v1/changes/{id}/approve|reject`. In the store, a model change re-shapes tables by column id: unchanged columns are shared (`Arc` per column), a new nullable column is a virtual all-NULL column holding nothing, removed tables and columns are dropped, and dictionaries are re-keyed. Joining populated tables builds one new dictionary per key domain and maps each source's codes into it; encoding and type changes convert values (exactly, or NULL where a value doesn't fit). That rewrite runs on its own thread from a snapshot while the old model serves and ingest carries on; the writer then catches up with what arrived meanwhile and swaps the model in at an epoch. Batches decoded under the old model are fitted to the new columns by id. The model is logged in the WAL at the swap and kept in checkpoints, rewritten segments and rebuilt dictionaries get a generation in their file names, and startup reconciles the config directory with the data's model (destructive only with `--allow-destructive`). Sessions get the model again on a change; selections on removed fields are dropped and a removed dashboard is closed. The config UI edits the files with the plan checked as you type (problems located, click to jump), applies or proposes, shows the history with diffs and reverts, and the approval queue. Tests: migration tests per kind of change, the crash suite with model changes in the stream, a migration oracle (data written under an earlier version of each random project, a background build with writes going on, then every query against DuckDB on all three paths; 400 seeds; a planted remap bug fails at seed 0), the pipeline over HTTP including the exit criterion and a restart, and an e2e of the config editor. Changes from the plan: the watcher polls the directory every second rather than using `notify` (no new dependency, deterministic in tests); the editor is a plain text area with server-side checking, not Monaco (which needs a node toolchain); a second change waits while a rebuild runs rather than queueing behind it; rebuild progress is rows rewritten, without a time estimate.

- Apply pipeline in the running server; background rebuild jobs (dictionary merge for new relationships, type widening).
- Write-back with git commits; file watcher; approval queue; version history and revert.
- Config UI (editor, plan, apply, history, pending queue). Clients follow model version changes.
- **Exit:**
  - an agent or human adds a table, posts data to its new endpoint, adds a relationship and a metric, and sees it on a dashboard without a restart
  - the config dir reflects every change, and a fresh `tidepool serve` from it reproduces the same model

### M8: Agents and v0.1 polish (2–3 wks)

**Status: done 2026-10-08.** MCP at `POST /mcp` (Streamable HTTP, JSON-RPC answered with one JSON body; protocol versions 2025-06-18, 2025-03-26 and 2024-11-05) with the §9.1 tools; each tool is an in-process call of the documented endpoint with the caller's credentials, and `apply_change` applies what `agents.auto_apply` allows (safe by default) and proposes the rest. Auth: token names under `auth:`, the admin token good for every level, browser sessions (`/v1/auth/session`, an `HttpOnly` cookie; the UI trades `?token=` for one and drops it from the address bar; the config pages ask for the admin token). `GET /metrics` in the Prometheus format; epoch publish and live refresh times on the status page. `time_offset` metrics: the time field's column is moved by the offset in the metric's own anchor plan (derived fields over it move too), on the general path and in the fast path as values load; the oracle generates them (400 seeds; a fast path that doesn't move values fails), and the retail KPI shows its year-before comparison. CSV: `POST /v1/query?format=csv`, and a CSV button on every visual (its query under the selection). `tidepool init` writes the retail example; `tidepool feed` sends it ninety days of orders and then live ones; a test runs init → validate → serve → feed → query in about three seconds. A Dockerfile (the UI embedded, 112 MB, non-root, the example as the default project) and `examples/kayak-demo`: a simulated plant on MQTT → Kayak → Tidepool's plant-floor dashboard, from one `docker compose up`. User documentation in `docs/guide/` (since 2026-10-12 the doc site, `website/`). Changes from the plan: MCP is hand-written rather than `rmcp` (a few methods; one axum stack); of Kayak's auth stack, the session cookie is in and accounts with passwords, JWT/JWKS and the iframe handshake come after v0.1; the demo uses Kayak's generic `http` output until K1's `tidepool` output exists; the epoch interval now changes live, as §8.2 says it should.
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

**Status: done 2026-10-08** (in the Kayak repo: `src/outputs/tidepool.rs`, a `tidepool` connection kind, the `sensors_to_tidepool` example pipeline, the site's "tidepool" page). An offline suite against a fake Tidepool on a loopback port that dedups by `Idempotency-Key` as the real one does: the NDJSON body and headers, every startup mismatch (unknown column, wrong type, null into a required column, a required column nothing writes, no such table), a `400` quoted and not retried, the table read again after a refusal, and a `503` after the write landed retried under the same key with the rows written once (a key minted per attempt fails it). `just demo-check` is the compose test: the demo on its own ports, Tidepool cut off the network for six seconds midway, then Tidepool's `readings` rows must equal the messages Kayak's history says it handed the output, with no output errors. The demo now uses the `tidepool` output. Changes from §15.2: the wire types are written out in Kayak rather than taken from `tidepool-protocol`, because the two repositories don't share a workspace and Kayak builds on its own CI; `emit` retries a busy or unreachable server itself for up to `retry_seconds` (Kayak's run loop drops a batch whose `emit` fails, so backpressure handled there would lose data); schema drift is detected by a refusal (a `400`, `404` or `409`) rather than a config version in the response, after which the next batch re-reads the table; the epoch goes to Kayak's debug log; Arrow IPC is not done. Tidepool's idempotency keys are durable with the writes they acknowledge (§4.5): logged in the batch's WAL record with its response, kept in checkpoints and rebuilt on recovery, so a retry across a Tidepool restart writes nothing twice. That is covered by the crash suite (recovered keys equal the acknowledged ones at every crash point; planted bugs that drop keys from checkpoints or from replay fail it), the `kill -9` loop (the batch in flight at each kill is retried under its key, and the table must then hold exactly the acknowledged orders) and a restart test over HTTP.

**Total:** roughly 26–35 engineer-weeks to v0.1.

### After v0.1 (ordered by expected value)

1. ~~Bidirectional filtering for filter widgets and selections~~ (done: `filter_direction: both`, §5.4)
2. ~~Exposure (§9.2, D15): credential-scoped idempotency keys, the `security` change class, scoped ingest credentials, listeners per surface with fail closed, public ingest grants with limits, server-filled columns~~ (done 2026-10-17)
3. Late binding of dimension attributes (no fact rescans on dimension upserts)
4. Percentiles/median, approximate distinct (now part of 19, §6.8)
5. Segment compaction and dictionary compaction
6. Row-level deltas in frames for large tables
7. Bookmarks and shareable selection URLs
8. ~~Drag-and-drop dashboard builder~~ (done 2026-10-13: the dashboard editor, §10.2), visual relationship editor (writing YAML through the pipeline), then editors for the other config objects the same way
9. Row-level security (mandatory per-user selection applied before everything else)
10. Kayak `tidepool` **input**: subscribe to a live metric query and turn updates into messages (alerting, webhooks), see §15.3
11. Perspective-based pivot visual
12. Engine self-metrics exposed as Tidepool tables (`_system.*`), i.e. a dashboard of Tidepool in Tidepool
13. The rest of Kayak's auth stack (§9): accounts with passwords, JWT validated against an identity provider's JWKS, and the iframe handshake for embedding
14. `time_offset` metrics over derived time fields and dictionary-encoded time columns on the fast path (they take the general path today)
15. ~~Relative time windows ("Today", "Last 7 days") on time selections and dashboard defaults~~ (done: §5.5)
16. ~~Hierarchical (parent–child, ragged) dimensions with tree filters, and measures on filters~~ (done: §5.6); replace ingest (§4.5) and indud's namespace through Kayak (§15.5)
17. GitOps: the config from a branch of a remote repository, plan checks on pull requests, UI changes pushed as commits. Design in [gitops.md](gitops.md)
18. A paid SaaS: a control plane and single-tenant cells (D2 and D3 kept). Threads in [saas.md](saas.md)
19. The measure vocabulary (§6.8, D16): `total`, `set`, more aggregates, `per`/`then`, `time_window`, sets from sets
20. Text search as a selection and the `rows` visual, for logs (§5.7), with an example of Postgres logs (`examples/pglogs`)
21. Time zoom (§5.8, D17): `hour` and `minute` grains (done 2026-11-04), an `auto` grain from the range, a viewer's grain per visual, a time scale with gaps for grained axes

---

## 13. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Incremental results silently wrong | medium | severe | oracle + incremental oracle in CI, nightly long runs, production shadow verification (§11) |
| Tail concurrency bug (unsafe) | retired in M2 | — | the store has no `unsafe`: immutable chunks frozen per epoch instead of a shared tail |
| Dictionary growth from high-cardinality columns | high | memory blow-up | `plain` encoding defaults for numerics, warnings and plan hints, dictionary compaction after v0.1 |
| Ingest reachable from the internet by accident (one listener; an unset token is open) | high, for any public deployment | data integrity (rows written, `PUT` empties a table), memory (dictionary growth) | D15 / §9.2: private ingest by default, fail closed off loopback, public grants scoped and limited |
| Dimension upserts cause frequent full rescans | medium | latency | column-change detection (v1); late binding (post v0.1) |
| Ossie spec churn (0.1 → 0.2 → 1.0) | high | low–medium | pin a version; all Ossie handling in `tidepool-config`; an internal IR; migrate with a converter |
| YAML crate ecosystem churn (serde_yaml deprecated) | medium | low | `serde_norway` (already proven in Kayak), isolated behind `tidepool-config` |
| Kayak and Tidepool overlapping (aggregation in both, query features in Kayak) | medium | product confusion | D12 boundary: Kayak computes over messages in motion, Tidepool over accumulated history with a selection applied |
| Scope creep in the UI | high | schedule | every UI editor writes the files through the one pipeline (§8.1), so the files stay the source of truth; one object kind at a time (dashboards first) |
| SQL expectations beyond the v1 expression set | medium | user friction | clear errors listing supported functions; add functions on demand; `/v1/query` for ad-hoc work |

---

## 14. Open questions

1. ~~License~~: decided, D13.
2. **Viewer identity:** Kayak's auth stack (D14) gives us named users via Basic/JWT. Should sessions and selection history be tied to the user (resume on another device) or only to the browser in v0.1?
3. **Time zones:** dashboard-level time zone for grains (`day` in which zone)? Proposal: a `timezone` setting in `tidepool.yaml`, overridable per dashboard. Time zoom (§5.8) makes it more pressing: zooming out from hours shows UTC days.
4. **Late-arriving data with `event_time` far in the past:** accept always (affects zone maps only), or reject outside a window?
5. **Ossie upgrade:** we write the 0.2 shape already (D8). When 0.2 is released, check whether it adds any of our extension concepts (derived metrics, filters, time grains) natively, and read those too.
6. **Who approves a `security` change?** (§9.2) Approval is an admin endpoint, and agents use the admin token too, so an agent could approve its own proposal. Options: approval only from a browser session (a human at the UI), or a separate approver token.
7. **Stronger producer identity for edge sites** (§9.2): mTLS or HMAC-signed requests (a timestamp against replays) for Kayaks pushing across the internet. Kayak's `tidepool` output would have to speak it too. After the bearer-token version is in.
8. **Metrics other Ossie tools misread** (§6.8): a metric whose meaning depends on its `TIDEPOOL` extension still carries a plain `ANSI_SQL` expression, which other tools compute without the extension. Options: accept it and say so in `describe_model` and the docs; read a `TIDEPOOL` dialect first and keep `ANSI_SQL` only for metrics that mean the same without the extension; or have `validate` warn. Related: extension `data` is a JSON string because Ossie says so, which is heavy to write by hand. Should we also accept a YAML mapping there and write it back as a string?
9. **Name check:** does `tidepool` collide on crates.io/npm or with trademarks in our space? (Rename cost is lowest now.)

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

### 15.5 Dimensions from indud: `indu_namespace` and `write: replace`

**Status: done 2026-10-10** (in the Kayak repo). Kayak's `indu_namespace` input reads indud's unified namespace (`GET /api/v1/namespace/nodes`) whole on a timer, a branch at a time where indud caps a reply (5 000 nodes, five levels, `truncated`), and hands each read on as **one batch**: every node a message `{id, parent_id, label, kind, unit}` with indud's kind-prefixed ids. The `tidepool` output's `write: replace` sends each batch as a `PUT` (§4.5), so a node moved, renamed or deleted in indud is the same in Tidepool at the next read; indud keeps no change feed for its tree, so reading it whole is the only way to notice. Kayak only builds a replace output in a pipeline whose batches are whole snapshots (one unbuffered `indu_namespace` input, transforms only `map`/`filter`), refuses an append table on start, and never sends a snapshot its mapping skipped entirely. `examples/indud-oee` is the project it feeds: the namespace as a hierarchy (§5.6), and an OEE dashboard over per-machine minutes.

### 15.4 The demo

`examples/kayak-demo/docker-compose.yaml`: an MQTT broker with a simulated plant publisher → Kayak (map, buffer, `tidepool` output) → Tidepool with a `machines` / `readings` / `stoppages` model → a live plant-floor dashboard. It's the showcase for both products and the integration test for K1.

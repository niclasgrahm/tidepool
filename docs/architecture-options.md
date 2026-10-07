# Live associative analytics: two candidate architectures

Status: proposal, nothing built yet · 2026-10-06

## 0. What we are building (restated precisely)

A BI server that:

1. Keeps data in a fast, compressed, in-memory columnar store organised as **tables**.
2. Takes data **pushed over HTTP** (the main ingest path). The ingest API's shape comes from a declared schema.
3. Has an explicit **semantic model**: relationships between tables (Power BI-style, not Qlik's "same field name"), plus reusable **metrics**.
4. Serves **dashboards** (collections of visualisations) whose results **update continuously** as data arrives.
5. Has a **global selection state** per user session. Filter widgets and clicks on chart elements both change it, and every visualisation reacts to it.
6. Is configured as code: a **physical schema** file (tables, types, keys, ingest behaviour) and a **semantic model** file (relationships, metrics). Where possible the semantic file uses an open standard (Apache Ossie).

---

## 1. Discussion and pushback

### 1.1 "Must fit in RAM" doesn't go away. Streaming makes it a bit worse.

Qlik's RAM ceiling comes from its design ("everything is a resident, dictionary-encoded column"), not from an implementation mistake. A streaming version keeps that constraint and adds two costs:

- the data grows without limit unless you set **retention**, and
- incremental computation needs **state** (partial aggregates, indexes, caches) on top of the raw data.

What actually fixes it is **tiering**: recent and hot data stays resident, and sealed historical segments live in files that are memory-mapped or paged in when needed. You lose some speed on cold scans, not correctness. Each table also needs a declared retention policy (e.g. `retain: 400d`, or "roll up to daily after 90d").

For scale: dictionary-encoded columnar data usually compresses 5–20× compared with raw CSV, and a 1–2 TB RAM box is ordinary today. Most BI datasets fit. The datasets that don't fit are the ones where you need tiering.

### 1.2 "Full reload" is really a durability and incrementality problem

Qlik reloads because its storage is a snapshot rebuilt by a script. Push ingest fixes the *steady state*, but you get the same pain at **restart** unless you have:

- a **write-ahead log** (ack after the batch is durable), and
- periodically persisted **immutable segments**, so restart means "mmap segments + replay the WAL tail" (seconds), not re-ingesting everything.

Schema changes are the other "reload" trigger. Adding a column must be cheap. Changing a key or a relationship may require rebuilding indexes, and the system should do that online.

### 1.3 "Every calculation continuously updated" needs a sharper definition

This is the most important pushback. The associative/global-filter UX means **the set of possible queries is combinatorial**: every selection state is a different query. Classic incremental view maintenance (IVM, e.g. Feldera, Materialize, RisingWave) maintains a *fixed* set of views. It isn't built for "user clicks a bar, now 14 visualisations have new WHERE clauses."

The realistic model:

- The unit of liveness is a **subscription** = (visualisation query, selection state). Only subscriptions that someone is currently looking at are maintained. Identical subscriptions across users are deduplicated.
- **Selection change**: compute from scratch (this must be fast, Qlik-fast). Then maintain incrementally.
- **New data**: update every live subscription incrementally, or recompute it cheaply.
- Updates are applied in **epochs** (micro-batches, e.g. every 100–500 ms when data has arrived). Humans can't perceive more than a few updates per second. Per-event propagation costs a lot and buys nothing on a dashboard.
- **Consistency matters more than latency.** All visualisations on a screen must show the *same epoch*. Otherwise a KPI tile and the bar chart beside it disagree, and users lose trust immediately. Every result carries an epoch id, and the client swaps them atomically.

Suggested SLO: *data acknowledged by the ingest API is visible in all live visualisations within 1 s (p95), and all visualisations on a dashboard are mutually consistent.*

### 1.4 Ingest semantics: append vs upsert must be declared

"Data streams in" hides a big fork:

- **Append-only facts** (events, transactions) are cheap. Every common aggregate is easy to maintain incrementally.
- **Upserts/deletes** (dimension tables, corrected facts) mean *retractions*. MIN/MAX/COUNT DISTINCT/percentiles stop being trivially incremental, and a dimension attribute change (customer moves region) can shift results across groups.

Each table should declare its mode: `append`, `upsert` (by primary key), or `append_with_deletes`. Facts are almost always `append`. Dimensions are almost always `upsert`, are small, and changes to them can trigger a targeted recompute.

### 1.5 Explicit relationships are right, but cross-filter direction needs a decision

Qlik propagates selections across associations **in all directions**. Selecting a product greys out customers who never bought it. Power BI defaults to **one direction** (from the 1-side to the many-side), because bidirectional filtering through a fact table is ambiguous and expensive.

If we want the Qlik feel ("possible / excluded values" in every filter list), we need bidirectional propagation **for selection and possible-values computation**, while **metric aggregation** strictly follows many-to-one paths. That split is the right default, and it should be stated in the spec of our product.

You also inherit the classic modelling hazards, and the model compiler has to reject or resolve them:

- **Ambiguous paths / loops** (Qlik calls them circular references). Example: `orders → date` via `order_date` *and* `ship_date`. Require **role-playing** relationships with distinct names and an explicit choice of one in metrics and visualisations, or alias the dimension.
- **Fan traps / chasm traps**. Two fact tables sharing a dimension (orders and returns by customer). Aggregate each fact at its own grain *first*, then join at the visualisation grain. Never join facts row-to-row. Looker calls this "symmetric aggregates", and MetricFlow does it by aggregating each measure separately before joining.

### 1.6 "SQL, not our own language" is right, but with one honest caveat

SQL expressions cover row-level fields and basic aggregates. Qlik's set analysis exists because of things a plain SQL fragment can't express *relative to a selection context*:

- "Sales, **ignoring** the region selection" (share of total, benchmarks)
- "Sales for **the same period last year**" (time shift relative to the current selection)
- "Ratio of two metrics" (must be aggregated separately, then divided)

The non-proprietary way: keep **expressions in SQL** and express these modifiers as **structured YAML attributes** on metrics (`ignore_selection: [region]`, `time_offset: -1y`, `type: ratio`). That's a small, declarative vocabulary, not a language. MetricFlow and Cube both do roughly this.

### 1.7 Apache Ossie: use it, but don't rely on it alone

What I found (as of Oct 2026):

- Apache Ossie (formerly Open Semantic Interchange) entered the Apache Incubator mid-2026. The latest stable spec is **0.1.1**, and **0.2.0** is in development. Backers include Snowflake, Databricks, dbt Labs, Microsoft, Salesforce, **Qlik** and about 50 others.
- The spec covers **datasets** (source, primary/unique keys, fields with optional datatypes), **relationships** (many-to-one foreign keys, composite supported), and **metrics** (expressions with per-dialect variants: ANSI_SQL, Snowflake, Databricks, …), plus `custom_extensions` and `ai_context`.
- The spec does **not** yet cover filters, time grains, **derived/ratio metrics**, **metric-level filter overrides**, or **cross-filter direction**, which are exactly the parts of 1.5–1.6 we need.

Recommendation:

- Use **Ossie as the native file format for the semantic layer**, pinned to a spec version.
- Put everything Ossie lacks into a **namespaced `custom_extensions` block** (`vendor: <ourproduct>`). That's the extension mechanism the spec intends, and other Ossie tools will preserve it.
- Internally, compile Ossie and the extensions into **our own model IR**. The engine never reads YAML directly, so spec churn (0.1 → 0.2 → 1.0) is absorbed in one translation layer.
- Bonus: Ossie's `ai_context` gives a natural-language "ask the dashboard" feature something to work with later.

### 1.8 Two config files: yes. Single source of truth: decide now.

The split is right:

- `schema.yaml` (physical): tables, column types, primary keys, ingest mode, retention. This generates the HTTP ingest API and its validation.
- `model.ossie.yaml` (semantic): datasets referencing physical tables, relationships, metrics.

But you also want users to "connect tables" in a **UI**. If both the UI and the files are editable sources of truth, they will drift. Recommendation: **the files are the truth**. The UI is an editor that reads and writes them through an API, which validates before saving, with optional git-backed history. Dashboards should be files too (`dashboards/*.yaml`), so the whole app is diffable and reviewable.

### 1.9 Things not in the brief that will come up quickly

- **"Global" filters are per session**, not shared across all users (Qlik works this way). Also: bookmarks / shareable selection URLs.
- **Row-level security** (Qlik's "section access"): a per-user mandatory selection, applied before everything else. It's cheap to design in early and painful to retrofit.
- **High-cardinality dimensions** in filter lists (a million customer ids) need search and virtualised lists, not a "possible values" dump.
- **HTTP ingest formats**: JSON per row is fine for demos and too slow for volume. Support NDJSON batches and **Arrow IPC** bodies from day one, with **idempotency keys** for safe retries. Kafka / CDC connectors can come later as adapters that call the same internal ingest path.

---

## 2. Example configuration (shared by both architectures)

### 2.1 `schema.yaml`: physical tables and ingest

```yaml
version: 1
tables:
  customers:
    mode: upsert                 # append | upsert | append_with_deletes
    primary_key: [customer_id]
    columns:
      customer_id: { type: string }
      name:        { type: string }
      region:      { type: string }
      segment:     { type: string, nullable: true }
  products:
    mode: upsert
    primary_key: [product_id]
    columns:
      product_id: { type: string }
      category:   { type: string }
      list_price: { type: decimal(12,2) }
  orders:
    mode: append
    retention: 730d
    event_time: ordered_at       # drives time partitioning / segment sealing
    columns:
      order_id:    { type: string }
      customer_id: { type: string }
      product_id:  { type: string }
      ordered_at:  { type: timestamptz }
      quantity:    { type: int32 }
      amount:      { type: decimal(14,2) }
```

The schema generates the ingest API:

```
POST /v1/tables/orders/rows        Content-Type: application/json | application/x-ndjson | application/vnd.apache.arrow.stream
     Idempotency-Key: <uuid>
→ 202 { "accepted": 5000, "epoch": 81723 }       # epoch at which rows become visible
GET  /v1/tables                                  # list raw tables + schemas (for the "connect tables" UI)
```

### 2.2 `model.ossie.yaml`: semantic layer (Ossie + our extensions)

Shape follows Ossie core spec 0.1/0.2. Our extension keys are illustrative.

```yaml
version: "0.1.1"
name: sales
datasets:
  - name: orders
    source: orders                # physical table from schema.yaml
    primary_key: [order_id]
    fields:
      - name: ordered_at
        expression: { dialects: [{ dialect: ANSI_SQL, expression: ordered_at }] }
        datatype: DateTimeTz
        dimension: { is_time: true }
  - name: customers
    source: customers
    primary_key: [customer_id]
    fields:
      - name: region
        expression: { dialects: [{ dialect: ANSI_SQL, expression: region }] }
        datatype: String
  - name: products
    source: products
    primary_key: [product_id]

relationships:
  - name: order_customer
    from: orders
    to: customers
    from_columns: [customer_id]
    to_columns: [customer_id]
  - name: order_product
    from: orders
    to: products
    from_columns: [product_id]
    to_columns: [product_id]

metrics:
  - name: revenue
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
  - name: order_count
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "COUNT(DISTINCT orders.order_id)" }] }
  - name: avg_order_value
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount) / COUNT(DISTINCT orders.order_id)" }] }
    custom_extensions:
      - vendor_name: ourproduct
        data: '{"type":"ratio","numerator":"revenue","denominator":"order_count"}'
  - name: revenue_share_of_region_total
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
    custom_extensions:
      - vendor_name: ourproduct
        data: '{"type":"share","of":"revenue","ignore_selection":["customers.region"]}'
  - name: revenue_last_year
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
    custom_extensions:
      - vendor_name: ourproduct
        data: '{"time_offset":"-1y","time_field":"orders.ordered_at"}'

custom_extensions:
  - vendor_name: ourproduct
    data: '{"cross_filter":"bidirectional_for_selection","default_time_field":"orders.ordered_at"}'
```

### 2.3 `dashboards/sales.yaml`

```yaml
name: Sales overview
filters: [customers.region, products.category, orders.ordered_at]
visuals:
  - id: kpi_rev
    type: kpi
    metrics: [revenue, revenue_last_year]
  - id: by_region
    type: bar
    dimensions: [customers.region]
    metrics: [revenue]
    on_click: select              # clicking a bar selects that region globally
  - id: trend
    type: line
    dimensions: [{ field: orders.ordered_at, grain: day }]
    metrics: [revenue, order_count]
```

---

## 3. Shared conceptual core (both architectures implement this)

```
 schema.yaml ─┐
              ├─► Model compiler ─► Model IR (tables, typed columns, relationship graph,
 model.ossie ─┘    (validate: types,          metric plans with modifiers, role-playing paths)
                    cycles, fan traps)
                                         │
 HTTP ingest ─► WAL ─► Epoch committer ──┤  epoch N: consistent snapshot of all tables
                                         ▼
 Client (WebSocket) ── selection Δ ──► Session state ─► Subscription manager
         ▲                                              (dedupe identical (query, selection) pairs)
         │                                                       │
         └──────── result Δ tagged with epoch N ◄── Executor ◄───┘
```

Key contracts that should stay the same whichever engine is behind them:

- **Model IR**: the compiled, validated semantic model.
- **Subscription protocol**: client sends `{dashboard, selection}`, server streams `{visual_id, epoch, result | delta}` as Arrow IPC frames, and the client renders an epoch once every visual in view has reported it.
- **Selection semantics**: a selection is a set of values per field. It propagates over relationships (bidirectionally for filtering and possible values) and is applied to metrics with their declared modifiers.

These contracts are what let us start with Architecture A and swap in B later.

---

## 4. Architecture A: assemble existing open source

**Principle:** reuse proven engines. Write only the semantic compiler, the reactive subscription layer and the UI. SQL everywhere.

### 4.1 Components

| Concern | Choice | Notes / alternatives |
|---|---|---|
| Storage + execution | **DuckDB** (in-process, in-memory with on-disk persistence) | Very fast vectorised scans and aggregations, MVCC so reads don't block appends. Alternative for > single-box scale: **ClickHouse** (server, disk-first, handles "doesn't fit in RAM" well, incremental MVs on append-only data). |
| Cold tier / history | **Parquet** on local disk or S3 (optionally **DuckLake** / Iceberg as table format) | DuckDB queries resident tables and Parquet together, so retention means moving sealed partitions to Parquet. |
| Ingest log | Built-in WAL (DuckDB's), or **Redpanda/Kafka** if multiple producers or replay are needed | Start without Kafka. Add it when a second consumer exists. |
| Ingest API | Small service (Python FastAPI or TS) generated from `schema.yaml` | Validates with Arrow schema, appends with DuckDB's Arrow appender, assigns epoch. |
| Semantic layer format | **Apache Ossie** + `custom_extensions` | Validate with Ossie's JSON schema and tooling. |
| Semantic → SQL compiler | **Own thin compiler on SQLGlot** | Selection propagation is the product's core semantics, and no existing tool models it. SQLGlot handles parsing metric expressions, AST rewriting (injecting selection predicates, `ignore_selection`), and dialect transpilation (DuckDB ↔ ClickHouse). |
| (alternative compiler) | **MetricFlow** (Apache 2.0 since late 2025) via Ossie→dbt converter | Gives join-path resolution, fan-out-safe aggregation, time offsets and derived metrics for free. Costs: impedance mismatch with associative selections, plus a Python dependency with its own release cadence. Worth a one-week spike before deciding. |
| (alternative all-in-one) | **Cube** (Apache 2.0 core) | Mature semantic layer with WebSocket "real-time" refresh and pre-aggregations. But its model format isn't Ossie, and its refresh model is polling. A good benchmark to compare against. |
| Reactivity | **Own subscription manager** | See 4.2. |
| Optional IVM for hot standing queries | **Feldera** (MIT, DBSP-based, incremental for arbitrary SQL) | Only for fixed, unfiltered or low-variance views (e.g. wall-screen KPIs). Not for the interactive path. |
| Frontend | React + **Apache ECharts** (click events → selections), **FINOS Perspective** for streaming tables/pivots | Perspective is Arrow-native, made for streaming updates, and Apache 2.0. |
| Transport | WebSocket, Arrow IPC frames | |

### 4.2 How continuous updates work in A

The **"re-query on epoch"** strategy. It's simple, correct, and fast enough up to a surprisingly large scale.

1. Ingest appends to DuckDB. Every *T* ms (configurable, e.g. 250 ms), if any table changed, the committer bumps the global epoch and records which tables changed.
2. The subscription manager holds the live set of `(compiled SQL, selection hash) → [sessions]`.
3. For each live subscription whose SQL depends on a changed table (known from the compiled SQL's table references), re-run the query against the new snapshot.
4. Diff against the previous result (keyed by dimension values) and push only changed rows, tagged with the epoch.
5. Selection change: compile, run once, add to the live set.

Optimisations, in the order they're likely needed:

- **Deduplication** across sessions (same dashboard and same selection is common on wall screens and default views).
- **Coalescing**: when a run is still in flight at the next epoch, skip to the newest epoch.
- **Partial pre-aggregation**: for append-only facts, maintain rollup tables at a fixed grain (e.g. per minute × all low-cardinality dims) and point the compiler at the rollup when a visual's grain and filters allow it. ClickHouse MVs or Feldera can maintain these incrementally.
- **Possible-values queries** (for Qlik-style grey/white lists) are one `SELECT DISTINCT` per visible filter per selection change. Batch them into a single query with `GROUPING SETS` or a UNION.

### 4.3 Strengths

- Months, not years, to a usable product. All hard query-engine problems (vectorisation, SQL semantics, joins, persistence) are already solved.
- SQL everywhere: users, tools and debugging all speak it. Ad-hoc SQL access comes for free.
- The scale path is clear: DuckDB → ClickHouse (or MotherDuck) without changing the semantic or subscription layers.

### 4.4 Weaknesses / risks

- **Not associative.** DuckDB is a relational engine, so every selection is a set of joins plus predicates, and every possible-values list is a separate query. At 50M+ fact rows with 20 filter lists and many concurrent users, selection-change latency will exceed Qlik's "instant" feel (think 100 ms–1 s instead of < 50 ms).
- **Not truly incremental.** Every epoch re-scans everything each live subscription touches. Cost per epoch grows with *data size × live subscriptions*, not with *new rows*. Fine up to ~10⁸ rows and dozens of distinct live subscriptions, then it starts to hurt.
- **Many moving parts across languages** if Feldera/Kafka/Cube get added. Keep the core to DuckDB + one service as long as possible.
- Upserts in DuckDB under constant ingest are workable but not its sweet spot. Dimension upserts are fine (small tables). Fact upserts at high rate are not.

### 4.5 Rough effort

2–3 engineers, ~3 months to an MVP (ingest, model compiler, subscriptions, 5–6 visual types, filters/clicks). ~6 months to something production-shaped (RLS, retention/tiering, auth, model editor UI).

---

## 5. Architecture B: built from scratch in Rust

**Principle:** build the engine Qlik would build today: an **associative, dictionary-encoded, segment-oriented, incrementally maintained** store. Still no proprietary language: expressions are SQL (parsed with `sqlparser-rs`) and modifiers are structured YAML. "From scratch" means *the engine*. We still use the Rust ecosystem's building blocks (`arrow-rs`, `sqlparser-rs`, `roaring`, `rayon`, `tokio`/`axum`, optionally `datafusion` as a fallback executor).

### 5.1 Storage layout (the heart of it)

**Dictionaries (Qlik's "symbol tables"), per field:**

- Each column is dictionary-encoded into dense `u32` codes (bit-packed in sealed segments to ⌈log₂(cardinality)⌉ bits).
- **Join keys share one dictionary across both sides of a relationship.** `orders.customer_id` and `customers.customer_id` use the same code space. This one decision is what makes selection propagation cheap (see 5.2).
- Dictionaries are **append-only**: codes are assigned in arrival order, so codes stay stable as data streams in. A separately maintained sort permutation provides ordered display and range filters.
- Measures (amounts, quantities) are stored as plain typed columns (with frame-of-reference / delta compression in sealed segments), not dictionary codes, unless their cardinality is low.

**Segments, per table:**

```
 table: orders
 ┌───────────────┬───────────────┬───────────────┬──────────────────────┐
 │ seg 0 sealed  │ seg 1 sealed  │ seg 2 sealed  │ tail (mutable,       │
 │ 1M rows       │ 1M rows       │ 1M rows       │ append buffer)       │
 │ bit-packed    │ bit-packed    │ bit-packed    │ plain u32 codes      │
 │ + zone maps   │ + zone maps   │ + zone maps   │                      │
 │ + delete bmp  │ + delete bmp  │ + delete bmp  │                      │
 └───────────────┴───────────────┴───────────────┴──────────────────────┘
       ▲ persisted as files, mmap'd → can be paged out (tiering)
```

- The **tail** takes appends. When it reaches N rows (or a time boundary on `event_time`), it is sealed: compressed, zone maps computed, written to disk, mmap'd.
- **Upserts** = append the new row version + set a bit in the **delete bitmap** (roaring) of the segment holding the old version. A primary-key → (segment, row) index exists only for `upsert` tables.
- **Relationship index**: the fact's key column already holds the shared-dictionary *code* of the related key, so no join column is needed. The one side keeps a dense `code → current row` array. A dimension upsert updates only that array, never the fact segments. Following a relationship is two array lookups.
- **Durability**: WAL with group-commit fsync, then epoch commit. Restart = mmap sealed segments + rebuild dictionaries from persisted dictionary files + replay WAL into the tail. Expected restart time: seconds.
- **Tiering**: sealed segments are just files, so cold ones are evicted by the OS page cache or explicitly. Zone maps (min/max per segment on the time field and key codes) let selections skip whole segments without touching them.

### 5.2 Selection engine (associative semantics)

A selection is a set of codes per field: `{customers.region: {EU, NA}, products.category: {Bikes}}`.

Evaluation for an epoch:

1. **Local row filters**: for each table with selected fields, build a row bitmap per segment (SIMD scan of codes against a code bitset, which is O(rows) and branch-free).
2. **Propagation over relationships**: convert table-level bitmaps into **key-code sets** and push them across relationships. Because keys share a dictionary, crossing a relationship is a bitset intersection over codes, not a hash join:
   - `customers` rows passing → set of `customer_id` codes → filter `orders` rows by `orders.customer_id ∈ codes`.
   - Reverse direction (for bidirectional possible values): `orders` rows passing → set of `customer_id` codes present → `customers` rows with those codes.
   - Iterate to a fixpoint over the relationship tree. A tree (no cycles; role-playing paths resolved by the compiler) means one pass up and one pass down.
3. **Possible values per field** = the set of codes occurring in passing rows → a bitset over the dictionary. That's the green/white/grey state for every filter list, computed in one shot.

Each step is a pure function of (selection, epoch) and is cached per segment: **sealed segments' bitmaps for a given selection never change**, except via delete bitmaps.

### 5.3 Aggregation and metric execution

- A visual = group-by dimensions (fields on the fact table or reached through many-to-one relationship indexes) + metrics.
- The executor runs **per segment, in parallel (rayon)**: gather group-key codes (following relationship indexes for dimension attributes), compute a dense group id (codes are small integers, so mostly direct array indexing, no hashing), and update **mergeable aggregate states**.
- Mergeable states, with a trick that dictionary encoding makes possible:
  - SUM / COUNT / MIN / MAX / AVG: trivial.
  - **COUNT DISTINCT is exact and mergeable**: a bitset over the dictionary codes, merged by OR. (Roaring bitsets for high cardinality.)
  - **Median / percentiles are exact and mergeable**: a histogram of *code counts*, sorted by value only when finalising. Feasible because dictionary cardinality is bounded, with t-digest as a fallback for very high-cardinality measures.
- **Fan-out safety by construction**: each base measure aggregates over its *own* table's rows at the visual grain, then measures are joined on group keys. No row-level fact-to-fact joins ever happen.
- **Metric modifiers**: `ignore_selection: [f]` evaluates that measure under a selection with `f` removed (its own cached bitmap). `time_offset` evaluates under a shifted time selection. Ratios and derived metrics are computed after aggregation.
- Expressions: `sqlparser-rs` → typed expression IR → vectorised kernels over Arrow-like buffers. Rare or complex functions fall back to **DataFusion** on that segment, so we never have to implement all of SQL ourselves.

### 5.4 Incremental maintenance: segment-level, no IVM theory required

For each **live subscription** (visual × selection), the server keeps:

```
 partials[seg_0], partials[seg_1], ..., partials[seg_k]   ← immutable once computed
 partial[tail]                                            ← recomputed each epoch
 result = merge(all partials)                             ← cheap: O(segments × groups)
```

Per epoch, cost ≈ **O(new tail rows + groups)**, independent of history size. That's the property A lacks.

Invalidation rules (deliberately simple):

| Event | Invalidates |
|---|---|
| Append to fact tail | tail partial only |
| Tail sealed | tail partial becomes seg_k partial (no recompute) |
| Delete/upsert hitting sealed segment s | partials[s] for subscriptions on that table |
| Dimension attribute change (e.g. customer changes region) | subscriptions grouping or filtering on that attribute: recompute affected segments, found via the relationship index and zone maps on key codes |
| Selection change | new subscription: cold compute (full parallel scan), then incremental |

Selection caches are kept LRU by selection hash, so toggling a selection back and forth is instant. That's very Qlik.

### 5.5 Process architecture

```
                ┌──────────────────────────── single binary ─────────────────────────────┐
 HTTP ingest ──►│ axum ingest ─► validate (Arrow schema) ─► WAL ─► epoch committer       │
                │                                                   │                     │
                │      model compiler (schema.yaml + Ossie) ─► Model IR                   │
                │                                                   ▼                     │
                │   store: dictionaries · segments · delete bitmaps · relationship idx    │
                │                                                   │                     │
 WebSocket ────►│ session mgr ─► selection engine ─► subscription mgr ─► executor (rayon) │
  (Arrow IPC) ◄─│                                    (dedupe, partial cache, epoch fanout)│
                │ admin API: tables, model validate/apply, dashboards CRUD                │
                │ optional: DataFusion TableProvider → ad-hoc SQL / Postgres wire         │
                └─────────────────────────────────────────────────────────────────────────┘
```

- Ingest and query threads are separate (tokio for I/O, rayon for compute), so heavy queries don't stall acknowledgements.
- **Snapshot isolation by epoch**: sealed segments are immutable, and the tail is read up to the epoch's row watermark, so readers never lock writers.
- **Horizontal scale later**: shard fact segments across nodes, replicate (small) dimension tables and dictionaries. Mergeable partials make distributed aggregation a straightforward scatter-gather. Don't build this in v1.
- Same frontend and transport as A.

### 5.6 Strengths

- Qlik-class (or better) interactivity: selection propagation and possible values are bitset operations on codes. Expect single-digit-ms to low-tens-of-ms selection response on 10⁸–10⁹ rows on one big box.
- **Genuinely incremental**: update cost tracks ingest rate, not data size. The "continuously updated everything" requirement is met structurally.
- Exact COUNT DISTINCT and percentiles stay cheap and incremental.
- Restart in seconds. Tiered storage removes the hard RAM ceiling.
- One binary: simple to deploy, which is a real advantage for an on-prem or embedded product.

### 5.7 Weaknesses / risks

- **Large build.** Storage format, compression, WAL/recovery, expression engine, aggregation kernels, cache invalidation, schema evolution. Each is a well-understood problem, but there are many of them and the edge cases are all yours.
- **Correctness surface**: incremental cache invalidation bugs show up as *silently wrong numbers*, the worst failure mode in BI. Mitigation: run a shadow mode that periodically recomputes from scratch and compares (property tests with DataFusion or DuckDB as an oracle). Budget for this.
- **SQL coverage**: users will write expressions we don't vectorise. The DataFusion fallback helps but costs performance.
- **Global dictionaries grow forever** under high-cardinality streams (order ids, session ids). We need dictionary compaction at segment sealing / retention time, or to store such fields un-dictionaried (they're rarely filter fields anyway). Decide per column (`encoding: dict | plain` in `schema.yaml`).

### 5.8 Rough effort

3–4 strong Rust engineers. ~6 months to a credible engine plus MVP UI. 12–18 months to production-grade (recovery hardening, schema evolution, RLS, tiering, observability, the correctness oracle).

---

## 6. Side-by-side

| | A: OSS assembly | B: Rust from scratch |
|---|---|---|
| Time to MVP | ~3 months | ~6 months |
| Selection latency (10⁸ rows, 20 filter lists) | 100 ms – 1 s | < 50 ms |
| Cost per epoch | O(data touched × live subscriptions) | O(new rows + groups) |
| "Possible values" (associative grey/white) | N extra queries per selection | Free by-product of propagation |
| Beyond-RAM data | Yes (DuckDB/Parquet, ClickHouse) | Yes (mmap'd sealed segments) |
| Ad-hoc SQL access | Native | Via DataFusion integration |
| Exact COUNT DISTINCT live | Re-scan | Incremental (bitset OR) |
| Ops footprint | 1 service + DuckDB (more if Kafka/Feldera/ClickHouse added) | 1 binary |
| Main risk | Ceiling on interactivity at scale | Build size, silent incrementality bugs |
| Semantic layer | Ossie + extensions → SQLGlot (or MetricFlow) | Ossie + extensions → native IR |

## 7. Recommendation

**Build A's contracts and B's ambitions.**

1. Lock down the parts that are the same in both: `schema.yaml`, Ossie + extensions, dashboard files, the **Model IR**, the **selection semantics** (1.5) and the **epoch/subscription protocol** (§3). That's the real product definition, and it's engine-agnostic.
2. Ship A first, on DuckDB with your own SQLGlot-based compiler, to validate the UX, the model language and the live-update experience with real users and real data.
3. Use A as the **correctness oracle** for B. Then build B's engine behind the same protocol, focused on the two things A can't do: associative propagation on shared dictionaries, and segment-level incremental partials.

If the product thesis is specifically "Qlik's interactivity, but live", be aware that A alone won't reach that at scale, and B (or something very like it) is the actual product. A is how to get there without betting 18 months on unvalidated assumptions.

## 8. Open questions to settle before building

1. Expected data scale per deployment (rows/day, total retained, peak ingest rate)?
2. Concurrent users per deployment, and how many look at the *same* dashboard and selection?
3. Is full Qlik-style bidirectional "possible values" a must, or is Power BI-style one-directional filtering acceptable? (This changes B's complexity and A's query count a lot.)
4. Do facts ever need updating or deleting, or are corrections modelled as new events?
5. Single-tenant or multi-tenant server?
6. Cloud service, on-prem, or embedded? (This favours a single binary, i.e. B.)

# tidepool: the positioning brief

This file is source material for the landing page, the readme and the doc
site. Sections 1 to 9 are facts about the product. Section 10 is the structure
of the landing page.

Write all copy by the rules in [`copy-style.md`](copy-style.md). That file is
binding. This file says what is true. `copy-style.md` says how to write it.

tidepool is **pre-1.0, one binary, with no hosted service, no signup and no
pricing**. The page is a project page. The only call to action is "run it".

---

## 1. The product in one paragraph

tidepool is a live analytics server. It is one Rust binary with the web UI in
it. You push rows to it over HTTP (JSON, NDJSON or Arrow). It keeps them in a
compressed column store in memory, with a write-ahead log on disk. You declare
the tables, a semantic model (Apache Ossie) and the dashboards in YAML files,
in git. The dashboards update while data arrives. A click on a value filters
every number on the page.

## 2. The problem

A team that wants live dashboards over its own data has two usual options:

- **A warehouse and a BI tool.** Data goes to a warehouse in batches. The BI
  tool queries it on a schedule or on each click. The dashboard is minutes or
  hours old. A click is a round trip to the warehouse.
- **An associative BI product (Qlik).** Selections are instant and show what
  is possible and excluded. The model lives in the product, not in git. The
  data reloads on a schedule.

tidepool is a third option. The data is pushed and is visible in less than a
second. The model and the dashboards are files in git. The selections are
associative, as in Qlik.

## 3. How it works

- **Tables.** One file per table: columns with types, `append` (facts) or
  `upsert` (rows by key). Each table gets an ingest endpoint at once:
  `/v1/tables/{table}/rows`.
- **The model.** `model.ossie.yaml`, in Apache Ossie 0.2: datasets, fields,
  relationships and metrics. Expressions are SQL. Tidepool's own settings are
  Ossie `custom_extensions`.
- **Dashboards.** One file per dashboard: KPIs, bars, lines and tables on a
  12-column grid, and filters on a rail.
- **Epochs.** The writer publishes the new rows every 200 ms (the default).
  Every dashboard gets one frame per epoch with only what changed. A screen
  never mixes two epochs.
- **Selections.** A click selects. The selection goes along the relationships,
  from the one side to the many side, or both ways with
  `filter_direction: both`.
- **Changes.** A change to the files, the config page, `POST /v1/config/apply`
  or an agent goes through one pipeline: compile, plan, check policy, apply,
  write back, commit.
- **kayak.** tidepool reads only HTTP. kayak (the sister project) connects
  MQTT, Kafka, NATS, OPC UA and databases, with its `tidepool` output.

### The typical workflow

1. `tidepool init shop`, or write the files.
2. Commit them.
3. `tidepool serve --config shop`, or the container image with the project
   mounted.
4. Push rows over HTTP, or put kayak in front of a broker.
5. Change a file. `tidepool plan` shows what the change does. The running
   server applies it within a second.

## 4. Inventory

Verified against `tidepool-config` (`VisualType`, `FilterWidget`, the column
types), `tidepool_protocol::api_docs::endpoints()` and the MCP tools in
`tidepool-server/src/mcp.rs`. Check these lists again when you add a feature.

| | |
| --- | --- |
| **ingest formats** (3) | JSON · NDJSON · Arrow IPC stream |
| **write operations** (3) | `POST` insert or upsert · `DELETE` by key · `PUT` the whole table |
| **column types** (9) | `bool` · `int32` · `int64` · `float64` · `decimal(p,s)` · `string` · `date` · `timestamp` · `timestamptz` |
| **visuals** (4) | `kpi` · `bar` · `line` · `table` |
| **filters** (3) | `list` · `tree` · `date_range` |
| **metric settings** | `kind: derived` · `ignore_selection` · `time_offset` · `format` · `table` |
| **MCP tools** (7) | `describe_model` · `preview_table` · `query_metrics` · `get_config` · `plan_change` · `apply_change` · `ingest_rows` |

Also in the product:

- **Strict ingest.** A wrong type, an unknown column or a missing required
  column rejects the whole batch. tidepool lists every problem by row and
  column. Decimals are exact.
- **Idempotency keys** per table, kept for 24 hours, durable with a data
  directory.
- **Backpressure.** A full write queue answers `503` with `Retry-After`.
- **Durability.** A write-ahead log with group commit, checkpoints in the
  background. `durability: fsync` (default) or `async`.
- **Retention** per table, on an event-time column.
- **Derived fields.** SQL over the columns of a dataset. A derived field over
  one dictionary column costs the same as a column.
- **Hierarchies.** A parent column makes a tree of the rows: a plant, a
  category tree, an org chart. A selection of a node selects its branch.
- **Measures on filters.** A filter shows a metric for each value, in place of
  the row count.
- **Relative time windows.** "Today", "Last 7 days", "Last month". They move
  with the clock.
- **Year-on-year.** A `time_offset` metric beside its base metric on a KPI.
- **CSV.** `POST /v1/query?format=csv`, and a CSV button on every visual.
- **Diagnostics.** `tidepool validate` points at the wrong token, with
  `file:line:col` and "did you mean".
- **Plans.** `tidepool plan --config new --base old`, and the config page shows
  the plan while you type.
- **History.** Each applied change is a version. The config page shows the
  diffs and can revert.
- **Approvals.** A destructive change needs approval. `propose` queues a
  change for a person.
- **Access.** A bearer token for each level (`read`, `ingest`, `admin`), and a
  session cookie for the browser.
- **Operation.** `/healthz`, `/readyz`, `/metrics` (Prometheus), a status page.
- **JSON Schemas** for every config file, at `/v1/schemas/{kind}`.
- **OpenAPI 3.1** at `/v1/openapi.json`, with the row shape of each table.
- **The live protocol** at `/v1/ws`. Its types are in `tidepool-protocol`
  (Apache-2.0).

## 5. Facts to quote

Do not make these numbers larger. Do not round them up. Always give the
machine. All numbers are from `docs/plan.md` §12 (M2, M3, M4) on an Apple M1
Max, 10 cores, release build.

**Ingest** (`just bench`, 8 clients, batches of 10,000 rows, timed until the
last row is visible):

| format | rows per second |
| --- | --- |
| NDJSON | about 2.2 million |
| Arrow IPC | about 2.4 million |

**Memory.** 28 bytes per stored `orders` row. About 2.8 GB for 100 million
orders.

**Queries** (`just bench-query`, 100 million orders, region × category, three
metrics, 200 selections that are each a new combination):

| | p50 | p95 |
| --- | --- | --- |
| a new selection | about 68 ms | about 113 ms |
| a selection toggled back | less than 1 ms | |

**Live** (`just bench-live`):

| | result |
| --- | --- |
| an epoch of 100,000 new orders, at 100 million orders | 0.6 to 0.9 ms |
| 50 WebSocket sessions, 100,000 rows/s ingest, ack to visible on screen | p50 128 to 180 ms, p95 220 to 228 ms |

The epoch interval is 200 ms in that run. Say that the freshness number
includes it.

**Correctness.**

- Every query of the oracle runs on the fast path, the general path and a
  cached engine, and is compared with DuckDB. Random schemas, random data with
  upserts and deletes, random queries, from a seed.
- The live oracle compares the incremental answers with DuckDB after every
  epoch.
- 1,000 simulated power losses (also during a checkpoint), and 25 `kill -9`
  cycles of the real binary. No acknowledged write is lost.

**Quick start.** `tidepool init` → `serve` → `feed` → a query takes about 3 s
in the test (`tests/quickstart.rs`).

**Deployment.**

- One Rust binary. The UI is compiled into it. No database, no JVM, no
  separate frontend server.
- The container image is about 112 MB. It runs as uid 10001. Without
  arguments it serves the retail example in memory. There is no published
  image yet: `docker build -t tidepool .`.
- The default port is 7070.
- The commands: `serve`, `init`, `feed`, `validate`, `plan`.
- The flags of `serve`: `--config`, `--listen`, `--data`, `--ui-dir`,
  `--allow-destructive`.

**Licence.** `tidepool-protocol` and `tidepool-config` are Apache-2.0. The
server, engine and UI are AGPL-3.0-or-later, with a commercial licence
available.

**Quality.** Clippy pedantic with `-D warnings`. New behaviour needs a test.
Every new compiler check has a golden snapshot of its message.

## 6. Who it is for

- Teams with operational data that changes all day: production lines (OEE),
  orders, logistics, IoT.
- Engineers who want dashboards and their model in git, with review.
- Users of Qlik who want the associative model with live data.
- Users of kayak who want to see what their pipelines deliver.

## 7. The web UI

The web UI is the product. Show it.

- The dashboards: filters on the rail with selected, possible and excluded
  values, visuals that follow each click, the selection as chips, back and
  forward.
- The model browser: tables with an ingest `curl`, a relationship diagram, the
  metric catalog.
- The config pages: the files, the plan while you type, the history with
  diffs, the approval queue.
- The status page: memory, epochs, checkpoints.

## 8. What tidepool does not do

Say this on the page. It helps the reader to make a correct decision.

- **One machine.** No cluster, no shards. The target is up to about 500
  million fact rows on one machine.
- **One model per server.** No tenants, no row-level security.
- **HTTP only.** tidepool has no connectors. Use kayak for brokers and
  databases.
- **No SQL console.** Queries are metrics by fields under a selection. There
  is no free SQL.
- **No drag-and-drop builder.** Dashboards are YAML files. The config page
  edits the files.
- **Pre-1.0.** The config format and the format of the data directory can
  change between minor versions.

## 9. Names and spellings

- The name is **tidepool**, lowercase, also at the start of a sentence.
- The sister project is **kayak**.
- Ossie is **Apache Ossie**. The model file is `model.ossie.yaml`.
- Field names are `dataset.field`, in backticks: `customers.region`.
- Headings are lowercase. Body text uses sentence capitalization.
- Built with Rust, Axum, Tokio and Leptos. The charts are our own, drawn as SVG.

## 10. Page structure

The landing page is `website/.vitepress/theme/Landing.vue`.

1. **Hero.** What tidepool is (live analytics, one binary, the project in
   git). A live replica of a filter list and a bar visual: a click on a value
   filters the bar, and new rows move it. The three commands
   (`init`, `serve`, `feed`) as the call to action.
2. **Associative.** Selected, possible, excluded. Relationships in one
   direction or both. Trees. Back and forward. A screenshot of a dashboard
   with a selection.
3. **Live.** Rows in over HTTP, a `curl`, epochs, the freshness and epoch
   numbers.
4. **The project is code.** A tree of the project directory, a metric in
   Ossie, the output of `tidepool plan` with its classes, the write-back.
5. **Fast and correct.** The ingest, query and memory numbers with the
   machine. The oracle and the crash tests.
6. **With kayak.** MQTT → kayak → tidepool. The `tidepool` output config, and
   `just demo`.
7. **Agents.** The MCP endpoint, the seven tools, `auto_apply`.
8. **What tidepool does not do.** Section 8, in boxes.
9. **Footer.** The three commands again, and links to github, getting
   started, the reference and operation.

Use the visual language in [`visual-language.md`](visual-language.md).

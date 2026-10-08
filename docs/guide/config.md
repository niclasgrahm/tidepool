# Configuration reference

A Tidepool project is a directory of YAML files:

```
my-project/
├── tidepool.yaml             how the server runs
├── tables/<name>.yaml        one file per table
├── model.ossie.yaml          the semantic model: datasets, relationships, metrics
└── dashboards/<id>.yaml      one file per dashboard; the id is the file name
```

`tidepool validate --config my-project` checks every file and how they fit
together, and points at the token that's wrong (`file:line:col`).
`tidepool plan --config new --base old` shows what a change would do. JSON
Schemas for every file are at `GET /v1/schemas/{kind}` (`server`, `table`,
`model`, `dashboard`, and the extensions' `metric-extension` etc.), for
editors.

Unknown keys are errors, everywhere: a typo never passes silently.

## `tidepool.yaml`

```yaml
listen: 127.0.0.1:7070         # --listen overrides
data_dir: data                 # relative to this directory; --data overrides (no default: in memory)
epoch_interval: 200ms          # how often new data becomes visible, at most
memory_budget: 75%             # of system memory, or e.g. 8GiB
durability: fsync              # fsync: acknowledge after the log is on disk; async: before
config_writeback:              # changes made over the API or in the UI…
  enabled: true                # …are written to these files
  git_commit: true             # …and committed, when the directory is in a git repository
auth:                          # which environment variables hold the tokens
  read_token_env: TIDEPOOL_READ_TOKEN
  ingest_token_env: TIDEPOOL_INGEST_TOKEN
  admin_token_env: TIDEPOOL_ADMIN_TOKEN
agents:
  auto_apply: [safe]           # change classes an agent may apply over MCP without approval
```

Apart from `data_dir`, every key's default is the value shown. `listen`,
`data_dir`, `auth` and `durability` take effect on restart; the rest at
once.

## Tables: `tables/<name>.yaml`

```yaml
name: orders                   # must match the file name
mode: append                   # append (facts, the default) or upsert (rows by key)
primary_key: [order_id]        # upsert tables: the key
event_time: ordered_at         # a time column: retention is measured on it
retention: 730d                # drop rows older than this (sealed data, in the background)
columns:
  order_id:    { type: string, encoding: plain }
  customer_id: { type: string }
  amount:      { type: "decimal(14,2)" }
  ordered_at:  { type: timestamptz }
  note:        { type: string, nullable: true }
```

Column types: `bool`, `int32`, `int64`, `float64`, `decimal(p,s)` (up to 38
digits, exact), `string`, `date`, `timestamp` (no zone), `timestamptz` (UTC).
Columns are required unless `nullable: true`.

`encoding: dict` (the default for strings, bools and dates) gives every
distinct value a small code: cheap to filter, group and join. `plain` (the
default for numbers and timestamps) stores values: right for unique ids
nobody filters by. Keys and the columns relationships join are always
`dict`.

`renamed_from: <old name>` on a table or a column makes a rename keep its
data; the server removes it once the rename is applied.

## The model: `model.ossie.yaml`

[Apache Ossie](https://ossie.apache.org/) 0.2, with Tidepool's own settings
in `custom_extensions` entries whose `vendor_name` is `TIDEPOOL` and whose
`data` is a JSON string.

```yaml
version: "0.2.0"
name: retail
datasets:
  - name: orders
    source: orders                       # a table
    fields:                              # every column is a field; these add or describe
      - name: ordered_at
        expression: { dialects: [{ dialect: ANSI_SQL, expression: ordered_at }] }
        dimension: { is_time: true }
      - name: order_year                 # a derived field: SQL over the table's columns
        expression: { dialects: [{ dialect: ANSI_SQL, expression: "EXTRACT(YEAR FROM ordered_at)" }] }
  - name: customers
    source: customers
    primary_key: [customer_id]
relationships:
  - name: order_customer
    from: orders                         # the many side
    to: customers                        # the one side: its primary key
    from_columns: [customer_id]
    to_columns: [customer_id]
metrics:
  - name: revenue
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
```

**Fields** are named `dataset.field` everywhere else (`customers.region`).
A derived field over one dictionary column (`CASE WHEN region IN …`) is
computed once per distinct value, so it's as cheap as a column.

**Relationships** point from the many side to the one side. Selections flow
along them in that direction: a selection on `customers.region` filters
`orders`, and a selection on an order field doesn't filter customers.

**Metrics** are aggregate SQL: `SUM`, `COUNT`, `COUNT(DISTINCT …)`, `AVG`,
`MIN`, `MAX`, with `FILTER (WHERE …)`, arithmetic, `CASE`, `COALESCE`, date
functions. A metric is computed over the rows of the table its expression
reads (its *anchor*). The extension's `data` can say:

| key | meaning |
|---|---|
| `"table": "orders"` | the anchor, for expressions that name no table (`COUNT(*)`) |
| `"kind": "derived"` | the expression combines other metrics (`revenue / order_count`), after aggregation |
| `"ignore_selection": ["customers.region"]` | compute as if these fields (or `"*"`) had no selection |
| `"time_offset": "-1y", "time_field": "orders.ordered_at"` | compute with the time field moved: `-1y` reads last year's rows into this year's periods (`d`, `w`, `m`, `q`, `y`) |
| `"format": "currency:EUR"` | a display hint: `int`, `decimal`, `percent`, `currency:XXX` |

## Dashboards: `dashboards/<id>.yaml`

```yaml
name: Sales overview
layout: { columns: 12, row_height: 80 }
filters:
  - field: customers.region             # a list: selected, possible, excluded
  - field: orders.ordered_at
    widget: date_range
visuals:
  - id: kpi_revenue
    type: kpi
    metrics: [revenue]
    compare: revenue_prev_year           # shown with the change in percent
    at: { x: 0, y: 0, w: 3, h: 2 }
  - id: by_region
    type: bar                            # horizontal past six bars
    dimensions: [customers.region]
    metrics: [revenue]
    sort: { by: revenue, desc: true }
    on_click: select                     # a click selects the value
    at: { x: 3, y: 0, w: 5, h: 4 }
  - id: trend
    type: line
    dimensions: [{ field: orders.ordered_at, grain: day }]
    metrics: [revenue, order_count]      # a second metric gets its own axis
    on_brush: select_range               # dragging across selects the dates
    at: { x: 0, y: 4, w: 12, h: 4 }
  - id: top_products
    type: table                          # sortable by any column; a row click selects
    dimensions: [products.name]
    metrics: [revenue]
    limit: 50
    at: { x: 8, y: 0, w: 4, h: 4 }
```

`at` places a visual on the grid: column `x`, row `y`, `w` columns wide and
`h` rows high. Time grains are `day`, `week`, `month`, `quarter` and `year`.

## Changing a running server

Edit the files, use the **Config** page, or `POST /v1/config/apply`. Each
change is planned first and classed:

- **safe** (a table, a nullable column, a metric, a field, a dashboard):
  live at the next epoch;
- **rebuild** (a relationship between tables that hold data, an encoding, a
  wider type, a different key): data is rewritten in the background while
  the old version serves, then swapped;
- **destructive** (a table or column removed, a narrower type, a mode
  change): needs explicit approval.

Edits to the files that aren't safe wait under **Config → Waiting for
approval**. A server started on a directory that changed while it was down
applies the difference at startup, and refuses destructive differences
unless started with `--allow-destructive`.

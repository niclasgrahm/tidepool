# tables

A table is the rows that tidepool stores. Each table has one file,
`tables/<name>.yaml`. When the server reads a new table file, the table gets
its ingest endpoint at once: `/v1/tables/<name>/rows`.

```yaml
# tables/orders.yaml
name: orders                   # must be the same as the file name
mode: append                   # append (facts, the default) or upsert (rows by key)
event_time: ordered_at         # a time column: retention uses it
retention: 730d                # remove rows older than this, in the background
columns:
  order_id:    { type: string, encoding: plain }
  customer_id: { type: string }
  product_id:  { type: string }
  status:      { type: string }
  ordered_at:  { type: timestamptz }
  quantity:    { type: int32 }
  amount:      { type: "decimal(14,2)" }
  note:        { type: string, nullable: true, encoding: plain }
```

The [table reference](/reference/config/table) lists every key.

## modes

- **`append`** is for facts: orders, readings, events. Each row that you send
  is a new row. This mode is the default and the cheapest.
- **`upsert`** is for dimensions: customers, products, machines. The table
  has a `primary_key`. A row with a key that exists replaces the old row. You
  can delete rows by key, and you can replace the whole table in one request.

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
```

## column types

| type | values |
| --- | --- |
| `bool` | `true`, `false` |
| `int32`, `int64` | integers |
| `float64` | floating point numbers |
| `decimal(p,s)` | exact decimals, up to 38 digits |
| `string` | text |
| `date` | `YYYY-MM-DD` |
| `timestamp` | a date and a time, with no zone |
| `timestamptz` | a date and a time in UTC |

A column is required. Add `nullable: true` to let it hold NULL.

## encodings

- **`dict`** gives each distinct value a small code. A filter, a group and a
  join on a dictionary column are cheap. It is the default for strings,
  bools and dates.
- **`plain`** stores the values. It is the default for numbers and
  timestamps. Use it for a unique id that nobody filters by, such as an order
  id.

Primary keys and the columns of relationships are always `dict`. The model
checks this.

## retention

`retention` removes rows that are older than a duration, measured on the
`event_time` column. tidepool removes them in the background, a sealed
segment at a time.

## filled columns

tidepool can fill a column on each write. Producers do not send it:

```yaml
# fragment
columns:
  written_by:  { type: string, filled: credential }
  received_at: { type: timestamptz, filled: received }
```

| `filled` | the value | the column type |
| --- | --- | --- |
| `credential` | the name of the [ingest credential](/operating/access#ingest-credentials) that wrote the row | `string` |
| `received` | the time when tidepool received the row | `timestamp` or `timestamptz` |

A row that sends a filled column gets `400`. A filled column cannot be part
of the primary key. `GET /v1/tables/<name>` marks each filled column, and
the OpenAPI document leaves them out of the rows.

Use `credential` to find the rows of one producer, for example to delete
them when its token leaked. Do not use `received` on a table that you send
again and again as a whole. Each upsert then changes the row, and tidepool
cannot skip the rows that did not change.

## renames

Add `renamed_from` to a table or a column to keep its data when you rename
it:

```yaml
# fragment
columns:
  comment: { type: string, nullable: true, renamed_from: note }
```

Without `renamed_from`, a rename is a removal and an addition. The removal
deletes the data, and the plan says so. The server removes `renamed_from`
from the file when it applies the rename.

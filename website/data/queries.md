# queries

`POST /v1/query` calculates metrics grouped by fields, under a selection. It
is the engine that dashboards use, without a live session.

```json
{
  "metrics": ["revenue", "order_count"],
  "dimensions": [{"field": "customers.region"}, {"field": "orders.ordered_at", "grain": "month"}],
  "filters": [
    {"field": "products.category", "in": ["boats"]},
    {"field": "orders.ordered_at", "from": "2026-01-01T00:00:00Z", "to": "2027-01-01T00:00:00Z"}
  ],
  "sort": [{"column": "revenue", "desc": true}],
  "limit": 10
}
```

```bash
curl -X POST http://127.0.0.1:7070/v1/query \
  -H 'content-type: application/json' --data @query.json
```

The [`QueryRequest` schema](/reference/schemas#schema-queryrequest) lists every
key.

## names

A query uses the names of the model: `dataset.field` for fields, and the
names of metrics. A wrong name fails with `400` and a suggestion.

## dimensions

- `{"field": "customers.region"}` groups by a field.
- `{"field": "orders.ordered_at", "grain": "month"}` groups a time field by
  `day`, `week`, `month`, `quarter` or `year`. A timestamp can also group by
  `hour` or `minute`.
- `{"field": "categories.tree", "level": 1}` groups a
  [hierarchy](/project/hierarchies) by the ancestor at a depth. The roots are
  level 0.

With no dimensions, the answer is exactly one row, as in SQL.

## filters

- `{"field": …, "in": [...]}` keeps the listed values. On a hierarchy field,
  it keeps the listed nodes and every node below them.
- `{"field": …, "from": …, "to": …}` keeps a half-open range: `from`
  included, `to` excluded.
- `{"field": …, "relative": {"unit": "day", "last": 7}}` keeps a window
  relative to the clock of the server: today and the six days before it.
  `ago: 1` ends the window one period earlier. See
  [relative time windows](/project/selections#relative-time-windows).
- `{"field": …, "contains": "timeout"}` keeps the rows whose value of a
  string field contains the text. The case of the letters A to Z does not
  matter. See [search](/project/selections#search).

Filters go the way selections go on a dashboard. A metric is filtered by the
fields that its anchor reaches through many-to-one relationships. With
`filter_direction: both`, also the other way.

## rows

`POST /v1/rows` gives the rows themselves: the newest rows of a table that
pass the filters, the newest first. `fields` are the columns. The filters
are the filters of a query.

```json
{
  "fields": ["orders.ordered_at", "customers.name", "orders.note"],
  "filters": [{"field": "orders.note", "contains": "door"}],
  "limit": 20
}
```

The answer has the shape of a query answer, with one column for each field.
`limit` is 100 by default, and 1000 at most. Add `?format=csv` to get CSV.

## the answer

The answer is a list of columns (the dimensions first, then the metrics),
the rows, and the epoch that tidepool calculated it from. Decimals, dates and
timestamps are strings, so that no digit is lost:

```json
{
  "columns": [
    {"name": "customers.region", "type": "string", "role": "dimension"},
    {"name": "revenue", "type": "decimal(2)", "role": "metric"}
  ],
  "rows": [["north", "48211.90"], ["south", "39102.40"]],
  "epoch": 812
}
```

Add `?format=csv` to get the same answer as CSV.

## semantics

The answers follow DuckDB, with one difference: a division by zero gives
NULL. A related row that does not exist reads as NULL in every column, as in
a SQL `LEFT JOIN`. Every query in the test suite is compared with DuckDB. See
[testing](/contributing/testing).

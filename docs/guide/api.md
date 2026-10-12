# HTTP API

Everything the UI does goes through this API, and it describes itself:
`GET /v1/openapi.json` is the OpenAPI document, with every table's row
shape spelled out. Probes and metrics are at the root (`/healthz`,
`/readyz`, `/metrics`); everything else is under `/v1`. Errors are
`{"error": "…"}` with a status that says what kind.

Endpoints that need a token say which in the document (`x-tidepool-access`:
`read`, `ingest` or `admin`); see [operations](operations.md#access).

## Writing rows

```
POST   /v1/tables/{table}/rows        insert (upsert tables: replace by key)
DELETE /v1/tables/{table}/rows        delete by key (upsert tables)
PUT    /v1/tables/{table}/rows        the table's whole contents (upsert tables)
```

`PUT` is for a dimension kept in step with a system that can only be read
whole (an asset tree, a product catalogue): send all of it every time, and
rows whose keys aren't in it are deleted, in the same step that writes the
rest. `replaced` counts the rows replaced and the rows deleted.

The body is a JSON array of row objects (`application/json`),
newline-delimited JSON (`application/x-ndjson`) or an Arrow IPC stream
(`application/vnd.apache.arrow.stream`). Deletes carry the key columns only.

- **Strict**: a value of the wrong type, an unknown column or a missing
  required one rejects the whole batch with `400`, every problem listed by
  row and column. Nothing is coerced. Decimals are exact (send them as
  strings to keep every digit through JSON tooling); dates are
  `YYYY-MM-DD`, timestamps RFC 3339.
- **Acknowledged** (`200`) means applied, and with a data directory logged
  and synced. `?wait=visible` waits until the rows are in a published
  epoch, which queries and dashboards see.
- **Backpressure**: a full write queue answers `503` with `Retry-After`.
- **Idempotency**: an `Idempotency-Key` header makes a retry of the same
  batch a no-op for 24 hours (per table): it gets the first response
  again. With a data directory the keys survive a restart, so a batch
  whose reply was lost to one can be retried safely.

```bash
curl -X POST 'http://127.0.0.1:7070/v1/tables/orders/rows?wait=visible' \
  -H 'content-type: application/json' -H 'idempotency-key: batch-42' \
  -d '[{"order_id": "o1", "customer_id": "c1", "product_id": "p1", "status": "paid",
        "ordered_at": "2026-10-01T12:00:00Z", "quantity": 1, "amount": "19.90"}]'
```

`GET /v1/tables` and `/v1/tables/{table}` describe tables (columns, rows,
ingest rate); `/v1/tables/{table}/preview?limit=20` shows the newest rows.

## Queries

`POST /v1/query` computes metrics grouped by fields, under a selection: the
engine dashboards use, without a subscription.

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

A dimension on a hierarchy field can take a `level`: `{"field":
"namespace.asset", "level": 1}` groups by each node's ancestor one below the
roots (the roots are level 0), and nodes that aren't that deep group as
null. A filter on a hierarchy field selects the listed nodes and everything
below them.

Names are the model's (`dataset.field`, metric names); a wrong one comes back
as a `400` with a suggestion. A filter on a date or time field can also be a
window relative to the server's clock, the way a dashboard's period is:
`{"field": "orders.ordered_at", "relative": {"unit": "day", "last": 7}}` is
today and the six days before it (UTC; `ago: 1` ends a period earlier).
Filters are one-directional: a metric is
filtered by the fields its table reaches through relationships. The answer is
columns (with types) and rows, values as JSON: decimals, dates and
timestamps as strings. `?format=csv` returns the same as CSV instead.

`GET /v1/model` describes the semantic model (datasets, fields,
relationships, metrics with their SQL); `GET /v1/status` the server.

## Live dashboards

`GET /v1/ws` upgrades to a WebSocket of JSON messages tagged by `type`
(`ClientMessage` in, `ServerMessage` out, defined in
`tidepool-protocol`'s `live` module, which is Apache-2.0 for clients to
use). A session says `hello`, gets the dashboards (`model`), opens one
(`open_dashboard`), and from then on receives one `frame` per epoch with
whatever changed on it. `select`, `select_range`, `select_relative`
(`{"field": …, "window": {"unit": "day", "last": 7}}`), `clear`, `back` and
`forward` change the selection and are answered at once; a relative window
comes back in `selection` with where it is now (`from`, `to`), and again
whenever the clock moves it into a new period; `pause` and
`resume` stop and restart frames; `filter_window` pages through a filter
list (or a tree's visible nodes, or searches it); `filter_open`
(`{"field": …, "keys": [...], "open": true}`) opens or closes nodes of a
tree filter, whose frames carry `nodes` instead of `values`; `sort`
re-sorts a table visual. When the model changes, the session
gets `model` again.

## The config API

| | |
|---|---|
| `GET /v1/config` | every file of the current version, the version, a running rebuild, unapplied edits in the directory |
| `POST /v1/config/plan` | what a change would do: problems (file, line, column) and every change with its class |
| `POST /v1/config/apply` | apply it (`approve: true` for destructive changes), or `propose: true` to queue it |
| `GET /v1/config/versions[/{v}]` | the history, and a version's files |
| `GET /v1/changes`, `POST /v1/changes/{id}/approve\|reject` | the approval queue |

A change is `{"base_version": 3, "files": {"tables/returns.yaml": "…", "dashboards/old.yaml": null}}`:
files to set, or to remove with `null`; `"replace": true` makes the files
the whole config (a revert). A change against an older version is a `409`
naming the current one: plan again.

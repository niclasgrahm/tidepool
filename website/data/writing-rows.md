# writing rows

Each table has one ingest endpoint, `/v1/tables/<name>/rows`:

| request | what it does | tables |
| --- | --- | --- |
| `POST` | writes rows. On an upsert table, a row with a key that exists replaces the old row. | all |
| `DELETE` | deletes rows by key | upsert |
| `PUT` | replaces the whole contents of the table | upsert |

```bash
curl -X POST 'http://127.0.0.1:7070/v1/tables/orders/rows?wait=visible' \
  -H 'content-type: application/json' -H 'idempotency-key: batch-42' \
  -d '[{"order_id": "o1", "customer_id": "c1", "product_id": "p1", "status": "paid",
        "ordered_at": "2026-10-01T12:00:00Z", "quantity": 1, "amount": "19.90"}]'
```

The reply counts the rows, and gives the epoch from which they are visible:

```json
{ "written": 1, "replaced": 0, "missing": 0, "epoch": 812 }
```

`replaced` counts the rows that an upsert replaced or a delete removed.
`missing` counts the keys of a delete that matched no row.

A row of an upsert table that is equal to the stored row with its key
changes nothing. tidepool skips it, and does not count it as written or
replaced. Thus you can send a dimension again and again, for example the full
product table from a database each few seconds. Only the rows that changed
cost work, and a batch with no changes does not make a new epoch.

The OpenAPI document of a server, `/v1/openapi.json`, has one typed path for
each table, with the shape of its rows. The [HTTP API
reference](/reference/api#api-tables) has every status and body.

## formats

The `content-type` header selects the format of the body:

| content type | body |
| --- | --- |
| `application/json` | an array of row objects |
| `application/x-ndjson` | one row object on each line |
| `application/vnd.apache.arrow.stream` | an Arrow IPC stream. tidepool matches its fields to the columns by name. |

A `DELETE` sends the key columns of each row, and no other columns.

## strict values

tidepool checks every value against the type of its column. It does not
convert a value to another type.

- A value of the wrong type, an unknown column or a missing required column
  rejects the whole batch with `400`. The reply lists every problem by row and
  column. tidepool writes nothing from that batch.
- Decimals are exact. Send them as strings to keep every digit through JSON
  tools: `"19.90"`.
- Dates are `YYYY-MM-DD`. Timestamps are RFC 3339.

## when rows are visible

`200` means that tidepool applied the batch. With a data directory, it also
means that the batch is in the log on disk (see
[durability](/operating/durability)).

Queries and dashboards see the rows at the next epoch, within the
`epoch_interval` (200 ms by default). Add `?wait=visible` to get the reply
only when the rows are visible.

## retries

Send an `Idempotency-Key` header with each batch. If you send the same key
again within 24 hours, tidepool writes nothing and sends the first reply
again. Thus a retry never writes a batch two times. Keys are per table and
per [credential](/operating/access#ingest-credentials). tidepool keeps
100,000 keys for each credential, and removes the oldest first.

With a data directory, the keys survive a restart. You can retry a batch
whose reply was lost in a crash.

## backpressure

The write queue of the server holds 64 batches. When it is full, tidepool
answers `503` with a `Retry-After` header. Wait that number of seconds, then
send the batch again with the same idempotency key.

A credential with [limits](/operating/access#limits) gets `429` with a
`Retry-After` header when it sends too fast, and `413` when a request is too
large. Retry a `429` in the same way.

## refusals

| status | why |
| --- | --- |
| `400` | a value, a column or the body is wrong. The reply lists every problem. |
| `401` | the request has no valid token |
| `403` | the credential cannot write this table, do this operation, or write from this origin |
| `404` | no such table. On the `public` listener, also a table that no public credential writes. |
| `413` | over `batch_rows` or `body` |
| `429` | over `rate` or `new_values`, or the public share of the queue is full |
| `503` | the write queue is full |

## replace a table

`PUT` is for a dimension that you keep in step with a system that can only be
read whole: an asset tree, a product catalogue. Send all of it each time.
tidepool writes the rows, and deletes every row whose key is not in the body,
in the same step. Queries never see a half-replaced table.

In the reply, `replaced` counts the rows replaced and the rows deleted. An
empty body empties the table.

## read a table

- `GET /v1/tables` lists the tables, with their rows and ingest rates.
- `GET /v1/tables/<name>` describes one table.
- `GET /v1/tables/<name>/preview?limit=20` shows the newest rows.

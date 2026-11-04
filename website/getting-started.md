# getting started

This page takes you from nothing to a live dashboard. It takes about five
minutes.

## 1. get the binary

Build it from a checkout. You need Rust, the `wasm32-unknown-unknown` target
and `trunk`:

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
just build
```

The binary is `./target/release/tidepool`. The web UI is in it.

You can also build the container image. It needs only Docker:

```bash
docker build -t tidepool .
```

## 2. start a project

```bash
tidepool init shop
```

The directory `shop/` now holds a small retail project:

- four tables: `orders`, `customers`, `products` and `categories`;
- a model with metrics such as `revenue` and `avg_order_value`;
- three dashboards: `sales`, `sales_recent` and `shipping_notes` (a search of the order notes).

All of them are YAML files. [The project directory](/project/the-project)
explains each file.

## 3. serve it

```bash
tidepool serve --config shop
```

Open <http://127.0.0.1:7070>. The project's `tidepool.yaml` sets
`data_dir: data`, so tidepool keeps the data in `shop/data`. The data stays
when the server stops.

The container image serves its own copy of the same project, in memory:

```bash
docker run -p 7070:7070 tidepool
```

## 4. feed it

In another terminal:

```bash
tidepool feed
```

The command sends 500 customers, 60 products, the category tree and ninety
days of orders. Then it sends twenty new orders each second. Open the
**Sales overview** dashboard. The numbers change while the orders arrive.

To feed the container, use the host network:
`docker run --network host tidepool feed`.

## 5. look around

- **Click** a region bar or a value in a filter. Every visual on the page
  shows only the orders of that region. Ctrl-click (⌘-click on a Mac) adds a
  value to the selection.
- **Drag** across the trend to select a date range.
- **Back** goes to the selection before.
- **Model** shows the tables, a `curl` command to send rows to each table,
  the relationships between the tables, and the SQL of each metric.
- **Config** edits the files of the project. Change a metric, read the plan
  of the change, and apply it. The server changes without a restart, and the
  file in `shop/` changes with it.
- **Status** shows the memory, the epochs, the log and the checkpoints.

## 6. your own data

Add a table file to the project, for example `shop/tables/readings.yaml`:

```yaml
# tables/readings.yaml
name: readings
columns:
  sensor:   { type: string }
  value:    { type: float64 }
  taken_at: { type: timestamptz }
```

The server reads the new file within one second. Then send rows to the new
endpoint of the table:

```bash
curl -X POST 'http://127.0.0.1:7070/v1/tables/readings/rows?wait=visible' \
  -H 'content-type: application/x-ndjson' \
  --data-binary @readings.ndjson
```

The OpenAPI document of the server,
<http://127.0.0.1:7070/v1/openapi.json>, shows the row shape of each table.

## next

- [Tables](/project/tables), [the model](/project/the-model) and
  [dashboards](/project/dashboards): how to describe your own data.
- [Writing rows](/data/writing-rows): the formats and the guarantees of
  ingest.
- [kayak](/data/kayak): data from MQTT, Kafka, NATS, OPC UA and databases.
- [Deployment](/operating/deployment): run tidepool as a service.

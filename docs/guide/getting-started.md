# Getting started

From nothing to a live dashboard in a few minutes.

## 1. Get the binary

From a checkout (needs Rust, the `wasm32-unknown-unknown` target and
`trunk`):

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
just build                      # → ./target/release/tidepool, UI included
```

Or with Docker, which needs nothing else:

```bash
docker build -t tidepool .
```

## 2. Start a project

```bash
tidepool init shop
```

`shop/` now holds a small retail project: three tables (`orders`,
`customers`, `products`), a model with metrics such as `revenue` and
`avg_order_value`, and a `sales` dashboard. They're plain YAML; the
[configuration reference](config.md) explains every key.

## 3. Serve it

```bash
tidepool serve --config shop
```

Open <http://127.0.0.1:7070>. Data is kept in `shop/data` (the project's
`tidepool.yaml` says `data_dir: data`), so it survives restarts.

With Docker, the image serves its own copy of the same project:

```bash
docker run -p 7070:7070 tidepool
```

## 4. Feed it

```bash
tidepool feed
```

This posts 500 customers, 60 products and ninety days of orders, then keeps
sending twenty new orders a second. Open the **sales** dashboard and watch it
move. (Against the Docker image: `docker run --network host tidepool feed`.)

## 5. Look around

- **Click** a region bar or a value in a filter list to select it; Ctrl/Cmd
  adds to the selection. Drag across the trend to select a date range. Every
  visual follows; **Back** undoes.
- **Model** shows the tables (with a `curl` to send rows), how they relate,
  and every metric's SQL.
- **Config** edits the files: change a metric or add a visual, read the plan
  of what it does, apply it. The server changes without a restart, and the
  file in `shop/` changes with it.
- **Status** shows memory, epochs, the log and checkpoints.

## 6. Your own data

Add a table file (`shop/tables/readings.yaml`, say), and the server picks it
up within a second; then push rows:

```bash
curl -X POST 'http://127.0.0.1:7070/v1/tables/readings/rows?wait=visible' \
  -H 'content-type: application/x-ndjson' \
  --data-binary @readings.ndjson
```

Every table gets its endpoint, documented with its row shape in
<http://127.0.0.1:7070/v1/openapi.json>. For streams from MQTT, Kafka, OPC UA
and friends, put [Kayak](https://github.com/niclasgrahm/kayak) in front; see
[the demo](../../examples/kayak-demo).

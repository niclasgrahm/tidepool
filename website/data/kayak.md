# kayak

tidepool reads data over HTTP only. For data from MQTT, Kafka, NATS, Redis,
OPC UA or a database, put [kayak](https://propell.dev/kayak/) in front of it.
kayak is the sister project of tidepool: a stream processor in one Rust
binary, with pipelines in a config file.

kayak's `tidepool` output writes the messages of a pipeline into one table:

```yaml
# fragment: kayak's config.connections.yaml
tidepool:
  type: tidepool
  url: http://localhost:7070
  token: ${TIDEPOOL_INGEST_TOKEN}   # only when the ingest of the server is guarded
```

```yaml
# fragment: kayak's config.yaml
pipelines:
- id: readings
  inputs:
  - type: mqtt
    connection: plant
    topic: plant/readings
  transforms:
  - type: buffer
    seconds: 1
    max_messages: 5000
  outputs:
  - type: tidepool
    connection: tidepool
    table: readings
    columns:
    - { name: machine_id, type: text, nullable: false }
    - { name: at, type: timestamp, nullable: false }
    - { name: temperature, type: float, nullable: false }
```

The output does these things:

- At startup, it reads the table from tidepool and checks the mapping. A
  column that the table does not have, or a type that the column cannot
  take, stops the start with a message.
- It sends one request for each batch. Put a `buffer` on the pipeline to send
  large batches.
- It sends an idempotency key with each batch. When tidepool is busy or not
  reachable, it retries. A retry never writes a batch two times.

The tidepool project declares the table. kayak does not create tables. The
[kayak documentation](https://propell.dev/kayak/io/tidepool) has every field
of the output.

## a dimension from a database

kayak's `postgres` and `clickhouse` inputs can read a full table at an
interval (`snapshot` mode). Send each read to an upsert table as it is:

```yaml
# fragment: kayak's config.yaml
pipelines:
- id: products
  inputs:
  - type: postgres
    connection: northwind
    query: "SELECT product_id::text AS product_id, product_name AS product, unit_price::text AS list_price FROM products"
    mode: { type: snapshot }
    interval_secs: 5
    max_batch: 1000          # the full table in one batch, not one row in each
  outputs:
  - type: tidepool
    connection: tidepool
    table: products
```

Most reads send the same rows again. tidepool skips a row that is equal to
the stored row, so these reads cost almost nothing: no new epoch, and the
dashboards do not calculate again. A row that changed in the database
changes in tidepool at the next read. Rename the columns in the query to
the names of the table. Send keys and decimals as text: tidepool keys are
strings, and text keeps every digit of a decimal.

A snapshot does not see deleted rows. A row deleted in the database stays
in tidepool.

## a whole tree

kayak's `indu_namespace` input reads the namespace of indud as a whole tree.
With `write: replace` on the `tidepool` output, each read replaces a
[hierarchy](/project/hierarchies) table. A node moved, renamed or deleted in
indud is the same in tidepool at the next read. `examples/indud-oee` in the
repository is a project for it.

## the demo

`examples/kayak-demo` is a simulated plant: six machines on two lines publish
readings and stoppages to MQTT. kayak sends them to tidepool, and a dashboard
shows the plant floor live.

```bash
just demo
```

The command needs Docker, and kayak's repository beside tidepool's (or
`KAYAK_SRC`). Then open:

- tidepool: <http://localhost:7070/#/d/plant>
- kayak: <http://localhost:6767>

`just demo-check` is the integration test of the two: the demo on its own
ports, tidepool off the network for a time, and then the row counts compared
with what kayak sent.

## northwind, live

`examples/northwind` is a sales test bed. The dimensions of Northwind are in
Postgres, and kayak reads them every 5 seconds. A publisher sends
Northwind's orders again, now, to the `http` inputs of kayak. Two dashboards
show sales and fulfilment live.

```bash
just northwind
```

Then open <http://localhost:7070/#/d/sales>. Change a row in Postgres, and
the dashboards change within 5 seconds. `just northwind-freshness` measures
the time from an order to the moment when a query sees it.

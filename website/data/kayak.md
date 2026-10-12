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

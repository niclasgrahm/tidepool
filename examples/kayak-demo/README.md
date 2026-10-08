# Kayak → Tidepool: a live plant floor

A simulated plant publishes machine readings and stoppages to MQTT;
[Kayak](https://github.com/niclasgrahm/kayak) subscribes, batches and posts
them to Tidepool's ingest API; Tidepool's plant-floor dashboard updates as
they arrive. The showcase for both, and the integration test of the two
(plan §15.4).

```bash
docker compose up --build
```

- Tidepool: <http://localhost:7070/#/d/plant>
- Kayak's pipelines: <http://localhost:6767>

The first build compiles both projects (several minutes); later starts are
quick. Kayak is built from its repository, expected next to this one at
`../../../../rust/kayak` (set `KAYAK_SRC` otherwise).

What's where:

| | |
|---|---|
| `plant/publish.sh` | the simulated plant: six machines on two lines (`mosquitto_pub` and `awk`) |
| `kayak/config.yaml` | three pipelines: `plant/…` topics → buffer → Kayak's `tidepool` output → `POST /v1/tables/{table}/rows` |
| `check.sh` | the integration test of the two (`just demo-check`): the demo on its own ports, Tidepool cut off the network midway, then its row count compared with what Kayak sent |
| `tidepool/` | the Tidepool project: `machines`, `readings`, `stoppages`, metrics, the `plant` dashboard |

Things to try: click a machine or a stoppage reason to select it; open
Config and add a metric (say `MIN(readings.temperature)`) and put it on the
dashboard, without restarting anything; `docker compose restart tidepool`
and see the data come back from its volume.

Kayak writes through its `tidepool` output: each pipeline reads its table when
it starts and checks the mapping against it (`readings` maps every column;
try changing a type in `kayak/config.yaml` and restarting Kayak), every batch
carries an idempotency key, and a busy or unreachable Tidepool is retried
without writing anything twice.

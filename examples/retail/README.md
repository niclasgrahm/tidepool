# retail

The example project: a small retail star schema (orders, customers, products)
with two dashboards (the second opens on a window of the last seven days). It is what `just dev` runs against, what the docs use, and
(from M1) what `tidepool validate` is tested on. Keep it covering every config
feature as features land, the way Kayak's `example_config/` covers every
component.

```
tidepool.yaml          server settings
tables/*.yaml          one file per table: shape, types, ingest mode
model.ossie.yaml       semantic layer: Apache Ossie + tidepool extensions
dashboards/*.yaml      dashboards
```

Nothing reads these files yet; the formats are specified in
[docs/plan.md §3](../../docs/plan.md) and parsing lands in M1.

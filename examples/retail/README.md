# retail

The example project: a small retail star schema (orders, customers, products)
with a category tree and two dashboards (the second opens on a window of the
last seven days). It is what `just dev` runs against, what the docs use, and
what `tidepool validate` is tested on. Keep it covering every config
feature as features land, the way Kayak's `example_config/` covers every
component.

```
tidepool.yaml          server settings
tables/*.yaml          one file per table: shape, types, ingest mode
model.ossie.yaml       semantic layer: Apache Ossie + tidepool extensions
dashboards/*.yaml      dashboards
```

`tidepool init` writes a copy of it, and the doc site's samples
(`website/`) are compiled against it. The formats are described on the doc
site (`website/project/`) and specified in [docs/plan.md §3](../../docs/plan.md).
Changing a file here changes the OpenAPI document on the site: run `just docs`.

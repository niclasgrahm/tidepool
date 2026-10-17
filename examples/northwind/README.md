# Northwind, live

Can BI dashboards be realtime, for real? This example tries it with a
classic: Northwind's dimensions live in Postgres, its orders arrive as
events, [Kayak](https://github.com/niclasgrahm/kayak) moves both into
Tidepool, and two dashboards follow every order as it is placed and shipped.

```text
Postgres (dims) ──snapshot every 5 s──┐
                                      ├─ Kayak ─→ Tidepool ─→ dashboards (WebSocket)
publisher (orders) ──HTTP, 10×/s──────┘
```

```bash
docker compose up --build
```

- Tidepool: <http://localhost:7070/#/d/sales> and `#/d/fulfilment`
- Kayak's pipelines: <http://localhost:6767>
- Postgres: `psql postgresql://northwind:northwind@localhost:5432/northwind`

The first build compiles both projects (several minutes). Kayak is built
from its repository, expected next to this one at `../../../../rust/kayak`
(set `KAYAK_SRC` otherwise). Ports can be moved with `TIDEPOOL_PORT`,
`KAYAK_PORT` and `POSTGRES_PORT`.

## what's where

| | |
|---|---|
| `postgres/init.sql` | categories, suppliers, products, customers, employees, shippers |
| `publisher/` | `publish.py` replays `orders.ndjson` (Northwind's 830 orders and 2,155 lines) as events, and loops |
| `kayak/config.yaml` | six `postgres` pipelines (`snapshot` mode, every 5 s) and two `http` pipelines (`orders`, `order_lines`), all into Kayak's `tidepool` output |
| `tidepool/` | the tables, the model (relationships, metrics, the sales organisation as a tree, `filter_direction: both`) and the `sales` and `fulfilment` dashboards |
| `freshness.py` | measures event time → queryable, from the host |

## how the data moves

**Dimensions** are read whole from Postgres every 5 seconds and sent to
Tidepool as they are. Tidepool skips a row that is equal to the stored row,
so the steady re-sending costs one comparison per row and nothing else: no
new epoch, and every cached answer stays valid. Edit a row and the
dashboards follow within the 5 seconds:

```sql
UPDATE products SET category_id = 1 WHERE product_name = 'Chai';
UPDATE employees SET reports_to = 5 WHERE last_name = 'Davolio';
UPDATE customers SET country = 'Sweden' WHERE customer_id = 'ALFKI';
```

**Facts** come from the publisher. Every Northwind order is placed again
*now* (`ORDERS_PER_SECOND`, default 2, with random gaps), with its lines,
and ships as many seconds later as it shipped days later in Northwind times
`DAY_SECONDS` (default 2). An order that shipped after its required date
ships `late`. Orders are an upsert table, so shipping rewrites the order;
lines are appended. When the 830 orders run out, the replay starts again
with new order ids.

## things to try

- Click a category, a country or a product: everything follows, both ways,
  including order counts and late rates on the fulfilment dashboard.
- Open the employees tree (Fuller → Buchanan → his reps) and pick a manager.
- Run an `UPDATE` above and watch the bars move.
- Go faster: `ORDERS_PER_SECOND=200 docker compose up -d publisher`.
- `docker compose restart tidepool`: the data comes back from its volume,
  and Kayak and the publisher catch up.

## how fresh is it?

```bash
python3 freshness.py --url http://localhost:7070 --seconds 30
```

It asks for the newest order Tidepool can see every 50 ms, and each time it
moves, how long ago that order was placed. That is the whole path: the
publisher's batching (up to 100 ms), Kayak, and Tidepool's next epoch (up to
200 ms). Dashboards are pushed from the same epoch.

## data

`postgres/init.sql` and `publisher/orders.ndjson` are a subset of Northwind,
Microsoft's sample database, taken from the Postgres port at
[pthom/northwind_psql](https://github.com/pthom/northwind_psql) and
distributed under its Microsoft Public License, in
[`LICENSE-northwind.md`](LICENSE-northwind.md). The rest of this directory is
under the repository's licence.

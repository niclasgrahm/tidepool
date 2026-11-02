# metrics

A metric is an aggregate in SQL, declared in `model.ossie.yaml`:

```yaml
# fragment
metrics:
  - name: revenue
    description: Gross order value
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
  - name: paid_revenue
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount) FILTER (WHERE orders.status = 'paid')" }] }
  - name: customer_count
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "COUNT(DISTINCT orders.customer_id)" }] }
```

The SQL can use:

- the aggregates `SUM`, `COUNT`, `COUNT(DISTINCT …)`, `AVG`, `MIN` and `MAX`;
- `FILTER (WHERE …)` on an aggregate;
- arithmetic, `CASE`, `COALESCE` and date functions.

## the anchor

tidepool calculates a metric over the rows of the table its expression reads.
That table is the *anchor* of the metric. A selection filters a metric through
the relationships of its anchor.

An expression that names no table, such as `COUNT(*)`, needs its anchor in an
extension:

```yaml
# fragment
  - name: order_count
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "COUNT(*)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"table": "orders"}'
```

## metrics over metrics

A derived metric combines other metrics. tidepool calculates it after the
aggregation:

```yaml
# fragment
  - name: avg_order_value
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "revenue / order_count" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"kind": "derived"}'
```

A division by zero gives NULL.

## ignore a selection

`ignore_selection` calculates a metric as if some fields had no selection.
Use it for a share of a total:

```yaml
# fragment
  - name: revenue_all_regions
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"ignore_selection": ["customers.region"]}'
```

Use `"*"` to ignore every selection.

## compare with a period before

`time_offset` calculates a metric with its time field moved. `-1y` reads the
rows of last year into the periods of this year. A KPI shows the change from
the comparison in percent:

```yaml
# fragment
  - name: revenue_prev_year
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"time_offset": "-1y", "time_field": "orders.ordered_at"}'
```

The units are `d`, `w`, `m`, `q` and `y`.

## all the settings

| key in `data` | what it does |
| --- | --- |
| `"table": "orders"` | sets the anchor, for an expression that names no table |
| `"kind": "derived"` | the expression combines other metrics, after the aggregation |
| `"ignore_selection": [...]` | calculates as if these fields (or `"*"`) had no selection |
| `"time_offset": "-1y"`, `"time_field": "orders.ordered_at"` | calculates with the time field moved |
| `"format": "currency:EUR"` | how the value shows: `int`, `decimal`, `percent` or `currency:XXX`, with `compact` and `decimals:N`. A visual can change it. See [number formats](/project/dashboards#number-formats). |

The [extensions reference](/reference/config/extensions#metrics) has the
same list, generated from the source.

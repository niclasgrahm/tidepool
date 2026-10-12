# dashboards

A dashboard is one file, `dashboards/<id>.yaml`. The id of the dashboard is
the name of the file. A dashboard has filters on a rail at the left, and
visuals on a grid.

```yaml
# dashboards/sales.yaml
name: Sales overview
layout: { columns: 12, row_height: 80 }
filters:
  - field: customers.region
    measure: revenue                     # the revenue of each region, in place of its row count
  - field: categories.tree               # a hierarchy: a tree
  - field: products.category
  - field: orders.ordered_at
    widget: date_range
visuals:
  - id: kpi_revenue
    type: kpi
    metrics: [revenue]
    compare: revenue_prev_year           # shown with the change in percent
    at: { x: 0, y: 0, w: 3, h: 2 }
  - id: by_region
    type: bar                            # horizontal when it has more than six bars
    dimensions: [customers.region]
    metrics: [revenue]
    sort: { by: revenue, desc: true }
    on_click: select                     # a click selects the value
    at: { x: 3, y: 0, w: 5, h: 4 }
  - id: trend
    type: line
    dimensions: [{ field: orders.ordered_at, grain: day }]
    metrics: [revenue, order_count]      # a second metric gets its own axis
    on_brush: select_range               # a drag selects the dates
    at: { x: 0, y: 4, w: 12, h: 4 }
  - id: top_products
    type: table                          # sort by any column; a click on a row selects
    dimensions: [products.name, products.category]
    metrics: [revenue, order_count, avg_order_value]
    sort: { by: revenue, desc: true }
    limit: 50
    at: { x: 8, y: 0, w: 4, h: 4 }
```

The [dashboard reference](/reference/config/dashboard) lists every key.

## the grid

`at` puts a visual on the grid: column `x`, row `y`, `w` columns wide and `h`
rows high. The default grid has 12 columns and rows of 80 px. The model checks
that no two visuals overlap and that no visual goes past the last column.

## visuals

| type | shows | a click |
| --- | --- | --- |
| `kpi` | one metric, large. With `compare`, the change from a second metric in percent. | |
| `bar` | metrics by one dimension | selects the value (`on_click: select`) |
| `line` | metrics over a time dimension at a grain | a drag selects a range (`on_brush: select_range`) |
| `table` | metrics by one or more dimensions, sortable | selects the row (`on_click: select`) |

Time grains are `day`, `week`, `month`, `quarter` and `year`.

Each visual has a CSV button. It downloads the data of the visual under the
selection.

## live

A dashboard updates at each epoch, every 200 ms by default. tidepool sends
only what changed. A value that changed flashes. The page never shows a mix
of two epochs.

The pulse at the top right shows the epoch and the ingest rate. **Pause**
stops the updates. The page then counts the updates that wait.

Filters and selections have [a page of their own](/project/selections).

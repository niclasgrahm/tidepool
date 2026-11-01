# dashboards

A dashboard is one file, `dashboards/<id>.yaml`. The id of the dashboard is
the name of the file. A dashboard has filters on a rail at the left, and
visuals on a grid. You can write the file, or make the dashboard in
[the editor](#the-editor) of the web UI.

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
| `bar` | metrics by one dimension. More than six bars lie down. | selects the value (`on_click: select`). A drag across bars selects all of them. |
| `line` | metrics over a time dimension at a grain. A second metric gets its own axis. | selects the period. A drag selects a range (`on_brush: select_range`). |
| `table` | metrics by one or more dimensions, sortable. Without metrics, each combination of its dimensions that the data has. | selects the row (`on_click: select`) |
| `filter` | the values of one dimension, as a list (a tree for a hierarchy). It has no metrics. | selects the value |

A table without metrics reads the rows of one table: the table that reaches
all its dimensions and is nearest to the one side. For `customers.region`
and `products.category`, that table is `orders`. The table then shows each
pair of a region and a category that some order in the selection has.

A `filter` visual shows the same list as a filter on the rail. Its states
are the same: selected, possible and excluded. Use it to put a filter next
to the visuals that it changes.

```yaml
# fragment: two visuals of dashboards/sales_recent.yaml
  - id: category
    type: filter
    dimensions: [products.category]
    at: { x: 0, y: 6, w: 3, h: 4 }
  - id: who_buys_what
    type: table
    dimensions: [customers.region, products.category]
    at: { x: 3, y: 6, w: 9, h: 4 }
```

Time grains are `day`, `week`, `month`, `quarter` and `year`.

Ctrl or ⌘ with a click adds to the selection. Charts also work from the
keyboard: Tab to a chart, move with the arrow keys, and push Enter to
select. On a line, Shift with the arrow keys makes a range.

Each visual has a CSV button. It downloads the data of the visual under the
selection.

## the editor

You can also make and change a dashboard on the web UI. The editor writes
the same file, through the same pipeline as every other change.

1. On the dashboard list, click **New dashboard**. To change a dashboard,
   click **Edit** at the top right of the dashboard.
2. Type the title.
3. Click **Add visual**. Select the dimensions and the metrics. The list
   shows in grey the fields that no relationship connects to your choice.
4. Click **Add**. One dimension without metrics becomes a filter. Any other
   choice becomes a table.
5. To change the type, select the visual and click a type. You can click
   only the types that fit the fields.
6. Drag a visual to move it. Drag its bottom right corner to change its size.
7. Click **Save**.

Each visual snaps to the cells of the grid. A visual cannot cover another
visual. When the place under the pointer is not free, the editor shows it in
red and does not move the visual. With the keyboard, the arrow keys move the
selected visual. Shift and the arrow keys change its size.

Each visual shows its data while you edit. tidepool checks the dashboard on
each change. **Save** is available when the dashboard compiles.

The editor needs the admin token, as the config page does. It writes the
file in the style of the examples. It does not keep the comments of the
file, and it tells you when the file has comments. The editor does not
change the filters on the rail. Edit them in the file or on the config page.

## live

A dashboard updates at each epoch, every 200 ms by default. tidepool sends
only what changed. A value that changed flashes. The page never shows a mix
of two epochs.

The pulse at the top right shows the epoch and the ingest rate. **Pause**
stops the updates. The page then counts the updates that wait.

Filters and selections have [a page of their own](/project/selections).

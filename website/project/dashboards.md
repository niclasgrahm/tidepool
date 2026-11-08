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
    format: { revenue: compact }         # 167.8M, not 167,791,795.92
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

A visual can have a `title` for its header, and a `description`: one
sentence about what it shows. The header shows the description when the
pointer is on it, and `describe_model` gives both to agents. Without a
`title`, the header names the metrics and the first dimension.

| type | shows | a click |
| --- | --- | --- |
| `kpi` | one metric, large. With `compare`, the change from a second metric in percent. | |
| `bar` | metrics by one dimension. More than six bars lie down. | selects the value (`on_click: select`). A drag across bars selects all of them. |
| `line` | metrics over a time dimension at a grain. A second metric gets its own axis. | selects the period. A drag selects a range (`on_brush: select_range`). |
| `table` | metrics by one or more dimensions, sortable. Without metrics, each combination of its dimensions that the data has. | selects the row (`on_click: select`) |
| `pie` | one metric by one dimension, as slices of a ring: the largest `limit` (8, at most 12). For a sum or a count, the rest is one more slice. | selects the slice (`on_click: select`) |
| `filter` | the values of one dimension, as a list (a tree for a hierarchy). It has no metrics. | selects the value |
| `rows` | the newest rows of one table that the selection allows, the newest first. Its dimensions are the columns. It has no metrics. | |

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

Time grains are `day`, `week`, `month`, `quarter` and `year`. A timestamp
also has `hour` and `minute`, and a date does not.

## number formats

A format sets how the value of a metric shows on the screen. It does not
change the value. The CSV download and the API give the full value.

A metric can have a format in the model (see
[metrics](/project/metrics#all-the-settings)). A visual can set a different
format for each metric that it shows, with `format`:

```yaml
# fragment
    format: { revenue: "currency:EUR compact", avg_order_value: "decimals:1" }
```

A format is one or more words, in any order:

| word | what it does | 1234567.891 shows as |
| --- | --- | --- |
| `int` | no decimals | 1,234,568 |
| `decimal` | two decimals | 1,234,567.89 |
| `percent` | the value is a fraction: 0.25 shows as 25.0% | |
| `currency:EUR` | the currency, by its ISO code. EUR, USD, GBP and JPY show as a sign, others as the code. | €1,234,567.89 |
| `compact` | from 1,000, the value in thousands (k), millions (M), billions (B) or trillions (T), with one decimal | 1.2M |
| `decimals:N` | N decimals, from 0 to 6 | |

Use `compact` for totals that change at each epoch. A number such as
167,791,795.92 changes in its last digits many times each second, and the
eye cannot read it. 167.8M changes only when the total really moves. A
shortened value shows the full value when you hold the pointer on it.

Without a format, a value shows as tidepool calculates it.

Ctrl or ⌘ with a click adds to the selection. Charts also work from the
keyboard: Tab to a chart, move with the arrow keys, and push Enter to
select. On a line, Shift with the arrow keys makes a range.

Each visual has a download button (an arrow) at the top right. It downloads
the data of the visual under the selection, as CSV. The header of a visual
also shows how it is set up: its time grain, and `on_click` or `on_brush`
when a click or a drag selects.

To see a visual larger, click its maximize button (four corners), next to
the download button. The visual then fills the grid, and the other visuals
are hidden. The filters and the selection bar stay, and the selection
applies as before: you can select in the large visual too. Click the button
again, or push Esc, to put the visual back on the grid.

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
7. To change how a metric shows, select the visual and set its
   [format](#number-formats) under **Format**: a style, the decimals, and
   **Compact**. The editor shows an example. **Reset** uses the format of
   the metric again.
8. Click **Save**.

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

The pulse at the top right shows the epoch on the page. **Pause**, beside
it, stops the updates. The page then counts the updates that wait.

The status bar at the bottom of each page shows the connection, the epoch
and its age, the rows and segments that the server holds, the live sessions
and the ingest rate. Above the dashboard, "answered in" shows how long the
last selection took, from the click to the last visual.

## search

Push ⌘K (Ctrl+K on Windows and Linux), or click **Search** in the top bar.
Type part of a name: a filter or a visual of the open dashboard, another
dashboard, a table or a page. The arrow keys move through the results, and
Enter goes there. On a filter, Enter puts the cursor in its search box.

Filters and selections have [a page of their own](/project/selections).

## rows

A `rows` visual shows the rows themselves: a log tail, the latest orders.
The newest row is at the top, in the order that the rows arrived. `limit`
sets how many rows it shows: 100 by default, 1000 at most. The columns can
be fields of the table, and fields of the tables that it relates to.

```yaml
# fragment: a visual of dashboards/shipping_notes.yaml
  - id: latest
    type: rows
    dimensions: [orders.ordered_at, customers.name, orders.status, orders.amount, orders.note]
    limit: 50
    at: { x: 3, y: 0, w: 9, h: 7 }
```

The rows are the rows of the table that reaches all the columns, as for a
table without metrics. The selection filters them as it filters a metric on
that table. When new rows arrive, the visual reads only the new rows. A row
visual has no sort and no grain.

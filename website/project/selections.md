# filters and selections

A selection is the set of values selected on a dashboard, over all its
fields. A click on a value in a filter, on a bar or on a table row selects
it. Every visual on the dashboard then shows only the data that the selection
allows. The selection shows at the top of the page, one chip for each field.

## filters

There are four filter widgets:

| widget | for | default for |
| --- | --- | --- |
| `list` | any field: a list of values with a search box | most fields |
| `tree` | a [hierarchy](/project/hierarchies) field: nodes to open and select | hierarchy fields |
| `date_range` | a date or time field: fixed dates, or a period relative to now | |
| `search` | a string field: a text box. It selects the rows that contain the text. | |

### the three states

A list shows each value in one of three states:

- **selected**: you selected it;
- **possible**: the other selections allow it;
- **excluded**: the other selections do not allow it.

### a measure on a filter

A list shows how many rows hold each value. With `measure`, it shows a metric
for each value, under the rest of the selection:

```yaml
# fragment
filters:
  - field: customers.region
    measure: revenue
```

## search

A `search` filter selects the rows whose value contains the text that you
type. It is for text: log lines, notes, messages. The case of the letters
A to Z does not matter. Other characters must be the same. The text is one
phrase: `lock timeout` finds those two words, together.

```yaml
# fragment
filters:
  - field: logs.message
    widget: search
```

The search is a selection, as a click is. Each visual on the dashboard then
shows only the rows that contain the text: the counts, the charts, the
lists. A click on a bar narrows the search results more. The chip of the
selection shows the text. **Back** removes the text in one step, not one
letter at a time. A `rows` visual marks the text where it occurs.

The search is fast on a column with `encoding: plain`. tidepool searches the
text where it stores it, about 16 GB a second for each core, and keeps the
result for each segment. On a column with a dictionary, tidepool searches
each value one time. To find text in log lines, store the text `plain`, and
store what you filter by (the severity, the service) as separate columns
with a dictionary.

## more than one value

Ctrl-click (⌘-click on a Mac) adds a value to the selection of its field. The
values of one field are alternatives: region `north` or region `south`. The
fields combine: region `north` and category `boats`.

## back and forward

Each change of the selection is a step in a history. The arrows above the
dashboard, **Back** and **Forward**, go through it.

## relative time windows

A `date_range` filter offers periods relative to now: "Today", "Last 7
days", "Last month", and "Last hour" for timestamps. A relative period moves
with the clock. A dashboard on "Today" shows the new day from midnight, with
or without new data.

Click the filter to open its picker. On the left are the relative periods.
On the right is a calendar: click the first day, then the last day, of a
fixed range. You can also type the days (`2026-10-04`), and for a timestamp
the times (`13:30`), and push Enter. The last day is in the range: "until
2026-10-27" includes all of 27 October. The footer of the picker says if the
period moves with the clock or is fixed, and how long it is. Esc closes the
picker.

`default` sets the period that a dashboard opens on, the first time a viewer
opens it. The viewer can change it or clear it.

```yaml
# fragment
filters:
  - field: orders.ordered_at
    widget: date_range
    default: { last: 7, unit: day }
```

A window is the `last` whole periods of `unit`, the current period included.
`ago` moves the window back:

- `{ last: 7, unit: day }` is today and the six days before it;
- `{ last: 1, unit: month, ago: 1 }` is last month.

The units are `minute`, `hour`, `day`, `week`, `month`, `quarter` and `year`.
Periods start where time grains start (weeks start on Monday). They are in
UTC. A date has no hours or minutes.

## the direction of a selection

A selection goes along the relationships of the model. By default it goes
from the one side to the many side. A metric is filtered by the selected
fields that its anchor reaches through many-to-one relationships:

- a selected region filters `orders`, so `revenue` changes;
- a selected order status does not filter `customers`. The region list does
  not change.

### both ways

To make selections go both ways, as in Qlik, set `filter_direction` on the
model:

```yaml
# fragment
custom_extensions:
  - vendor_name: TIDEPOOL
    data: '{"filter_direction": "both"}'
```

A selected product category then also narrows the customers to the customers
who ordered from it. Their region list shows the other regions as excluded.

With `both`, the relationships must form a tree. Two paths between the same
tables (for example the billing customer and the shipping customer of an
order) are an error. The answer would depend on which path a selection took.

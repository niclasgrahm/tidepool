# the model

The model says what the data means: which tables relate, which values are
fields, and which aggregates are metrics. It is one file,
`model.ossie.yaml`, in [Apache Ossie](https://ossie.apache.org/) 0.2.
Dashboards, queries and agents use the names of the model, never the names
of tables.

```yaml
# fragment
version: "0.2.0"
name: retail
datasets:
  - name: orders
    source: orders                       # a table in tables/
    fields:
      - name: ordered_at
        expression: { dialects: [{ dialect: ANSI_SQL, expression: ordered_at }] }
        dimension: { is_time: true }
  - name: customers
    source: customers
    primary_key: [customer_id]
relationships:
  - name: order_customer
    from: orders                         # the many side
    to: customers                        # the one side: its primary key
    from_columns: [customer_id]
    to_columns: [customer_id]
metrics:
  - name: revenue
    expression: { dialects: [{ dialect: ANSI_SQL, expression: "SUM(orders.amount)" }] }
```

The [model reference](/reference/config/model) lists every key.

## datasets and fields

A dataset is the model's view of one table. Every column of the table is a
field of the dataset, with the name of the column. Declare a field to give it
a label, a description, or an expression.

Fields have the name `dataset.field` everywhere else: `customers.region`,
`orders.ordered_at`.

## derived fields

A field with an expression over the columns of its dataset is a derived
field. The expression is SQL:

```yaml
# fragment
      - name: order_year
        expression: { dialects: [{ dialect: ANSI_SQL, expression: "EXTRACT(YEAR FROM ordered_at)" }] }
```

A derived field over one dictionary column is cheap. tidepool calculates it
one time for each distinct value of the column, not for each row. Then it
groups and filters like a column:

```yaml
# fragment
      - name: coast
        expression:
          dialects:
            - dialect: ANSI_SQL
              expression: "CASE WHEN region IN ('east', 'west') THEN 'coastal' ELSE COALESCE(region, 'unknown') END"
```

## relationships

A relationship goes from the many side (`orders`) to the one side
(`customers`). The `to_columns` are the primary key of the one side.

A selection goes along relationships. By default it goes from the one side to
the many side: a selected region filters the orders. A selected order status
does not filter the customers. To make selections go both ways, see
[filters and selections](/project/selections#both-ways).

A related row that does not exist reads as NULL in every column, as in a SQL
`LEFT JOIN`.

## extensions

Ossie keeps the settings of a vendor in `custom_extensions`. tidepool reads
the entries whose `vendor_name` is `TIDEPOOL`. Their `data` is a JSON
document in a string:

```yaml
# fragment
custom_extensions:
  - vendor_name: TIDEPOOL
    data: '{"filter_direction": "both"}'
```

A model, a dataset, a field, a relationship and a metric can each have a
TIDEPOOL extension. The [extensions reference](/reference/config/extensions)
lists what each one can say. Metrics use them the most: see
[metrics](/project/metrics).

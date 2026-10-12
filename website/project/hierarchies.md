# hierarchies

Some tables hold a tree: a plant (site, hall, line, machine), a category
tree, an org chart. Each row is a node, and one column holds the key of the
parent node. Declare the tree on the dataset of the table:

```yaml
# fragment
  - name: categories
    source: categories
    primary_key: [category]
    custom_extensions:
      - vendor_name: TIDEPOOL
        data: '{"hierarchy": {"name": "tree", "parent": "parent", "kind": "level"}}'
```

The table must be an `upsert` table with a primary key of one column:

```yaml
# tables/categories.yaml
name: categories
mode: upsert
primary_key: [category]
columns:
  category: { type: string }
  parent:   { type: string, nullable: true }   # NULL at the top
  level:    { type: string }                   # department, aisle or category
```

The extension adds one field to the dataset, here `categories.tree`. Its
values are the keys of the nodes.

| key | what it is |
| --- | --- |
| `name` | the name of the new field |
| `parent` | the column that holds the key of the parent node |
| `label` | the column that a node is shown by. Without it, the key. |
| `kind` | a column that says what a node is (`site`, `line`), shown beside it |

## what a selection of a node does

A selection of a node selects the node and every node below it. A selection
of "Line 2" filters every fact that relates to a machine under Line 2.

tidepool uses the tree as it is now. When a machine moves to another line,
the selection follows at the next epoch. Facts from the past also count under
the new line.

The tree can be ragged. Each node is at the depth that its parents give it.
A row whose parent does not exist is a root.

## on a dashboard

A hierarchy field is a tree filter by default:

```yaml
# fragment
filters:
  - field: categories.tree
```

Open a node to see its children. Click a node to select its branch.
Ctrl-click (⌘-click) adds a branch. A node with a selection below it has a
dot, so that you can find the selection when the branch is closed.

With `measure`, a tree shows a metric for each node. tidepool calculates the
metric for each level, so a ratio is correct at each level. The OEE of a line
is over the time and the units of its machines, not an average of the OEE of
each machine.

## in a query

A dimension on a hierarchy field can group by a level of the tree. Level 0 is
the roots:

```json
{ "metrics": ["revenue"], "dimensions": [{ "field": "categories.tree", "level": 1 }] }
```

A node that is not as deep as the level groups as NULL.

## keep the tree in step

To keep a tree in step with a system that can only be read whole, send the
whole tree each time with `PUT`. tidepool deletes the nodes that are not in
it, in the same step. See [writing rows](/data/writing-rows#replace-a-table).
kayak's `indu_namespace` input does this for the namespace of indud. See
`examples/indud-oee` in the repository.

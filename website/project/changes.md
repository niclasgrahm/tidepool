# changes

A change is a set of config files to set or to remove. tidepool applies a
change while it serves. Every change goes through the same steps, from any
source:

1. tidepool compiles the new files and reports every problem.
2. It plans the change and gives each part a class.
3. It checks the class against the policy of the source.
4. It applies the change.
5. It records the change as a new version.
6. It writes the files back to the project directory, and commits them when
   the directory is a git repository.

The sources are:

- the files of the project directory: the server reads them every second;
- the **Config** page of the web UI;
- `POST /v1/config/apply`;
- an agent, over [MCP](/agents).

## classes

| class | examples | what happens |
| --- | --- | --- |
| **safe** | a table, a nullable column, a field, a metric, a dashboard | live at the next epoch |
| **rebuild** | a relationship between tables that hold data, an encoding, a wider type, a new primary key | tidepool rewrites the data in the background. The old version serves until the new version is ready. |
| **destructive** | a table or a column removed, a narrower type, a new mode | needs approval |
| **security** | a change to `listen`, `auth` or `agents` in `tidepool.yaml` | needs approval. An agent never applies it. See [access](/operating/access#changes-to-access). |

`tidepool plan` shows the class of each part before you apply it:

```
1 change (0 safe, 1 rebuild, 0 destructive)

  + relationship product_supplier (products → suppliers)  rebuild  (joins existing data: the key dictionaries are merged)
```

## approval

tidepool applies a safe change from the files at once. A change of another
class from the files waits for approval on **Config → Waiting for
approval**.

Over the API, add `"approve": true` to apply a destructive or a security
change. Add
`"propose": true` to put a change in the queue for a person.

An agent applies only the classes in `agents.auto_apply` (`[safe]` by
default). The other changes wait for a person. `auto_apply` cannot hold
`security`.

## history

Each applied change is a version. **Config → History** shows each version,
its source and the diff of its files. You can revert to a version. A revert is
a change too, with a plan and a class.

## at startup

The data directory remembers the version of the config that it holds. When
the server starts on a project directory that changed while the server was
stopped, it applies the difference as a change. If the change is destructive,
the server does not start. Start it with `--allow-destructive` to apply the
change.

## two changes at the same time

A change names the version that it was made against (`base_version`). A
change against an older version fails with `409` and the current version.
Plan the change again against the current version.

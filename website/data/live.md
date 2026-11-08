# the live protocol

The web UI gets its dashboards over one WebSocket, `GET /v1/ws`. Other
clients can use the same protocol. Its message types are `ClientMessage` and
`ServerMessage` in the `live` module of the `tidepool-protocol` crate. That
crate is Apache-2.0, so a client can use it under any licence.

Each message is a JSON object with a `type`.

## a session

1. The client sends `hello`.
2. The server sends `model`: the dashboards.
3. The client sends `open_dashboard`.
4. The server sends one `frame` at each epoch, with only what changed on the
   dashboard.

When the model changes, the server sends `model` again.

## views

A view is a dashboard that is only in the memory of the server. Make one
with `POST /v1/views`: the body is a dashboard file as JSON. A visual
without `at` gets a place. The answer has the id of the view, for example
`~3f9c2a7d41b0e85c6d2f1a9b7e4c3d20`.

```json
{"name": "Revenue by region", "visuals": [
  {"id": "regions", "type": "bar", "dimensions": ["customers.region"], "metrics": ["revenue"]}
]}
```

Open a view with `open_dashboard` and its id, as a dashboard. Views are not
in `model`, so the server sends `view` with what the view shows. Frames and
selections are the same as for a dashboard. In the web UI, a view is at
`#/v/<id>`.

The server checks a view with the same rules as a dashboard file, and
answers `422` with the problems if it is wrong. A view is never written to
the config. Its YAML is in `GET /v1/views/{id}`; **Save as dashboard** in
the UI writes it to `dashboards/` as a config change.

When the config changes, the server checks the view again. If it no longer
fits the model (for example, a metric it shows was removed), the session
gets an `error`. The server forgets a view that nobody opens for 24 hours.
It keeps 1000 views at most.

## the selection

`GET /v1/sessions/{id}/selection` gives the selection of a session, by the
`session_id` of its `welcome`. `PUT` on the same path replaces it, with
conditions written as the filters of a query. The session gets `selection`
and a new frame at once. The change is one step in its history.

A connection that closes can come back to its session for one hour: send
`hello` with its `session_id`. The web UI does this when the connection
drops.

These messages change the selection. The server answers them at once, out of
the epoch loop:

| message | what it does |
| --- | --- |
| `select` | selects values of a field |
| `select_many` | selects values of several fields as one step: `{"fields": [{"field": …, "values": [...]}, …]}`. Back undoes all of it. |
| `select_range` | selects a range of a time field |
| `select_relative` | selects a window relative to now: `{"field": …, "window": {"unit": "day", "last": 7}}` |
| `clear` | clears the selection of a field, or all of it |
| `back`, `forward` | go through the history of the selection |

The server sends the selection back in `selection`. A relative window comes
back with its current `from` and `to`. The server sends it again when the
clock moves the window into a new period.

## filters and visuals

| message | what it does |
| --- | --- |
| `filter_window` | pages through a filter list, or the visible nodes of a tree, or searches it |
| `filter_open` | opens or closes nodes of a tree: `{"field": …, "keys": [...], "open": true}` |
| `sort` | sorts a table visual again |

The frames of a tree filter carry `nodes` in place of `values`.

## flow control

`pause` stops the frames. `resume` starts them again. A frame always holds
the newest data. A slow client gets fewer frames, never old ones.

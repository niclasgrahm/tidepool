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

## the selection

These messages change the selection. The server answers them at once, out of
the epoch loop:

| message | what it does |
| --- | --- |
| `select` | selects values of a field |
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

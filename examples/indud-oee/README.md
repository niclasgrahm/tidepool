# OEE over Indu's namespace

An OEE dashboard filtered by the plant as Indu knows it: the org tree and
its machines, read from an [indud](../../../../indu/indud) instance by
[Kayak](https://github.com/niclasgrahm/kayak)'s `indu_namespace` input and
kept in step by its `tidepool` output's `write: replace`. In Tidepool the
namespace is a **hierarchy** (plan §5.6): a tree filter whose nodes each
show their OEE, and selecting a site, hall or line filters every number to
the machines under it.

```
tidepool.yaml          server settings
tables/namespace.yaml  Indu's nodes: id, parent_id, label, kind (replaced whole by Kayak)
tables/devices.yaml    each machine's ideal cycle time (Indu doesn't keep one)
tables/minutes.yaml    each machine's minutes: state, stop reason, good and rejected units
model.ossie.yaml       the hierarchy, and OEE = availability × performance × quality
dashboards/oee.yaml    the tree filter (measure: oee), state and reason, KPIs, trend
kayak/                 the Kayak pipeline that reads the namespace
simulate.sh            machine minutes for whatever devices the namespace holds
```

## Without Indu

```bash
cargo run -p tidepool -- serve --config examples/indud-oee
./examples/indud-oee/simulate.sh --sample
```

`--sample` puts a made-up plant in the namespace first (Sweden and
Germany, three factories, seven machines), shaped as Kayak writes Indu's
(`org:…`, `device:…`). Then open <http://127.0.0.1:7070/#/d/oee>.

## With Indu

Run indud (it listens on :8080) and register a key for Kayak, then Kayak
on `kayak/config.yaml` (the key as `INDU_API_KEY` in its secrets) and the
simulator without `--sample`: it finds the machines Kayak wrote. Move a
machine to another line in Indu and its numbers move with it at Kayak's
next read; delete one and it's gone from the tree.

## Why one table of minutes

Availability is run time over planned time, performance ideal time over run
time, quality good units over all of them — each a ratio of sums, so a
line's OEE is over its machines' minutes and units rather than an average
of their OEEs, and the tree filter computes it per level. Keeping state and
counts in one per-minute table means one time selection narrows all three.
Indu has no OEE model of its own; in production an edge (Kayak, from a
state sensor and two counters) would compute these minutes.

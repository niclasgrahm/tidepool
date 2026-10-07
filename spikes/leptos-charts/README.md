# spike: Leptos × ECharts (decision D10)

**Question:** can a Leptos client drive ECharts (click-to-select, brush-to-select,
live updates) and a large virtualised list through thin JS interop? If yes, the
Tidepool UI is Leptos like Kayak's; if not, a TypeScript SPA.

**Verdict (2026-10-07): pass. The UI is Leptos (CSR, built with trunk).**

| criterion | result |
|---|---|
| ECharts interop is a thin, reusable wrapper (< ~300 lines) | `src/echarts.rs`: 120 lines of code for bindings + one `EChart` component with click, brush, live option updates and resize |
| 50k-row virtualised list with search | `src/vlist.rs`: 49 lines; 17 rows in the DOM for 50k items |
| interaction feels native | search over 50k rows 4–5 ms; bar click → every view recomputed and both charts re-rendered 5–14 ms; no long tasks (> 50 ms) during 5 s of live updates every 200 ms (release build, task timing) |

What the interop needed to get right (all now encoded in `echarts.rs`):

1. ECharts event objects contain circular DOM references, so they can't be
   JSON round-tripped whole: the wrapper picks named fields. (Getting this wrong
   silently turned every click into index 0.)
2. Charts must follow their *element's* size (`ResizeObserver`), not the
   window's: grid layouts resize charts without the window changing.
3. Brush mode is a "global cursor" that has to be re-armed after `setOption`.
4. Event closures are `forget()`-ed here; the real component should keep them
   and drop them on cleanup.

Measuring note: the browser pane was hidden during measurement, which
throttles `requestAnimationFrame` to ~1 Hz. rAF-based timings showed a flat
~1000 ms; the numbers above use `setTimeout(0)` task timing instead.

Run it:

```bash
npm install            # fetches echarts into node_modules (copied into dist by trunk)
trunk build --release
cargo run -p tidepool -- serve --ui-dir spikes/leptos-charts/dist   # from the repo root
```

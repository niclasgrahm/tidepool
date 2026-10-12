<script setup lang="ts">
// tidepool's landing page. The copy follows landing/copy-style.md and the
// facts landing/product-and-copy.md; the look is landing/visual-language.md.
//
// The hero holds a working replica of a dashboard: a region filter with a
// measure, a KPI and a bar visual over one small table of orders, with
// `filter_direction: both`. A click selects, as in the product: the region
// list shows selected, possible and excluded, and every number follows. New
// orders arrive at each "epoch", and the values they change flash, as the
// product's do.
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { withBase } from 'vitepress'

const REGIONS = ['north', 'south', 'east', 'west', 'central'] as const
const CATEGORIES = ['kayaks', 'tents', 'bikes', 'boats', 'shoes'] as const
type Region = (typeof REGIONS)[number]
type Category = (typeof CATEGORIES)[number]

// revenue in whole euros by region and category; a zero is "no orders", which
// is what makes a value excluded when the other field is selected
const seed: Record<Region, Record<Category, number>> = {
  north: { kayaks: 0, tents: 18400, bikes: 9100, boats: 31200, shoes: 4300 },
  south: { kayaks: 22800, tents: 7600, bikes: 12900, boats: 0, shoes: 6100 },
  east: { kayaks: 14100, tents: 0, bikes: 15800, boats: 19900, shoes: 5200 },
  west: { kayaks: 26500, tents: 11200, bikes: 0, boats: 8800, shoes: 7400 },
  central: { kayaks: 0, tents: 9800, bikes: 17300, boats: 0, shoes: 3900 },
}
const revenue = reactive(structuredClone(seed))
const orders = reactive(
  Object.fromEntries(
    REGIONS.map((r) => [r, Object.fromEntries(CATEGORIES.map((c) => [c, Math.round(seed[r][c] / 61)]))]),
  ) as Record<Region, Record<Category, number>>,
)

const region = ref<Region[]>([])
const category = ref<Category[]>([])
const history = ref<{ region: Region[]; category: Category[] }[]>([])
const epoch = ref(4812)
const paused = ref(false)
const fresh = reactive(new Set<string>())

function remember() {
  history.value.push({ region: [...region.value], category: [...category.value] })
}
function toggle<T>(list: T[], value: T, add: boolean): T[] {
  if (add) return list.includes(value) ? list.filter((v) => v !== value) : [...list, value]
  return list.length === 1 && list[0] === value ? [] : [value]
}
function pickRegion(r: Region, e: MouseEvent) {
  remember()
  region.value = toggle(region.value, r, e.metaKey || e.ctrlKey)
}
function pickCategory(c: Category, e: MouseEvent) {
  remember()
  category.value = toggle(category.value, c, e.metaKey || e.ctrlKey)
}
function clearField(field: 'region' | 'category') {
  remember()
  if (field === 'region') region.value = []
  else category.value = []
}
function back() {
  const last = history.value.pop()
  if (!last) return
  region.value = last.region
  category.value = last.category
}

const inRegions = (r: Region) => region.value.length === 0 || region.value.includes(r)
const inCategories = (c: Category) => category.value.length === 0 || category.value.includes(c)

function sum(table: Record<Region, Record<Category, number>>, r: (r: Region) => boolean, c: (c: Category) => boolean) {
  let total = 0
  for (const x of REGIONS) if (r(x)) for (const y of CATEGORIES) if (c(y)) total += table[x][y]
  return total
}

// the region list: its measure under the *other* field's selection, and its
// state; with selections both ways, a region with no orders in the selected
// categories is excluded
const regionRows = computed(() =>
  REGIONS.map((r) => {
    const value = sum(revenue, (x) => x === r, inCategories)
    const state = region.value.includes(r) ? 'selected' : value === 0 ? 'excluded' : 'possible'
    return { r, value, state }
  }),
)
const categoryBars = computed(() => {
  const rows = CATEGORIES.map((c) => ({ c, value: sum(revenue, inRegions, (y) => y === c) }))
  const max = Math.max(1, ...rows.map((b) => b.value))
  return rows
    .filter((b) => b.value > 0)
    .sort((a, b) => b.value - a.value)
    .map((b) => ({ ...b, width: (b.value / max) * 100, selected: category.value.includes(b.c) }))
})
const kpiRevenue = computed(() => sum(revenue, inRegions, inCategories))
const kpiOrders = computed(() => sum(orders, inRegions, inCategories))
const rate = ref(20)

const euro = (n: number) => '€' + n.toLocaleString('en-US').replace(/,/g, ' ')
const count = (n: number) => n.toLocaleString('en-US').replace(/,/g, ' ')

let timer: ReturnType<typeof setInterval> | undefined
let reduced = false
onMounted(() => {
  reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches
  // an "epoch": a few new orders where orders already happen, and a flash on
  // every value they changed
  timer = setInterval(() => {
    if (paused.value || document.hidden) return
    epoch.value += 1
    const touched = new Set<string>()
    const n = 2 + Math.floor(Math.random() * 3)
    for (let i = 0; i < n; i++) {
      const r = REGIONS[Math.floor(Math.random() * REGIONS.length)]
      const c = CATEGORIES[Math.floor(Math.random() * CATEGORIES.length)]
      if (seed[r][c] === 0) continue
      revenue[r][c] += 40 + Math.floor(Math.random() * 260)
      orders[r][c] += 1
      touched.add('r:' + r)
      touched.add('c:' + c)
      if (inRegions(r) && inCategories(c)) touched.add('kpi')
    }
    rate.value = 15 + Math.floor(Math.random() * 11)
    if (reduced) return
    fresh.clear()
    // a new set each tick restarts the animation on the same element
    requestAnimationFrame(() => touched.forEach((k) => fresh.add(k)))
  }, 1400)
})
onBeforeUnmount(() => {
  if (timer) clearInterval(timer)
})

const link = (path: string) => withBase(path)
const shot = (name: string) => withBase(`/screens/${name}`)
</script>

<template>
<div class="landing">
<!-- ================================================================ hero -->
<header class="hero">
  <div class="hero-inner">
    <div class="hero-copy">
      <h1>live analytics <span class="thin">in one binary</span></h1>
      <p class="lede">tidepool is a live analytics server. You push rows to it over HTTP. You keep the tables, the model and the dashboards in YAML files in git. The dashboards update while data arrives, and a click filters every number on the page.</p>
      <pre class="install"><span class="p">$ </span>tidepool init shop
<span class="p">$ </span>tidepool serve --config shop      <span class="c"># http://127.0.0.1:7070</span>
<span class="p">$ </span>tidepool feed                     <span class="c"># sample orders, live</span></pre>
      <div class="hero-links">
        <a class="primary" :href="link('/getting-started')">getting started</a>
        <a :href="link('/reference/')">reference</a>
        <a href="https://github.com/niclasgrahm/tidepool">github</a>
      </div>
      <p class="fine">one rust binary · rows over HTTP · the model in git · pre-1.0</p>
    </div>

    <figure class="replica" aria-label="a working replica of a tidepool dashboard">
      <div class="strip">
        <span class="brand">tidepool</span>
        <span class="pulse" :class="{ paused }">
          <span class="dot" aria-hidden="true"></span>
          <span>{{ paused ? 'paused' : 'Live' }} · epoch {{ epoch }} · {{ rate }} rows/s</span>
          <button type="button" @click="paused = !paused">{{ paused ? 'Resume' : 'Pause' }}</button>
        </span>
      </div>
      <div class="toolbar">
        <strong>Sales overview</strong>
        <ul class="chips" aria-label="the selection">
          <li v-if="region.length" class="chip"><span class="chip-field">region</span> {{ region.join(', ') }}<button type="button" aria-label="clear region" @click="clearField('region')">×</button></li>
          <li v-if="category.length" class="chip"><span class="chip-field">category</span> {{ category.join(', ') }}<button type="button" aria-label="clear category" @click="clearField('category')">×</button></li>
          <li v-if="!region.length && !category.length" class="hint">Click a region or a bar. Ctrl/⌘ adds to the selection.</li>
        </ul>
        <button type="button" class="back" :disabled="!history.length" @click="back">Back</button>
      </div>
      <div class="board">
        <div class="rail">
          <h3>region <span class="measure">revenue</span></h3>
          <button
            v-for="row in regionRows"
            :key="row.r"
            type="button"
            class="value"
            :class="[row.state, { fresh: fresh.has('r:' + row.r) }]"
            @click="pickRegion(row.r, $event)"
          >
            <span>{{ row.r }}</span>
            <span class="num">{{ row.value ? euro(row.value) : '–' }}</span>
          </button>
        </div>
        <div class="visuals">
          <div class="visual kpi">
            <h3>revenue</h3>
            <p class="kpi-value" :class="{ fresh: fresh.has('kpi') }">{{ euro(kpiRevenue) }}</p>
            <p class="kpi-sub">{{ count(kpiOrders) }} orders</p>
          </div>
          <div class="visual bars">
            <h3>revenue by category</h3>
            <button
              v-for="bar in categoryBars"
              :key="bar.c"
              type="button"
              class="bar-row"
              :class="{ selected: bar.selected, fresh: fresh.has('c:' + bar.c) }"
              @click="pickCategory(bar.c, $event)"
            >
              <span class="bar-label">{{ bar.c }}</span>
              <span class="bar-track"><span class="bar" :style="{ width: bar.width + '%' }"></span></span>
              <span class="num">{{ euro(bar.value) }}</span>
            </button>
          </div>
        </div>
      </div>
      <figcaption>A replica of a dashboard, with <code>filter_direction: both</code>. The data is invented. The rules are the product's.</figcaption>
    </figure>
  </div>
</header>

<!-- ======================================================== associative -->
<section class="section" id="associative">
  <div class="section-inner split">
    <div>
      <p class="label">associative</p>
      <h2>a click filters the whole page</h2>
      <p>A click on a value, a bar or a table row selects it. Every visual on the dashboard then shows only the data that the selection allows. Each filter shows its values in three states:</p>
      <ul class="states">
        <li><span class="state selected">selected</span> you selected it</li>
        <li><span class="state possible">possible</span> the other selections allow it</li>
        <li><span class="state excluded">excluded</span> the other selections do not allow it</li>
      </ul>
      <ul class="facts">
        <li>Selections go along the relationships of the model: from the one side to the many side, or both ways.</li>
        <li>A tree filter selects a whole branch: a line and every machine under it.</li>
        <li>A filter can show a metric for each value, under the rest of the selection.</li>
        <li>"Today" and "Last 7 days" move with the clock.</li>
        <li><strong>Back</strong> and <strong>Forward</strong> go through the selections.</li>
      </ul>
      <p><a :href="link('/project/selections')">filters and selections →</a></p>
    </div>
    <figure class="shot">
      <img class="only-light" :src="shot('dashboard-selected-light.jpg')" alt="The sales dashboard with a region selected: a coral chip, the other regions possible, every visual filtered" loading="lazy" width="1600" height="1000">
      <img class="only-dark" :src="shot('dashboard-selected-dark.jpg')" alt="The sales dashboard with a region selected, in the dark theme" loading="lazy" width="1600" height="1000">
    </figure>
  </div>
</section>

<!-- =============================================================== live -->
<section class="section alt" id="live">
  <div class="section-inner split">
    <div>
      <p class="label">live</p>
      <h2>rows in, dashboards move</h2>
      <p>Each table gets an ingest endpoint when you declare it. Send JSON, NDJSON or Arrow. tidepool checks every value and writes all of a batch or none of it. At each epoch, every open dashboard gets only what changed.</p>
      <ul class="facts">
        <li>An <code>Idempotency-Key</code> makes a retry safe, also across a restart.</li>
        <li>With a data directory, tidepool acknowledges a batch after it is in the log on disk.</li>
        <li>A full write queue answers <code>503</code> with <code>Retry-After</code>.</li>
        <li>tidepool updates each answer with the new rows only. It does not scan the table again.</li>
      </ul>
      <p><a :href="link('/data/writing-rows')">writing rows →</a></p>
    </div>
    <div class="stack">
      <pre class="code"><span class="p">$ </span>curl -X POST 'localhost:7070/v1/tables/orders/rows?wait=visible' \
    -H 'content-type: application/x-ndjson' \
    -H 'idempotency-key: batch-42' \
    --data-binary @orders.ndjson
<span class="o">{"written":5000,"replaced":0,"missing":0,"epoch":4813}</span></pre>
      <dl class="numbers">
        <div><dt>ack to every screen, p95</dt><dd>228 ms</dd><p>50 live sessions, 100,000 rows/s, 200 ms epochs, 10 million orders</p></div>
        <div><dt>an epoch of 100,000 orders</dt><dd>&lt; 1 ms</dd><p>0.6 to 0.9 ms, at 100 million orders</p></div>
      </dl>
    </div>
  </div>
</section>

<!-- ============================================================ as code -->
<section class="section" id="code">
  <div class="section-inner split">
    <div>
      <p class="label">the project is code</p>
      <h2>the model lives in git</h2>
      <p>Tables, the semantic model and dashboards are YAML files. The model is <a href="https://ossie.apache.org/">Apache Ossie</a>, with SQL for fields and metrics. tidepool plans every change and gives it a class. It applies the change while it serves, and writes it back to the files.</p>
      <ul class="classes">
        <li><span class="badge safe">safe</span> a table, a metric, a dashboard: live at the next epoch</li>
        <li><span class="badge rebuild">rebuild</span> a join of tables with data: rewritten in the background</li>
        <li><span class="badge destructive">destructive</span> a column removed: needs approval</li>
      </ul>
      <p>The same steps apply to the files, the config page, the API and agents. <code>tidepool validate</code> points at the wrong token, and says what you meant.</p>
      <p><a :href="link('/project/changes')">changes →</a></p>
    </div>
    <div class="stack">
      <pre class="code tree">shop/
├── tidepool.yaml
├── model.ossie.yaml
├── tables/
│   ├── orders.yaml
│   └── customers.yaml
└── dashboards/
    └── sales.yaml</pre>
      <pre class="code"><span class="c"># model.ossie.yaml</span>
<span class="k">metrics</span>:
  - <span class="k">name</span>: <span class="s">paid_revenue</span>
    <span class="k">expression</span>: { <span class="k">dialects</span>: [{ <span class="k">dialect</span>: <span class="s">ANSI_SQL</span>,
      <span class="k">expression</span>: <span class="s">"SUM(orders.amount) FILTER (WHERE orders.status = 'paid')"</span> }] }</pre>
      <pre class="code"><span class="p">$ </span>tidepool plan --config shop --base main
1 change (0 safe, 1 rebuild, 0 destructive)

  + relationship product_supplier (products → suppliers)  <span class="o">rebuild</span>
    (joins existing data: the key dictionaries are merged)</pre>
    </div>
  </div>
</section>

<!-- ========================================================== numbers -->
<section class="section alt" id="numbers">
  <div class="section-inner">
    <p class="label">fast, and checked</p>
    <h2>measured, and compared with DuckDB</h2>
    <p class="wide">The numbers are from an Apple M1 Max (10 cores), release build, with the benchmarks in the repository. The reference is a retail star schema.</p>
    <dl class="numbers four">
      <div><dt>ingest, Arrow</dt><dd>2.4M rows/s</dd><p>8 clients, batches of 10,000, until visible. NDJSON: 2.2M.</p></div>
      <div><dt>a new selection, p95</dt><dd>113 ms</dd><p>100 million orders, region × category, three metrics</p></div>
      <div><dt>memory</dt><dd>28 bytes</dd><p>for each stored order. 100 million orders: about 2.8 GB.</p></div>
      <div><dt>restart</dt><dd>1.2 s</dd><p>100 million orders, from a checkpoint and the log</p></div>
    </dl>
    <div class="checks">
      <div>
        <h3>the oracle</h3>
        <p>Random schemas, data and queries from a seed. Each query runs on three paths of the engine, and each answer must be the same as DuckDB's. Live answers are compared after every epoch.</p>
      </div>
      <div>
        <h3>power losses</h3>
        <p>1,000 simulated crashes, some during a checkpoint, and 25 <code>kill -9</code> cycles of the binary. No acknowledged write is lost.</p>
      </div>
      <div>
        <h3>the compiler</h3>
        <p>Each check of the model has a golden test of its exact message. Every YAML sample on this site compiles in the test suite.</p>
      </div>
    </div>
    <p><a :href="link('/contributing/testing')">testing →</a> · <a :href="link('/contributing/benchmarks')">benchmarks →</a></p>
  </div>
</section>

<!-- ============================================================= kayak -->
<section class="section" id="kayak">
  <div class="section-inner split">
    <div>
      <p class="label">with kayak</p>
      <h2>brokers in, through kayak</h2>
      <p>tidepool reads HTTP only. Its sister project, <a href="https://propell.dev/kayak/">kayak</a>, reads MQTT, Kafka, NATS, OPC UA and databases. kayak's <code>tidepool</code> output checks its mapping against the table at startup, and sends each batch with an idempotency key.</p>
      <p><code>just demo</code> starts a simulated plant: MQTT → kayak → tidepool, with a live dashboard of the plant floor.</p>
      <p><a :href="link('/data/kayak')">kayak →</a></p>
    </div>
    <div class="stack">
      <div class="flow" aria-label="MQTT to kayak to tidepool">
        <span class="node">MQTT · Kafka · OPC UA</span>
        <span class="arrow" aria-hidden="true"></span>
        <span class="node">kayak</span>
        <span class="arrow" aria-hidden="true"></span>
        <span class="node tide">tidepool</span>
      </div>
      <pre class="code"><span class="c"># fragment: kayak's config.yaml</span>
<span class="k">outputs</span>:
  - <span class="k">type</span>: <span class="s">tidepool</span>
    <span class="k">connection</span>: <span class="s">tidepool</span>
    <span class="k">table</span>: <span class="s">readings</span></pre>
    </div>
  </div>
</section>

<!-- ============================================================ agents -->
<section class="section alt" id="agents">
  <div class="section-inner split">
    <div>
      <p class="label">agents</p>
      <h2>an MCP server, with a policy</h2>
      <p>An agent can read the model, calculate metrics, and plan changes over MCP. Each tool calls the HTTP endpoint that it wraps, with the token of the agent. The agent applies only the change classes that you allow. A person approves the rest.</p>
      <p><a :href="link('/agents')">agents →</a></p>
    </div>
    <div class="stack">
      <pre class="code"><span class="p">$ </span>claude mcp add --transport http tidepool http://127.0.0.1:7070/mcp</pre>
      <pre class="code"><span class="c"># tidepool.yaml</span>
<span class="k">agents</span>:
  <span class="k">auto_apply</span>: [<span class="s">safe</span>]    <span class="c"># the default</span></pre>
    </div>
  </div>
</section>

<!-- ============================================================ limits -->
<section class="section" id="limits">
  <div class="section-inner">
    <p class="label">limits</p>
    <h2>what tidepool does not do</h2>
    <div class="limits">
      <div><h3>one machine</h3><p>No cluster and no shards. The target is up to about 500 million fact rows on one machine.</p></div>
      <div><h3>one model</h3><p>One model for each server. No tenants and no row-level security.</p></div>
      <div><h3>HTTP only</h3><p>tidepool has no connectors. Use kayak for brokers and databases.</p></div>
      <div><h3>no free SQL</h3><p>A query is metrics by fields, under a selection. There is no SQL console.</p></div>
      <div><h3>no drag and drop</h3><p>Dashboards are YAML files. The config page edits the files.</p></div>
      <div><h3>pre-1.0</h3><p>The config format and the data directory can change between minor versions.</p></div>
    </div>
  </div>
</section>

<!-- ============================================================ footer -->
<footer class="end">
  <div class="section-inner">
    <h2>run it</h2>
    <pre class="install"><span class="p">$ </span>just build
<span class="p">$ </span>./target/release/tidepool init shop && ./target/release/tidepool serve --config shop</pre>
    <div class="hero-links">
      <a class="primary" :href="link('/getting-started')">getting started</a>
      <a :href="link('/reference/')">reference</a>
      <a :href="link('/operating/deployment')">deployment</a>
      <a href="https://github.com/niclasgrahm/tidepool">github</a>
    </div>
    <p class="fine">AGPL-3.0-or-later, with a commercial licence available. The protocol and config crates are Apache-2.0.</p>
  </div>
</footer>
</div>
</template>

<style>
.landing-page .VPContent { padding-top: var(--vp-nav-height) !important; }
.landing-page .VPPage { padding: 0 !important; }

.landing {
  --display: var(--tp-display);
  color: var(--tp-ink);
  font-variant-numeric: tabular-nums;
}
.landing a { color: var(--tp-tide); text-decoration: none; }
.landing a:hover { text-decoration: underline; }
.landing code { font-family: var(--vp-font-family-mono); font-size: 0.88em; background: color-mix(in srgb, var(--tp-kelp) 55%, transparent); padding: 1px 5px; border-radius: 4px; }
.landing h1, .landing h2, .landing h3 { font-family: var(--display); letter-spacing: -0.01em; color: var(--tp-ink); margin: 0; border: 0; padding: 0; }
.landing p { margin: 0 0 14px; line-height: 1.55; }

/* ---------------------------------------------------------------- hero */
.hero { border-bottom: 1px solid var(--tp-sand); }
.hero-inner {
  max-width: 1240px;
  margin: 0 auto;
  padding: 64px 24px 56px;
  display: grid;
  grid-template-columns: minmax(0, 5fr) minmax(0, 6fr);
  gap: 48px;
  align-items: center;
}
.hero h1 { font-size: clamp(38px, 5vw, 60px); line-height: 1.02; font-weight: 700; margin-bottom: 20px; }
.hero h1 .thin { display: block; font-weight: 500; color: var(--tp-tide); }
.lede { font-size: 17px; color: var(--tp-ink-soft); max-width: 34em; }
.install {
  margin: 22px 0 18px;
  padding: 14px 16px;
  background: var(--tp-surface);
  border-left: 3px solid var(--tp-tide);
  font: 13px/1.7 var(--vp-font-family-mono);
  overflow-x: auto;
  white-space: pre;
}
.install .p, .code .p { color: var(--tp-ink-soft); user-select: none; }
.install .c, .code .c { color: var(--tp-ink-soft); }
.hero-links { display: flex; flex-wrap: wrap; gap: 10px; margin-bottom: 16px; }
.hero-links a {
  padding: 6px 16px;
  border: 1px solid var(--tp-sand);
  border-radius: 4px;
  color: var(--tp-ink);
  font-weight: 700;
}
.hero-links a.primary { background: var(--tp-tide); border-color: var(--tp-tide); color: var(--tp-paper); }
.hero-links a:hover { text-decoration: none; border-color: var(--tp-tide); }
.fine { font-size: 13px; color: var(--tp-ink-soft); }

/* ------------------------------------------------------------ replica */
.replica { margin: 0; background: var(--tp-paper); border: 1px solid var(--tp-sand); font-size: 13px; }
.replica .strip { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 8px 14px; border-bottom: 1px solid var(--tp-sand); }
.replica .brand { font: 700 15px/1 var(--display); }
.pulse { display: flex; align-items: center; gap: 8px; color: var(--tp-ink-soft); font-size: 12px; }
.pulse .dot { width: 7px; height: 7px; border-radius: 50%; background: var(--tp-tide); }
.pulse.paused .dot { background: var(--tp-anemone); }
.pulse button, .toolbar .back { border: 1px solid var(--tp-sand); background: transparent; border-radius: 4px; padding: 1px 8px; cursor: pointer; color: var(--tp-ink); font: inherit; }
.toolbar .back:disabled { opacity: 0.4; cursor: default; }
.toolbar { display: flex; align-items: center; gap: 8px 14px; padding: 10px 14px 0; flex-wrap: wrap; }
.toolbar strong { font: 700 16px/1.1 var(--display); }
.chips { display: flex; gap: 6px; list-style: none; margin: 0; padding: 0; flex: 1; flex-wrap: wrap; min-height: 22px; align-items: center; }
.chip { display: flex; align-items: center; gap: 5px; padding: 1px 2px 1px 9px; border-radius: 999px; background: var(--tp-anemone); color: #fff; font-size: 12px; }
.chip-field { opacity: 0.85; }
.chip button { border: 0; background: transparent; color: inherit; cursor: pointer; padding: 0 6px; font: inherit; }
.hint { color: var(--tp-ink-soft); font-size: 12px; }
.board { display: grid; grid-template-columns: 190px minmax(0, 1fr); gap: 14px; padding: 12px 14px 14px; }
.rail h3, .visual h3 { font: 500 13px/1.2 var(--display); margin-bottom: 7px; }
.measure { color: var(--tp-ink-soft); font-weight: 400; }
.value {
  display: flex; justify-content: space-between; align-items: center; width: 100%;
  height: 28px; margin-bottom: 2px; padding: 3px 8px;
  border: 0; border-left: 4px solid var(--tp-tide); background: var(--tp-surface);
  color: var(--tp-ink); font: inherit; text-align: left; cursor: pointer;
}
.value.selected { border-left-color: var(--tp-anemone); background: var(--tp-anemone); color: #fff; }
.value.excluded { border-left-color: var(--tp-kelp); background: transparent; color: var(--tp-ink-soft); }
.value .num { opacity: 0.8; }
.visuals { display: grid; grid-template-rows: auto 1fr; gap: 12px; min-width: 0; }
.visual { padding: 10px 12px; background: var(--tp-surface); border-top: 3px solid var(--tp-tide); }
.kpi-value { font: 700 30px/1 var(--display); margin: 2px 0 4px; display: inline-block; }
.kpi-sub { color: var(--tp-ink-soft); margin: 0; font-size: 12px; }
.bar-row {
  display: grid; grid-template-columns: 52px minmax(0, 1fr) 70px; align-items: center; gap: 8px;
  width: 100%; padding: 3px 0; border: 0; background: transparent; color: var(--tp-ink);
  font: inherit; text-align: left; cursor: pointer;
}
.bar-track { height: 16px; }
.bar { display: block; height: 100%; background: var(--tp-tide); transition: width 0.4s ease; }
.bar-row.selected .bar { background: var(--tp-anemone); }
.bar-row .num { text-align: right; color: var(--tp-ink-soft); }
.replica figcaption { padding: 8px 14px; border-top: 1px solid var(--tp-sand); color: var(--tp-ink-soft); font-size: 12px; }
@media (prefers-reduced-motion: no-preference) {
  .landing .fresh { animation: tp-fresh 1.2s ease-out; }
  .landing .value.selected.fresh { animation: none; }
}
@keyframes tp-fresh {
  from { background-color: color-mix(in srgb, var(--tp-anemone) 30%, transparent); }
}

/* ------------------------------------------------------------ sections */
.section { border-bottom: 1px solid var(--tp-sand); }
.section.alt { background: var(--tp-surface); }
.section-inner { max-width: 1240px; margin: 0 auto; padding: 64px 24px; }
.split { display: grid; grid-template-columns: minmax(0, 5fr) minmax(0, 6fr); gap: 48px; align-items: start; }
.label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.08em; color: var(--tp-tide) !important; font-weight: 700; margin-bottom: 8px !important; }
.section h2, .end h2 { font-size: clamp(26px, 3vw, 36px); font-weight: 700; margin-bottom: 16px; line-height: 1.1; }
.section h3 { font-size: 17px; font-weight: 500; margin-bottom: 6px; }
.section p { color: var(--tp-ink-soft); }
.wide { max-width: 46em; }
.facts { margin: 0 0 16px; padding-left: 18px; color: var(--tp-ink-soft); list-style: disc; }
.facts li::marker { color: var(--tp-tide); }
.facts li { margin: 4px 0; line-height: 1.5; }
.states, .classes { list-style: none; padding: 0; margin: 0 0 16px; display: flex; flex-direction: column; gap: 6px; color: var(--tp-ink-soft); }
.state { display: inline-block; width: 92px; margin-right: 10px; padding: 3px 8px; border-left: 4px solid var(--tp-tide); background: var(--tp-surface); color: var(--tp-ink); }
.section.alt .state { background: var(--tp-paper); }
.state.selected { border-left-color: var(--tp-anemone); background: var(--tp-anemone); color: #fff; }
.state.excluded { border-left-color: var(--tp-kelp); background: transparent; color: var(--tp-ink-soft); }
.badge { display: inline-block; min-width: 92px; margin-right: 10px; padding: 0 8px; border-radius: 999px; font-size: 12px; border: 1px solid var(--tp-sand); text-align: center; color: var(--tp-ink); }
.badge.safe { border-color: var(--tp-tide); color: var(--tp-tide); }
.badge.rebuild { background: var(--tp-kelp); }
.badge.destructive { background: var(--tp-anemone); border-color: var(--tp-anemone); color: #fff; }
.shot { margin: 0; }
.shot img { width: 100%; height: auto; border: 1px solid var(--tp-sand); display: block; }
.only-dark { display: none !important; }
.dark .only-dark { display: block !important; }
.dark .only-light { display: none !important; }
.stack { display: flex; flex-direction: column; gap: 14px; min-width: 0; }
.code {
  margin: 0; padding: 14px 16px;
  background: var(--tp-surface); border-left: 3px solid var(--tp-tide);
  font: 13px/1.6 var(--vp-font-family-mono); white-space: pre; overflow-x: auto; color: var(--tp-ink);
}
.section.alt .code { background: var(--tp-paper); }
.code .k { color: var(--tp-tide); }
.code .s { color: var(--tp-ink); }
.code .o { color: var(--tp-ink-soft); }
.numbers { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; margin: 0; }
.numbers.four { grid-template-columns: repeat(4, minmax(0, 1fr)); margin: 8px 0 32px; }
.numbers div { background: var(--tp-paper); padding: 14px 16px; border-top: 3px solid var(--tp-tide); }
.section:not(.alt) .numbers div { background: var(--tp-surface); }
.numbers dt { font-size: 13px; color: var(--tp-ink-soft); }
.numbers dd { margin: 4px 0 6px; font: 700 28px/1.1 var(--display); color: var(--tp-ink); }
.numbers p { font-size: 13px; margin: 0; }
.checks { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 28px; margin-bottom: 20px; }
.flow { display: flex; align-items: center; gap: 0; flex-wrap: wrap; }
.flow .node { padding: 10px 14px; background: var(--tp-surface); border: 1.5px solid var(--tp-tide); font: 500 15px var(--display); }
.flow .node.tide { background: var(--tp-tide); color: var(--tp-paper); }
.flow .arrow { width: 36px; height: 2px; background: var(--tp-ink-soft); position: relative; }
.flow .arrow::after { content: ''; position: absolute; right: -1px; top: -4px; border: 5px solid transparent; border-left: 7px solid var(--tp-ink-soft); border-right: 0; }
.limits { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px; }
.limits div { padding: 14px 16px; background: var(--tp-surface); border-top: 3px solid var(--tp-sand); }
.limits p { margin: 0; font-size: 14px; }
.end { background: var(--tp-surface); }
.end .install { background: var(--tp-paper); max-width: 760px; }

@media (max-width: 960px) {
  .hero-inner, .split { grid-template-columns: minmax(0, 1fr); gap: 32px; }
  .hero-inner { padding-top: 40px; }
  .numbers.four { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .checks, .limits { grid-template-columns: minmax(0, 1fr); }
}
@media (max-width: 520px) {
  .hero-inner, .section-inner { padding-left: 16px; padding-right: 16px; }
  .board { grid-template-columns: minmax(0, 1fr); }
  .numbers, .numbers.four { grid-template-columns: minmax(0, 1fr); }
  .pulse span:not(.dot) { display: none; }
}
</style>

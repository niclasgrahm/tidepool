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
// categories is excluded. The measure is also a bar behind each value, as
// long as its share of the largest, as in the product.
const regionRows = computed(() => {
  const rows = REGIONS.map((r) => {
    const value = sum(revenue, (x) => x === r, inCategories)
    const state = region.value.includes(r) ? 'selected' : value === 0 ? 'excluded' : 'possible'
    return { r, value, state }
  })
  const max = Math.max(1, ...rows.map((row) => row.value))
  return rows.map((row) => ({ ...row, share: (row.value / max) * 100 }))
})
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
      <div class="app">
        <div class="strip">
          <span class="brand">
            <svg class="brand-mark" viewBox="0 0 20 20" aria-hidden="true">
              <rect x="1" y="1" width="18" height="18" rx="5" />
              <path d="M4 12c2-2 4-2 6 0s4 2 6 0" />
              <path class="faint" d="M4 8c2-2 4-2 6 0s4 2 6 0" />
            </svg>
            tidepool
          </span>
          <span class="app-nav" aria-hidden="true"><span class="current">Dashboards</span><span>Model</span><span>Config</span></span>
          <span class="grow"></span>
          <span class="pulse" :class="{ paused }">
            <span class="pill"><span class="dot" aria-hidden="true"></span>{{ paused ? 'Paused' : 'Live' }} <code>epoch {{ count(epoch) }}</code></span>
            <button type="button" class="icon-button" :aria-label="paused ? 'Resume' : 'Pause'" @click="paused = !paused">
              <svg v-if="paused" viewBox="0 0 12 12" aria-hidden="true"><path class="solid" d="M3 1.5v9l7-4.5z" /></svg>
              <svg v-else viewBox="0 0 12 12" aria-hidden="true"><rect class="solid" x="2" y="1.5" width="2.5" height="9" rx="0.5" /><rect class="solid" x="7.5" y="1.5" width="2.5" height="9" rx="0.5" /></svg>
            </button>
          </span>
        </div>
        <div class="toolbar">
          <span class="crumbs"><span>Dashboards</span><span class="crumb-sep">/</span><strong>Sales overview</strong></span>
          <span class="divider" aria-hidden="true"></span>
          <span class="history">
            <button type="button" class="icon-button" aria-label="Back" :disabled="!history.length" @click="back">
              <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M7.5 2.5 4 6l3.5 3.5" /></svg>
            </button>
          </span>
          <ul class="chips" aria-label="the selection">
            <li v-if="region.length" class="chip"><span class="chip-field">customers.region</span><span class="chip-value">{{ region.join(', ') }}</span><button type="button" aria-label="Clear customers.region" @click="clearField('region')"><svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2 2l6 6M8 2 2 8" /></svg></button></li>
            <li v-if="category.length" class="chip"><span class="chip-field">products.category</span><span class="chip-value">{{ category.join(', ') }}</span><button type="button" aria-label="Clear products.category" @click="clearField('category')"><svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2 2l6 6M8 2 2 8" /></svg></button></li>
            <li v-if="!region.length && !category.length" class="hint">Click a value or a bar. Ctrl/⌘-click adds to the selection.</li>
          </ul>
        </div>
        <div class="board">
          <section class="filter">
            <header class="filter-head">
              <h3>region</h3>
              <span class="filter-meta">{{ region.length ? `${region.length} of ${REGIONS.length}` : `${REGIONS.length} values` }} · revenue</span>
            </header>
            <button
              v-for="row in regionRows"
              :key="row.r"
              type="button"
              class="value"
              :class="[row.state, { fresh: fresh.has('r:' + row.r) }]"
              :aria-pressed="row.state === 'selected'"
              @click="pickRegion(row.r, $event)"
            >
              <span class="meter" :style="{ width: row.share + '%' }"></span>
              <span class="label">{{ row.r }}</span>
              <span class="rows">{{ row.value ? euro(row.value) : '–' }}</span>
            </button>
          </section>
          <div class="visuals">
            <div class="visual kpi">
              <div class="visual-head"><h3>Revenue</h3></div>
              <p class="kpi-value" :class="{ fresh: fresh.has('kpi') }">{{ euro(kpiRevenue) }}</p>
              <p class="kpi-delta"><span class="delta">{{ count(kpiOrders) }}</span> orders · {{ rate }} rows/s</p>
            </div>
            <div class="visual bars">
              <div class="visual-head"><h3>Revenue by category</h3></div>
              <button
                v-for="bar in categoryBars"
                :key="bar.c"
                type="button"
                class="bar-row"
                :class="{ selected: bar.selected, fresh: fresh.has('c:' + bar.c) }"
                :aria-pressed="bar.selected"
                @click="pickCategory(bar.c, $event)"
              >
                <span class="bar-label">{{ bar.c }}</span>
                <span class="bar-track"><span class="bar" :style="{ width: bar.width + '%' }"></span></span>
                <span class="num">{{ euro(bar.value) }}</span>
              </button>
            </div>
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
      <p class="eyebrow">associative</p>
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
      <img class="only-light" :src="shot('dashboard-selected-light.jpg')" alt="The sales dashboard with a region selected: a selection chip, the other regions possible, every visual filtered" loading="lazy" width="1600" height="1000">
      <img class="only-dark" :src="shot('dashboard-selected-dark.jpg')" alt="The sales dashboard with a region selected, in the dark theme" loading="lazy" width="1600" height="1000">
    </figure>
  </div>
</section>

<!-- =============================================================== live -->
<section class="section alt" id="live">
  <div class="section-inner split">
    <div>
      <p class="eyebrow">live</p>
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
      <p class="eyebrow">the project is code</p>
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
    <p class="eyebrow">fast, and checked</p>
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
      <p class="eyebrow">with kayak</p>
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
      <p class="eyebrow">agents</p>
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
    <p class="eyebrow">limits</p>
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
  color: var(--tp-ink);
  font-variant-numeric: tabular-nums;
}
.landing a { color: var(--tp-tide); text-decoration: none; }
.landing a:hover { text-decoration: underline; }
.landing code { font-family: var(--tp-mono); font-size: 0.86em; background: var(--tp-gray-3); padding: 1px 5px; border-radius: 3px; }
.landing h1, .landing h2, .landing h3 { font-family: var(--tp-display); letter-spacing: -0.01em; color: var(--tp-ink); margin: 0; border: 0; padding: 0; }
.landing p { margin: 0 0 14px; line-height: 1.55; }
.landing button { font: inherit; color: inherit; }

/* ---------------------------------------------------------------- hero */
.hero { border-bottom: 1px solid var(--tp-line); }
.hero-inner {
  max-width: 1240px;
  margin: 0 auto;
  padding: 64px 24px 56px;
  display: grid;
  grid-template-columns: minmax(0, 5fr) minmax(0, 6fr);
  gap: 48px;
  align-items: center;
}
.hero h1 { font-size: clamp(38px, 5vw, 58px); line-height: 1.02; font-weight: 700; letter-spacing: -0.035em; margin-bottom: 20px; }
.hero h1 .thin { display: block; font-weight: 600; color: var(--tp-tide); }
.lede { font-size: 17px; color: var(--tp-ink-soft); max-width: 34em; }
.install {
  margin: 22px 0 18px;
  padding: 12px 14px;
  background: var(--tp-surface);
  border: 1px solid var(--tp-line);
  border-radius: 6px;
  font: 13px/1.7 var(--tp-mono);
  overflow-x: auto;
  white-space: pre;
}
.install .p, .code .p { color: var(--tp-gray-9); user-select: none; }
.install .c, .code .c { color: var(--tp-ink-soft); }
.hero-links { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 16px; }
.landing .hero-links a {
  height: 32px;
  display: inline-flex;
  align-items: center;
  padding: 0 14px;
  border: 1px solid var(--tp-sand);
  border-radius: 4px;
  background: var(--tp-surface);
  color: var(--tp-ink);
  font-size: 14px;
  font-weight: 500;
}
.landing .hero-links a:hover { text-decoration: none; background: var(--tp-gray-3); }
.landing .hero-links a.primary { background: var(--tp-tide-fill); border-color: var(--tp-tide-fill); color: #fff; }
.landing .hero-links a.primary:hover { background: var(--tp-tide-fill); filter: brightness(0.94); }
.fine { font-size: 13px; color: var(--tp-ink-soft); }

/* ------------------------------------------------------------ replica */
/* The UI's own rules (frontend/style.css), at the UI's own sizes: 13px
   text, 1px edges, 6px cards, numbers in mono. */
.replica { margin: 0; }
.app {
  overflow: hidden;
  background: var(--tp-paper);
  border: 1px solid var(--tp-line);
  border-radius: 8px;
  box-shadow: var(--tp-shadow-pop);
  font-size: 13px;
  line-height: 1.45;
}
.app .grow { flex: 1; }
.app .icon-button {
  width: 24px; height: 24px; flex: none;
  display: inline-grid; place-items: center;
  padding: 0;
  border: 1px solid var(--tp-sand); border-radius: 4px;
  background: var(--tp-surface); color: var(--tp-ink);
  cursor: pointer;
}
.app .icon-button:hover:not(:disabled) { background: var(--tp-gray-3); }
.app .icon-button:disabled { opacity: 0.45; cursor: default; }
.app .icon-button svg { width: 12px; height: 12px; fill: none; stroke: currentColor; stroke-width: 1.5; }
.app .icon-button svg .solid { fill: currentColor; stroke: none; }
.app :focus-visible { outline: 2px solid var(--tp-tide-fill); outline-offset: 2px; }
.app code { padding: 0; background: none; font-size: 11px; }

.strip {
  display: flex; align-items: center; gap: 12px;
  min-height: 40px; padding: 6px 10px;
  background: var(--tp-panel);
  border-bottom: 1px solid var(--tp-line);
}
.brand { display: flex; align-items: center; gap: 8px; font: 600 14px/1 var(--tp-display); letter-spacing: -0.01em; }
.brand-mark { width: 20px; height: 20px; }
.brand-mark rect { fill: var(--tp-tide-fill); }
.brand-mark path { fill: none; stroke: #fff; stroke-width: 1.6; stroke-linecap: round; }
.brand-mark path.faint { stroke-opacity: 0.6; }
.app-nav { display: flex; gap: 2px; }
.app-nav span { padding: 3px 8px; border-radius: 4px; color: var(--tp-ink-soft); }
.app-nav .current { background: var(--tp-kelp); color: var(--tp-ink); font-weight: 500; }
.pulse { display: flex; align-items: center; gap: 6px; }
.pulse .pill {
  height: 24px; display: inline-flex; align-items: center; gap: 6px; padding: 0 8px;
  border-radius: 999px; background: var(--tp-green-3); color: var(--tp-green-11);
  font-size: 12px; font-weight: 500; white-space: nowrap;
}
.pulse .pill code { font-weight: 400; color: inherit; }
.pulse .dot { width: 6px; height: 6px; border-radius: 50%; background: var(--tp-green-9); box-shadow: 0 0 0 3px var(--tp-green-6); }
.pulse.paused .pill { background: var(--tp-amber-3); color: var(--tp-amber-11); }
.pulse.paused .dot { background: var(--tp-amber-11); box-shadow: none; }

.toolbar {
  display: flex; align-items: center; flex-wrap: wrap; gap: 6px 10px;
  min-height: 38px; padding: 6px 10px;
  background: var(--tp-panel);
  border-bottom: 1px solid var(--tp-line);
}
.crumbs { display: flex; align-items: center; gap: 6px; color: var(--tp-ink-soft); white-space: nowrap; }
.crumbs strong { font-weight: 600; color: var(--tp-ink); }
.crumb-sep { color: var(--tp-gray-8); }
.divider { width: 1px; height: 18px; background: var(--tp-line); }
.history { display: flex; }
.chips { display: flex; flex: 1; flex-wrap: wrap; align-items: center; gap: 6px; min-width: 0; min-height: 24px; margin: 0; padding: 0; list-style: none; }
.chip {
  height: 24px; display: flex; align-items: center; gap: 6px; padding: 0 2px 0 8px;
  border: 1px solid var(--tp-anemone-line); border-radius: 4px;
  background: var(--tp-anemone-soft); color: var(--tp-anemone-text);
  font-size: 12px;
}
.chip-field { font: 11px var(--tp-mono); opacity: 0.8; }
.chip-value { font-weight: 500; }
.chip button { width: 18px; height: 18px; display: grid; place-items: center; padding: 0; border: 0; border-radius: 3px; background: transparent; color: inherit; cursor: pointer; }
.chip button:hover { background: var(--tp-ruby-4); }
.chip button svg { width: 10px; height: 10px; stroke: currentColor; stroke-width: 1.4; }
.hint { color: var(--tp-ink-soft); font-size: 12px; }

.board { display: grid; grid-template-columns: 200px minmax(0, 1fr); gap: 8px; padding: 8px; align-items: start; }
.filter, .visual {
  min-width: 0;
  background: var(--tp-surface);
  border: 1px solid var(--tp-line);
  border-radius: 6px;
}
.filter { padding: 8px; }
.filter-head { display: flex; align-items: baseline; justify-content: space-between; gap: 8px; margin-bottom: 6px; }
.landing .filter-head h3 { font: 500 11px/1.2 var(--tp-display); letter-spacing: 0.04em; text-transform: uppercase; color: var(--tp-ink-soft); }
.filter-meta { font: 10.5px var(--tp-mono); color: var(--tp-ink-soft); white-space: nowrap; }
.value {
  position: relative; display: flex; align-items: center; gap: 8px;
  width: 100%; height: 24px; margin-bottom: 2px; padding: 0 6px;
  border: 0; border-radius: 3px; background: transparent;
  font-size: 12px; text-align: left; cursor: pointer; overflow: hidden;
}
/* the state's mark: a small square before the value */
.value::before, .state::before {
  content: ""; position: relative; z-index: 1;
  flex: 0 0 8px; width: 8px; height: 8px; border-radius: 2px;
  background: var(--tp-tide-fill);
}
.value:hover { background: var(--tp-gray-3); }
.value .label { position: relative; z-index: 1; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.value .rows { position: relative; z-index: 1; font: 11px var(--tp-mono); color: var(--tp-ink-soft); white-space: nowrap; }
.value .meter { position: absolute; left: 0; top: 0; bottom: 0; border-radius: 3px; background: var(--tp-tide-soft); transition: width 0.3s ease; }
.value.selected, .value.selected:hover { background: var(--tp-anemone); color: #fff; }
.value.selected::before { background: #fff; }
.value.selected .rows { color: inherit; opacity: 0.85; }
.value.selected .meter { background: rgb(255 255 255 / 0.16); }
.value.excluded { background: var(--tp-gray-2); color: var(--tp-gray-9); }
.value.excluded::before { background: var(--tp-gray-7); }
.value.excluded .rows { color: var(--tp-gray-9); }

.visuals { display: grid; gap: 8px; min-width: 0; }
.visual { padding: 10px 12px; }
.visual-head { margin-bottom: 6px; }
.landing .visual-head h3 { font: 600 13px/1.3 var(--tp-display); }
.kpi-value { display: inline-block; margin: 0 !important; font: 600 30px/1.1 var(--tp-display); letter-spacing: -0.02em; border-radius: 3px; }
.kpi-delta { display: flex; align-items: center; gap: 6px; margin: 6px 0 0 !important; font-size: 11px; color: var(--tp-ink-soft); }
.kpi-delta .delta { padding: 0 4px; border-radius: 3px; background: var(--tp-gray-3); color: var(--tp-ink); font: 500 11px var(--tp-mono); }
.bar-row {
  display: grid; grid-template-columns: 52px minmax(0, 1fr) 64px; align-items: center; gap: 8px;
  width: 100%; padding: 2px 0; border: 0; border-radius: 3px; background: transparent;
  font: 10.5px var(--tp-mono); text-align: left; cursor: pointer;
}
.bar-label { color: var(--tp-ink-soft); }
.bar-track { height: 16px; }
.bar { display: block; height: 100%; border-radius: 2px; background: var(--tp-tide-fill); transition: width 0.3s ease; }
.bar-row:hover .bar { filter: brightness(1.08); }
.bar-row.selected .bar { background: var(--tp-anemone); }
.bar-row .num { text-align: right; color: var(--tp-ink-soft); }
.replica figcaption { padding: 10px 2px 0; color: var(--tp-ink-soft); font-size: 12px; }
@media (prefers-reduced-motion: no-preference) {
  .landing .fresh { animation: tp-fresh 1.2s ease-out; }
  .landing .value.selected.fresh { animation: none; }
}
@media (prefers-reduced-motion: reduce) {
  .value .meter, .bar { transition: none; }
}
@keyframes tp-fresh {
  from { background-color: color-mix(in srgb, var(--tp-anemone) 22%, transparent); }
}

/* ------------------------------------------------------------ sections */
.section { border-bottom: 1px solid var(--tp-line); }
.section.alt { background: var(--tp-panel); }
.section-inner { max-width: 1240px; margin: 0 auto; padding: 64px 24px; }
.split { display: grid; grid-template-columns: minmax(0, 5fr) minmax(0, 6fr); gap: 48px; align-items: start; }
.eyebrow { font-size: 11px; text-transform: uppercase; letter-spacing: 0.06em; color: var(--tp-tide) !important; font-weight: 600; margin-bottom: 8px !important; }
.section h2, .end h2 { font-size: clamp(26px, 3vw, 34px); font-weight: 700; letter-spacing: -0.025em; margin-bottom: 16px; line-height: 1.1; }
.section h3 { font-size: 15px; font-weight: 600; margin-bottom: 6px; }
.section p { color: var(--tp-ink-soft); }
.wide { max-width: 46em; }
.facts { margin: 0 0 16px; padding-left: 18px; color: var(--tp-ink-soft); list-style: disc; }
.facts li::marker { color: var(--tp-tide-fill); }
.facts li { margin: 4px 0; line-height: 1.5; }
.states, .classes { list-style: none; padding: 0; margin: 0 0 16px; display: flex; flex-direction: column; gap: 6px; color: var(--tp-ink-soft); }
.states li, .classes li { display: flex; align-items: center; gap: 10px; }
/* the three states, as a filter list draws them */
.state {
  display: inline-flex; align-items: center; gap: 8px; flex: none;
  width: 104px; height: 24px; padding: 0 6px; border-radius: 3px;
  background: var(--tp-surface); border: 1px solid var(--tp-line);
  color: var(--tp-ink); font-size: 12px;
}
.state.selected { background: var(--tp-anemone); border-color: var(--tp-anemone); color: #fff; }
.state.selected::before { background: #fff; }
.state.excluded { background: var(--tp-gray-2); color: var(--tp-gray-9); }
.state.excluded::before { background: var(--tp-gray-7); }
/* the plan's class badges, as the config page draws them */
.badge {
  display: inline-flex; align-items: center; justify-content: center; flex: none;
  min-width: 92px; height: 20px; padding: 0 6px; border-radius: 3px;
  background: var(--tp-gray-3); color: var(--tp-ink-soft);
  font: 500 11px var(--tp-mono);
}
.badge.safe { background: var(--tp-tide-soft); color: var(--tp-tide); }
.badge.rebuild { background: var(--tp-amber-3); color: var(--tp-amber-11); }
.badge.destructive { background: var(--tp-anemone); color: #fff; }
.shot { margin: 0; }
.shot img { display: block; width: 100%; height: auto; border: 1px solid var(--tp-line); border-radius: 8px; box-shadow: var(--tp-shadow-pop); }
.only-dark { display: none !important; }
.dark .only-dark { display: block !important; }
.dark .only-light { display: none !important; }
.stack { display: flex; flex-direction: column; gap: 12px; min-width: 0; }
.code {
  margin: 0; padding: 12px 14px;
  background: var(--tp-surface); border: 1px solid var(--tp-line); border-radius: 6px;
  font: 13px/1.6 var(--tp-mono); white-space: pre; overflow-x: auto; color: var(--tp-ink);
}
/* the config editor's YAML colours (frontend/style.css, --hl-*) */
.code .k { color: var(--tp-teal-11); }
.code .s { color: var(--tp-amber-11); }
.code .o { color: var(--tp-ink-soft); }
.numbers { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; margin: 0; }
.numbers.four { grid-template-columns: repeat(4, minmax(0, 1fr)); margin: 8px 0 32px; }
.numbers div, .limits div { background: var(--tp-surface); border: 1px solid var(--tp-line); border-radius: 6px; padding: 12px 14px; }
.numbers dt { font-size: 12px; color: var(--tp-ink-soft); }
.numbers dd { margin: 4px 0 6px; font: 600 26px/1.1 var(--tp-display); letter-spacing: -0.02em; color: var(--tp-ink); }
.numbers p { font-size: 13px; margin: 0; }
.checks { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 28px; margin-bottom: 20px; }
.flow { display: flex; align-items: center; gap: 0; flex-wrap: wrap; }
.flow .node { padding: 8px 12px; background: var(--tp-surface); border: 1px solid var(--tp-line); border-radius: 6px; font-size: 14px; font-weight: 500; }
.flow .node.tide { background: var(--tp-tide-fill); border-color: var(--tp-tide-fill); color: #fff; }
.flow .arrow { width: 32px; height: 1px; background: var(--tp-gray-8); position: relative; }
.flow .arrow::after { content: ''; position: absolute; right: -1px; top: -4px; border: 4.5px solid transparent; border-left: 6px solid var(--tp-gray-8); border-right: 0; }
.limits { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 8px; }
.limits p { margin: 0; font-size: 14px; }
.end { background: var(--tp-panel); }
.end .install { max-width: 760px; }

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
  .app-nav, .pulse .pill code, .crumbs > span:not(:last-child), .divider { display: none; }
}
</style>

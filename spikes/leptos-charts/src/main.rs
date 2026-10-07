//! D10 spike: a miniature Tidepool dashboard in Leptos.
//!
//! - live data: a timer appends orders every 200 ms
//! - a bar chart (revenue by region): click selects, Ctrl/Cmd-click toggles
//! - a line chart (revenue by day): brush selects a day range
//! - a 50k-customer list box: virtualised, searchable, selected/possible/excluded
//! - every selection filters every other view, and chips show/clear them

mod echarts;
mod vlist;

use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use echarts::{Click, EChart};
use leptos::prelude::*;
use serde_json::json;
use vlist::VirtualList;

const REGIONS: [&str; 6] = ["North", "South", "East", "West", "Central", "Islands"];
const CUSTOMERS: u32 = 50_000;
const DAYS: usize = 60;

#[derive(Clone, Copy)]
struct Order {
    region: usize,
    day: usize,
    customer: u32,
    amount: f64,
}

/// A tiny deterministic PRNG, so the spike needs no `rand` (and no getrandom on wasm).
#[derive(Clone)]
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn order(rng: &mut Lcg, day: usize) -> Order {
    Order {
        region: rng.below(REGIONS.len() as u64) as usize,
        day,
        customer: rng.below(u64::from(CUSTOMERS)) as u32,
        amount: 5.0 + rng.below(20_000) as f64 / 100.0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum State {
    Selected,
    Possible,
    Excluded,
}

#[derive(Clone, PartialEq)]
struct CustomerRow {
    id: u32,
    label: String,
    state: State,
}

#[component]
fn App() -> impl IntoView {
    // --- data ---------------------------------------------------------------
    let rng = StoredValue::new(Lcg(42));
    let orders = RwSignal::new({
        let mut seed = Lcg(7);
        (0..DAYS - 10)
            .flat_map(|day| (0..400).map(move |_| day))
            .map(|day| order(&mut seed, day))
            .collect::<Vec<_>>()
    });
    let today = RwSignal::new(DAYS - 10);
    let live = RwSignal::new(true);
    let ticks = RwSignal::new(0_u32);

    // the "ingest": 40 orders every 200 ms; a new day every 25 ticks
    let _ = set_interval_with_handle(
        move || {
            if !live.get_untracked() {
                return;
            }
            ticks.update(|t| *t += 1);
            if ticks.get_untracked() % 25 == 0 && today.get_untracked() < DAYS - 1 {
                today.update(|d| *d += 1);
            }
            let day = today.get_untracked();
            orders.update(|all| {
                let mut r = rng.get_value();
                for _ in 0..40 {
                    all.push(order(&mut r, day));
                }
                rng.set_value(r);
            });
        },
        std::time::Duration::from_millis(200),
    );

    // --- selections -----------------------------------------------------------
    let regions = RwSignal::new(BTreeSet::<usize>::new());
    let days = RwSignal::new(None::<(usize, usize)>);
    let customers = RwSignal::new(BTreeSet::<u32>::new());

    let passes = move |o: &Order, skip: &str| {
        (skip == "region" || regions.with(|s| s.is_empty() || s.contains(&o.region)))
            && (skip == "day" || days.get().is_none_or(|(a, b)| o.day >= a && o.day <= b))
            && (skip == "customer" || customers.with(|s| s.is_empty() || s.contains(&o.customer)))
    };

    // --- views ----------------------------------------------------------------
    let by_region = Memo::new(move |_| {
        let mut totals = [0.0_f64; REGIONS.len()];
        orders.with(|all| all.iter().filter(|o| passes(o, "region")).for_each(|o| totals[o.region] += o.amount));
        totals
    });
    let bar_option = Signal::derive(move || {
        let selected = regions.get();
        let data: Vec<_> = by_region
            .get()
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let colour = if selected.is_empty() || selected.contains(&i) { "#2f6fdb" } else { "#c9ced6" };
                json!({ "value": (v * 100.0).round() / 100.0, "itemStyle": { "color": colour } })
            })
            .collect();
        json!({
            "animationDurationUpdate": 150,
            "grid": { "left": 70, "right": 20, "top": 20, "bottom": 30 },
            "tooltip": {},
            "xAxis": { "type": "category", "data": REGIONS },
            "yAxis": { "type": "value" },
            "series": [{ "type": "bar", "data": data }]
        })
    });

    let by_day = Memo::new(move |_| {
        let mut totals = vec![0.0_f64; DAYS];
        orders.with(|all| all.iter().filter(|o| passes(o, "day")).for_each(|o| totals[o.day] += o.amount));
        totals
    });
    let line_option = Signal::derive(move || {
        let labels: Vec<String> = (0..DAYS).map(|d| format!("d{d}")).collect();
        let marked = days.get().map(|(a, b)| json!([[{ "xAxis": labels[a] }, { "xAxis": labels[b] }]]));
        json!({
            "animation": false,
            "grid": { "left": 70, "right": 20, "top": 20, "bottom": 30 },
            "tooltip": { "trigger": "axis" },
            "brush": { "xAxisIndex": 0, "brushStyle": { "color": "rgba(47,111,219,0.15)" } },
            "xAxis": { "type": "category", "data": labels },
            "yAxis": { "type": "value" },
            "series": [{
                "type": "line", "showSymbol": false, "areaStyle": { "opacity": 0.15 },
                "data": by_day.get().iter().map(|v| (v * 100.0).round() / 100.0).collect::<Vec<_>>(),
                "markArea": { "data": marked.unwrap_or(json!([])), "itemStyle": { "color": "rgba(47,111,219,0.12)" } }
            }]
        })
    });

    let search = RwSignal::new(String::new());
    let possible = Memo::new(move |_| {
        orders.with(|all| all.iter().filter(|o| passes(o, "customer")).map(|o| o.customer).collect::<BTreeSet<_>>())
    });
    let rows = Memo::new(move |_| {
        let needle = search.get().to_lowercase();
        let possible = possible.get();
        let selected = customers.get();
        (0..CUSTOMERS)
            .filter_map(|id| {
                let label = format!("Customer {id:05}");
                if !needle.is_empty() && !label.to_lowercase().contains(&needle) {
                    return None;
                }
                let state = if selected.contains(&id) {
                    State::Selected
                } else if possible.contains(&id) {
                    State::Possible
                } else {
                    State::Excluded
                };
                Some(CustomerRow { id, label, state })
            })
            .collect::<Vec<_>>()
    });

    // --- interactions -----------------------------------------------------------
    let on_bar = Callback::new(move |click: Click| {
        regions.update(|s| {
            let was = s.contains(&click.data_index);
            if !click.additive {
                s.clear();
            }
            if was && click.additive { s.remove(&click.data_index); } else { s.insert(click.data_index); }
        });
    });
    let on_brush = Callback::new(move |(a, b): (usize, usize)| days.set(Some((a.min(b), a.max(b)))));
    let toggle_customer = move |id: u32| {
        customers.update(|s| {
            if !s.remove(&id) {
                s.insert(id);
            }
        });
    };

    let total = Memo::new(move |_| orders.with(|all| all.iter().filter(|o| passes(o, "")).map(|o| o.amount).sum::<f64>()));

    view! {
        <header>
            <h1>"Leptos × ECharts spike"</h1>
            <span class="meta">{move || format!("{} orders · tick {}", orders.with(Vec::len), ticks.get())}</span>
            <button on:click=move |_| live.update(|l| *l = !*l)>{move || if live.get() { "pause" } else { "resume" }}</button>
        </header>
        <div class="chips">
            <span class="kpi">{move || format!("revenue {:.0}", total.get())}</span>
            {move || (!regions.get().is_empty()).then(|| {
                let names: Vec<&str> = regions.get().iter().map(|i| REGIONS[*i]).collect();
                view! { <button class="chip" on:click=move |_| regions.set(BTreeSet::new())>{format!("region: {} ✕", names.join(", "))}</button> }
            })}
            {move || days.get().map(|(a, b)| view! { <button class="chip" on:click=move |_| days.set(None)>{format!("days: d{a}–d{b} ✕")}</button> })}
            {move || (!customers.get().is_empty()).then(|| view! {
                <button class="chip" on:click=move |_| customers.set(BTreeSet::new())>{format!("customers: {} ✕", customers.with(BTreeSet::len))}</button>
            })}
        </div>
        <main>
            <section class="card"><h2>"Revenue by region"</h2><EChart option=bar_option on_click=on_bar height="260px" /></section>
            <section class="card list">
                <h2>"Customers"</h2>
                <input placeholder="search…" prop:value=search on:input=move |ev| search.set(event_target_value(&ev)) />
                <div class="meta">{move || format!("{} shown · {} possible", rows.with(Vec::len), possible.with(BTreeSet::len))}</div>
                <VirtualList
                    items=rows
                    height=230.0
                    key=|row: &CustomerRow| { let mut h = DefaultHasher::new(); (row.id, row.state).hash(&mut h); h.finish() }
                    row=move |row: CustomerRow| {
                        let class = match row.state { State::Selected => "selected", State::Possible => "possible", State::Excluded => "excluded" };
                        view! { <div class=class on:click=move |_| toggle_customer(row.id)>{row.label}</div> }
                    }
                />
            </section>
            <section class="card wide"><h2>"Revenue by day (drag to select a range)"</h2><EChart option=line_option on_brush_x=on_brush height="260px" /></section>
        </main>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

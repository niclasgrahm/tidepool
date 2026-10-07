//! A virtualised list: only the rows in view (plus a margin) exist in the DOM,
//! however long the list is.

use leptos::prelude::*;

const OVERSCAN: usize = 8;

#[component]
pub fn VirtualList<T, F, V, K>(
    #[prop(into)] items: Signal<Vec<T>>,
    row: F,
    /// Identifies a row's content: a row is re-rendered when its key changes.
    key: K,
    #[prop(default = 26.0)] row_height: f64,
    #[prop(default = 320.0)] height: f64,
) -> impl IntoView
where
    T: Clone + Send + Sync + 'static,
    F: Fn(T) -> V + Clone + Send + Sync + 'static,
    V: IntoView + 'static,
    K: Fn(&T) -> u64 + Clone + Send + Sync + 'static,
{
    let scroll_top = RwSignal::new(0.0_f64);

    let window = Memo::new(move |_| {
        let len = items.with(Vec::len);
        let first = ((scroll_top.get() / row_height) as usize).saturating_sub(OVERSCAN);
        let count = (height / row_height).ceil() as usize + 2 * OVERSCAN;
        first..(first + count).min(len)
    });

    view! {
        <div
            class="vlist"
            style:height=format!("{height}px")
            on:scroll=move |ev| scroll_top.set(event_target::<web_sys::Element>(&ev).scroll_top() as f64)
        >
            <div style:height=move || format!("{}px", items.with(Vec::len) as f64 * row_height) style:position="relative">
                <For
                    each=move || {
                        let range = window.get();
                        items.with(|all| range.clone().map(|i| (i, all[i].clone())).collect::<Vec<_>>())
                    }
                    key=move |(i, item)| (*i, key(item))
                    children=move |(i, item)| {
                        let row = row.clone();
                        view! {
                            <div class="vrow" style:top=format!("{}px", i as f64 * row_height) style:height=format!("{row_height}px")>
                                {row(item)}
                            </div>
                        }
                    }
                />
            </div>
        </div>
    }
}

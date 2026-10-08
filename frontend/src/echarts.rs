//! The whole ECharts interop: bindings, and one component that owns a chart
//! (from `spikes/leptos-charts`, decision D10; read its README's pitfalls
//! before changing this). Options are built in `charts.rs`.

use leptos::html::Div;
use leptos::prelude::*;
use serde_json::Value;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[derive(Clone)]
    type ECharts;

    #[wasm_bindgen(js_namespace = echarts, js_name = init)]
    fn init(el: &web_sys::HtmlElement) -> ECharts;

    #[wasm_bindgen(method, js_name = setOption)]
    fn set_option(this: &ECharts, option: &JsValue, opts: &JsValue);

    #[wasm_bindgen(method)]
    fn on(this: &ECharts, event: &str, handler: &js_sys::Function);

    #[wasm_bindgen(method, js_name = dispatchAction)]
    fn dispatch_action(this: &ECharts, action: &JsValue);

    #[wasm_bindgen(method)]
    fn resize(this: &ECharts);

    #[wasm_bindgen(method)]
    fn dispose(this: &ECharts);
}

fn to_js(value: &Value) -> JsValue {
    js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
}

/// Copy the listed keys of an ECharts event into JSON. Event objects hold DOM
/// events and back-references, so they can't be stringified whole.
fn pick(params: &JsValue, keys: &[&str]) -> Value {
    let picked = js_sys::Object::new();
    for key in keys {
        let key = JsValue::from_str(key);
        if let Ok(value) = js_sys::Reflect::get(params, &key)
            && !value.is_undefined()
        {
            let _ = js_sys::Reflect::set(&picked, &key, &value);
        }
    }
    js_sys::JSON::stringify(&picked)
        .ok()
        .and_then(|s| s.as_string())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

/// A clicked data item.
#[derive(Clone, Debug)]
pub struct Click {
    pub data_index: usize,
    /// Ctrl/Cmd held: toggle instead of replace.
    pub additive: bool,
}

/// One ECharts instance bound to a reactive option.
///
/// - `option` is re-applied (merged) whenever it changes: that is the live
///   update path, and ECharts animates between the two states itself.
/// - `on_click` fires for clicks on data items.
/// - `on_brush_x` turns on single-range brushing along the x axis and fires
///   with the (start, end) data indices when a brush ends; the brush is then
///   cleared, since the selection it produced is shown by the data instead.
#[component]
pub fn EChart(
    #[prop(into)] option: Signal<Value>,
    #[prop(optional)] on_click: Option<Callback<Click>>,
    #[prop(optional)] on_brush_x: Option<Callback<(usize, usize)>>,
    #[prop(default = "100%")] height: &'static str,
) -> impl IntoView {
    let node = NodeRef::<Div>::new();
    let chart = StoredValue::new_local(None::<ECharts>);

    Effect::new(move |_| {
        let Some(el) = node.get() else { return };
        let opts = to_js(&serde_json::json!({ "lazyUpdate": true }));
        let instance = chart.get_value().unwrap_or_else(|| {
            let created = init(&el);
            if let Some(callback) = on_click {
                let handler = Closure::<dyn Fn(JsValue)>::new(move |params: JsValue| {
                    let picked = pick(&params, &["dataIndex"]);
                    let event = js_sys::Reflect::get(&params, &"event".into())
                        .and_then(|e| js_sys::Reflect::get(&e, &"event".into()))
                        .unwrap_or(JsValue::UNDEFINED);
                    let modifier = |key: &str| {
                        js_sys::Reflect::get(&event, &key.into()).is_ok_and(|v| v.is_truthy())
                    };
                    callback.run(Click {
                        data_index: usize::try_from(picked["dataIndex"].as_u64().unwrap_or(0))
                            .unwrap_or(0),
                        additive: modifier("ctrlKey") || modifier("metaKey"),
                    });
                });
                created.on("click", handler.as_ref().unchecked_ref());
                handler.forget(); // lives as long as the chart, which owns it
            }
            if let Some(callback) = on_brush_x {
                let inner = created.clone();
                let handler = Closure::<dyn Fn(JsValue)>::new(move |params: JsValue| {
                    let picked = pick(&params, &["areas"]);
                    let range = &picked["areas"][0]["coordRange"];
                    if let (Some(a), Some(b)) = (range[0].as_u64(), range[1].as_u64()) {
                        callback.run((
                            usize::try_from(a).unwrap_or(0),
                            usize::try_from(b).unwrap_or(0),
                        ));
                    }
                    inner.dispatch_action(&to_js(
                        &serde_json::json!({"type": "brush", "areas": []}),
                    ));
                });
                created.on("brushEnd", handler.as_ref().unchecked_ref());
                handler.forget();
            }
            chart.set_value(Some(created.clone()));
            created
        });
        instance.set_option(&to_js(&option.get()), &opts);
        if on_brush_x.is_some() {
            // brushing is a "global cursor" mode; re-arm it after each update
            instance.dispatch_action(&to_js(&serde_json::json!({
                "type": "takeGlobalCursor", "key": "brush",
                "brushOption": { "brushType": "lineX", "brushMode": "single" }
            })));
        }
    });

    // follow the element's size, not the window's: grid and panel layouts
    // resize charts without the window changing
    let observer = StoredValue::new_local(None::<web_sys::ResizeObserver>);
    Effect::new(move |_| {
        let Some(el) = node.get() else { return };
        let on_resize = Closure::<dyn Fn()>::new(move || {
            chart.with_value(|c| {
                if let Some(c) = c {
                    c.resize();
                }
            });
        });
        if let Ok(created) = web_sys::ResizeObserver::new(on_resize.as_ref().unchecked_ref()) {
            created.observe(&el);
            observer.set_value(Some(created));
        }
        on_resize.forget();
    });
    on_cleanup(move || {
        observer.with_value(|o| {
            if let Some(o) = o {
                o.disconnect();
            }
        });
        chart.with_value(|c| {
            if let Some(c) = c {
                c.dispose();
            }
        });
    });

    view! { <div node_ref=node style:height=height style:width="100%"></div> }
}

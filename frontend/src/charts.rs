//! ECharts options for the dashboard's visuals, as plain JSON: what a chart
//! shows is decided here, where `cargo test` reaches it, and `echarts.rs`
//! only hands the result to the library.

use serde_json::{Value, json};

use crate::board::{display, number};

/// The page's colours, for what ECharts draws itself (it can't read CSS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub ink: &'static str,
    pub soft: &'static str,
    pub tide: &'static str,
    pub anemone: &'static str,
    pub sand: &'static str,
}

/// The palette of `style.css`, light or dark.
#[must_use]
pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            ink: "#e2eeeb",
            soft: "#9db5b1",
            tide: "#4fb3a9",
            anemone: "#f07a98",
            sand: "#35504c",
        }
    } else {
        Palette {
            ink: "#14302f",
            soft: "#4d6664",
            tide: "#1f7a73",
            anemone: "#d9466f",
            sand: "#b9c7c3",
        }
    }
}

fn axis_style(p: Palette) -> Value {
    json!({
        "axisLine": { "lineStyle": { "color": p.sand } },
        "axisLabel": { "color": p.soft },
        "splitLine": { "lineStyle": { "color": p.sand, "opacity": 0.4 } },
    })
}

fn merge(mut base: Value, extra: &Value) -> Value {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    base
}

/// A bar chart of one dimension: horizontal past six bars (labels read
/// better), the selected values in the selection colour.
#[must_use]
pub fn bar(rows: &[Vec<Value>], metrics: &[String], selected: &[String], p: Palette) -> Value {
    let labels: Vec<String> = rows
        .iter()
        .map(|r| r.first().map(display).unwrap_or_default())
        .collect();
    let horizontal = rows.len() > 6;
    let series: Vec<Value> = metrics
        .iter()
        .enumerate()
        .map(|(m, name)| {
            let data: Vec<Value> = rows
                .iter()
                .zip(&labels)
                .map(|(row, label)| {
                    let value = row
                        .get(m + 1)
                        .and_then(number)
                        .map_or(Value::Null, |v| json!(v));
                    let colour = if selected.contains(label) {
                        p.anemone
                    } else if m == 0 {
                        p.tide
                    } else {
                        p.soft
                    };
                    json!({ "value": value, "itemStyle": { "color": colour } })
                })
                .collect();
            // the series colour is what the tooltip's marker shows
            let colour = if m == 0 { p.tide } else { p.soft };
            json!({ "type": "bar", "name": name, "data": data, "barMaxWidth": 28, "color": colour })
        })
        .collect();
    let categories = merge(
        json!({ "type": "category", "data": labels, "inverse": horizontal }),
        &axis_style(p),
    );
    let values = merge(json!({ "type": "value" }), &axis_style(p));
    let (x, y) = if horizontal {
        (values, categories)
    } else {
        (categories, values)
    };
    json!({
        "animationDuration": 300,
        "animationDurationUpdate": 300,
        "grid": { "left": 8, "right": 16, "top": 12, "bottom": 8, "containLabel": true },
        "tooltip": { "trigger": "axis", "axisPointer": { "type": "shadow" } },
        "xAxis": x,
        "yAxis": y,
        "series": series,
    })
}

/// A line over a time dimension: one series per metric, the first as an
/// area, a second metric on its own axis (revenue and a count don't share
/// a scale).
#[must_use]
pub fn line(rows: &[Vec<Value>], metrics: &[String], p: Palette) -> Value {
    let labels: Vec<String> = rows
        .iter()
        .map(|r| r.first().map(display).unwrap_or_default())
        .collect();
    let colours = [p.tide, p.soft];
    let series: Vec<Value> = metrics
        .iter()
        .take(2)
        .enumerate()
        .map(|(m, name)| {
            let data: Vec<Value> = rows
                .iter()
                .map(|row| {
                    row.get(m + 1)
                        .and_then(number)
                        .map_or(Value::Null, |v| json!(v))
                })
                .collect();
            let mut series = json!({
                "type": "line",
                "name": name,
                "data": data,
                "yAxisIndex": m,
                "showSymbol": rows.len() <= 60,
                "itemStyle": { "color": colours[m] },
                "lineStyle": { "color": colours[m], "width": 2 },
            });
            if m == 0 {
                series["areaStyle"] = json!({ "color": p.tide, "opacity": 0.12 });
            }
            series
        })
        .collect();
    let axes: Vec<Value> = (0..metrics.len().min(2))
        .map(|m| {
            merge(
                json!({ "type": "value", "position": if m == 0 { "left" } else { "right" } }),
                &axis_style(p),
            )
        })
        .collect();
    json!({
        "animationDuration": 300,
        "animationDurationUpdate": 300,
        "grid": { "left": 8, "right": 8, "top": 28, "bottom": 8, "containLabel": true },
        "legend": { "top": 0, "right": 0, "textStyle": { "color": p.soft } },
        "tooltip": { "trigger": "axis" },
        "brush": { "xAxisIndex": 0, "brushStyle": { "color": "rgba(217,70,111,0.15)", "borderColor": p.anemone } },
        "toolbox": { "show": false },
        "xAxis": merge(json!({ "type": "category", "data": labels, "boundaryGap": false }), &axis_style(p)),
        "yAxis": axes,
        "series": series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<Vec<Value>> {
        (0..n)
            .map(|i| vec![json!(format!("r{i}")), json!(format!("{i}.50"))])
            .collect()
    }

    #[test]
    fn a_few_bars_stand_and_many_lie_down() {
        let p = palette(false);
        let few = bar(&rows(3), &["revenue".into()], &[], p);
        assert_eq!(few["xAxis"]["type"], "category");
        let many = bar(&rows(10), &["revenue".into()], &[], p);
        assert_eq!(many["yAxis"]["type"], "category");
        assert_eq!(many["yAxis"]["inverse"], true);
    }

    #[test]
    fn selected_bars_take_the_selection_colour() {
        let p = palette(false);
        let option = bar(&rows(3), &["revenue".into()], &["r1".into()], p);
        let data = &option["series"][0]["data"];
        assert_eq!(data[0]["itemStyle"]["color"], p.tide);
        assert_eq!(data[1]["itemStyle"]["color"], p.anemone);
        assert_eq!(data[1]["value"], 1.5);
    }

    #[test]
    fn a_second_metric_gets_its_own_axis() {
        let p = palette(true);
        let rows = vec![vec![json!("2026-10-01"), json!("10.00"), json!(3)]];
        let option = line(&rows, &["revenue".into(), "order_count".into()], p);
        assert_eq!(option["yAxis"].as_array().map(Vec::len), Some(2));
        assert_eq!(option["series"][1]["yAxisIndex"], 1);
        assert_eq!(option["series"][1]["data"][0], 3.0);
        assert!(option["series"][1].get("areaStyle").is_none());
    }
}

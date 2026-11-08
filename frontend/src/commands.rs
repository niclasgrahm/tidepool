//! What the command palette (⌘K) offers and how a query ranks it: pages,
//! dashboards, tables, and on a dashboard its filters and visuals. No DOM;
//! `palette.rs` draws it.

use tidepool_protocol::live::DashboardInfo;

use crate::route::{ConfigTab, Route, Tab};

/// Where a command takes you.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A page, by its hash.
    Go(String),
    /// A filter on the open dashboard: its search box gets the focus.
    Filter(String),
    /// A visual on the open dashboard: scrolled into view.
    Visual(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub label: String,
    /// Beside the label, quieter: an id, a field's dataset.
    pub detail: String,
    pub group: &'static str,
    pub target: Target,
}

fn go(group: &'static str, label: &str, detail: &str, route: &Route) -> Command {
    Command {
        label: label.into(),
        detail: detail.into(),
        group,
        target: Target::Go(route.href()),
    }
}

/// Everything the palette can do, in the order it lists them with no
/// query: the open dashboard's own parts first, then dashboards, tables
/// and pages.
#[must_use]
pub fn commands(
    dashboards: &[DashboardInfo],
    open: Option<&str>,
    tables: &[String],
) -> Vec<Command> {
    let mut all = Vec::new();
    if let Some(board) = open.and_then(|id| dashboards.iter().find(|d| d.id == id)) {
        for filter in &board.filters {
            let (dataset, field) = filter.field.split_once('.').unwrap_or(("", &filter.field));
            all.push(Command {
                label: field.to_string(),
                detail: format!("{dataset} · {}", filter.widget),
                group: "Filters",
                target: Target::Filter(filter.field.clone()),
            });
        }
        for visual in &board.visuals {
            let shows = if visual.metrics.is_empty() {
                visual
                    .dimensions
                    .iter()
                    .map(|d| d.field.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                visual.metrics.join(", ")
            };
            all.push(Command {
                label: visual.id.clone(),
                detail: format!("{} · {shows}", visual.visual_type),
                group: "Visuals",
                target: Target::Visual(visual.id.clone()),
            });
        }
    }
    for d in dashboards.iter().filter(|d| Some(d.id.as_str()) != open) {
        all.push(go(
            "Dashboards",
            &d.name,
            &format!("{}.yaml", d.id),
            &Route::Dashboard(d.id.clone()),
        ));
    }
    for t in tables {
        all.push(go("Tables", t, "table", &Route::Table(t.clone())));
    }
    all.extend([
        go("Pages", "Dashboards", "all of them", &Route::Home),
        go("Pages", "New dashboard", "the editor", &Route::NewDashboard),
        go("Pages", "Tables", "model", &Route::Model(Tab::Tables)),
        go(
            "Pages",
            "Relationships",
            "model",
            &Route::Model(Tab::Relationships),
        ),
        go("Pages", "Metrics", "model", &Route::Model(Tab::Metrics)),
        go(
            "Pages",
            "Config files",
            "config",
            &Route::Config(ConfigTab::Edit),
        ),
        go(
            "Pages",
            "Config history",
            "config",
            &Route::Config(ConfigTab::History),
        ),
        go(
            "Pages",
            "Pending changes",
            "config",
            &Route::Config(ConfigTab::Changes),
        ),
        go("Pages", "Status", "server", &Route::Status),
    ]);
    all
}

/// How well `query` (lower case) matches `text`: a prefix best, then the
/// start of a word, then anywhere, then its letters in order. `None` when
/// it doesn't.
fn score(text: &str, query: &str) -> Option<u32> {
    let text = text.to_lowercase();
    if text.starts_with(query) {
        return Some(400);
    }
    if text
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(query))
    {
        return Some(300);
    }
    if text.contains(query) {
        return Some(200);
    }
    let mut letters = text.chars();
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|q| letters.any(|c| c == q))
        .then_some(100)
}

/// The commands matching `query`: groups by their best match, best first
/// within each (ties keep their order). An empty query lists them all.
#[must_use]
pub fn rank(commands: &[Command], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..commands.len()).collect();
    }
    let hits: Vec<(u32, usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let best = match (score(&c.label, &query), score(&c.detail, &query)) {
                (Some(l), Some(d)) => l.max(d / 2),
                (Some(l), None) => l,
                (None, Some(d)) => d / 2,
                (None, None) => return None,
            };
            Some((best, i))
        })
        .collect();
    // whole groups by their best hit, so each heading shows once
    let best = |group: &str| {
        hits.iter()
            .filter(|(_, i)| commands[*i].group == group)
            .map(|(score, i)| (*score, std::cmp::Reverse(*i)))
            .max()
    };
    let mut order: Vec<_> = hits
        .iter()
        .map(|(score, i)| (best(commands[*i].group), *score, *i))
        .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    order.into_iter().map(|(_, _, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use tidepool_protocol::live::{FilterInfo, GridAt, VisualInfo};

    use super::*;

    fn board() -> DashboardInfo {
        let filter = |field: &str, widget: &str| FilterInfo {
            field: field.into(),
            widget: widget.into(),
            timed: false,
            default: None,
            measure: None,
            measure_format: None,
        };
        DashboardInfo {
            id: "sales".into(),
            name: "Sales overview".into(),
            columns: 12,
            row_height: 80,
            filters: vec![
                filter("customers.region", "list"),
                filter("orders.ordered_at", "date_range"),
            ],
            visuals: vec![VisualInfo {
                id: "by_region".into(),
                visual_type: "bar".into(),
                title: None,
                description: None,
                dimensions: vec![],
                metrics: vec!["revenue".into()],
                compare: None,
                on_click: true,
                on_brush: false,
                widget: None,
                formats: std::collections::BTreeMap::new(),
                at: GridAt {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 4,
                },
            }],
        }
    }

    fn other() -> DashboardInfo {
        DashboardInfo {
            id: "ops".into(),
            name: "Operations".into(),
            filters: vec![],
            visuals: vec![],
            ..board()
        }
    }

    #[test]
    fn the_open_dashboard_comes_first() {
        let all = commands(&[board(), other()], Some("sales"), &["orders".into()]);
        let groups: Vec<&str> = all.iter().map(|c| c.group).collect();
        assert_eq!(
            &groups[..5],
            &["Filters", "Filters", "Visuals", "Dashboards", "Tables"]
        );
        assert_eq!(all[0].label, "region");
        assert_eq!(all[0].detail, "customers · list");
        assert_eq!(all[0].target, Target::Filter("customers.region".into()));
        assert_eq!(
            all[3].target,
            Target::Go("#/d/ops".into()),
            "only the other dashboard"
        );
        assert_eq!(all[4].target, Target::Go("#/model/tables/orders".into()));
        // away from a dashboard, there are none of its parts
        let away = commands(&[board(), other()], None, &[]);
        assert_eq!(away[0].group, "Dashboards");
    }

    #[test]
    fn a_query_ranks_prefixes_first() {
        let all = commands(&[board(), other()], Some("sales"), &["orders".into()]);
        let labels = |q: &str| {
            rank(&all, q)
                .into_iter()
                .map(|i| all[i].label.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels("ord").first().map(String::as_str),
            Some("ordered_at")
        );
        assert_eq!(labels("ord")[1], "orders");
        // a word start beats anywhere: "history" is a word of "Config history"
        assert_eq!(
            labels("hist").first().map(String::as_str),
            Some("Config history")
        );
        // letters in order
        assert!(labels("rlshps").contains(&"Relationships".to_string()));
        // the detail matches too, below a label
        assert!(labels("customers").contains(&"region".to_string()));
        // a group is never split: its weaker hits stay under its heading
        let groups: Vec<&str> = rank(&all, "o").into_iter().map(|i| all[i].group).collect();
        let mut runs = groups.clone();
        runs.dedup();
        let mut unique = runs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(runs.len(), unique.len(), "each group once: {groups:?}");
        assert_eq!(labels("zzz"), Vec::<String>::new());
        assert_eq!(
            rank(&all, "  ").len(),
            all.len(),
            "nothing typed lists everything"
        );
    }
}

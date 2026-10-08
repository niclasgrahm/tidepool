//! Where the app is: hash routes, so the server only ever serves
//! `index.html` and a reload lands on the same page.

/// A page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// The dashboard list.
    Home,
    Dashboard(String),
    /// The model browser, on one of its tabs.
    Model(Tab),
    /// One table in the model browser.
    Table(String),
    Status,
    /// The config pages, on one of their tabs.
    Config(ConfigTab),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigTab {
    /// The files, edited, with their plan.
    Edit,
    /// The versions so far.
    History,
    /// Changes waiting for approval.
    Changes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Tables,
    Relationships,
    Metrics,
}

impl Route {
    /// The route of a location hash (`#/d/sales`).
    #[must_use]
    pub fn parse(hash: &str) -> Self {
        let path = hash.trim_start_matches('#').trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        match parts.as_slice() {
            ["d", slug] => Self::Dashboard((*slug).to_string()),
            ["model"] | ["model", "tables"] => Self::Model(Tab::Tables),
            ["model", "relationships"] => Self::Model(Tab::Relationships),
            ["model", "metrics"] => Self::Model(Tab::Metrics),
            ["model", "tables", name] => Self::Table((*name).to_string()),
            ["status"] => Self::Status,
            ["config"] => Self::Config(ConfigTab::Edit),
            ["config", "history"] => Self::Config(ConfigTab::History),
            ["config", "changes"] => Self::Config(ConfigTab::Changes),
            _ => Self::Home,
        }
    }

    /// The hash that leads here.
    #[must_use]
    pub fn href(&self) -> String {
        match self {
            Self::Home => "#/".into(),
            Self::Dashboard(slug) => format!("#/d/{slug}"),
            Self::Model(Tab::Tables) => "#/model/tables".into(),
            Self::Model(Tab::Relationships) => "#/model/relationships".into(),
            Self::Model(Tab::Metrics) => "#/model/metrics".into(),
            Self::Table(name) => format!("#/model/tables/{name}"),
            Self::Status => "#/status".into(),
            Self::Config(ConfigTab::Edit) => "#/config".into(),
            Self::Config(ConfigTab::History) => "#/config/history".into(),
            Self::Config(ConfigTab::Changes) => "#/config/changes".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_routes_go_both_ways() {
        for route in [
            Route::Home,
            Route::Dashboard("sales".into()),
            Route::Model(Tab::Tables),
            Route::Model(Tab::Relationships),
            Route::Model(Tab::Metrics),
            Route::Table("orders".into()),
            Route::Status,
            Route::Config(ConfigTab::Edit),
            Route::Config(ConfigTab::History),
            Route::Config(ConfigTab::Changes),
        ] {
            assert_eq!(Route::parse(&route.href()), route);
        }
    }

    #[test]
    fn anything_else_is_home() {
        assert_eq!(Route::parse(""), Route::Home);
        assert_eq!(Route::parse("#/nowhere/at/all"), Route::Home);
        assert_eq!(Route::parse("#/model"), Route::Model(Tab::Tables));
    }
}

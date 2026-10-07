//! The Tidepool web UI: a client-side-rendered Leptos app.
//!
//! M0 is only the shell: it proves the build, the embedding and the API round
//! trip (`GET /v1/info`, typed with `tidepool-protocol`'s own `ServerInfo`).
//! Dashboards arrive in M6; `spikes/leptos-charts` is the reference for how the
//! charts and the virtualised list are built.

mod api;

use leptos::prelude::*;
use tidepool_protocol::ServerInfo;

/// The line the footer shows for a server.
fn describe(info: &ServerInfo) -> String {
    format!("{} {} · API v{}", info.name, info.version, info.api_version)
}

#[component]
fn App() -> impl IntoView {
    let info = LocalResource::new(|| api::get_json::<ServerInfo>("/v1/info"));

    view! {
        <header>
            <h1>"tidepool"</h1>
        </header>
        <main>
            <p class="lede">"Nothing to look at yet: dashboards arrive in M6."</p>
        </main>
        <footer>
            <Suspense fallback=|| view! { <span class="muted">"connecting…"</span> }>
                {move || info.get().map(|result| match result {
                    Ok(info) => view! { <span class="ok">{describe(&info)}</span> }.into_any(),
                    Err(err) => view! { <span class="error">{format!("server unreachable: {err}")}</span> }.into_any(),
                })}
            </Suspense>
        </footer>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_names_server_version_and_api() {
        let info = ServerInfo {
            name: "tidepool".into(),
            version: "1.2.3".into(),
            api_version: 1,
        };
        assert_eq!(describe(&info), "tidepool 1.2.3 · API v1");
    }
}

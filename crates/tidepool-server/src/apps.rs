//! The MCP App (`ui://tidepool/view`): a view shown live inside an AI app
//! (Claude, `ChatGPT`, VS Code), drawn by the same UI as the server's own
//! pages.
//!
//! The host renders the resource's HTML in a sandboxed frame of its own
//! origin. That HTML is the UI's `index.html` with every asset loaded from
//! the server's public address (declared in the resource's CSP as a
//! resource and connect domain), and a small script that speaks the MCP
//! Apps bridge (JSON-RPC over `postMessage`) to the host. It keeps the
//! tool result that names the view (its id and an embed token, in the
//! result's `_meta`, which the model doesn't see) for the UI to pick up,
//! and, should the UI not start (a host that allows no WebAssembly, or no
//! connection), shows the numbers the tool answered with as tables.

use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::AppState;

/// The resource's URI.
pub(crate) const VIEW_URI: &str = "ui://tidepool/view";

/// The resource's media type: HTML for an MCP App.
pub(crate) const MIME: &str = "text/html;profile=mcp-app";

/// Where people reach the server: `public_url`, or the address the request
/// came to (`X-Forwarded-Proto` and `-Host` from a proxy, or `Host`).
pub(crate) fn origin(state: &AppState, headers: &HeaderMap) -> String {
    let snapshot = state.writer.snapshot();
    if let Some(url) = &snapshot.model.server.public_url {
        return url.trim_end_matches('/').to_string();
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(',').next().unwrap_or(v).trim().to_string())
    };
    let scheme = header("x-forwarded-proto").unwrap_or_else(|| "http".into());
    let host = header("x-forwarded-host")
        .or_else(|| header("host"))
        .unwrap_or_else(|| "127.0.0.1:7070".into());
    format!("{scheme}://{host}")
}

/// `http(s)://host` as the WebSocket origin.
fn socket_origin(origin: &str) -> String {
    origin
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| origin.strip_prefix("http://").map(|r| format!("ws://{r}")))
        .unwrap_or_else(|| origin.to_string())
}

/// The bridge to the host, and the fallback. Plain script on purpose: it
/// runs before (and without) the UI.
const BRIDGE: &str = r#"<script>
(() => {
  let next = 1;
  const waiting = new Map();
  const post = (message) => window.parent.postMessage(Object.assign({ jsonrpc: "2.0" }, message), "*");
  const bridge = {
    request(method, params) {
      const id = next++;
      post({ id, method, params });
      return new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
    },
    notify(method, params) { post({ method, params }); },
    result: null,
  };
  window.tidepoolBridge = bridge;
  window.addEventListener("message", (event) => {
    if (event.source !== window.parent) return;
    const message = event.data || {};
    if (message.id !== undefined && waiting.has(message.id)) {
      const { resolve, reject } = waiting.get(message.id);
      waiting.delete(message.id);
      message.error ? reject(message.error) : resolve(message.result);
      return;
    }
    if (message.method === "ui/notifications/tool-result") {
      bridge.result = message.params || {};
      window.dispatchEvent(new CustomEvent("tidepool-tool-result"));
      setTimeout(fallback, 8000);
    }
  });
  bridge.request("ui/initialize", {
    protocolVersion: "2026-01-26",
    appInfo: { name: "tidepool", version: "__VERSION__" },
    appCapabilities: {},
  }).then(() => bridge.notify("ui/notifications/initialized", {}), () => {});
  // the numbers as tables, when the UI didn't start
  function fallback() {
    if (window.tidepoolStarted || !bridge.result) return;
    const answers = (bridge.result.structuredContent || {}).answers || {};
    const box = document.getElementById("tidepool-fallback");
    for (const [id, answer] of Object.entries(answers)) {
      const table = document.createElement("table");
      const caption = table.createCaption();
      caption.textContent = id;
      const head = table.createTHead().insertRow();
      for (const column of answer.columns || []) head.insertCell().textContent = column.name;
      const body = table.createTBody();
      for (const row of answer.rows || []) {
        const tr = body.insertRow();
        for (const cell of row) tr.insertCell().textContent = cell === null ? "" : String(cell);
      }
      box.appendChild(table);
    }
    box.hidden = false;
  }
})();
</script>"#;

/// The resource's HTML: the UI's `index.html`, loading from `origin`.
pub(crate) fn view_html(state: &AppState, origin: &str) -> String {
    let index = state
        .ui()
        .and_then(|ui| ui.get("index.html"))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let head = format!(
        "<meta name=\"tidepool-origin\" content=\"{origin}\">\
         <meta name=\"tidepool-mode\" content=\"mcp-app\">{}",
        BRIDGE.replace("__VERSION__", env!("CARGO_PKG_VERSION"))
    );
    match index {
        Some(index) => from_index(&index, &head, origin),
        // a server without its UI: the numbers alone
        None => format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">{head}</head>\
             <body>{FALLBACK}</body></html>"
        ),
    }
}

/// Where the tool's numbers go when the UI doesn't start.
const FALLBACK: &str = "<div id=\"tidepool-fallback\" hidden>\
                        <p>The live view couldn't start here; these are its numbers as the \
                        tool answered.</p></div>";

/// The UI's `index.html` with `head` added and every asset loaded from
/// `origin`.
fn from_index(index: &str, head: &str, origin: &str) -> String {
    index
        .replacen("<head>", &format!("<head>{head}"), 1)
        .replace("\"/assets/", &format!("\"{origin}/assets/"))
        .replace("'/assets/", &format!("'{origin}/assets/"))
        // a stylesheet with an integrity hash from another origin needs CORS
        .replace(
            "<link rel=\"stylesheet\" href=",
            "<link rel=\"stylesheet\" crossorigin=\"anonymous\" href=",
        )
        .replacen("<body>", &format!("<body>{FALLBACK}"), 1)
}

/// The resource's `_meta`: where the frame may load from and connect to.
pub(crate) fn view_meta(origin: &str) -> Value {
    json!({
        "ui": {
            "csp": {
                "connectDomains": [origin, socket_origin(origin)],
                "resourceDomains": [origin],
            },
            "prefersBorder": true,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ui_loads_from_the_server_however_it_is_framed() {
        // what trunk writes
        let index = "<!doctype html><html><head><title>tidepool</title>\
            <link rel=\"stylesheet\" href=\"/assets/style-1.css\" integrity=\"sha384-x\"/>\
            <script type=\"module\">import init from '/assets/app-1.js'; \
            await init({ module_or_path: '/assets/app-1_bg.wasm' });</script>\
            <link rel=\"modulepreload\" href=\"/assets/app-1.js\" crossorigin=\"anonymous\">\
            </head><body></body></html>";
        let html = from_index(index, "<meta name=\"x\">", "https://t.example.com");
        assert!(html.contains("<head><meta name=\"x\"><title>"), "{html}");
        assert!(
            !html.contains("\"/assets/") && !html.contains("'/assets/"),
            "{html}"
        );
        assert!(
            html.contains("'https://t.example.com/assets/app-1_bg.wasm'"),
            "{html}"
        );
        assert!(
            html.contains("<link rel=\"stylesheet\" crossorigin=\"anonymous\" href=\"https://t.example.com/assets/style-1.css\""),
            "{html}"
        );
        assert!(
            html.contains("<body><div id=\"tidepool-fallback\" hidden>"),
            "{html}"
        );
    }

    #[test]
    fn the_socket_origin_follows_the_scheme() {
        assert_eq!(
            socket_origin("https://t.example.com"),
            "wss://t.example.com"
        );
        assert_eq!(
            socket_origin("http://127.0.0.1:7070"),
            "ws://127.0.0.1:7070"
        );
    }
}

//! The browser end to end (plan §11, M6): the real binary serving the built
//! UI, headless Chrome driven over its debugging protocol (CDP), and the path a
//! person takes: open the dashboard, click a bar, see the selection applied,
//! see a live update arrive under it, go back.
//!
//! It needs a built UI (`just frontend`) and Chrome, so it only runs when
//! `TIDEPOOL_E2E` is set: `just e2e` does both. `TIDEPOOL_CHROME` points at
//! a browser binary if it isn't in a usual place.
//!
//! Playwright would need a node toolchain, which the project doesn't have
//! (CLAUDE.md); CDP is a WebSocket of JSON, so a few
//! dozen lines of client below do the same job.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

fn free_port() -> SocketAddr {
    match TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()) {
        Ok(address) => address,
        Err(err) => panic!("finding a free port: {err}"),
    }
}

fn http(address: SocketAddr, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status = response.split(' ').nth(1)?.parse().ok()?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())?;
    Some((status, body))
}

/// Post rows and wait until they are visible.
fn post(address: SocketAddr, table: &str, rows: &Value) {
    let path = format!("/v1/tables/{table}/rows?wait=visible");
    match http(address, "POST", &path, &rows.to_string()) {
        Some((200, _)) => {}
        other => panic!("posting to {table}: {other:?}"),
    }
}

/// Kills a child process when dropped, so a failing test leaves nothing
/// running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A copy of the retail example: config changes are written back to it.
fn example_copy() -> tempfile::TempDir {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(err) => panic!("{err}"),
    };
    let from = workspace().join("examples/retail");
    for sub in ["", "tables", "dashboards"] {
        let Ok(entries) = std::fs::read_dir(from.join(sub)) else {
            continue;
        };
        let _ = std::fs::create_dir_all(dir.path().join(sub));
        for entry in entries.flatten() {
            if entry.path().is_file() {
                let _ = std::fs::copy(entry.path(), dir.path().join(sub).join(entry.file_name()));
            }
        }
    }
    dir
}

fn start_server(address: SocketAddr, config: &Path) -> Running {
    let root = workspace();
    let child = Command::new(env!("CARGO_BIN_EXE_tidepool"))
        .args(["serve", "--config"])
        .arg(config)
        .arg("--ui-dir")
        .arg(root.join("frontend/dist"))
        .args(["--listen", &address.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let server = match child {
        Ok(child) => Running(child),
        Err(err) => panic!("starting tidepool: {err}"),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if http(address, "GET", "/readyz", "").is_some_and(|(status, _)| status == 200) {
            return server;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("tidepool didn't become ready on {address}");
}

fn chrome_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("TIDEPOOL_CHROME") {
        return PathBuf::from(path);
    }
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
    ];
    match candidates.iter().map(PathBuf::from).find(|p| p.exists()) {
        Some(path) => path,
        None => panic!("no Chrome found; set TIDEPOOL_CHROME to a Chrome or Chromium binary"),
    }
}

/// Headless Chrome and a CDP connection to one page in it.
struct Browser {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    session: String,
    next_id: u64,
    _chrome: Running,
    _profile: tempfile::TempDir,
}

impl Browser {
    fn launch() -> Self {
        let profile = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(err) => panic!("{err}"),
        };
        let child = Command::new(chrome_binary())
            .args([
                "--headless",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--window-size=1280,900",
            ])
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut chrome = match child {
            Ok(child) => child,
            Err(err) => panic!("starting Chrome: {err}"),
        };
        // Chrome says where its debugging endpoint is on stderr
        let Some(stderr) = chrome.stderr.take() else {
            panic!("no stderr from Chrome");
        };
        let chrome = Running(chrome);
        let mut url = None;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(at) = line.strip_prefix("DevTools listening on ") {
                url = Some(at.trim().to_string());
                break;
            }
        }
        let Some(url) = url else {
            panic!("Chrome didn't print its DevTools address");
        };
        let mut socket = match tungstenite::connect(url.as_str()) {
            Ok((socket, _)) => socket,
            Err(err) => panic!("connecting to Chrome at {url}: {err}"),
        };
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        }
        let mut browser = Self {
            socket,
            session: String::new(),
            next_id: 0,
            _chrome: chrome,
            _profile: profile,
        };
        let target = browser.call("Target.createTarget", &json!({ "url": "about:blank" }));
        let attached = browser.call(
            "Target.attachToTarget",
            &json!({ "targetId": target["targetId"], "flatten": true }),
        );
        match attached["sessionId"].as_str() {
            Some(session) => browser.session = session.to_string(),
            None => panic!("attaching to the page: {attached}"),
        }
        browser
    }

    /// One CDP command; its result. Events arriving meanwhile are
    /// skipped.
    fn call(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if !self.session.is_empty() {
            message["sessionId"] = json!(self.session);
        }
        if let Err(err) = self.socket.send(Message::text(message.to_string())) {
            panic!("sending {method}: {err}");
        }
        loop {
            let text = match self.socket.read() {
                Ok(Message::Text(text)) => text,
                Ok(_) => continue,
                Err(err) => panic!("waiting for {method}: {err}"),
            };
            let Ok(reply) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if reply["id"] == json!(id) {
                if let Some(error) = reply.get("error") {
                    panic!("{method} failed: {error}");
                }
                return reply["result"].clone();
            }
        }
    }

    /// Evaluate an expression in the page (promises are awaited); its value.
    fn eval(&mut self, expression: &str) -> Value {
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        );
        if let Some(exception) = result.get("exceptionDetails") {
            panic!("evaluating `{expression}`: {exception}");
        }
        result["result"]["value"].clone()
    }

    /// Wait until `expression` is truthy in the page; `what` says what was
    /// expected when it never is.
    fn wait_for(&mut self, expression: &str, what: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = Value::Null;
        while Instant::now() < deadline {
            last = self.eval(expression);
            let truthy = match &last {
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::String(s) => !s.is_empty(),
                _ => true,
            };
            if truthy {
                return last;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what} (last: {last})");
    }

    /// A real mouse click at page coordinates, as a person makes one.
    fn click(&mut self, x: f64, y: f64) {
        for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                &json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            );
        }
    }

    /// Click the middle of the element `selector` finds.
    fn click_element(&mut self, selector: &str) {
        let at = self.wait_for(
            &format!(
                "(() => {{ const e = {selector}; if (!e || e.disabled) return null; \
                 e.scrollIntoView({{block: 'center'}}); const r = e.getBoundingClientRect(); \
                 return [r.x + r.width / 2, r.y + r.height / 2]; }})()"
            ),
            &format!("{selector} to be clickable"),
        );
        let (Some(x), Some(y)) = (at[0].as_f64(), at[1].as_f64()) else {
            panic!("no position for {selector}: {at}");
        };
        self.click(x, y);
    }
}

const KPI: &str = "document.querySelector('.visual.kpi .kpi-value')?.textContent";
const CHIPS: &str = "[...document.querySelectorAll('.chip')].map(c => c.textContent).join(' | ')";

#[test]
fn click_a_bar_watch_it_update_go_back() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
    assert!(
        workspace().join("frontend/dist/index.html").exists(),
        "no frontend/dist: run `just frontend` first"
    );
    let address = free_port();
    let config = example_copy();
    let _server = start_server(address, config.path());

    // two regions, one customer and product each, known amounts
    post(
        address,
        "customers",
        &json!([
            {"customer_id": "c-north", "name": "Nora", "region": "north", "segment": null},
            {"customer_id": "c-south", "name": "Sami", "region": "south", "segment": null},
        ]),
    );
    post(
        address,
        "products",
        &json!([{"product_id": "p1", "name": "Kelp", "category": "food", "list_price": "5.00"}]),
    );
    let order = |id: &str, customer: &str, amount: &str| {
        json!({
            "order_id": id, "customer_id": customer, "product_id": "p1", "status": "paid",
            "ordered_at": "2026-10-01T12:00:00Z", "quantity": 1, "amount": amount, "note": null,
        })
    };
    post(
        address,
        "orders",
        &json!([
            order("o1", "c-north", "100.00"),
            order("o2", "c-north", "50.00"),
            order("o3", "c-south", "30.00"),
        ]),
    );

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/d/sales") }),
    );

    // the dashboard opens with everything
    browser.wait_for(
        &format!("{KPI} === '180.00'"),
        "the revenue KPI to show 180.00",
    );
    let bars = "(() => { const el = document.querySelector('.visual.bar .chart > div'); \
                const c = el && echarts.getInstanceByDom(el); \
                return c ? c.getOption().series[0].data.length : 0; })()";
    browser.wait_for(&format!("{bars} === 2"), "two bars in revenue by region");

    // click the north bar: half way up it
    let at = browser.eval(
        "(() => { const el = document.querySelector('.visual.bar .chart > div'); \
         const c = echarts.getInstanceByDom(el); const o = c.getOption(); \
         const i = o.xAxis[0].data.indexOf('north'); const v = o.series[0].data[i].value; \
         const [x, y] = c.convertToPixel({ gridIndex: 0 }, [i, v / 2]); \
         const r = el.getBoundingClientRect(); return [r.x + x, r.y + y]; })()",
    );
    let (Some(x), Some(y)) = (at[0].as_f64(), at[1].as_f64()) else {
        panic!("no position for the north bar: {at}");
    };
    browser.click(x, y);

    // the selection is applied: a chip, the filter list, the KPI
    browser.wait_for(
        &format!("{CHIPS}.includes('north')"),
        "a chip for region north",
    );
    browser.wait_for(
        &format!("{KPI} === '150.00'"),
        "the KPI to show north's 150.00",
    );
    let selected = browser.eval(
        "[...document.querySelectorAll('.filter .value.selected')].map(b => b.textContent).join()",
    );
    assert!(
        selected.as_str().is_some_and(|s| s.starts_with("north")),
        "north selected in the region list, got {selected}"
    );

    // a live update arrives under the selection; the other region's doesn't count
    post(
        address,
        "orders",
        &json!([
            order("o4", "c-north", "25.00"),
            order("o5", "c-south", "1000.00")
        ]),
    );
    browser.wait_for(
        &format!("{KPI} === '175.00'"),
        "the KPI to update to 175.00 live",
    );

    // back: no selection, everything again
    browser.click_element(
        "[...document.querySelectorAll('.selection button')].find(b => b.textContent === 'Back')",
    );
    browser.wait_for(&format!("{CHIPS} === ''"), "the chip to go");
    browser.wait_for(
        &format!("{KPI} === '1,205.00'"),
        "the KPI to show all 1,205.00",
    );
}

/// A time `days` before now, as the API takes it.
fn days_ago(days: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
    tidepool_store::value::format_timestamp(now - days * 86_400_000_000, true)
}

/// Pick `label` in a date range's period list (what choosing an option does:
/// the value set, then `change`).
fn choose_period(browser: &mut Browser, label: &str) {
    let picked = browser.eval(&format!(
        "(() => {{ const s = document.querySelector('.range select'); \
         const o = [...s.options].find(o => o.textContent === '{label}'); \
         if (!o) return false; s.value = o.value; \
         s.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }})()"
    ));
    assert_eq!(picked, json!(true), "no period `{label}` to choose");
}

#[test]
fn a_dashboard_opens_on_its_relative_period_and_changes_it() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
    assert!(
        workspace().join("frontend/dist/index.html").exists(),
        "no frontend/dist: run `just frontend` first"
    );
    let address = free_port();
    let config = example_copy();
    let _server = start_server(address, config.path());
    post(
        address,
        "customers",
        &json!([{"customer_id": "c1", "name": "Nora", "region": "north", "segment": null}]),
    );
    post(
        address,
        "products",
        &json!([{"product_id": "p1", "name": "Kelp", "category": "food", "list_price": "5.00"}]),
    );
    let order = |id: &str, days: i64, amount: &str| {
        json!({
            "order_id": id, "customer_id": "c1", "product_id": "p1", "status": "paid",
            "ordered_at": days_ago(days), "quantity": 1, "amount": amount, "note": null,
        })
    };
    // one order today-ish (an hour back), one three weeks back, one months back
    post(
        address,
        "orders",
        &json!([
            order("o1", 0, "10.00"),
            order("o2", 21, "20.00"),
            order("o3", 200, "40.00"),
        ]),
    );

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/d/sales_recent") }),
    );
    // it opens on its default, the last seven days
    browser.wait_for(
        &format!("{CHIPS}.includes('Last 7 days')"),
        "a chip for the default window",
    );
    browser.wait_for(&format!("{KPI} === '10.00'"), "only this week's order");
    let shown = browser.eval(
        "(() => { const s = document.querySelector('.range select'); \
         return s.options[s.selectedIndex].textContent; })()",
    );
    assert_eq!(shown, json!("Last 7 days"), "the period list shows it");

    choose_period(&mut browser, "Last 30 days");
    browser.wait_for(
        &format!("{CHIPS}.includes('Last 30 days')"),
        "the chip to follow",
    );
    browser.wait_for(&format!("{KPI} === '30.00'"), "three weeks' orders");

    choose_period(&mut browser, "All time");
    browser.wait_for(&format!("{CHIPS} === ''"), "no chip for all time");
    browser.wait_for(&format!("{KPI} === '70.00'"), "every order");
}

#[test]
fn a_table_added_in_the_config_editor_goes_live() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
    let address = free_port();
    let config = example_copy();
    let _server = start_server(address, config.path());
    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/config") }),
    );
    browser.wait_for(
        "document.querySelector('.files .file') !== null",
        "the config files listed",
    );

    // a new file, written in the editor
    browser.click_element("document.querySelector('.new-file input')");
    browser.call(
        "Input.insertText",
        &json!({ "text": "tables/returns.yaml" }),
    );
    browser.click_element(
        "[...document.querySelectorAll('.new-file button')].find(b => b.textContent === 'Add')",
    );
    browser.click_element("document.querySelector('textarea.code-editor')");
    browser.call(
        "Input.dispatchKeyEvent",
        &json!({ "type": "keyDown", "key": "End", "code": "End", "modifiers": 4 }),
    );
    browser.call(
        "Input.insertText",
        &json!({ "text": "  refunded: { type: \"decimal(14,2)\" }\n" }),
    );

    // its plan, then apply
    browser.wait_for(
        "[...document.querySelectorAll('.changes li')].some(l => l.textContent.includes('returns') && l.textContent.includes('safe'))",
        "a safe plan adding the table",
    );
    browser.click_element(
        "[...document.querySelectorAll('.actions button')].find(b => b.textContent === 'Apply')",
    );
    browser.wait_for(
        "document.querySelector('.plan .ok')?.textContent === 'Version 2 is live.'",
        "version 2 live",
    );
    let written =
        std::fs::read_to_string(config.path().join("tables/returns.yaml")).unwrap_or_default();
    assert!(written.contains("refunded"), "written back: {written}");
    match http(address, "GET", "/v1/tables/returns", "") {
        Some((200, _)) => {}
        other => panic!("the new table should be served: {other:?}"),
    }
}

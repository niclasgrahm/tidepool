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
        // what the page showed, so a timeout says why
        let page = self.eval(
            "(() => `${location.href} · ${document.styleSheets.length} sheets · ` + \
             (document.body?.innerText ?? '').replace(/\\s+/g, ' ').slice(0, 400))()",
        );
        panic!("timed out waiting for {what} (last: {last}); the page: {page}");
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

    /// A real drag with the mouse: pressed at `from`, moved in steps,
    /// released at `to`.
    fn drag(&mut self, from: (f64, f64), to: (f64, f64)) {
        let event = |kind: &str, (x, y): (f64, f64), buttons: u8| json!({ "type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1 });
        self.call("Input.dispatchMouseEvent", &event("mouseMoved", from, 0));
        self.call("Input.dispatchMouseEvent", &event("mousePressed", from, 1));
        for step in 1..=8 {
            let t = f64::from(step) / 8.0;
            let at = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            self.call("Input.dispatchMouseEvent", &event("mouseMoved", at, 1));
        }
        self.call("Input.dispatchMouseEvent", &event("mouseReleased", to, 0));
    }

    /// The middle of `selector`'s element once it has stopped moving: a
    /// chart tweens from its last layout for a moment after new data, and a
    /// position read mid-way lands on the wrong mark.
    fn settled(&mut self, selector: &str) -> (f64, f64) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = self.middle(selector);
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let now = self.middle(selector);
            if now == last {
                return now;
            }
            assert!(Instant::now() < deadline, "{selector} never stopped moving");
            last = now;
        }
    }

    /// The middle of the element `selector` finds, once it's there.
    fn middle(&mut self, selector: &str) -> (f64, f64) {
        let at = self.wait_for(
            &format!(
                "(() => {{ const e = {selector}; if (!e) return null; \
                 const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; }})()"
            ),
            &format!("{selector} to be there"),
        );
        match (at[0].as_f64(), at[1].as_f64()) {
            (Some(x), Some(y)) => (x, y),
            _ => panic!("no position for {selector}: {at}"),
        }
    }

    /// A key pressed and released, as a keyboard sends it.
    fn press(&mut self, key: &str, code: u32, shift: bool) {
        let modifiers = if shift { 8 } else { 0 };
        let text = if key == "Enter" { "\r" } else { "" };
        for kind in ["keyDown", "keyUp"] {
            self.call(
                "Input.dispatchKeyEvent",
                &json!({ "type": kind, "key": key, "code": key, "windowsVirtualKeyCode": code,
                         "modifiers": modifiers, "text": if kind == "keyDown" { text } else { "" } }),
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

/// The KPI's whole value: its `title` when a format shortens what it shows
/// (the example's sales KPI is `compact`), else its text.
const KPI: &str =
    "(e => e && (e.title || e.textContent))(document.querySelector('.visual.kpi .kpi-value'))";
/// What the KPI shows, as formatted.
const KPI_SHOWN: &str = "document.querySelector('.visual.kpi .kpi-value')?.textContent";
/// The bars of the first bar chart, in category order (one metric).
const BARS: &str = "[...document.querySelectorAll('.visual.bar .plot .bar')]";

/// The bar whose category label reads `label`.
fn bar_labelled(label: &str) -> String {
    format!(
        "(() => {{ const svg = document.querySelector('.visual.bar .plot'); \
         const i = [...svg.querySelectorAll('.labels text')].findIndex(t => t.textContent === '{label}'); \
         return i < 0 ? null : svg.querySelectorAll('.bar')[i]; }})()"
    )
}
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
    browser.wait_for(
        &format!("{BARS}.length === 2"),
        "two bars in revenue by region",
    );

    // click the north bar, found by its label
    browser.click_element(&bar_labelled("north"));

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
    // the example formats it compact: short on the tile, whole on hover
    browser.wait_for(&format!("{KPI_SHOWN} === '1.2k'"), "the KPI to read 1.2k");
}

/// The width of the first bar chart's plot.
const BAR_WIDTH: &str = "document.querySelector('.visual.bar .plot').getBoundingClientRect().width";

#[test]
fn maximize_a_visual_and_the_selection_stays() {
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
    two_regions(address);

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/d/sales") }),
    );
    browser.wait_for(
        &format!("{BARS}.length === 2"),
        "two bars in revenue by region",
    );
    let small = browser.eval(BAR_WIDTH).as_f64().unwrap_or(0.0);

    // a selection made on the grid
    browser.click_element(&bar_labelled("north"));
    browser.wait_for(
        &format!("{CHIPS}.includes('north')"),
        "a chip for region north",
    );

    // maximized: the bar chart alone and larger, the selection still shown
    // in the bar, the chip and the rail
    browser.click_element("document.querySelector('.visual.bar .maximize')");
    browser.wait_for(
        "document.querySelector('.visual.bar.maximized') \
         && document.querySelector('.visual.kpi').offsetParent === null",
        "the bar chart alone on the grid",
    );
    browser.wait_for(
        &format!("{BAR_WIDTH} > {small} * 1.5"),
        "the maximized chart to be wider",
    );
    browser.wait_for(
        &format!("{}.classList.contains('selected')", bar_labelled("north")),
        "north still selected in the maximized chart",
    );
    browser.wait_for(&format!("{CHIPS}.includes('north')"), "the chip to stay");
    browser.wait_for(
        "[...document.querySelectorAll('.rail .filter .value.selected')].some(b => b.textContent.startsWith('north'))",
        "north still selected in the rail",
    );

    // the rail still selects (a list click adds), and the maximized chart
    // follows
    browser.click_element(
        "[...document.querySelectorAll('.rail .filter .value')].find(b => b.textContent.startsWith('south'))",
    );
    browser.wait_for(
        &format!("{CHIPS}.includes('north, south')"),
        "the chip to say north and south",
    );
    browser.wait_for(
        &format!(
            "document.querySelector('.visual.bar.maximized') && {}?.classList.contains('selected')",
            bar_labelled("south")
        ),
        "south selected in the maximized chart",
    );

    // Escape puts it back, among the others, under the new selection
    browser.press("Escape", 27, false);
    browser.wait_for(
        "!document.querySelector('.visual.maximized') \
         && document.querySelector('.visual.kpi').offsetParent !== null",
        "every visual back on the grid",
    );
    browser.wait_for(
        &format!("{KPI} === '130.00'"),
        "the KPI to show both regions' 130.00",
    );
    browser.wait_for(
        &format!("Math.abs({BAR_WIDTH} - {small}) < 2"),
        "the bar chart back to its size",
    );
}

/// The rows of the first `rows` visual, as their text.
const ROWS: &str =
    "[...document.querySelectorAll('.visual.rows tbody tr')].map(r => r.textContent)";

#[test]
fn search_as_you_type_and_the_rows_follow() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
    let address = free_port();
    let config = example_copy();
    let _server = start_server(address, config.path());
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
    let order = |id: &str, customer: &str, note: Value| {
        json!({
            "order_id": id, "customer_id": customer, "product_id": "p1", "status": "paid",
            "ordered_at": "2026-10-01T12:00:00Z", "quantity": 1, "amount": "1.00", "note": note,
        })
    };
    post(
        address,
        "orders",
        &json!([
            order("o1", "c-north", json!("Leave at the DOOR")),
            order("o2", "c-south", json!("gift wrap")),
            order("o3", "c-south", json!("door code 4417")),
            order("o4", "c-north", Value::Null),
        ]),
    );

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/d/shipping_notes") }),
    );
    browser.wait_for(&format!("{ROWS}.length === 4"), "four orders in the rows");
    browser.wait_for(
        &format!("{ROWS}[0].includes('Nora')"),
        "the newest order first",
    );

    // typed: once typing pauses, a chip, the KPI and the rows follow
    browser.click_element("document.querySelector('[data-field=\"orders.note\"] input')");
    browser.call("Input.insertText", &json!({ "text": "door" }));
    browser.wait_for(
        &format!("{CHIPS}.includes('“door”')"),
        "a chip for the search",
    );
    browser.wait_for(
        &format!("{KPI} === '2'"),
        "two orders with door in the note",
    );
    browser.wait_for(&format!("{ROWS}.length === 2"), "the two rows");
    let marked = browser
        .eval("[...document.querySelectorAll('.visual.rows mark')].map(m => m.textContent).join()");
    assert_eq!(
        marked,
        json!("door,DOOR"),
        "the matches marked as typed, in any case"
    );

    // a click on a bar narrows the search
    browser.click_element(&bar_labelled("south"));
    browser.wait_for(&format!("{ROWS}.length === 1"), "south's one door");

    // back twice: the click, then the whole search, and the box empties
    let back =
        "[...document.querySelectorAll('.selection button')].find(b => b.textContent === 'Back')";
    browser.click_element(back);
    browser.wait_for(
        &format!("!{CHIPS}.includes('south') && {CHIPS}.includes('“door”')"),
        "the click gone, the search kept",
    );
    browser.click_element(back);
    browser.wait_for(&format!("{CHIPS} === ''"), "the search gone at once");
    browser.wait_for(
        "document.querySelector('[data-field=\"orders.note\"] input').value === ''",
        "the search box empty",
    );
    browser.wait_for(&format!("{ROWS}.length === 4"), "every order again");
}

#[test]
#[allow(clippy::too_many_lines)] // one scenario, start to end
fn brush_the_trend_drag_across_bars_and_choose_with_keys() {
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
        &json!([
            {"customer_id": "c-east", "name": "Eda", "region": "east", "segment": null},
            {"customer_id": "c-north", "name": "Nora", "region": "north", "segment": null},
            {"customer_id": "c-south", "name": "Sami", "region": "south", "segment": null},
        ]),
    );
    post(
        address,
        "products",
        &json!([{"product_id": "p1", "name": "Kelp", "category": "food", "list_price": "5.00"}]),
    );
    let order = |id: &str, customer: &str, day: &str, amount: &str| {
        json!({
            "order_id": id, "customer_id": customer, "product_id": "p1", "status": "paid",
            "ordered_at": format!("2026-10-{day}T12:00:00Z"), "quantity": 1, "amount": amount, "note": null,
        })
    };
    // three days; revenue by region sorts north (400), east (20), south (2)
    post(
        address,
        "orders",
        &json!([
            order("o1", "c-north", "01", "100.00"),
            order("o2", "c-east", "02", "20.00"),
            order("o3", "c-north", "03", "300.00"),
            order("o4", "c-south", "03", "2.00"),
        ]),
    );

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/d/sales") }),
    );
    browser.wait_for(&format!("{KPI} === '422.00'"), "every order");

    // drag across the first two days of the trend: their range is chosen
    let dots = "[...document.querySelectorAll('.visual.line .plot .dot.m0')]";
    browser.wait_for(&format!("{dots}.length === 3"), "a dot per day");
    // a drag happens on screen: the trend is below the fold in a small window
    browser.eval("document.querySelector('.visual.line').scrollIntoView({block: 'center'})");
    let first = browser.settled(&format!("{dots}[0]"));
    let second = browser.settled(&format!("{dots}[1]"));
    browser.drag(first, second);
    browser.wait_for(
        &format!("{CHIPS}.includes('2026-10-01 to 2026-10-02')"),
        "a chip for October 1 and 2",
    );
    browser.wait_for(&format!("{KPI} === '120.00'"), "the first two days' orders");
    browser.click_element(
        "[...document.querySelectorAll('.selection button')].find(b => b.textContent === 'Back')",
    );
    browser.wait_for(&format!("{KPI} === '422.00'"), "every order again");

    // drag from the first bar to the second: both regions are chosen
    browser.wait_for(&format!("{BARS}.length === 3"), "a bar per region");
    browser.eval("document.querySelector('.visual.bar').scrollIntoView({block: 'center'})");
    let north = browser.settled(&bar_labelled("north"));
    let east = browser.settled(&bar_labelled("east"));
    browser.drag(north, east);
    browser.wait_for(
        &format!("{CHIPS}.includes('north') && {CHIPS}.includes('east')"),
        "a chip for north and east",
    );
    browser.wait_for(&format!("{KPI} === '420.00'"), "north's and east's orders");
    browser.click_element(
        "[...document.querySelectorAll('.selection button')].find(b => b.textContent === 'Back')",
    );
    browser.wait_for(&format!("{KPI} === '422.00'"), "every order again");

    // the keyboard: into the bar chart, two bars along, Enter
    browser.eval("document.querySelector('.visual.bar .plot').focus()");
    browser.press("ArrowRight", 39, false);
    browser.press("ArrowRight", 39, false);
    browser.wait_for(
        "document.querySelector('.visual.bar .tip')?.textContent.startsWith('east')",
        "the tooltip on the second bar",
    );
    let described = browser.eval(
        "(() => { const s = document.querySelector('.visual.bar .plot'); \
         return document.getElementById(s.getAttribute('aria-activedescendant'))?.getAttribute('aria-label'); })()",
    );
    assert_eq!(
        described,
        json!("east: revenue 20.00"),
        "the active bar, in words"
    );
    browser.press("Enter", 13, false);
    browser.wait_for(&format!("{CHIPS}.includes('east')"), "a chip for east");
    browser.wait_for(&format!("{KPI} === '20.00'"), "east's order");

    // and in the trend: Shift+arrows stretch a range from the first day
    browser.click_element(
        "[...document.querySelectorAll('.selection button')].find(b => b.textContent === 'Back')",
    );
    browser.wait_for(&format!("{KPI} === '422.00'"), "every order again");
    browser.eval("document.querySelector('.visual.line .plot').focus()");
    browser.press("ArrowRight", 39, false);
    browser.press("ArrowRight", 39, true);
    browser.press("ArrowRight", 39, true);
    browser.press("Enter", 13, false);
    browser.wait_for(
        &format!("{CHIPS}.includes('2026-10-01 to 2026-10-03')"),
        "a chip for all three days",
    );
    browser.wait_for(&format!("{KPI} === '422.00'"), "the three days' orders");
}

#[test]
fn open_a_tree_select_a_branch_see_its_path() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
    let address = free_port();
    let config = example_copy();
    let _server = start_server(address, config.path());
    post(
        address,
        "categories",
        &json!([
            {"category": "outdoor", "parent": null, "level": "department"},
            {"category": "water", "parent": "outdoor", "level": "aisle"},
            {"category": "camping", "parent": "outdoor", "level": "aisle"},
            {"category": "kayaks", "parent": "water", "level": "category"},
            {"category": "tents", "parent": "camping", "level": "category"},
        ]),
    );
    post(
        address,
        "customers",
        &json!([{"customer_id": "c1", "name": "Nora", "region": "north", "segment": null}]),
    );
    post(
        address,
        "products",
        &json!([
            {"product_id": "p1", "name": "Kayak", "category": "kayaks", "list_price": "900.00"},
            {"product_id": "p2", "name": "Tent", "category": "tents", "list_price": "300.00"},
        ]),
    );
    let order = |id: &str, product: &str, amount: &str| {
        json!({
            "order_id": id, "customer_id": "c1", "product_id": product, "status": "paid",
            "ordered_at": "2026-10-01T12:00:00Z", "quantity": 1, "amount": amount, "note": null,
        })
    };
    post(
        address,
        "orders",
        &json!([order("o1", "p1", "900.00"), order("o2", "p2", "300.00")]),
    );

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/d/sales") }),
    );
    browser.wait_for(&format!("{KPI} === '1,200.00'"), "the KPI to show 1,200.00");

    // open outdoor, then select its water aisle
    // a node's own text, without its kind beside it
    let label = "(n => [...n.childNodes].filter(c => c.nodeType === 3).map(c => c.textContent).join('').trim())";
    let rows = format!(
        "[...document.querySelectorAll('.filter.tree .node-label')].map({label}).join(' ')"
    );
    browser.wait_for(&format!("{rows} === 'outdoor'"), "the tree's one root");
    browser.click_element("document.querySelector('.filter.tree .twist')");
    browser.wait_for(
        &format!("{rows} === 'outdoor camping water'"),
        "outdoor's aisles under it",
    );
    browser.click_element(
        "[...document.querySelectorAll('.filter.tree .value')].find(b => b.textContent.startsWith('water'))",
    );
    browser.wait_for(
        &format!("{CHIPS}.includes('outdoor › water')"),
        "a chip with the aisle's path",
    );
    browser.wait_for(
        &format!("{KPI} === '900.00'"),
        "the KPI to show water's 900.00",
    );
    let holds = browser.eval(&format!(
        "[...document.querySelectorAll('.filter.tree .value.holds .node-label')].map({label}).join()"
    ));
    assert_eq!(
        holds,
        json!("outdoor"),
        "outdoor marks the selection below it"
    );
}

/// A time `days` before now, as the API takes it.
fn days_ago(days: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
    tidepool_store::value::format_timestamp(now - days * 86_400_000_000, true)
}

/// Pick `label` among a date range's relative periods: open the picker,
/// click the window.
fn choose_period(browser: &mut Browser, label: &str) {
    browser.click_element("document.querySelector('.picker-trigger')");
    browser.click_element(&format!(
        "[...document.querySelectorAll('.picker-preset')]\
         .find(b => b.firstElementChild?.textContent === '{label}')"
    ));
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
    let shown = browser.eval("document.querySelector('.picker-trigger .picker-label').textContent");
    assert_eq!(shown, json!("Last 7 days"), "the picker shows it");

    choose_period(&mut browser, "Last 30 days");
    browser.wait_for(
        &format!("{CHIPS}.includes('Last 30 days')"),
        "the chip to follow",
    );
    browser.wait_for(&format!("{KPI} === '30.00'"), "three weeks' orders");

    choose_period(&mut browser, "All time");
    browser.wait_for(&format!("{CHIPS} === ''"), "no chip for all time");
    browser.wait_for(&format!("{KPI} === '70.00'"), "every order");

    // on the calendar: today, clicked twice, is that one day
    let today =
        "document.querySelector(`.picker-day[data-day='${Math.floor(Date.now() / 864e5)}']`)";
    browser.click_element("document.querySelector('.picker-trigger')");
    browser.click_element(today);
    browser.click_element(today);
    browser.wait_for(
        "!document.querySelector('.picker-pop')",
        "the picker to close",
    );
    browser.wait_for(&format!("{KPI} === '10.00'"), "only today's order");

    // typed: from 25 days back until today, Enter
    browser.click_element("document.querySelector('.picker-trigger')");
    let (from, until) = (&days_ago(25)[..10], &days_ago(0)[..10]);
    browser.eval(&format!(
        "(() => {{ const [from, until] = document.querySelectorAll('.picker-input:not(.picker-time)'); \
         const set = (e, v) => {{ e.value = v; e.dispatchEvent(new Event('input', {{ bubbles: true }})); }}; \
         set(from, '{from}'); set(until, '{until}'); \
         until.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'Enter', bubbles: true }})); }})()"
    ));
    browser.wait_for(&format!("{KPI} === '30.00'"), "the typed range's orders");
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

/// The `n`th visual in the dashboard editor, as a selector.
fn tile(n: usize) -> String {
    format!("document.querySelectorAll('.canvas .tile')[{n}]")
}

/// Where the `n`th visual in the editor sits: its grid placement.
fn tile_at(browser: &mut Browser, n: usize) -> Value {
    browser.eval(&format!("{}?.getAttribute('style')", tile(n)))
}

/// In the editor's picker: tick these fields and metrics, then add.
fn add_visual(browser: &mut Browser, names: &[&str]) {
    browser.click_element(
        "[...document.querySelectorAll('.designer-bar button')].find(b => b.textContent === 'Add visual')",
    );
    for name in names {
        browser.click_element(&format!(
            "document.querySelector('.picker input[aria-label=\"{name}\"]')"
        ));
    }
    browser.click_element(
        "[...document.querySelectorAll('.picker button')].find(b => b.textContent === 'Add')",
    );
}

/// Two customers in two regions, one product, an order each.
fn two_regions(address: SocketAddr) {
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
            order("o2", "c-south", "30.00")
        ]),
    );
}

/// In the editor: the second visual resized, refused a place on the
/// first, then moved into free cells.
fn arrange_the_bar(browser: &mut Browser) {
    // a cell is a twelfth of the grid, gap included
    let cell = browser.eval(
        "(() => { const r = document.querySelector('.canvas').getBoundingClientRect(); \
         return (r.width + 16) / 12; })()",
    );
    let Some(cell) = cell.as_f64() else {
        panic!("no cell width: {cell}");
    };
    // resized by its corner, two cells narrower and a row shorter
    let corner = browser.middle(&format!("{}.querySelector('.handle')", tile(1)));
    browser.drag(corner, (corner.0 - 2.0 * cell, corner.1 - 96.0));
    browser.wait_for(
        &format!(
            "{}.getAttribute('style') === 'grid-column: 4 / span 4; grid-row: 1 / span 3;'",
            tile(1)
        ),
        "the bar 4 wide and 3 tall",
    );
    // dropped on the filter, it stays where it was
    let middle = browser.middle(&tile(1));
    browser.drag(middle, (middle.0 - 2.0 * cell, middle.1));
    assert_eq!(
        tile_at(browser, 1),
        json!("grid-column: 4 / span 4; grid-row: 1 / span 3;"),
        "visuals can't overlap"
    );
    // moved into free cells, it snaps to them
    browser.drag(middle, (middle.0 + 3.0 * cell + 10.0, middle.1 + 96.0));
    browser.wait_for(
        &format!(
            "{}.getAttribute('style') === 'grid-column: 7 / span 4; grid-row: 2 / span 3;'",
            tile(1)
        ),
        "the bar three cells right and a row down",
    );
}

#[test]
fn a_dashboard_made_in_the_editor_goes_live() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
    let address = free_port();
    let config = example_copy();
    let _server = start_server(address, config.path());
    two_regions(address);

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Emulation.setDeviceMetricsOverride",
        &json!({ "width": 1400, "height": 1000, "deviceScaleFactor": 1, "mobile": false }),
    );
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/") }),
    );
    browser.click_element("document.querySelector('a.button.primary')");
    browser.click_element("document.querySelector('.designer-bar input')");
    browser.call("Input.insertText", &json!({ "text": "Team view" }));

    // one dimension alone is a filter; with a metric, a table
    add_visual(&mut browser, &["customers.region"]);
    browser.wait_for(
        &format!(
            "{}?.querySelector('.type-tag').textContent === 'Filter'",
            tile(0)
        ),
        "a filter",
    );
    add_visual(&mut browser, &["customers.region", "revenue"]);
    browser.wait_for(
        &format!(
            "{}?.querySelector('.type-tag').textContent === 'Table'",
            tile(1)
        ),
        "a table",
    );
    assert_eq!(
        tile_at(&mut browser, 1),
        json!("grid-column: 4 / span 6; grid-row: 1 / span 4;"),
        "placed beside the filter"
    );
    // then a bar
    browser.click_element(
        "[...document.querySelectorAll('.types .type')].find(b => b.textContent === 'Bar')",
    );
    browser.wait_for(
        &format!(
            "{}.querySelector('.type-tag').textContent === 'Bar'",
            tile(1)
        ),
        "a bar",
    );

    arrange_the_bar(&mut browser);

    // saved, it's a dashboard
    browser.click_element(
        "[...document.querySelectorAll('.designer-bar button')].find(b => b.textContent === 'Save')",
    );
    browser.wait_for("location.hash === '#/d/team_view'", "the new dashboard");
    browser.wait_for(
        "document.querySelector('.visual.bar .plot .bar') !== null",
        "its bar chart",
    );
    let written = std::fs::read_to_string(config.path().join("dashboards/team_view.yaml"))
        .unwrap_or_default();
    assert!(
        written.starts_with("name: Team view\n")
            && written.contains("    type: filter\n    dimensions: [customers.region]\n")
            && written.contains("    type: bar\n")
            && written.contains("at: { x: 6, y: 1, w: 4, h: 3 }"),
        "written back: {written}"
    );

    // the filter on the grid selects
    browser.click_element(
        "[...document.querySelectorAll('.filter-visual .value')].find(b => b.textContent.startsWith('north'))",
    );
    browser.wait_for(
        &format!("{CHIPS}.includes('north')"),
        "a chip for the region",
    );
}

#[test]
fn the_relationship_diagram_counts_live_and_opens_a_table() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
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
    let order = |id: &str| {
        json!({
            "order_id": id, "customer_id": "c1", "product_id": "p1", "status": "paid",
            "ordered_at": "2026-10-01T12:00:00Z", "quantity": 1, "amount": "5.00", "note": null,
        })
    };
    post(address, "orders", &json!([order("o1"), order("o2")]));
    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/model/relationships") }),
    );
    browser.wait_for(
        "document.querySelectorAll('.er-card').length === 4 && document.querySelectorAll('.er-line').length === 3",
        "a card per table and a line per relationship",
    );
    let orders =
        "document.querySelector('.er-card[href=\"#/model/tables/orders\"] .er-count')?.textContent";
    browser.wait_for(&format!("{orders}?.startsWith('2rows')"), "two orders");

    // a line, chosen: it says what a selection on either side does
    browser.click_element(
        "[...document.querySelectorAll('.er-line')].find(l => l.textContent.includes('product_category'))?.querySelector('.er-hit')",
    );
    browser.wait_for(
        "document.querySelector('.er-detail')?.textContent.includes('Selecting categories filters products.')",
        "the chosen line's detail",
    );

    // the counts follow the data, without a reload
    post(address, "orders", &json!([order("o3")]));
    browser.wait_for(&format!("{orders}?.startsWith('3rows')"), "three orders");
    browser.wait_for(
        "document.querySelector('.er-line.chosen') !== null",
        "the chosen line still chosen after a refresh",
    );

    // a card opens its table
    browser
        .click_element("document.querySelector('.er-card[href=\"#/model/tables/customers\"] h3')");
    browser.wait_for(
        "location.hash === '#/model/tables/customers'",
        "the customers table",
    );
}

#[test]
fn a_view_an_agent_made_goes_live_and_is_saved_as_a_dashboard() {
    if std::env::var_os("TIDEPOOL_E2E").is_none() {
        eprintln!("skipped: needs a built UI and Chrome; run `just e2e`");
        return;
    }
    let address = free_port();
    let config = example_copy();
    let _server = start_server(address, config.path());
    two_regions(address);
    let view = json!({"name": "Regions now", "visuals": [
        {"id": "regions", "type": "bar", "dimensions": ["customers.region"],
         "metrics": ["revenue"], "on_click": "select"}
    ]});
    let id = match http(address, "POST", "/v1/views", &view.to_string()) {
        Some((200, body)) => match serde_json::from_str::<Value>(&body) {
            Ok(made) => made["id"].as_str().unwrap_or_default().to_string(),
            Err(err) => panic!("the view's answer: {err}: {body}"),
        },
        other => panic!("making a view: {other:?}"),
    };

    let mut browser = Browser::launch();
    browser.call("Page.enable", &json!({}));
    browser.call(
        "Page.navigate",
        &json!({ "url": format!("http://{address}/#/v/{id}") }),
    );
    browser.wait_for(&format!("{BARS}.length === 2"), "the view's two bars");
    browser.wait_for(
        "document.querySelector('.toolbar h1')?.textContent === 'Regions now'",
        "the view's name in the toolbar",
    );
    browser.wait_for(
        "document.querySelector('.toolbar .tag')?.textContent === 'view'",
        "the toolbar to say it's a view",
    );
    // it selects like a dashboard
    browser.click_element(&bar_labelled("north"));
    browser.wait_for(
        &format!("{CHIPS}.includes('north')"),
        "a chip for region north",
    );
    // and saving it makes a dashboard file of it
    browser.eval("window.prompt = () => 'regions_now'");
    browser.click_element("document.querySelector('.save-view')");
    browser.wait_for(
        "location.hash === '#/d/regions_now'",
        "the saved dashboard to open",
    );
    // the session's selection goes with it: north alone
    browser.wait_for(
        &format!("{BARS}.length === 1"),
        "the saved dashboard's bar, under the selection",
    );
    assert!(
        config.path().join("dashboards/regions_now.yaml").exists(),
        "the view written to the config directory"
    );
}

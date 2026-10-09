//! The quick-start: `tidepool init` writes a project to start from (the
//! retail example), and `tidepool feed` keeps a server busy with sample
//! data for it, so a live dashboard is a few commands away.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The project `init` writes: the retail example, compiled in.
const FILES: [(&str, &str); 7] = [
    (
        "model.ossie.yaml",
        include_str!("../../../examples/retail/model.ossie.yaml"),
    ),
    (
        "tables/orders.yaml",
        include_str!("../../../examples/retail/tables/orders.yaml"),
    ),
    (
        "tables/customers.yaml",
        include_str!("../../../examples/retail/tables/customers.yaml"),
    ),
    (
        "tables/products.yaml",
        include_str!("../../../examples/retail/tables/products.yaml"),
    ),
    (
        "dashboards/sales.yaml",
        include_str!("../../../examples/retail/dashboards/sales.yaml"),
    ),
    (
        "dashboards/sales_recent.yaml",
        include_str!("../../../examples/retail/dashboards/sales_recent.yaml"),
    ),
    ("tidepool.yaml", SERVER),
];

/// A new project's server settings: data kept next to the config.
const SERVER: &str = "# How the server runs (plan §3.2). Everything has a default; these are
# here to be changed.
listen: 127.0.0.1:7070
data_dir: data                    # relative to this directory; delete it to start over
epoch_interval: 200ms             # how often new data becomes visible, at most
durability: fsync                 # acknowledge writes once they're on disk
# changes made in the UI or over the API are written back to these files,
# and committed when this directory is a git repository
config_writeback: { enabled: true, git_commit: true }
";

/// `tidepool init <dir>`.
pub fn init(dir: &Path) -> ExitCode {
    if dir.exists() && std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some()) {
        eprintln!(
            "error: {} isn't empty; init writes a new project",
            dir.display()
        );
        return ExitCode::FAILURE;
    }
    for (path, text) in FILES {
        let to = dir.join(path);
        let written = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&to, text));
        if let Err(err) = written {
            eprintln!("error: writing {}: {err}", to.display());
            return ExitCode::FAILURE;
        }
    }
    let shown = dir.display();
    println!(
        "Created {shown}: three tables (orders, customers, products), a model with metrics, \
         and a sales dashboard.\n\n\
         Next:\n  \
         tidepool serve --config {shown}     # then open http://127.0.0.1:7070\n  \
         tidepool feed                      # in another terminal: sample orders, live\n\n\
         Edit the files (or use Config in the UI) and the server follows. \
         `tidepool validate --config {shown}` checks them."
    );
    ExitCode::SUCCESS
}

/// A small, fast, good-enough generator (`SplitMix64`).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const REGIONS: [&str; 6] = ["north", "south", "east", "west", "central", "islands"];
const CATEGORIES: [&str; 8] = [
    "kayaks", "paddles", "dry bags", "wetsuits", "maps", "lights", "tents", "stoves",
];
const CUSTOMERS: u64 = 500;
const PRODUCTS: u64 = 60;

/// `YYYY-MM-DDTHH:MM:SSZ` for seconds since the epoch (UTC).
fn rfc3339(seconds: i64) -> String {
    // days → civil date (Howard Hinnant)
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn price(product: u64) -> u64 {
    // cents, stable per product
    500 + (product * 7919) % 30_000
}

/// One order at `at`, as an NDJSON line.
fn order(rng: &mut Rng, id: &str, at: i64) -> String {
    let customer = rng.below(CUSTOMERS);
    let product = rng.below(PRODUCTS);
    let quantity = 1 + rng.below(4);
    let cents = price(product) * quantity;
    let status = match rng.below(20) {
        0 => "returned",
        1..=3 => "shipped",
        _ => "paid",
    };
    format!(
        r#"{{"order_id":"{id}","customer_id":"c{customer}","product_id":"p{product}","status":"{status}","ordered_at":"{}","quantity":{quantity},"amount":"{}.{:02}","note":null}}"#,
        rfc3339(at),
        cents / 100,
        cents % 100
    )
}

/// Where the server is: `host:port` from `http://host:port`.
fn address(server: &str) -> Result<String, String> {
    let rest = server
        .strip_prefix("http://")
        .ok_or("the server is an http:// URL (the feeder doesn't speak TLS)")?;
    let host = rest.split('/').next().unwrap_or(rest);
    Ok(if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    })
}

/// POST NDJSON to `path`; the status. Retries while the server is busy.
fn post(address: &str, token: Option<&str>, path: &str, body: &str) -> Result<u16, String> {
    loop {
        let mut stream = TcpStream::connect(address).map_err(|e| format!("{address}: {e}"))?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/x-ndjson\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        let status: u16 = response
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or("no HTTP response")?;
        if status == 503 {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        if status != 200 {
            let body = response.split_once("\r\n\r\n").map_or("", |(_, b)| b);
            return Err(format!("{path} answered {status}: {body}"));
        }
        return Ok(status);
    }
}

/// `tidepool feed`: the example's customers and products, ninety days of
/// orders, then `rate` new orders a second for `seconds` (or until
/// stopped).
pub fn feed(server: &str, rate: u64, token: Option<&str>, seconds: Option<u64>) -> ExitCode {
    match run_feed(server, rate, token, seconds) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_feed(
    server: &str,
    rate: u64,
    token: Option<&str>,
    seconds: Option<u64>,
) -> Result<(), String> {
    let address = address(server)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let mut rng = Rng(u64::try_from(now).unwrap_or(1));
    let customers: Vec<String> = (0..CUSTOMERS)
        .map(|c| {
            format!(
                r#"{{"customer_id":"c{c}","name":"Customer {c}","region":"{}","segment":null}}"#,
                REGIONS[usize::try_from(c % 6).unwrap_or(0)]
            )
        })
        .collect();
    post(
        &address,
        token,
        "/v1/tables/customers/rows",
        &customers.join("\n"),
    )?;
    let products: Vec<String> = (0..PRODUCTS)
        .map(|p| {
            let cents = price(p);
            format!(
                r#"{{"product_id":"p{p}","name":"Product {p}","category":"{}","list_price":"{}.{:02}"}}"#,
                CATEGORIES[usize::try_from(p % 8).unwrap_or(0)],
                cents / 100,
                cents % 100
            )
        })
        .collect();
    post(
        &address,
        token,
        "/v1/tables/products/rows",
        &products.join("\n"),
    )?;

    // ninety days back, more on weekdays, growing a little: a trend to look at
    let run = format!("{now:x}");
    let mut n = 0u64;
    let mut back = Vec::new();
    for day in (1..=90i64).rev() {
        let start = now - day * 86_400;
        let weekday = (start.div_euclid(86_400) + 4).rem_euclid(7);
        let base = if weekday >= 5 { 120 } else { 200 };
        let count = base + (90 - day) * 2 + i64::try_from(rng.below(60)).unwrap_or(0);
        for _ in 0..count {
            n += 1;
            let at = start + i64::try_from(rng.below(86_400)).unwrap_or(0);
            back.push(order(&mut rng, &format!("o{run}-{n}"), at));
        }
    }
    for chunk in back.chunks(5_000) {
        post(&address, token, "/v1/tables/orders/rows", &chunk.join("\n"))?;
    }
    println!(
        "fed {} customers, {} products and {} orders from the last 90 days to {server}",
        CUSTOMERS,
        PRODUCTS,
        back.len()
    );
    println!("now {rate} orders a second; stop with ctrl-c");
    let started = Instant::now();
    let per_tick = (rate / 5).max(1);
    let mut tick = Instant::now();
    while seconds.is_none_or(|s| started.elapsed() < Duration::from_secs(s)) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let lines: Vec<String> = (0..per_tick)
            .map(|_| {
                n += 1;
                order(&mut rng, &format!("o{run}-{n}"), now)
            })
            .collect();
        post(&address, token, "/v1/tables/orders/rows", &lines.join("\n"))?;
        tick += Duration::from_millis(200);
        if let Some(wait) = tick.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_783_000_000), "2026-07-02T13:46:40Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn the_server_address_comes_from_its_url() {
        assert_eq!(
            address("http://127.0.0.1:7070"),
            Ok("127.0.0.1:7070".into())
        );
        assert_eq!(address("http://tidepool/"), Ok("tidepool:80".into()));
        assert!(address("https://x").is_err());
    }
}

"""How fresh is the dashboard? Event time to queryable, measured.

Asks Tidepool for `latest_order` (the newest `orders.ordered_at` it can
see) every 50 ms. Each time it moves, the order it moved to was placed
`now - latest_order` ago: that is the whole path, publisher to Kayak to
Tidepool's next epoch, as a query sees it. (Dashboards are pushed from the
same epoch, so they see it then too, plus the frame's trip.) Also times
each query.

  python3 freshness.py [--url http://localhost:7070] [--seconds 30]

The publisher's clock is the container's and this one is the host's; with
Docker Desktop they agree to a few milliseconds.
"""

import argparse
import json
import statistics
import time
import urllib.request
from datetime import datetime


def latest(url: str) -> tuple[float | None, float]:
    body = json.dumps({"metrics": ["latest_order"]}).encode()
    request = urllib.request.Request(
        f"{url}/v1/query", body, {"content-type": "application/json"}, method="POST"
    )
    started = time.time()
    with urllib.request.urlopen(request, timeout=5) as reply:
        answer = json.load(reply)
    took = time.time() - started
    value = answer["rows"][0][0] if answer.get("rows") else None
    if value is None:
        return None, took
    return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp(), took


def summary(name: str, values: list[float]) -> str:
    if not values:
        return f"{name}: no samples"
    ms = sorted(v * 1000 for v in values)
    p = lambda q: ms[min(len(ms) - 1, int(q * len(ms)))]  # noqa: E731
    return (
        f"{name}: p50 {p(0.5):.0f} ms, p95 {p(0.95):.0f} ms, max {ms[-1]:.0f} ms "
        f"(mean {statistics.fmean(ms):.0f}, n={len(ms)})"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description="Event time to queryable, measured.")
    parser.add_argument("--url", default="http://localhost:7070")
    parser.add_argument("--seconds", type=float, default=30)
    args = parser.parse_args()
    url = args.url.rstrip("/")

    seen = None
    lags, queries = [], []
    end = time.time() + args.seconds
    while time.time() < end:
        newest, took = latest(url)
        queries.append(took)
        now = time.time()
        if newest is not None and seen is not None and newest > seen:
            lags.append(now - newest)
        if newest is not None:
            seen = newest
        time.sleep(max(0.0, 0.05 - took))
    print(summary("event → queryable", lags))
    print(summary("query", queries))


if __name__ == "__main__":
    main()

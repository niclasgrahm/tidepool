"""Northwind's orders, replayed as live events.

Every order in orders.ndjson (Northwind's 830, oldest first) is placed again
now, with its lines, and ships later: as many seconds later as it shipped
days later in Northwind, times DAY_SECONDS. An order that shipped after its
required date ships `late`; the few that never shipped stay open. When the
history runs out it starts again, with new order ids.

Events go to Kayak's `http` inputs (one post per pipeline per flush), and
from there into Tidepool. Standard library only.

Environment:
  KAYAK_URL           default http://kayak:6767
  ORDERS_PER_SECOND   default 2 (exponential gaps, so it comes in bursts)
  DAY_SECONDS         default 2: one Northwind day of shipping lag
  FLUSH_MS            default 100: how long events are held to make a batch
"""

import heapq
import json
import os
import random
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from decimal import ROUND_HALF_UP, Decimal
from pathlib import Path

KAYAK = os.environ.get("KAYAK_URL", "http://kayak:6767").rstrip("/")
RATE = float(os.environ.get("ORDERS_PER_SECOND", "2"))
DAY = float(os.environ.get("DAY_SECONDS", "2"))
FLUSH = float(os.environ.get("FLUSH_MS", "100")) / 1000
HISTORY = Path(__file__).with_name("orders.ndjson")
CENT = Decimal("0.01")


def stamp(at: float) -> str:
    return datetime.fromtimestamp(at, timezone.utc).isoformat(timespec="milliseconds")


def days(later: str, earlier: str) -> int:
    return (datetime.fromisoformat(later) - datetime.fromisoformat(earlier)).days


def post(pipeline: str, messages: list) -> None:
    """One batch into one pipeline; waits for Kayak rather than dropping it."""
    body = json.dumps(messages).encode()
    url = f"{KAYAK}/api/pipelines/{pipeline}/messages"
    pause = 0.5
    while True:
        request = urllib.request.Request(
            url, body, {"content-type": "application/json"}, method="POST"
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as reply:
                if reply.status == 202:
                    return
                problem = f"HTTP {reply.status}"
        except urllib.error.HTTPError as err:
            problem = f"HTTP {err.code}: {err.read()[:200]!r}"
        except (urllib.error.URLError, OSError) as err:
            problem = str(err)
        print(f"{pipeline}: {problem}; trying again in {pause:.1f}s", flush=True)
        time.sleep(pause)
        pause = min(pause * 2, 5)


def main() -> None:
    history = [json.loads(line) for line in HISTORY.read_text().splitlines()]
    print(
        f"replaying {len(history)} orders at {RATE}/s into {KAYAK} "
        f"(a shipping day is {DAY}s)",
        flush=True,
    )
    shipments = []  # (when, sequence, order row)
    orders, lines = [], []
    placed = shipped = sent_lines = 0
    rounds = 0
    index = 0
    next_order = time.time()
    last_flush = last_report = time.time()

    while True:
        now = time.time()
        while now >= next_order:
            source = history[index]
            order_id = str(source["order_id"] + 1000 * rounds)
            at = next_order
            order = {
                "order_id": order_id,
                "customer_id": source["customer_id"],
                "employee_id": source["employee_id"],
                "shipper_id": source["shipper_id"],
                "ordered_at": stamp(at),
                "required_at": stamp(
                    at + DAY * days(source["required_date"], source["order_date"])
                ),
                "shipped_at": None,
                "status": "open",
                "days_to_ship": None,
                "freight": source["freight"],
                "ship_city": source["ship_city"],
                "ship_country": source["ship_country"],
            }
            orders.append(order)
            placed += 1
            for number, line in enumerate(source["lines"], start=1):
                price = Decimal(line["unit_price"])
                discount = Decimal(str(line["discount"]))
                amount = (price * line["quantity"] * (1 - discount)).quantize(
                    CENT, ROUND_HALF_UP
                )
                lines.append(
                    {
                        "order_id": order_id,
                        "line": number,
                        "product_id": line["product_id"],
                        "ordered_at": order["ordered_at"],
                        "quantity": line["quantity"],
                        "unit_price": line["unit_price"],
                        "discount": line["discount"],
                        "amount": str(amount),
                    }
                )
            if source["shipped_date"]:
                lag = days(source["shipped_date"], source["order_date"])
                late = source["shipped_date"] > source["required_date"]
                ships = dict(order, days_to_ship=lag, status="late" if late else "shipped")
                heapq.heappush(shipments, (at + DAY * lag, placed, ships))

            index += 1
            if index == len(history):
                index = 0
                rounds += 1
            next_order += random.expovariate(RATE)

        while shipments and shipments[0][0] <= now:
            when, _, order = heapq.heappop(shipments)
            order["shipped_at"] = stamp(when)
            orders.append(order)
            shipped += 1

        if now - last_flush >= FLUSH:
            # the header first, so a line rarely arrives before its order
            if orders:
                post("orders", orders)
            if lines:
                post("order_lines", lines)
                sent_lines += len(lines)
            orders, lines = [], []
            last_flush = now

        if now - last_report >= 10:
            print(
                f"placed {placed} orders ({sent_lines} lines), shipped {shipped}, "
                f"{len(shipments)} waiting to ship, round {rounds + 1}",
                flush=True,
            )
            last_report = now

        wake = min(next_order, last_flush + FLUSH, shipments[0][0] if shipments else next_order)
        time.sleep(max(0.0, min(wake - time.time(), FLUSH)))


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)

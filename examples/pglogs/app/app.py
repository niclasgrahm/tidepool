"""A small shop working against Postgres, badly enough to be interesting.

Most of what it does is ordinary: customers sign up, place orders, pay,
read their history. Mixed in, at random, are the things that go wrong in
real systems, each of which makes Postgres log something worth finding:
duplicate sign-ups (unique_violation), overdrawn balances
(check_violation), a typo in a report (syntax_error), a table that was
dropped (undefined_table), two payments locking the same accounts in
opposite orders (deadlock_detected), a slow report hitting
statement_timeout, a lock wait hitting lock_timeout, a wrong password, a
database that doesn't exist, and notices and warnings raised on purpose.

The app logs too, one JSON object per line in APP_LOG, which the shipper
sends along with Postgres' own lines.

Environment:
  DATABASE_URL        default postgresql://shop:shop@postgres:5432/shop
  ACTIONS_PER_SECOND  default 8
  APP_LOG             default /applogs/app.jsonl
"""

import json
import os
import random
import threading
import time
from datetime import datetime, timezone

import psycopg

URL = os.environ.get("DATABASE_URL", "postgresql://shop:shop@postgres:5432/shop")
RATE = float(os.environ.get("ACTIONS_PER_SECOND", "8"))
APP_LOG = os.environ.get("APP_LOG", "/applogs/app.jsonl")
NAMES = ["ada", "bo", "cy", "dee", "eve", "flo", "gus", "hal", "ivy", "jo"]
ITEMS = ["kettle", "lamp", "mug", "notebook", "pen", "rug", "scarf", "teapot"]

log_lock = threading.Lock()


def log(level: str, message: str, **fields) -> None:
    """One line of the app's own log."""
    line = {
        "time": datetime.now(timezone.utc).isoformat(timespec="milliseconds"),
        "level": level,
        "message": message,
        **fields,
    }
    with log_lock, open(APP_LOG, "a", encoding="utf-8") as out:
        out.write(json.dumps(line) + "\n")


def connect(application: str, **options) -> psycopg.Connection:
    return psycopg.connect(URL, autocommit=True, application_name=application, **options)


def setup() -> None:
    """The schema, and a few customers to start with."""
    while True:
        try:
            with connect("setup") as db:
                db.execute(
                    """
                    CREATE TABLE IF NOT EXISTS customers (
                        id serial PRIMARY KEY,
                        email text UNIQUE NOT NULL,
                        balance numeric(12, 2) NOT NULL DEFAULT 100 CHECK (balance >= 0)
                    );
                    CREATE TABLE IF NOT EXISTS orders (
                        id bigserial PRIMARY KEY,
                        customer_id int NOT NULL REFERENCES customers (id),
                        item text NOT NULL,
                        amount numeric(12, 2) NOT NULL,
                        placed_at timestamptz NOT NULL DEFAULT now()
                    );
                    """
                )
                for name in NAMES:
                    db.execute(
                        "INSERT INTO customers (email) VALUES (%s) ON CONFLICT DO NOTHING",
                        (f"{name}@example.com",),
                    )
            log("INFO", "schema ready", customers=len(NAMES))
            return
        except psycopg.OperationalError as error:
            print(f"waiting for postgres: {error}", flush=True)
            time.sleep(1)


def customer_ids(db) -> list:
    return [row[0] for row in db.execute("SELECT id FROM customers").fetchall()]


# --- what the shop does -----------------------------------------------------


def sign_up(db) -> None:
    # now and then someone signs up twice: unique_violation
    name = random.choice(NAMES) if random.random() < 0.3 else f"user{random.randrange(10**6)}"
    email = f"{name}@example.com"
    try:
        db.execute("INSERT INTO customers (email) VALUES (%s)", (email,))
        log("INFO", f"signed up {email}")
    except psycopg.errors.UniqueViolation:
        log("WARNING", f"{email} is already a customer")


def place_order(db) -> None:
    ids = customer_ids(db)
    customer, item = random.choice(ids), random.choice(ITEMS)
    amount = round(random.uniform(5, 80), 2)
    row = db.execute(
        "INSERT INTO orders (customer_id, item, amount) VALUES (%s, %s, %s) RETURNING id",
        (customer, item, amount),
    ).fetchone()
    log("INFO", f"order {row[0]} placed: {item} for {amount}", customer=customer)


def pay(db) -> None:
    # a big enough payment overdraws the account: check_violation
    ids = customer_ids(db)
    customer = random.choice(ids)
    amount = round(random.uniform(1, 150), 2)
    try:
        db.execute(
            "UPDATE customers SET balance = balance - %s WHERE id = %s", (amount, customer)
        )
        log("INFO", f"customer {customer} paid {amount}")
    except psycopg.errors.CheckViolation:
        log("ERROR", f"payment of {amount} declined: insufficient balance", customer=customer)
        db.execute("UPDATE customers SET balance = balance + 200 WHERE id = %s", (customer,))
        log("INFO", f"customer {customer} topped up 200")


def history(db) -> None:
    ids = customer_ids(db)
    db.execute(
        "SELECT item, amount, placed_at FROM orders WHERE customer_id = %s "
        "ORDER BY placed_at DESC LIMIT 20",
        (random.choice(ids),),
    ).fetchall()


def report_with_a_typo(db) -> None:
    try:
        db.execute("SELECT item, count(*) FORM orders GROUP BY item")
    except psycopg.errors.SyntaxError:
        log("ERROR", "the sales report failed: syntax error")


def dropped_table(db) -> None:
    try:
        db.execute("SELECT * FROM coupons WHERE code = 'WELCOME10'")
    except psycopg.errors.UndefinedTable:
        log("ERROR", "coupon lookup failed: no coupons table")


def average_of_nothing(db) -> None:
    try:
        db.execute("SELECT sum(amount) / count(*) FROM orders WHERE item = 'unicorn'")
    except psycopg.errors.DivisionByZero:
        log("ERROR", "average order of unicorns: division by zero")


def slow_report(db) -> None:
    try:
        db.execute("SET statement_timeout = '100ms'")
        db.execute("SELECT pg_sleep(0.5), count(*) FROM orders")
    except psycopg.errors.QueryCanceled:
        log("WARNING", "the slow report timed out")
    finally:
        db.execute("RESET statement_timeout")


def notices(db) -> None:
    level = random.choice(["NOTICE", "WARNING"])
    db.execute(
        f"DO $$ BEGIN RAISE {level} 'stock of % is running low', "
        f"'{random.choice(ITEMS)}'; END $$"
    )


def maintenance(db) -> None:
    db.execute("VACUUM (ANALYZE) orders")
    if random.random() < 0.2:
        db.execute("CHECKPOINT")


def transfer(a: int, b: int, pause: float) -> None:
    """Move money from `a` to `b`, locking `a` first: run two of these in
    opposite directions at once and one of them deadlocks."""
    with connect("transfers") as db:
        try:
            with db.transaction():
                db.execute("UPDATE customers SET balance = balance + 1 WHERE id = %s", (a,))
                time.sleep(pause)
                db.execute("UPDATE customers SET balance = balance + 1 WHERE id = %s", (b,))
        except psycopg.errors.DeadlockDetected:
            log("ERROR", f"transfer {a} → {b} deadlocked, retrying later")


def deadlock(db) -> None:
    a, b = random.sample(customer_ids(db)[:20], 2)
    threads = [
        threading.Thread(target=transfer, args=(a, b, 0.4)),
        threading.Thread(target=transfer, args=(b, a, 0.4)),
    ]
    for t in threads:
        t.start()


def lock_timeout(db) -> None:
    """Hold a row in one session and give up waiting for it in another."""
    target = random.choice(customer_ids(db))

    def holder():
        with connect("batch-job") as held, held.transaction():
            held.execute("SELECT * FROM customers WHERE id = %s FOR UPDATE", (target,))
            time.sleep(1.5)

    threading.Thread(target=holder).start()
    time.sleep(0.1)
    try:
        db.execute("SET lock_timeout = '300ms'")
        db.execute("UPDATE customers SET balance = balance WHERE id = %s", (target,))
    except psycopg.errors.LockNotAvailable:
        log("WARNING", f"customer {target} was locked; update skipped")
    finally:
        db.execute("RESET lock_timeout")


def wrong_password(_db) -> None:
    try:
        psycopg.connect(URL.replace("shop:shop@", "shop:hunter2@"), application_name="intruder")
    except psycopg.OperationalError:
        log("ERROR", "login refused: password authentication failed", user="shop")


def missing_database(_db) -> None:
    try:
        psycopg.connect(URL.rsplit("/", 1)[0] + "/shop_archive", application_name="archiver")
    except psycopg.OperationalError:
        log("ERROR", "the archive database doesn't exist")


# (what, how often)
ACTIONS = [
    (place_order, 30),
    (history, 25),
    (pay, 14),
    (sign_up, 8),
    (notices, 4),
    (report_with_a_typo, 2),
    (dropped_table, 2),
    (average_of_nothing, 2),
    (slow_report, 2),
    (lock_timeout, 1),
    (deadlock, 1),
    (wrong_password, 1),
    (missing_database, 1),
    (maintenance, 1),
]


def main() -> None:
    os.makedirs(os.path.dirname(APP_LOG), exist_ok=True)
    setup()
    actions, weights = zip(*ACTIONS)
    while True:
        # a session a while, then a new one: connections and disconnections
        # are logged too
        with connect(random.choice(["web", "api", "worker"])) as db:
            for _ in range(random.randrange(50, 300)):
                action = random.choices(actions, weights)[0]
                try:
                    action(db)
                except psycopg.Error as error:
                    log("ERROR", f"{action.__name__} failed: {error}".strip())
                time.sleep(random.expovariate(RATE))


if __name__ == "__main__":
    main()

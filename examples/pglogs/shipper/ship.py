"""Postgres' log and the app's, tailed into Tidepool. Standard library only.

Postgres writes its log as JSON (`log_destination = jsonlog`), one object
per line, into files it rotates every minute; `current_logfiles` in its
data directory names the one it's writing. The shipper follows that file,
reading the old one to its end before it moves on, and the app's log
(APP_LOG) beside it. Lines are mapped to the `logs` table's columns and
posted as NDJSON every FLUSH_MS, retried until Tidepool has them. Where
each file was read to is saved after every batch (STATE), and a batch's
idempotency key is where it starts in its files: when the shipper or
Tidepool restarts, nothing is lost and nothing is written twice.

On start it writes the `severities` table whole, and the common SQLSTATE
codes; a code it hasn't written yet is written before the first line that
has it.

Environment:
  TIDEPOOL_URL  default http://tidepool:7070
  PGDATA        default /pgdata (Postgres' data directory, read only)
  APP_LOG       default /applogs/app.jsonl
  FLUSH_MS      default 250
  STATE         default /state/positions.json: where each file was read to
  DUMP          a file to append every row to as well (for the tests'
                sample); off by default
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid
import zlib
from datetime import datetime, timezone
from pathlib import Path

TIDEPOOL = os.environ.get("TIDEPOOL_URL", "http://tidepool:7070").rstrip("/")
PGDATA = Path(os.environ.get("PGDATA", "/pgdata"))
APP_LOG = Path(os.environ.get("APP_LOG", "/applogs/app.jsonl"))
FLUSH = float(os.environ.get("FLUSH_MS", "250")) / 1000
DUMP = os.environ.get("DUMP")
STATE = Path(os.environ.get("STATE", "/state/positions.json"))

# the severities Postgres' jsonlog writes (DEBUG for every debug level), in
# order, and the level each one is
SEVERITIES = [
    ("DEBUG", "debug"), ("LOG", "info"), ("INFO", "info"), ("NOTICE", "info"),
    ("WARNING", "warning"), ("ERROR", "error"), ("FATAL", "fatal"), ("PANIC", "fatal"),
]
# the app's levels, as Postgres would say them
APP_LEVELS = {"DEBUG": "DEBUG", "INFO": "INFO", "WARNING": "WARNING", "ERROR": "ERROR"}

# SQLSTATE classes (the code's first two characters), from Postgres' docs
CLASSES = {
    "00": "Successful Completion", "01": "Warning", "02": "No Data",
    "03": "SQL Statement Not Yet Complete", "08": "Connection Exception",
    "09": "Triggered Action Exception", "0A": "Feature Not Supported",
    "0B": "Invalid Transaction Initiation", "0F": "Locator Exception",
    "0L": "Invalid Grantor", "0P": "Invalid Role Specification",
    "0Z": "Diagnostics Exception", "20": "Case Not Found",
    "21": "Cardinality Violation", "22": "Data Exception",
    "23": "Integrity Constraint Violation", "24": "Invalid Cursor State",
    "25": "Invalid Transaction State", "26": "Invalid SQL Statement Name",
    "27": "Triggered Data Change Violation",
    "28": "Invalid Authorization Specification",
    "2B": "Dependent Privilege Descriptors Still Exist",
    "2D": "Invalid Transaction Termination", "2F": "SQL Routine Exception",
    "34": "Invalid Cursor Name", "38": "External Routine Exception",
    "39": "External Routine Invocation Exception", "3B": "Savepoint Exception",
    "3D": "Invalid Catalog Name", "3F": "Invalid Schema Name",
    "40": "Transaction Rollback", "42": "Syntax Error or Access Rule Violation",
    "44": "WITH CHECK OPTION Violation", "53": "Insufficient Resources",
    "54": "Program Limit Exceeded", "55": "Object Not In Prerequisite State",
    "57": "Operator Intervention", "58": "System Error",
    "72": "Snapshot Failure", "F0": "Configuration File Error",
    "HV": "Foreign Data Wrapper Error", "P0": "PL/pgSQL Error",
    "XX": "Internal Error",
}
# the conditions this example meets, and a few more
CONDITIONS = {
    "00000": "successful_completion", "01000": "warning", "02000": "no_data",
    "08006": "connection_failure", "08P01": "protocol_violation",
    "22003": "numeric_value_out_of_range", "22012": "division_by_zero",
    "22P02": "invalid_text_representation", "23502": "not_null_violation",
    "23503": "foreign_key_violation", "23505": "unique_violation",
    "23514": "check_violation", "25P02": "in_failed_sql_transaction",
    "28000": "invalid_authorization_specification", "28P01": "invalid_password",
    "3D000": "invalid_catalog_name", "40001": "serialization_failure",
    "40P01": "deadlock_detected", "42601": "syntax_error",
    "42703": "undefined_column", "42883": "undefined_function",
    "42P01": "undefined_table", "53300": "too_many_connections",
    "55P03": "lock_not_available", "57014": "query_canceled",
    "57P01": "admin_shutdown", "P0001": "raise_exception", "XX000": "internal_error",
}


def post(path: str, body: bytes, kind: str, method: str = "POST", key: str | None = None) -> None:
    """One request, retried (under the same idempotency key) until Tidepool
    takes it. A refusal (4xx) is printed and dropped: it won't get better."""
    key = key or str(uuid.uuid4())
    pause = 0.5
    while True:
        request = urllib.request.Request(
            f"{TIDEPOOL}{path}",
            body,
            {"content-type": kind, "idempotency-key": key},
            method=method,
        )
        try:
            with urllib.request.urlopen(request, timeout=10):
                return
        except urllib.error.HTTPError as error:
            if 400 <= error.code < 500:
                print(f"{path}: refused ({error.code}): {error.read()[:500]!r}", flush=True)
                return
            problem = f"HTTP {error.code}"
        except (urllib.error.URLError, OSError) as error:
            problem = str(error)
        print(f"{path}: {problem}; retrying", flush=True)
        time.sleep(pause)
        pause = min(pause * 2, 5)


def ndjson(rows: list) -> bytes:
    return "".join(json.dumps(row) + "\n" for row in rows).encode()


def instant(text: str) -> datetime:
    """Postgres' `2026-11-03 18:40:01.123 UTC`, or an ISO 8601 time."""
    text = text.strip()
    if text.endswith(" UTC"):
        return datetime.strptime(text[:-4], "%Y-%m-%d %H:%M:%S.%f").replace(tzinfo=timezone.utc)
    return datetime.fromisoformat(text.replace("Z", "+00:00")).astimezone(timezone.utc)


def stamp(at: datetime) -> str:
    return at.isoformat(timespec="microseconds").replace("+00:00", "Z")


class Codes:
    """The SQLSTATE codes Tidepool has a row for."""

    def __init__(self) -> None:
        self.known: set = set()

    def ensure(self, codes: set) -> None:
        new = sorted(c for c in codes if c and c not in self.known)
        if not new:
            return
        rows = [
            {
                "code": code,
                "condition": CONDITIONS.get(code, code),
                "class": CLASSES.get(code[:2], f"Class {code[:2]}"),
            }
            for code in new
        ]
        post("/v1/tables/sqlstates/rows", ndjson(rows), "application/x-ndjson")
        self.known.update(new)


def from_postgres(line: dict) -> dict:
    at = instant(line["timestamp"])
    return {
        "logged_at": stamp(at),
        "minute": stamp(at.replace(second=0, microsecond=0)),
        "source": "postgres",
        "severity": line.get("error_severity", "LOG"),
        # left out when it's 00000: the line reports success
        "sqlstate": line.get("state_code", "00000"),
        "database": line.get("dbname"),
        "user_name": line.get("user"),
        "application": line.get("application_name") or None,
        "backend": line.get("backend_type"),
        "pid": line.get("pid"),
        "message": line.get("message", ""),
        "detail": line.get("detail"),
        "hint": line.get("hint"),
        "statement": line.get("statement"),
    }


def from_app(line: dict) -> dict:
    at = instant(line["time"])
    extra = {k: v for k, v in line.items() if k not in ("time", "level", "message")}
    return {
        "logged_at": stamp(at),
        "minute": stamp(at.replace(second=0, microsecond=0)),
        "source": "app",
        "severity": APP_LEVELS.get(line.get("level", "INFO"), "INFO"),
        "sqlstate": None,
        "database": "shop",
        "user_name": None,
        "application": "app",
        "backend": None,
        "pid": None,
        "message": line.get("message", ""),
        "detail": json.dumps(extra) if extra else None,
        "hint": None,
        "statement": None,
    }


class Tail:
    """New whole lines of a file, from where it was last read; read again
    from the top when the file is replaced or truncated. Where it is can be
    saved and resumed ([`mark`]), so a restart neither skips nor repeats."""

    def __init__(self, path: Path, resume: list | None = None) -> None:
        self.path, self.inode, self.offset, self.resume = path, None, 0, resume

    def read(self) -> tuple:
        """(where the lines start, as a key part, and the lines)."""
        try:
            stat = self.path.stat()
        except FileNotFoundError:
            return None, []
        if stat.st_ino != self.inode or stat.st_size < self.offset:
            self.inode, self.offset = stat.st_ino, 0
            if self.resume and self.resume[0] == stat.st_ino and self.resume[1] <= stat.st_size:
                self.offset = self.resume[1]
            self.resume = None
        with open(self.path, "rb") as handle:
            # which writing of the file this is: Postgres writes the same
            # names again an hour later, into the same inode
            first = handle.read(64)
            handle.seek(self.offset)
            data = handle.read()
        cut = data.rfind(b"\n") + 1
        if cut == 0:
            return None, []
        start = f"{self.path.name}:{self.inode}:{zlib.crc32(first):08x}:{self.offset}"
        self.offset += cut
        return start, [line for line in data[:cut].split(b"\n") if line.strip()]

    def mark(self) -> list:
        return [self.inode, self.offset]


def load_state() -> dict:
    try:
        return json.loads(STATE.read_text())
    except (OSError, ValueError):
        return {}


def save_state(tails: list) -> None:
    """Where every file was read to, written whole and renamed into place."""
    state = {str(t.path): t.mark() for t in tails if t is not None and t.inode is not None}
    try:
        STATE.parent.mkdir(parents=True, exist_ok=True)
        temporary = STATE.with_suffix(".tmp")
        temporary.write_text(json.dumps(state))
        temporary.replace(STATE)
    except OSError as error:
        print(f"couldn't save where the logs were read to: {error}", flush=True)


def current_log() -> Path | None:
    """The jsonlog file Postgres is writing, from `current_logfiles`."""
    try:
        for entry in (PGDATA / "current_logfiles").read_text().splitlines():
            kind, _, name = entry.partition(" ")
            if kind == "jsonlog":
                return PGDATA / name.strip()
    except OSError:
        pass
    return None


def parse(raw: list, mapper) -> list:
    rows = []
    for line in raw:
        try:
            rows.append(mapper(json.loads(line)))
        except (ValueError, KeyError) as error:
            print(f"skipped a line ({error}): {line[:200]!r}", flush=True)
    return rows


def main() -> None:
    severities = [
        {"severity": name, "rank": rank, "level": level}
        for rank, (name, level) in enumerate(SEVERITIES)
    ]
    post("/v1/tables/severities/rows", json.dumps(severities).encode(), "application/json", "PUT")
    codes = Codes()
    codes.ensure(set(CONDITIONS))
    print("tables ready; shipping", flush=True)

    saved = load_state()
    app = Tail(APP_LOG, saved.get(str(APP_LOG)))
    postgres = None
    dump = open(DUMP, "a", encoding="utf-8") if DUMP else None
    while True:
        started = time.monotonic()
        rows, starts = [], []

        def take(tail: Tail, mapper) -> None:
            start, lines = tail.read()
            if lines:
                starts.append(start)
                rows.extend(parse(lines, mapper))

        now = current_log()
        if postgres is None or (now is not None and now != postgres.path):
            # the rest of the old file first, then the new one from its top
            # (or, just started, from where the last run got to)
            if postgres is not None:
                take(postgres, from_postgres)
                postgres = Tail(now)
            elif now is not None:
                postgres = Tail(now, saved.get(str(now)))
        if postgres is not None:
            take(postgres, from_postgres)
        take(app, from_app)
        if rows:
            codes.ensure({r["sqlstate"] for r in rows})
            # the key says where the batch starts: sent again after a
            # restart, before its position was saved, it's the same batch
            key = "logs " + " ".join(starts)
            post("/v1/tables/logs/rows", ndjson(rows), "application/x-ndjson", key=key)
            save_state([postgres, app])
            if dump is not None:
                dump.write(ndjson(rows).decode())
                dump.flush()
        time.sleep(max(0.0, FLUSH - (time.monotonic() - started)))


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(0)

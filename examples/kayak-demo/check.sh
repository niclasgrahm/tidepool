#!/bin/sh
# The Kayak ↔ Tidepool integration test (plan §12, K1): the demo, run on its
# own ports under its own project name, fills Tidepool's `readings` table
# through Kayak's `tidepool` output. Midway Tidepool is cut off the network
# for a few seconds, so batches fail and are retried under their idempotency
# keys. Then the plant stops, and the rows Tidepool holds must be exactly
# the messages Kayak's history says the pipeline handed its output, with no
# output errors: nothing lost to the outage, nothing written twice.
#
#   ./check.sh            (or `just demo-check` from the repository root)
set -eu
cd "$(dirname "$0")"

export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-kayak-demo-check}"
export TIDEPOOL_PORT="${TIDEPOOL_PORT:-17070}"
export KAYAK_PORT="${KAYAK_PORT:-16767}"
tidepool="http://localhost:$TIDEPOOL_PORT"
kayak="http://localhost:$KAYAK_PORT"

cleanup() { docker compose down -v >/dev/null 2>&1 || true; }
trap cleanup EXIT

json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
rows() { curl -sf "$tidepool/v1/tables/readings" | json 'd["rows"]'; }
history() {
  curl -sf "$kayak/api/pipelines/readings/history" |
    json "sum(b['$1'] for b in d['buckets'])"
}
wait_for() { # seconds, description, command…
  limit=$1; what=$2; shift 2
  i=0
  until "$@" >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge "$limit" ]; then echo "timed out waiting for $what" >&2; exit 1; fi
    sleep 1
  done
}
has_rows() { [ "$(rows)" -gt 0 ]; }

echo "building and starting (the first build takes several minutes)…"
docker compose up -d --build >/dev/null
wait_for 900 "Tidepool to be ready" curl -sf "$tidepool/readyz"
wait_for 300 "readings to arrive" has_rows
sleep 5

tidepool_container=$(docker compose ps -q tidepool)
network="${COMPOSE_PROJECT_NAME}_default"
echo "cutting Tidepool off for 6 seconds…"
docker network disconnect "$network" "$tidepool_container"
sleep 6
docker network connect --alias tidepool "$network" "$tidepool_container"
sleep 8

echo "stopping the plant and letting the last batches land…"
docker compose stop plant >/dev/null
sleep 6

written=$(rows)
sent=$(history outbound)
errors=$(history errors)
echo "Tidepool holds $written readings; Kayak handed its output $sent, with $errors output errors"
if [ "$written" -ne "$sent" ] || [ "$errors" -ne 0 ] || [ "$written" -eq 0 ]; then
  echo "FAILED: the counts differ, or something failed" >&2
  docker compose logs kayak | tail -30 >&2
  exit 1
fi
echo "ok"

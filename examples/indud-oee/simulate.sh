#!/bin/sh
# A plant's minutes for the OEE example: every machine in the namespace gets
# one row a minute (its state, why it stopped, units good and rejected): a
# day of history first, then a minute a second.
#
#   ./simulate.sh                 # against http://127.0.0.1:7070
#   TIDEPOOL=http://host:7070 ./simulate.sh
#   ./simulate.sh --sample        # no Indu: a sample namespace first
#
# With Kayak reading Indu (kayak/config.yaml) the machines are Indu's; with
# --sample they are a made-up plant in the same shape (`org:…`, `device:…`).
# Needs curl and awk.
set -eu
TIDEPOOL=${TIDEPOOL:-http://127.0.0.1:7070}

send() { # method table body
  if [ -n "${TIDEPOOL_INGEST_TOKEN:-}" ]; then
    set -- "$@" -H "Authorization: Bearer $TIDEPOOL_INGEST_TOKEN"
  fi
  method=$1 table=$2 body=$3; shift 3
  printf '%s' "$body" | curl -sf -X "$method" "$@" -H 'Content-Type: application/x-ndjson' \
    --data-binary @- "$TIDEPOOL/v1/tables/$table/rows?wait=visible" >/dev/null
}

if [ "${1:-}" = "--sample" ]; then
  node() { printf '{"id":"%s","parent_id":%s,"label":"%s","kind":"%s","unit":null}\n' "$1" "$2" "$3" "$4"; }
  sample=$(
    node org:se null Sweden org
    node org:got '"org:se"' Gothenburg org
    node org:got-l1 '"org:got"' 'Line 1' org
    node device:press-1 '"org:got-l1"' 'Press 1' device
    node device:press-2 '"org:got-l1"' 'Press 2' device
    node org:got-l2 '"org:got"' 'Line 2' org
    node device:welder-1 '"org:got-l2"' 'Welder 1' device
    node org:sod '"org:se"' 'Södertälje' org
    node org:sod-a '"org:sod"' 'Line A' org
    node device:cnc-1 '"org:sod-a"' 'CNC 1' device
    node device:cnc-2 '"org:sod-a"' 'CNC 2' device
    node org:de null Germany org
    node org:ing '"org:de"' Ingolstadt org
    node org:ing-4 '"org:ing"' 'Line 4' org
    node device:press-7 '"org:ing-4"' 'Press 7' device
    node device:robot-3 '"org:ing-4"' 'Robot 3' device
  )
  send PUT namespace "$sample"
fi

# the machines: the namespace's devices
devices=$(curl -sf "$TIDEPOOL/v1/tables/namespace/preview?limit=100000" |
  awk 'BEGIN { RS = "]" } { if (match($0, /"device:[^"]*"/)) print substr($0, RSTART + 1, RLENGTH - 2) }' |
  sort -u)
if [ -z "$devices" ]; then
  echo "no devices in the namespace yet: is Kayak reading Indu? (or use --sample)" >&2
  exit 1
fi
echo "$(echo "$devices" | wc -l | tr -d ' ') machines"

# how long a unit should take on each: 2 to 4 seconds, by its name.
# (Bodies go through a variable: bash 3.2, macOS's sh, misreads a quoted
# awk program inside "$(…)".)
cycles=$(echo "$devices" | awk '{
  n = 0; for (i = 1; i <= length($0); i++) n += index("0123456789abcdefghijklmnopqrstuvwxyz", substr($0, i, 1))
  printf "{\"device_id\":\"%s\",\"ideal_cycle_s\":%d}\n", $0, 2 + n % 3 }')
send POST devices "$cycles"

# `count` minutes from `start` (seconds since the epoch) for every machine:
# mostly running, sometimes set up, sometimes stopped
minutes() { # start count seed
  echo "$devices" | awk -v start="$1" -v count="$2" -v seed="$3" '
    # seconds since the epoch as RFC 3339, UTC (civil from days)
    function iso(t,   d, s, z, era, doe, yoe, y, doy, mp, dd, mm) {
      d = int(t / 86400); s = t - d * 86400
      z = d + 719468; era = int(z / 146097); doe = z - era * 146097
      yoe = int((doe - int(doe / 1460) + int(doe / 36524) - int(doe / 146096)) / 365)
      y = yoe + era * 400; doy = doe - (365 * yoe + int(yoe / 4) - int(yoe / 100))
      mp = int((5 * doy + 2) / 153); dd = doy - int((153 * mp + 2) / 5) + 1
      mm = mp < 10 ? mp + 3 : mp - 9; if (mm <= 2) y++
      return sprintf("%04d-%02d-%02dT%02d:%02d:%02dZ", y, mm, dd, int(s / 3600), int(s % 3600 / 60), s % 60)
    }
    BEGIN { srand(seed); split("jam,tooling,no material,maintenance", why, ",") }
    { machines[NR] = $0 }
    END {
      for (m = 0; m < count; m++)
        for (i = 1; i <= NR; i++) {
          r = rand()
          state = r < 0.82 ? "running" : (r < 0.9 ? "setup" : "stopped")
          reason = state == "stopped" ? "\"" why[1 + int(rand() * 4)] "\"" : "null"
          good = state == "running" ? 6 + int(rand() * 8) : 0
          reject = state == "running" && rand() < 0.3 ? 1 : 0
          printf "{\"device_id\":\"%s\",\"at\":\"%s\",\"state\":\"%s\",\"reason\":%s,\"good\":%d,\"reject\":%d}\n",
            machines[i], iso(start + m * 60), state, reason, good, reject
        }
    }'
}

now=$(date +%s)
t=$(( now - now % 60 - 1440 * 60 ))
for chunk in 1 2 3 4 5 6; do
  rows=$(minutes "$t" 240 "$chunk")
  send POST minutes "$rows"
  t=$(( t + 240 * 60 ))
done
echo "a day of history; now a minute a second (ctrl-c to stop)"
seed=100
while :; do
  rows=$(minutes "$t" 1 "$seed")
  send POST minutes "$rows"
  t=$(( t + 60 )); seed=$(( seed + 1 ))
  sleep 1
done

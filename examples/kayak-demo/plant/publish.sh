#!/bin/sh
# A simulated plant floor: six machines on two lines, one reading per
# machine a second, and now and then a stoppage. Rows are published shaped
# as Tidepool's tables are (see ../tidepool/tables).
set -eu
HOST="${MQTT_HOST:-mosquitto}"

until mosquitto_pub -h "$HOST" -t plant/hello -m up 2>/dev/null; do sleep 1; done

# the machines, retained: a subscriber that starts later still gets them
for m in 1 2 3 4 5 6; do
  line=$(( (m - 1) / 3 + 1 ))
  case $m in 1|4) kind=press ;; 2|5) kind=lathe ;; *) kind=packer ;; esac
  mosquitto_pub -h "$HOST" -q 1 -r -t "plant/machines/m$m" \
    -m "{\"machine_id\":\"m$m\",\"name\":\"Machine $m\",\"line\":\"line $line\",\"kind\":\"$kind\"}"
done

# stoppages: one in about every forty seconds, on a random machine
awk 'BEGIN {
  srand(); n = 0
  split("jam,tool change,material,maintenance,no operator", reasons, ",")
  while (1) {
    system("sleep 1")
    if (rand() < 0.025) {
      n++
      m = 1 + int(rand() * 6)
      printf "{\"stoppage_id\":\"s%d-%d\",\"machine_id\":\"m%d\",\"started_at\":\"%s\",\"minutes\":%.1f,\"reason\":\"%s\"}\n",
        systime(), n, m, strftime("%Y-%m-%dT%H:%M:%SZ"), 1 + rand() * 25, reasons[1 + int(rand() * 5)]
      fflush()
    }
  }
}' | mosquitto_pub -h "$HOST" -q 1 -t plant/stoppages -l &

# readings: a machine runs warmer as its line speeds up, and rejects more
awk 'BEGIN {
  srand()
  for (m = 1; m <= 6; m++) { temp[m] = 55 + m; speed[m] = 80 + 10 * m }
  while (1) {
    now = strftime("%Y-%m-%dT%H:%M:%SZ")
    for (m = 1; m <= 6; m++) {
      speed[m] += int(rand() * 7) - 3
      if (speed[m] < 60) speed[m] = 60
      if (speed[m] > 160) speed[m] = 160
      temp[m] += (40 + speed[m] / 4 - temp[m]) * 0.1 + rand() - 0.5
      produced = int(speed[m] / 60 + rand() * 2)
      rejected = (rand() < 0.02 + (speed[m] - 60) / 4000) ? 1 : 0
      printf "{\"machine_id\":\"m%d\",\"at\":\"%s\",\"temperature\":%.2f,\"vibration\":%.3f,\"speed\":%d,\"produced\":%d,\"rejected\":%d}\n",
        m, now, temp[m], 0.2 + rand() * speed[m] / 200, speed[m], produced, rejected
    }
    fflush()
    system("sleep 1")
  }
}' | mosquitto_pub -h "$HOST" -q 1 -t plant/readings -l

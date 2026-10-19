#!/bin/bash
# Kills llama-server if available memory falls below 6 GB, so a bad load cannot lock the machine.
LOG=~/sg/memguard.log
echo "guard started $(date +%T)" > "$LOG"
while true; do
  avail_kb=$(awk '/MemAvailable/ {print $2}' /proc/meminfo)
  if [ "$avail_kb" -lt 6291456 ]; then
    echo "$(date +%T) available $((avail_kb/1048576)) GB: killing llama-server" >> "$LOG"
    pkill -9 -x llama-server
    sleep 5
  fi
  echo "$(date +%T) available $((avail_kb/1048576)) GB" > ~/sg/mem.last
  sleep 2
done

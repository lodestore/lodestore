#!/bin/bash
set -eu

name=${AGENT_NAME:?AGENT_NAME is required}
root=${SYNTH_ROOT:-/agent/data}
cap_mb=${SYNTH_CAP_MB:-200}
interval=${SYNTH_INTERVAL_SECS:-600}
words="amber birch cedar delta ember fjord grove harbor island juniper kestrel lantern meadow nectar orchid prairie quartz ridge summit thistle upland valley willow yarrow zephyr atlas beacon cobalt dune"

agent_seed=$(printf '%s' "$name" | cksum | cut -d' ' -f1)

text() {
  awk -v seed="$1" -v lines="$2" -v words="$words" 'BEGIN {
    srand(seed); n = split(words, w, " ")
    for (i = 0; i < lines; i++) {
      line = ""
      for (j = 0; j < 12; j++) line = line w[int(rand() * n) + 1] " "
      print line
    }
  }'
}

noise() {
  awk -v seed="$1" -v lines="$2" 'BEGIN {
    srand(seed); chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
    for (i = 0; i < lines; i++) {
      line = ""
      for (j = 0; j < 63; j++) line = line substr(chars, int(rand() * 62) + 1, 1)
      print line
    }
  }'
}

init() {
  [ -f "$root/.ready" ] && return 0
  mkdir -p "$root/docs" "$root/projects" "$root/logs" "$root/media"
  text 1 400 >"$root/docs/handbook.txt"
  text 2 120 >"$root/docs/glossary.txt"
  for i in $(seq 1 30); do
    text $((agent_seed + i)) $((20 + i * 5)) >"$root/docs/note-$i.txt"
  done
  for p in aurora basalt corona; do
    mkdir -p "$root/projects/$p/src"
    for i in $(seq 1 10); do
      text $((agent_seed + i * 7)) 40 >"$root/projects/$p/src/module-$i.txt"
    done
    text 3 15 >"$root/projects/$p/README.txt"
  done
  noise 11 32768 >"$root/media/.block-a"
  noise 12 32768 >"$root/media/.block-b"
  for i in $(seq 1 8); do
    {
      cat "$root/media/.block-a" "$root/media/.block-b"
      noise $((agent_seed + i)) 4096
      cat "$root/media/.block-a"
    } >"$root/media/clip-$i.bin"
  done
  rm -f "$root/media/.block-a" "$root/media/.block-b"
  text $((agent_seed + 99)) 300 >"$root/logs/app.log"
  : >"$root/.ready"
}

tick() {
  cycle=$(($(date +%s) / interval))
  {
    for i in $(seq 1 20); do
      echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $name worker-$((cycle % 4)) processed batch $((cycle * 20 + i))"
    done
  } >>"$root/logs/app.log"
  for k in 1 2; do
    n=$(((cycle * 3 + k) % 30 + 1))
    echo "revision $cycle: $(text $((cycle + k)) 1)" >>"$root/docs/note-$n.txt"
  done
  used=$(du -sm "$root" | cut -f1)
  if [ "$used" -gt "$cap_mb" ]; then
    tail -n 2000 "$root/logs/app.log" >"$root/logs/app.log.tmp"
    mv "$root/logs/app.log.tmp" "$root/logs/app.log"
  fi
}

case "${1:-}" in
  init) init ;;
  tick) tick ;;
  loop)
    while true; do
      sleep "$interval"
      "$0" tick || echo "synth tick failed" >&2
    done
    ;;
  *) echo "usage: synth.sh init|loop" >&2; exit 2 ;;
esac

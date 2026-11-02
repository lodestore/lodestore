#!/bin/bash
set -eu

name=${AGENT_NAME:?AGENT_NAME is required}
dirs=(--config-dir /agent/config --state-dir /agent/state)
token_file=/seed/join-$name.token

/usr/local/bin/synth.sh init

if [ ! -f /agent/state/identity.json ]; then
  for _ in $(seq 300); do
    [ -f "$token_file" ] && break
    sleep 1
  done
  if [ ! -f "$token_file" ]; then
    echo "no join token at $token_file; run the seed step first" >&2
    exit 1
  fi
  LODESTORE_JOIN_TOKEN=$(cat "$token_file") lodestore-agent enroll "${dirs[@]}" --hostname "$name"
fi
rm -f "$token_file"

/usr/local/bin/synth.sh loop &
exec lodestore-agent run "${dirs[@]}"

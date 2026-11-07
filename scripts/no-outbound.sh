#!/usr/bin/env bash
# Fails if tracedb opens any network connection that isn't loopback (spec QR-001 / SC-05; M2: through a flush to
# data files and a `tracedb demo` run, local storage). Linux only (uses strace). Usage: scripts/no-outbound.sh [path/to/tracedb]
set -euo pipefail
BIN=${1:-target/release/tracedb}
command -v strace >/dev/null || { echo "strace not found (Linux only); on macOS check with: nettop -p tracedb"; exit 2; }
DATA=$(mktemp -d); LOG=$(mktemp)
# A small memtable, so the seeded traces are flushed to data files while traced.
strace -f -qq --seccomp-bpf -e trace=connect -o "$LOG" "$BIN" serve --data "$DATA" --port 7979 --memtable 256KB >/dev/null 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true; rm -rf "$DATA" "$LOG" "$LOG".*' EXIT
for _ in $(seq 50); do curl -sf localhost:7979/healthz >/dev/null && break; sleep 0.1; done
strace -f -qq --seccomp-bpf -e trace=connect -o "$LOG.seed" "$BIN" seed --traces 20 --no-delay --url http://127.0.0.1:7979 >/dev/null
for _ in $(seq 100); do curl -sf localhost:7979/healthz | grep -q '"segments":[1-9]' && break; sleep 0.1; done
curl -sf localhost:7979/healthz | grep -q '"segments":[1-9]' || { echo "no flush happened"; exit 1; }
curl -sf localhost:7979/api/roots >/dev/null
kill $PID; wait $PID 2>/dev/null || true
strace -f -qq --seccomp-bpf -e trace=connect -o "$LOG.demo" "$BIN" demo --seed 7 >/dev/null
OUT=$(cat "$LOG" "$LOG.seed" "$LOG.demo" | grep 'connect(' | grep -v 'AF_UNIX' | grep -v '127.0.0.1' | grep -v '::1' | grep -v 'ENOENT' || true)
if [ -n "$OUT" ]; then echo "outbound connections found:"; echo "$OUT"; exit 1; fi
echo "no outbound connections ($(grep -c 'connect(' "$LOG" "$LOG.seed" "$LOG.demo" | paste -sd' ' -) connect calls, all loopback/unix; flush and demo included)"

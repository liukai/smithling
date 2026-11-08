#!/usr/bin/env bash
# Fails if smithling opens any network connection that isn't loopback (spec QR-001 / SC-05; M2: through a flush to
# data files and a `smithling demo` run, local storage; M3: a search, an aggregate with costs from the local price list,
# a multipart upload and the M2 + M3 demo). Linux only (uses strace). Usage: scripts/no-outbound.sh [path/to/smithling]
set -euo pipefail
BIN=${1:-target/release/smithling}
command -v strace >/dev/null || { echo "strace not found (Linux only); on macOS check with: nettop -p smithling"; exit 2; }
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
ID=00000000-0000-4000-8000-000000000777
RUN="{\"id\":\"$ID\",\"trace_id\":\"$ID\",\"dotted_order\":\"20261105T020000000000Z$ID\",\"session_name\":\"no-outbound\",\"name\":\"mp\",\"run_type\":\"chain\",\"start_time\":\"2026-11-05T02:00:00.000000\"}"
curl -sf localhost:7979/runs/multipart -F "post.$ID=$RUN;type=application/json" >/dev/null
curl -sf "localhost:7979/api/runs/$ID" >/dev/null || { echo "multipart upload not stored"; exit 1; }
JSON=(-H 'content-type: application/json')
curl -sf localhost:7979/api/query "${JSON[@]}" -d '{"filter": "search(\"429 Too Many Requests\")", "limit": 5}' >/dev/null
curl -sf localhost:7979/api/aggregate "${JSON[@]}" -d '{"stats": "by name | count, error_rate, p95(latency), sum(cost)"}' >/dev/null
kill $PID; wait $PID 2>/dev/null || true
strace -f -qq --seccomp-bpf -e trace=connect -o "$LOG.demo" "$BIN" demo --seed 7 >/dev/null
OUT=$(cat "$LOG" "$LOG.seed" "$LOG.demo" | grep 'connect(' | grep -v 'AF_UNIX' | grep -v '127.0.0.1' | grep -v '::1' | grep -v 'ENOENT' || true)
if [ -n "$OUT" ]; then echo "outbound connections found:"; echo "$OUT"; exit 1; fi
echo "no outbound connections ($(grep -c 'connect(' "$LOG" "$LOG.seed" "$LOG.demo" | paste -sd' ' -) connect calls, all loopback/unix; flush, search, aggregate, multipart and M2 + M3 demo included)"

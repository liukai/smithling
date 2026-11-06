# Quickstart: prove M2 works

Validation run for M2. Expected values come from the [spec](spec.md#success-criteria-mandatory); query syntax is in
[contracts/filter-language.md](contracts/filter-language.md); commands in [contracts/cli.md](contracts/cli.md).

**Prerequisites**: a release build (`cd ui && npm ci && npm run build && cd .. && cargo build --release`). For step 7:
Docker on Linux (CI runs it).

## 1. The scenario checks (one command)

```bash
./target/release/tracedb demo --seed 7
```
Expect: `14 failing roots ✓`, `23 quiet failures ✓`, `prompt_version top values ✓ (v6 928 · v7 912)`, the planted
v7 ∧ 429 count ✓, and `checks across memtable and files ✓`. Exit code 0.

## 2. Triage in the browser, no typing (SC-01, SC-02)

```bash
./target/release/tracedb serve --data /tmp/m2 &
./target/release/tracedb demo --url http://localhost:7070 --seed 7    # loads the scenario into this server
```
Open http://localhost:7070, then, by clicking only:
1. Time → *last 12h*; click the tag `batch=2026-11-05-a` on any row; click *root only*; click an `error` badge → **14**.
2. Change the status chip to `ok`; in *Any run in trace*, add `status = error` from the field browser → **23**.
3. Field browser → `metadata.prompt_version` shows `v6 (928)`, `v7 (912)`; click `v7`. Open *Table*, add column
   `outputs.status_code`, click a `429` cell.
4. *Text* → the panel's query appears as `and(…)`; edit `status` to `staus` → inline error pointing at it.
5. Copy the URL, open it in a new tab → same query and view.

## 3. Data survives, restarts are fast (SC-05, SC-06)

```bash
./target/release/tracedb seed --traces 20000 --no-delay --url http://localhost:7070   # ≈ 2M runs
curl -s localhost:7070/healthz        # segments > 0, memtable_bytes ≤ 128 MB
kill -9 %1; time ./target/release/tracedb serve --data /tmp/m2 &   # ready ≤ 10 s
```
Then repeat step 2 → same 14 and 23.

```bash
cargo test --release -p tracedb --test crash -- --ignored          # 10/10 rounds, incl. kills during flush
cargo test --release -p tracedb --test restart -- --ignored        # 2M runs ready ≤ 10 s
```

## 4. No pause during flushes; bursts slow down instead of failing (SC-07, R-8)

```bash
cargo test --release -p tracedb --test throughput -- --ignored     # ≥ 5,000 events/s, p99 upload ≤ 1 s during flush
cargo test -p tracedb --test slowdown                               # burst → 429s, nothing lost, memory under cap
```

## 5. Live views for any query

Open the quiet-failures query with *live* on, then `tracedb seed --traces 50`. Roots appear as soon as one of their
steps errors; a run that stops matching is marked *no longer matching*.

## 6. Correctness against a naive check (SC-09)

```bash
cargo test -p tracedb-query --test merge_on_read      # property test: query == run-by-run filter over merged runs
```

## 7. Same numbers on cloud-style storage (SC-08)

```bash
docker compose -f ci/cloud.yml up -d                  # MinIO + Postgres, local containers only
tracedb demo --seed 7 --storage s3://tracedb/test --metastore postgres://localhost/tracedb
```
Expect the same table as step 1.

## 8. Maintainer workflow

```bash
cargo build                                            # no Node needed; embeds ui/dist if present
./target/debug/tracedb serve --ui-dir ui/dist          # UI from disk; rebuild ui only
```

**Last verified**: not yet (M2 not built).

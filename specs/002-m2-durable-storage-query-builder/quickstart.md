# Quickstart: prove M2 works

Validation run for M2, as of M2's close (`c7595c6`); since M3 the binary's defaults differ (noted inline). Expected
values come from the [spec](spec.md#success-criteria-mandatory); query syntax is in
[contracts/filter-language.md](contracts/filter-language.md); commands in [contracts/cli.md](contracts/cli.md).

**Prerequisites**: a release build (`cd ui && npm ci && npm run build && cd .. && cargo build --release`). For step 7:
Docker on Linux (CI runs it).

## 1. The scenario checks (one command)

```bash
./target/release/smithling demo --seed 7 --checks m2
```
Expect: `14 failing roots ✓`, `23 quiet failures ✓`, `prompt_version top values ✓ (v6 928 · v7 912)`, the planted
v7 ∧ 429 count ✓, and `checks across memtable and files ✓`. Exit code 0. (In M2 `--checks m2` was the default; since
M3 the default is `m2,m3`, 10 checks.)

## 2. Triage in the browser, no typing (SC-01, SC-02)

```bash
./target/release/smithling serve --data /tmp/m2 &
./target/release/smithling demo --url http://localhost:7070 --seed 7 --memtable 128MB   # into this server; pass its --memtable
# since 4ec794c: --url reads the server's memtable_limit_bytes from /healthz; --memtable is only a fallback
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
for s in $(seq 10); do ./target/release/smithling seed --traces 1220 --seed $s --no-delay --url http://localhost:7070; done   # ≈ 2M runs
curl -s localhost:7070/healthz        # segments > 0, memtable_bytes ≤ the memtable limit (128 MB in M2; memtable_limit_bytes since M3)
kill -9 %1; ./target/release/smithling serve --data /tmp/m2 &
time (until curl -sf localhost:7070/healthz >/dev/null; do sleep 0.1; done)   # ready ≤ 10 s
```
Then repeat step 2's clicks (not the `demo` load) → same 14 and 23.

```bash
cargo test --release -p smithling --test crash -- --ignored          # 10/10 rounds, incl. kills during flush
cargo test --release -p smithling --test restart -- --ignored        # 2M runs ready ≤ 10 s
```

## 4. No pause during flushes; bursts slow down instead of failing (SC-07, R-8)

```bash
cargo test --release -p smithling --test throughput -- --ignored     # ≥ 5,000 events/s, p99 upload ≤ 1 s during flush
# M3 renamed and raised it: sustains_20000_events_per_second_while_querying (crates/smithling/tests/throughput.rs)
cargo test -p smithling-ingest --test slowdown                        # burst → 429s, nothing lost, memory under cap
```

## 5. Live views for any query

Open the quiet-failures query with *live* on, then `smithling seed --traces 50`. Roots appear as soon as one of their
steps errors; a run that stops matching is marked *no longer matching*.

## 6. Correctness against a naive check (SC-09)

```bash
cargo test -p smithling-query --test merge_on_read      # property test: query == run-by-run filter over merged runs
```

## 7. Same numbers on cloud-style storage (SC-08)

```bash
export AWS_ACCESS_KEY_ID=$(openssl rand -hex 20) AWS_SECRET_ACCESS_KEY=$(openssl rand -hex 20) PGPASSWORD=$(openssl rand -hex 20)
export AWS_ENDPOINT=http://127.0.0.1:9000 AWS_REGION=us-east-1
docker compose -f ci/cloud.yml up -d --wait minio postgres && docker compose -f ci/cloud.yml run --rm bucket
./target/release/smithling demo --seed 7 --storage s3://smithling/demo --metastore postgres://smithling@127.0.0.1/smithling
SMITHLING_TEST_S3_URL=s3://smithling/test SMITHLING_TEST_PG_URL=postgres://smithling@127.0.0.1/smithling \
  cargo test -p smithling-storage --test traits -- --nocapture            # no "skipped:" lines
```
Expect the same check rows as step 1 (trace and flush counts and timings vary). To run it again, start from empty
storage (`docker compose -f ci/cloud.yml down -v` first): the demo's catalog and files outlive its temporary folder. CI's `cloud` job runs this, plus the
crash harness on the same storage.

## 8. Maintainer workflow

```bash
cargo build                                            # no Node needed; embeds ui/dist if present
./target/debug/smithling serve --ui-dir ui/dist          # UI from disk; rebuild ui only
```

## Last verified

2026-11-06, macOS (Apple silicon, 64 GB), release build of `ab1afb1` plus the cloud CI / docs changes; no browser,
no Docker, MinIO or Postgres here, so the browser and cloud steps were checked through the API or not run.

| Step | Result |
|---|---|
| 1. `demo --seed 7` | **passed**: all 5 checks, 14 · 23 · v6 928 · v7 912 · 19 · yes; exit 0, 1.5 s |
| 2. `serve` + `demo --url` | **passed** with `--memtable 128MB` (8.7 s); without it the across-memory-and-files check fails (the step said so wrongly; fixed) |
| 2. clicks 1–5 | **not runnable here** (no browser). The same queries through `smithling query` / the API: 14 and 23 roots, `v6 928 · v7 912`, v7 ∧ 429 rows, `staus` → error pointing at it (exit 1). `ui` tests pass (132) |
| 3. seed ≈ 2M runs | **passed** in 10 batches of 1,220 traces (1.95M runs, 425 s, 400 segments). The old one-shot `--traces 20000` (≈ 3.3M runs, not 2M) was killed by the OS: `seed` builds every trace in memory first (88 GB peak); fixed |
| 3. `kill -9`, restart | **partly**: the sandbox couldn't `kill -9` its server, so a copy of the idle folder was opened instead: ready in 0.19 s (replay 105 ms), then 14 and 23 again. Those two queries took 14–22 s at 400 segments, far over SC-03's 500 ms; the bench's 14–53 ms was at 37 files. Fixed by `7c5b094` ([D-024](../000-smithling-master-plan/decisions.md#d-024)): 52 ms / 109 ms; inside the project holding the late updates they still took 4.5 s, brought down in M3 (last step [D-029](../000-smithling-master-plan/decisions.md#d-029): failing roots 1,070 → 155 ms) |
| 3. `crash`, `restart` tests | **passed**: 10/10 rounds; 2M runs ready in 0.43 s |
| 4. `throughput`, `slowdown` | **passed**: 38,616 events/s, p99 114 ms; burst test ok (it is `-p smithling-ingest`, not `-p smithling`; fixed) |
| 5. live views | **not runnable here** (browser); `ui` live-toggle tests pass |
| 6. `merge_on_read` | **passed** (50 s) |
| 7. cloud storage | **not runnable here** (no Docker): `ci/cloud.yml` parses, the gated tests compile and skip; CI's `cloud` job is unrun. Step rewritten for the compose file's credentials and the bucket service |
| 8. `--ui-dir ui/dist` | **passed**: serves the built UI |

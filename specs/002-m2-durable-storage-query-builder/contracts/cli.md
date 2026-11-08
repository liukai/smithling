# CLI contract (M2)

Extends the [M1 CLI](../../001-m1-see-agent-trace-live/contracts/cli.md).

| Command | Purpose |
|---|---|
| `smithling serve [--data ./.smithling] [--port 7070] [--bind 127.0.0.1]` | run smithling (as M1) |
| `smithling serve … [--storage file://<data>/data \| s3://bucket/prefix]` | where data files go; S3 credentials and endpoint from the standard `AWS_*` environment variables |
| `smithling serve … [--metastore sqlite://<data>/meta.sqlite \| postgres://host/db]` | where the catalog goes; the password comes from `PGPASSWORD` or the URL and is never logged |
| `smithling serve … [--memtable 128MB] [--flush-age 10m] [--dedup-window 10m]` | storage tuning. The planned `--cache 256MB` wasn't built in M2 ([D-013](../../000-smithling-master-plan/decisions.md#d-013)). M3: `--memtable 64MB`, `--flush-age 2m`, `--cache 32MB`, `--dedup-keys` ([D-028](../../000-smithling-master-plan/decisions.md#d-028)) |
| `smithling serve … [--ui-dir PATH]` (or `SMITHLING_UI_DIR`) | serve UI files from a folder instead of the built-in copy |
| `smithling seed …` | as M1 |
| `smithling demo [--seed 7] [--url URL \| --data DIR] [--checks m2]` | generate the overnight-eval scenario, run the M2 checks, print a pass/fail table; without `--url` it runs its own server on a temporary folder (or `--data`), and accepts the `serve` storage flags; `--memtable` (default 4MB) sizes the background volume that forces flushes. M3: `--checks` defaults to `m2,m3` (10 checks); with `--url` the demo reads the server's `memtable_limit_bytes` from `/healthz`, and `--memtable` is only the fallback (`4ec794c`) |
| `smithling query [--project P] [--time 12h] [--root] [--trace-filter EXPR] [--tree-filter EXPR] [--limit 50] [--json] 'EXPR'` | run a query against a running server and print a table (or JSON lines) |

Removed: `--max-memory`, `--max-runs` (M1 memory budget). They are accepted with a warning that they no longer apply.

`smithling demo --checks m2` output (illustrative; trace and flush counts depend on the seed; the four counts are fixed by the generator):

```
smithling demo — M2 checks  (seed 7, 2,214 traces, 3 flushes)
  ✓ 14 failing roots                     14       31 ms
  ✓ 23 quiet failures                    23       58 ms
  ✓ prompt_version top values            v6 928 · v7 912    12 ms
  ✓ v7 ∧ status_code = 429               <planted>  22 ms
  ✓ checks across memtable and files     yes
all 5 checks passed
```

Exit codes: 0 = ok, 1 = usage or failure (including a failed check), 2 = server not reachable.

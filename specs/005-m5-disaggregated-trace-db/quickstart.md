# Quickstart: see M5 working

Validation guide for when M5 is built. Commands follow the proposed [CLI contract](contracts/cli.md); details may
change as design decisions settle.

## Prerequisites

- A `smithling` build with M4 and M5.
- For steps 1–5: one machine, one shared folder. For step 6: MinIO and Postgres (the cloud CI job provides them).

## 1. One writer, two readers on one machine

```sh
smithling serve --data ./.smithling --port 7070                                  # writer (all roles)
smithling serve --role reader --storage file://$PWD/.smithling/data \
  --metastore sqlite://$PWD/.smithling/meta.sqlite --port 7071                   # reader 1
smithling serve --role reader --storage file://$PWD/.smithling/data \
  --metastore sqlite://$PWD/.smithling/meta.sqlite --port 7072                   # reader 2
```

Expected: `smithling status --url http://localhost:7070` lists both readers; each reader reports `current`.

## 2. Same answers on readers (P2, SC-01)

```sh
smithling demo --url http://localhost:7070 --seed 7     # loads the scenario on the writer
smithling demo --url http://localhost:7071 --seed 7 --checks-only
smithling demo --url http://localhost:7072 --seed 7 --checks-only
```

Expected: every M2–M4 check passes on both readers with the writer's counts (14, 23, 31, 14% / 31 s, $4.12, …).

## 3. Readers stay current (P9)

Send a feedback score and a late update for a scenario run to the writer. Wait one flush age (2 min by default, or
start the writer with `--flush-age 5s` for the demo). Expected: both readers filter on the feedback and show the run's
latest values; `stats.catalog_version` in their responses has moved.

## 4. Long queries survive compaction (P7)

Start a long grouped query on reader 1 while the writer compacts (demo option to force compaction). Expected: the
query returns the same answer as before compaction; `/healthz` on the writer shows the replaced files under
`files_pending_delete` until the grace period ends.

## 5. Only one writer (P10)

```sh
smithling serve --data ./.smithling --port 7073
```

Expected: exits 1, naming the running writer and when its lease expires.

## 6. Cloud-style storage

Repeat steps 1–2 with `--storage s3://smithling-ci/m5 --metastore postgres://…` on all three processes (the cloud CI
job). Expected: identical counts.

## 7. A reader that falls behind (P12)

Block reader 2's access to the catalog (stop Postgres briefly, or for SQLite rename the file). Expected: within one
check its `/healthz` shows `current: false`, the writer's status panel shows its `seen_at` going stale, and it keeps
serving its last version.

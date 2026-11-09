# CLI contract: M5 additions

Proposed; options follow the design register ([design.md](../design.md)). Everything from M1–M4 is unchanged.

| | Command | Meaning | Decision |
|---|---|---|---|
| 🟢 | `smithling serve --role reader --storage <url> --metastore <url> [--port 7071] [--bind 127.0.0.1]` | Run a read worker against the writer's storage and catalog. Serves every read API and the UI; refuses writes. Needs no `--data` folder of its own beyond its cache | KD-1, KD-8 |
| 🟠 | `smithling serve [--role all]` | Default, as M4: every role in one process. `--role writer` is the same as `all` in M5 (the writer still serves reads) | KD-1 |
| 🟢 | `smithling serve … [--grace 24h]` | How long the writer keeps files replaced by compaction before deleting them | KD-6 |
| 🟢 | `smithling serve --role reader … [--query-time-limit 1h]` | Reader queries running longer fail with "query took too long, retry"; must be shorter than the writer's `--grace` | KD-6 |
| 🟢 | `smithling serve … [--lease-expiry 30s] [--lease-renew 10s]` | Writer lease timing | KD-7 |
| 🟠 | `smithling serve --role reader … [--cache 32MB]` | Reader data cache; the default may be larger when `--storage` is `s3://` | KD-11 |
| 🟢 | `smithling status [--url URL]` | Print the role, catalog version, age of newest data and whether current; on a writer, also the readers that checked in during the last minute. Exit 0 = current, 1 = behind, 2 = not reachable | KD-10 |
| 🟠 | `smithling demo --url <reader> [--checks-only]` | As M3; waits until the reader reports the writer's newest catalog version before checking counts. `--checks-only` (new) skips loading, for readers, which refuse writes | — |

Refusals at start (exit 1, with a message):

- A second writer while another holds a valid lease: names the holder and when its lease expires.
- A reader whose `--metastore` is a SQLite file on a network filesystem, or not reachable.
- A reader whose store has a newer format than it understands.

# CLI contract (master): retired

This page was the planning draft of the command line, written before M1. The commands were then specified and built
one milestone at a time, and each milestone's contract is the source of truth for what it added. Read them in order:

1. [M1](../../001-m1-see-agent-trace-live/contracts/cli.md): `serve`, `seed`.
2. [M2](../../002-m2-durable-storage-query-builder/contracts/cli.md): storage flags, `query`, `demo`.
3. [M3](../../003-m3-search-aggregates-speed/contracts/cli.md): `bench`, aggregate flags for `query`, memory and
   cache flags, the M3 `demo` checks.

`smithling --help` (and `smithling <command> --help`) prints the flags of the binary you have.

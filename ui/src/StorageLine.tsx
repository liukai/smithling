import { useEffect, useState } from "react";
import { type StorageHealth, getStorageHealth } from "./api";
import { fmtAgo, plural } from "./format";

const POLL_MS = 30_000;

/** Data files and whether compaction keeps up, from `/healthz` (M4); nothing if the server doesn't say. */
export function StorageLine() {
  const [h, setH] = useState<StorageHealth | null>(null);
  useEffect(() => {
    let live = true;
    const load = () => getStorageHealth().then((v) => live && setH(v), () => live && setH(null));
    load();
    const t = setInterval(load, POLL_MS);
    return () => { live = false; clearInterval(t); };
  }, []);
  const f = h?.files;
  const c = h?.compaction;
  if (!f || !c) return null;
  const files = `${plural(f.l0 + f.l1, "data file")}${f.l1 > 0 && f.l0 > 0 ? ` (${f.l1.toLocaleString()} compacted, ${f.l0.toLocaleString()} new)` : ""}`;
  const [state, warn] = c.last_error ? [`compaction failed: ${c.last_error}`, true]
    : c.stopped ? ["compaction stopped; restart to resume", true]
    : c.running ? ["compacting…", false]
    : !c.enabled ? ["compaction off", false]
    : c.last_run_at ? [`compacted ${fmtAgo(c.last_run_at, Date.now())}`, false]
    : ["not compacted yet", false];
  return (
    <span className="storage-line" title="Small files are merged into larger ones in the background; answers stay the same.">
      {files} · <span className={warn ? "warn" : undefined}>{state}</span>
      {f.replaced > 0 && <> · {plural(f.replaced, "old file")} waiting for deletion</>}
    </span>
  );
}

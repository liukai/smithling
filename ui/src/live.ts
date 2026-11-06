import type { RunHeader } from "./api";

export interface LiveHandlers {
  /** The stream is (re)connected: load the view now; later changes arrive via onRuns. */
  onHello: () => void;
  onRuns: (runs: RunHeader[]) => void;
  /** The tab fell behind: reload the view. */
  onResync: () => void;
  onConnected?: (connected: boolean) => void;
}

/** Subscribes to the live tail. EventSource reconnects on its own; each reconnect sends `hello` again. */
export function subscribe(query: { project?: string; trace?: string }, h: LiveHandlers): () => void {
  const q = new URLSearchParams();
  if (query.project) q.set("project", query.project);
  if (query.trace) q.set("trace", query.trace);
  const es = new EventSource(`/api/tail?${q}`);
  es.addEventListener("hello", () => {
    h.onConnected?.(true);
    h.onHello();
  });
  es.addEventListener("runs", (e) => h.onRuns(JSON.parse((e as MessageEvent).data)));
  es.addEventListener("resync", () => h.onResync());
  es.onerror = () => h.onConnected?.(false);
  return () => es.close();
}

/** Keeps the newer version of each run, by `last_seq`. */
export function upsert(map: Map<string, RunHeader>, runs: RunHeader[]): boolean {
  let changed = false;
  for (const r of runs) {
    const old = map.get(r.run_id);
    if (!old || old.last_seq < r.last_seq) {
      map.set(r.run_id, r);
      changed = true;
    }
  }
  return changed;
}

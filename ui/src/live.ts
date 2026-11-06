import type { RunHeader } from "./api";
import type { Order } from "./query/url";

export interface LiveHandlers {
  /** The stream is (re)connected: load the view now; later changes arrive via onRuns. */
  onHello: () => void;
  onRuns: (runs: RunHeader[]) => void;
  /** The tab fell behind: reload the view. */
  onResync: () => void;
  /** Ids of runs that no longer match; ids the view doesn't show can be ignored. */
  onLeft?: (ids: string[]) => void;
  /** `closed`: the stream won't reconnect (the server refused it). */
  onConnected?: (connected: boolean, closed?: boolean) => void;
}

/**
 * Subscribes to the live tail; `q` is a `POST /api/query` body. EventSource reconnects on its own; each
 * reconnect sends `hello` again.
 */
export function subscribe(query: { project?: string; trace?: string; q?: object }, h: LiveHandlers): () => void {
  const q = new URLSearchParams();
  if (query.q) q.set("q", JSON.stringify(query.q));
  if (query.project) q.set("project", query.project);
  if (query.trace) q.set("trace", query.trace);
  const es = new EventSource(`/api/tail?${q}`);
  es.addEventListener("hello", () => {
    h.onConnected?.(true);
    h.onHello();
  });
  es.addEventListener("runs", (e) => h.onRuns(JSON.parse((e as MessageEvent).data)));
  es.addEventListener("left", (e) => h.onLeft?.(JSON.parse((e as MessageEvent).data)));
  es.addEventListener("resync", () => h.onResync());
  es.onerror = () => h.onConnected?.(false, es.readyState === 2);
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

/** The start time in a dotted order's last segment (`20261105T020100123456Z…`), as the server sorts it. */
const startKey = (r: RunHeader) => {
  const seg = r.dotted_order.slice(r.dotted_order.lastIndexOf(".") + 1);
  return seg.length < 22 ? "" : seg.slice(0, 22);
};

/** The order `POST /api/query` returns: runs without a latency sort as the smallest; ties by run id, ascending. */
export function compareRuns(order: Order): (a: RunHeader, b: RunHeader) => number {
  const key = order.by === "latency" ? (r: RunHeader) => r.latency_ms ?? -1 : startKey;
  const sign = order.dir === "asc" ? 1 : -1;
  const cmp = <T>(x: T, y: T) => (x < y ? -1 : x > y ? 1 : 0);
  return (a, b) => sign * cmp(key(a), key(b)) || cmp(a.run_id, b.run_id);
}

/**
 * Live runs into a loaded page: newer versions replace rows; new runs join where they sort, unless more pages
 * remain and they sort past the last loaded row (paging brings those).
 */
export function mergeLive<T extends RunHeader>(rows: T[], runs: T[], order: Order, more: boolean): T[] {
  const cmp = compareRuns(order);
  const fresh = new Map(runs.map((r) => [r.run_id, r]));
  let changed = false;
  const out = rows.map((r) => {
    const n = fresh.get(r.run_id);
    fresh.delete(r.run_id);
    if (!n || n.last_seq <= r.last_seq) return r;
    changed = true;
    return n;
  });
  const end = out[out.length - 1];
  for (const n of fresh.values()) {
    if (more && end && cmp(n, end) > 0) continue;
    out.push(n);
    changed = true;
  }
  return changed ? out.sort(cmp) : rows;
}

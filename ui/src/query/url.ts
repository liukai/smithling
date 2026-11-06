import { print } from "./print";

export type Slot = "filter" | "trace" | "tree";
export const SLOTS: Slot[] = ["filter", "trace", "tree"];
/** The API's name for each URL slot. */
export const API_SLOT: Record<Slot, string> = { filter: "filter", trace: "trace_filter", tree: "tree_filter" };

export interface Order { by: "start_time" | "latency"; dir: "desc" | "asc" }

/** The runs view as the URL describes it: `?project=&time=&filter=&trace=&tree=&root=1&order=&limit=&cols=&view=&live=1`. */
export interface QueryState {
  project?: string;
  /** A span such as `12h`; none = all time. */
  time?: string;
  filter?: string;
  trace?: string;
  tree?: string;
  root: boolean;
  order: Order;
  limit: number;
  cols: string[];
  view: "samples" | "table";
  live: boolean;
}

export const DEFAULT_LIMIT = 50;

export function fromUrl(search: string): { q: QueryState; m1: boolean } {
  const p = new URLSearchParams(search);
  const keys = new Set(p.keys());
  const m1 = keys.size > 0 && [...keys].every((k) => k === "project" || k === "status");
  const status = p.get("status");
  const [by, dir] = (p.get("order") ?? "").split(".");
  const limit = Number(p.get("limit"));
  const q: QueryState = {
    project: p.get("project") || undefined,
    time: p.get("time") || undefined,
    filter: p.get("filter") || undefined,
    trace: p.get("trace") || undefined,
    tree: p.get("tree") || undefined,
    root: p.get("root") === "1",
    order: { by: by === "latency" ? "latency" : "start_time", dir: dir === "asc" ? "asc" : "desc" },
    limit: Number.isInteger(limit) && limit >= 1 && limit <= 200 ? limit : DEFAULT_LIMIT,
    cols: (p.get("cols") ?? "").split(",").filter(Boolean),
    view: p.get("view") === "table" ? "table" : "samples",
    live: p.get("live") === "1",
  };
  if (m1) {
    // M1's runs list showed root runs, optionally with one status.
    q.root = true;
    if (status) q.filter = print({ t: "cmp", op: "eq", path: "status", value: status });
  }
  return { q, m1 };
}

export function toUrl(q: QueryState): string {
  const p = new URLSearchParams();
  const set = (k: string, v: string | undefined) => { if (v) p.set(k, v); };
  set("project", q.project);
  set("time", q.time);
  set("filter", q.filter);
  set("trace", q.trace);
  set("tree", q.tree);
  if (q.root) p.set("root", "1");
  if (q.order.by !== "start_time" || q.order.dir !== "desc") p.set("order", `${q.order.by}.${q.order.dir}`);
  if (q.limit !== DEFAULT_LIMIT) p.set("limit", String(q.limit));
  set("cols", q.cols.join(","));
  if (q.view !== "samples") p.set("view", q.view);
  if (q.live) p.set("live", "1");
  // `?project=` alone reads as an M1 link (roots only), so say "all runs" explicitly.
  if ([...p.keys()].join() === "project") p.set("root", "0");
  const s = [...p].map(([k, v]) => `${k}=${encodeURIComponent(v).replace(/%2C/g, ",").replace(/%3A/g, ":")}`).join("&");
  return s ? `/?${s}` : "/";
}

/** The `POST /api/query` body. */
export function toBody(q: QueryState, cursor?: string | null) {
  return {
    project: q.project,
    time: q.time ? { last: q.time } : undefined,
    filter: q.filter,
    trace_filter: q.trace,
    tree_filter: q.tree,
    is_root: q.root || undefined,
    order: q.order,
    limit: q.limit,
    cursor: cursor ?? undefined,
    columns: q.cols.length ? q.cols : undefined,
  };
}

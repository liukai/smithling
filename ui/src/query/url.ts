import type { Expr } from "./ast";
import { parse } from "./parse";
import { print } from "./print";
import { DEFAULT_TOP } from "./stats";

export type Slot = "filter" | "trace" | "tree";
export const SLOTS: Slot[] = ["filter", "trace", "tree"];
/** The API's name for each URL slot. */
export const API_SLOT: Record<Slot, string> = { filter: "filter", trace: "trace_filter", tree: "tree_filter" };

export interface Order { by: "start_time" | "latency"; dir: "desc" | "asc" }

export type View = "samples" | "table" | "stats" | "series" | "dist";
const VIEW_NAMES: View[] = ["samples", "table", "stats", "series", "dist"];
export const DEFAULT_METRICS = ["count", "error_rate", "p95(latency)", "sum(cost)"];
/** Time-series bucket widths the server takes; none = `auto`. */
export const BUCKETS = ["1m", "5m", "15m", "1h", "6h", "1d"];
const SHIFT = /^[1-9][0-9]*[smhd]$/;

/**
 * The runs view as the URL describes it:
 * `?project=&time=&filter=&q=&trace=&tree=&root=1&order=&limit=&cols=&view=&by=&m=&top=&bucket=&cmp=&shift=&live=1`.
 */
export interface QueryState {
  project?: string;
  /** A span such as `12h`; none = all time. */
  time?: string;
  filter?: string;
  /** The search box: one more `search("…")` in the filter slot. */
  q?: string;
  trace?: string;
  tree?: string;
  root: boolean;
  order: Order;
  limit: number;
  cols: string[];
  view: View;
  /** Stats: group keys, metrics and top N, as the stats line writes them. */
  by: string[];
  m: string[];
  top: number;
  bucket?: string;
  /** Compare: side b is the query with this filter in place of `filter`, or the window `shift` earlier. */
  cmp?: string;
  shift?: string;
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
  const top = Number(p.get("top"));
  const list = (k: string) => (p.get(k) ?? "").split(",").filter(Boolean);
  const view = p.get("view") as View;
  const q: QueryState = {
    project: p.get("project") || undefined,
    time: p.get("time") || undefined,
    filter: p.get("filter") || undefined,
    q: p.get("q") || undefined,
    trace: p.get("trace") || undefined,
    tree: p.get("tree") || undefined,
    root: p.get("root") === "1",
    order: { by: by === "latency" ? "latency" : "start_time", dir: dir === "asc" ? "asc" : "desc" },
    limit: Number.isInteger(limit) && limit >= 1 && limit <= 200 ? limit : DEFAULT_LIMIT,
    cols: (p.get("cols") ?? "").split(",").filter(Boolean),
    view: VIEW_NAMES.includes(view) ? view : "samples",
    by: list("by"),
    m: list("m").length ? list("m") : DEFAULT_METRICS,
    top: Number.isInteger(top) && top >= 1 && top <= 50 ? top : DEFAULT_TOP,
    bucket: BUCKETS.find((b) => b === p.get("bucket")),
    cmp: p.get("cmp") || undefined,
    shift: p.get("cmp") ? undefined : (SHIFT.exec(p.get("shift") ?? "")?.[0]),
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
  set("q", q.q);
  set("trace", q.trace);
  set("tree", q.tree);
  if (q.root) p.set("root", "1");
  if (q.order.by !== "start_time" || q.order.dir !== "desc") p.set("order", `${q.order.by}.${q.order.dir}`);
  if (q.limit !== DEFAULT_LIMIT) p.set("limit", String(q.limit));
  set("cols", q.cols.join(","));
  if (q.view !== "samples") p.set("view", q.view);
  set("by", q.by.join(","));
  if (q.m.join() !== DEFAULT_METRICS.join()) set("m", q.m.join(","));
  if (q.top !== DEFAULT_TOP) p.set("top", String(q.top));
  set("bucket", q.bucket);
  set("cmp", q.cmp);
  set("shift", q.shift);
  if (q.live) p.set("live", "1");
  // `?project=` alone reads as an M1 link (roots only), so say "all runs" explicitly.
  if ([...p.keys()].join() === "project") p.set("root", "0");
  const s = [...p].map(([k, v]) => `${k}=${encodeURIComponent(v).replace(/%2C/g, ",").replace(/%3A/g, ":")}`).join("&");
  return s ? `/?${s}` : "/";
}

export const boxSearch = (text: string): Expr => ({ t: "search", side: "any", text });

/** The filter slot's whole text: `filter` and then the search box's `search("…")`. */
export function filterOf(q: QueryState): string | undefined {
  if (!q.q) return q.filter;
  const box = boxSearch(q.q);
  if (!q.filter) return print(box);
  const r = parse(q.filter);
  if (!r.ok) return `and(${q.filter}, ${print(box)})`;
  return print({ t: "and", args: [...(r.expr.t === "and" ? r.expr.args : [r.expr]), box] });
}

/** A slot's text as the panel and the server see it. */
export const slotText = (q: QueryState, s: Slot) => (s === "filter" ? filterOf(q) : q[s]);

const scope = (q: QueryState) => ({
  project: q.project,
  time: q.time ? { last: q.time } : undefined,
  filter: filterOf(q),
  trace_filter: q.trace,
  tree_filter: q.tree,
  is_root: q.root || undefined,
});

const AGG_VIEW: Partial<Record<View, string>> = { series: "series", dist: "dist" };

/** The `POST /api/aggregate` body; side b keeps the search box. */
export function aggBody(q: QueryState) {
  const view = AGG_VIEW[q.view] ?? "table";
  const compare = q.cmp ? { filter: filterOf({ ...q, filter: q.cmp }) } : q.shift ? { shift: q.shift } : undefined;
  return {
    ...scope(q), group_by: q.by, metrics: q.m, top: q.top, view,
    bucket: view === "series" ? q.bucket : undefined, compare,
  };
}

/** The `POST /api/query` body. */
export function toBody(q: QueryState, cursor?: string | null) {
  return {
    ...scope(q),
    order: q.order,
    limit: q.limit,
    cursor: cursor ?? undefined,
    columns: q.view === "table" && q.cols.length ? q.cols : undefined,
  };
}

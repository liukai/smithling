export type Status = "running" | "ok" | "error";

export interface RunHeader {
  run_id: string;
  trace_id: string;
  parent_run_id: string | null;
  dotted_order: string;
  depth: number;
  is_root: boolean;
  project: string;
  address: string | null;
  tags: string[];
  metadata: Record<string, unknown>;
  extra: Record<string, unknown>;
  thread_id: string | null;
  name: string | null;
  run_type: string;
  run_type_raw: string | null;
  start_time: string | null;
  end_time: string | null;
  error: string | null;
  status: Status;
  latency_ms: number | null;
  model: string | null;
  input_tokens: number | null;
  output_tokens: number | null;
  total_tokens: number | null;
  dropped: Record<string, number>;
  warnings: string[];
  payload_sizes: { inputs: number; outputs: number; events: number };
  child_count: number;
  first_seq: number;
  last_seq: number;
  event_count: number;
}

export interface RunView extends RunHeader {
  inputs?: unknown;
  outputs?: unknown;
  events?: unknown[];
}

export interface TraceView {
  root: RunHeader | null;
  runs: RunHeader[];
  run_count: number;
  has_running: boolean;
}

export interface ProjectInfo {
  name: string;
  runs: number;
}

/** Where the query's search matched: `start..end` count characters (code points) of `excerpt`. */
export interface Match { path: string; excerpt: string; start: number; end: number }

export interface QueryRun extends RunHeader {
  columns?: Record<string, unknown>;
  match?: Match;
}

export type QueryWarning =
  | { kind: "mixed_types"; path: string; counts: Record<string, number> }
  | { kind: "not_indexed"; path: string; runs: number }
  | { kind: "not_indexed"; files: string[] }
  | { kind: "skipped_files"; ids: string[] }
  | { kind: "identity_conflict"; run_id: string };

export interface PageStats {
  files_total: number;
  files_scanned: number;
  rows_read: number;
  rows_merged: number;
  index_files?: number;
  scan_files?: number;
  candidates?: number;
  payload_rows?: number;
  cache_hits?: number;
  matched: number;
  complete?: boolean;
  ms: number;
}

export interface QueryPage {
  runs: QueryRun[];
  next_cursor: string | null;
  warnings: QueryWarning[];
  stats?: PageStats;
}

/** One group's figures; latency in ms, cost in picodollars. Missing values are null, never 0. */
export interface Cells {
  runs: number;
  finished: number;
  running: number;
  errors: number;
  error_rate: number | null;
  p50: number | null;
  p95: number | null;
  p99: number | null;
  n_latency: number;
  input_tokens: number | null;
  output_tokens: number | null;
  total_tokens: number | null;
  cost_pico: number | null;
  no_price_runs: number;
  feedback_avg: number | null;
  feedback_n: number;
  estimated: boolean;
  /** Percentiles came from a histogram. */
  approximate: boolean;
}

/** `where` is filter text selecting exactly the group's runs; null when no filter can say it. */
export interface AggGroup { key: unknown[]; cells: Cells | null; where: string | null }

/** One time bucket: `t` alone, `offset_ms` from the side's start when compared; one cell per group (null: no runs). */
export interface SeriesBucket { t?: string; offset_ms?: number; per_group: (Cells | null)[]; where: string }
export interface Series { step_ms: number; offset: boolean; buckets: SeriesBucket[] }
/** Log-latency bins: lower bounds in ms; finished runs per group per bin. */
export interface Dist { bins_ms: number[]; per_group: number[][]; where_per_bin: string[] }

export interface AggResult {
  query_id: string;
  partial: boolean;
  /** Percent of rows read for an estimate. */
  sampled_pct?: number;
  totals: Cells;
  groups: AggGroup[];
  cut: boolean;
  overlap: boolean;
  series?: Series;
  dist?: Dist;
  /** Side b: its groups pair with side a's by position. */
  compare?: { side_b: AggResult };
  warnings?: QueryWarning[];
  skipped_files?: string[];
  stats: { files_scanned: number; clean_rows: number; dirty_runs: number; ms: number };
}

export interface FieldInfo {
  path: string;
  types: { type: string; count: number }[];
  complete: boolean;
}

export interface FieldValues {
  values: { value: unknown; count: number }[];
  other_count: number;
  approximate: boolean;
}

/** An error response; `bad_filter` carries the slot and UTF-8 byte range it points at. */
export class ApiError extends Error {
  constructor(
    message: string,
    public code?: string,
    public slot?: string,
    public start?: number,
    public end?: number,
  ) {
    super(message);
  }
}

async function json<T>(r: Response): Promise<T> {
  if (!r.ok) {
    const e = (await r.json().catch(() => null))?.error;
    throw new ApiError(e?.message ?? `${r.status} ${r.statusText}`, e?.code, e?.slot, e?.start, e?.end);
  }
  return r.json();
}

const get = <T>(path: string) => fetch(path).then((r) => json<T>(r));

export const postQuery = (body: object) =>
  fetch("/api/query", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) })
    .then((r) => json<QueryPage>(r));

export const postAggregate = (body: object) =>
  fetch("/api/aggregate", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) })
    .then((r) => json<AggResult>(r));

export interface AggHandlers {
  onEstimate: (r: AggResult) => void;
  onResult: (r: AggResult) => void;
  onError: (e: ApiError) => void;
}

/**
 * `GET /api/aggregate/stream`: an `estimate` (maybe), then the `result` or an `error`. Events for another
 * `query_id` than the stream's first are ignored; a dropped connection is an error, not a silent re-run.
 * The returned function closes the stream, which cancels the query.
 */
export function streamAggregate(body: object, h: AggHandlers): () => void {
  const es = new EventSource(`/api/aggregate/stream?${new URLSearchParams({ q: JSON.stringify(body) })}`);
  let id: string | null = null;
  let done = false;
  const close = () => { done = true; es.close(); };
  const read = (e: Event): { query_id?: string | null } & Record<string, any> | null => {
    if (done) return null;
    const d = JSON.parse((e as MessageEvent).data);
    if (d.query_id != null) {
      id ??= d.query_id;
      if (d.query_id !== id) return null;
    }
    return d;
  };
  es.addEventListener("estimate", (e) => { const d = read(e); if (d) h.onEstimate(d as AggResult); });
  es.addEventListener("result", (e) => { const d = read(e); if (d) { close(); h.onResult(d as AggResult); } });
  es.addEventListener("error", (e) => {
    if (done) return;
    if (typeof (e as MessageEvent).data !== "string") {
      close();
      return h.onError(new ApiError("Lost the connection to smithling; change the query to retry.", "disconnected"));
    }
    const d = read(e);
    if (!d) return;
    close();
    h.onError(new ApiError(d.message ?? "query failed", d.code, d.slot, d.start, d.end));
  });
  return close;
}

export const getProjects = () => get<ProjectInfo[]>("/api/projects");
export const getTrace = (id: string) => get<TraceView>(`/api/traces/${id}`);
export const getRunHeader = (id: string) => get<RunHeader>(`/api/runs/${id}`);
export const getRun = (id: string) => get<RunView>(`/api/runs/${id}?include=inputs,outputs,events`);


/** The catalog's scope: a project and a span such as `12h`. */
export interface FieldScope { project?: string; time?: string }

const qs = (p: Record<string, string | undefined>) => {
  const s = new URLSearchParams(Object.entries(p).filter((e): e is [string, string] => Boolean(e[1]))).toString();
  return s ? `?${s}` : "";
};

export const getFields = (s: FieldScope, prefix: string) =>
  get<{ fields: FieldInfo[]; complete: boolean }>(`/api/fields${qs({ project: s.project, time: s.time, prefix })}`);
export const getFieldValues = (s: FieldScope, path: string, prefix: string) =>
  get<FieldValues>(`/api/fields/values${qs({ project: s.project, time: s.time, path, prefix })}`);

/** One feedback key in scope: how many items, mean score (null when none has a score). */
export interface FeedbackKey { key: string; count: number; avg: number | null }

export const getFeedbackKeys = (s: FieldScope) =>
  get<{ keys: FeedbackKey[] }>(`/api/feedback${qs({ keys: "true", project: s.project, time: s.time })}`);
export const getOrphanFeedbackCount = () => get<{ orphan_feedback?: number }>("/healthz").then((h) => h.orphan_feedback ?? 0);

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

export interface QueryRun extends RunHeader {
  columns?: Record<string, unknown>;
}

export type QueryWarning =
  | { kind: "mixed_types"; path: string; counts: Record<string, number> }
  | { kind: "not_indexed"; path: string; runs: number }
  | { kind: "identity_conflict"; run_id: string };

export interface QueryPage {
  runs: QueryRun[];
  next_cursor: string | null;
  warnings: QueryWarning[];
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

export const getProjects = () => get<ProjectInfo[]>("/api/projects");
export const getTrace = (id: string) => get<TraceView>(`/api/traces/${id}`);
export const getRun = (id: string) => get<RunView>(`/api/runs/${id}?include=inputs,outputs,events`);


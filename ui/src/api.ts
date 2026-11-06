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

export interface RootsPage {
  runs: RunHeader[];
  next_cursor: string | null;
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

async function get<T>(path: string): Promise<T> {
  const r = await fetch(path);
  if (!r.ok) {
    const body = await r.json().catch(() => null);
    throw new Error(body?.error?.message ?? `${r.status} ${r.statusText}`);
  }
  return r.json();
}

export function getRoots(p: { project?: string; status?: string; cursor?: string; limit?: number }): Promise<RootsPage> {
  const q = new URLSearchParams();
  if (p.project) q.set("project", p.project);
  if (p.status) q.set("status", p.status);
  if (p.cursor) q.set("cursor", p.cursor);
  q.set("limit", String(p.limit ?? 50));
  return get(`/api/roots?${q}`);
}

export const getProjects = () => get<ProjectInfo[]>("/api/projects");
export const getTrace = (id: string) => get<TraceView>(`/api/traces/${id}`);
export const getRun = (id: string) => get<RunView>(`/api/runs/${id}?include=inputs,outputs,events`);

/** Root list order: newest start first, then run id. */
export function rootOrder(a: RunHeader, b: RunHeader): number {
  const ta = a.start_time ?? "", tb = b.start_time ?? "";
  if (ta !== tb) return ta < tb ? 1 : -1;
  return a.run_id < b.run_id ? -1 : a.run_id > b.run_id ? 1 : 0;
}

import { vi } from "vitest";
import type { Cells, RunHeader } from "./api";

export class FakeEventSource {
  static last: FakeEventSource | null = null;
  listeners = new Map<string, ((e: MessageEvent) => void)[]>();
  onerror: (() => void) | null = null;
  constructor(public url: string) {
    FakeEventSource.last = this;
  }
  addEventListener(type: string, fn: (e: MessageEvent) => void) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }
  emit(type: string, data: unknown = {}) {
    for (const fn of this.listeners.get(type) ?? []) fn(new MessageEvent(type, { data: JSON.stringify(data) }));
  }
  closed = false;
  readyState = 0;
  close() { this.closed = true; }
}

export function header(n: number, over: Partial<RunHeader> = {}): RunHeader {
  const id = `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
  return {
    run_id: id, trace_id: id, parent_run_id: null, dotted_order: `20261105T02${String(n).padStart(2, "0")}00000000Z${id}`, depth: 0, is_root: true,
    project: "research-agent", address: null, tags: ["batch=a"], metadata: {}, extra: {}, thread_id: null,
    name: `task-${n}`, run_type: "chain", run_type_raw: null,
    start_time: `2026-11-05T02:${String(n).padStart(2, "0")}:00.000000Z`, end_time: null, error: null,
    status: "running", latency_ms: null, model: null, input_tokens: null, output_tokens: null, total_tokens: null,
    dropped: {}, warnings: [], payload_sizes: { inputs: 10, outputs: 0, events: 0 }, child_count: 0,
    first_seq: n, last_seq: n, event_count: 1, ...over,
  };
}

/** Routes fetch calls to handlers by path prefix; a handler may return a `Response` as is. */
export function mockFetch(routes: Record<string, (url: URL, body: any) => unknown>) {
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (input: string, init?: RequestInit) => {
    const url = new URL(input, "http://localhost");
    calls.push(url.pathname + url.search);
    const key = Object.keys(routes).find((k) => url.pathname.startsWith(k));
    if (!key) return new Response(JSON.stringify({ error: { message: "not found" } }), { status: 404 });
    const out = await routes[key](url, init?.body ? JSON.parse(String(init.body)) : undefined);
    if (out instanceof Response) return out;
    return new Response(JSON.stringify(out), { status: 200, headers: { "content-type": "application/json" } });
  }));
  return calls;
}

export const queryPage = (runs: unknown[], next_cursor: string | null = null) =>
  ({ runs, next_cursor, warnings: [], stats: { files_total: 0, files_scanned: 0, rows_read: 0, rows_merged: 0, matched: 0, ms: 1 } });

export const cells = (over: Partial<Cells> = {}): Cells => ({
  runs: 0, finished: 0, running: 0, errors: 0, error_rate: null, p50: null, p95: null, p99: null, n_latency: 0,
  input_tokens: null, output_tokens: null, total_tokens: null, cost_pico: null, no_price_runs: 0,
  feedback_avg: null, feedback_n: 0, estimated: false, approximate: false, ...over,
});

/**
 * `GET /api/aggregate/stream` answering each stream once with `agg(body)` as its `result`, or a `Response` error
 * body as its `error` event; `bodies` collects the requests. Streams stay open until closed, as a server's would.
 */
export function stubAggStream(agg: (body: any) => unknown, bodies: unknown[] = []) {
  const streams: FakeEventSource[] = [];
  vi.stubGlobal("EventSource", class extends FakeEventSource {
    constructor(url: string) {
      super(url);
      streams.push(this);
      const body = JSON.parse(new URL(url, "http://localhost").searchParams.get("q")!);
      bodies.push(body);
      Promise.resolve(agg(body)).then(async (out) => {
        if (this.closed || out === undefined) return;
        if (out instanceof Response) this.emit("error", { ...(await out.json()).error, query_id: null });
        else this.emit("result", out);
      });
    }
  });
  return { bodies, streams };
}

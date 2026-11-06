import { vi } from "vitest";
import type { RunHeader } from "./api";

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
  close() {}
}

export function header(n: number, over: Partial<RunHeader> = {}): RunHeader {
  const id = `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
  return {
    run_id: id, trace_id: id, parent_run_id: null, dotted_order: id, depth: 0, is_root: true,
    project: "research-agent", address: null, tags: ["batch=a"], metadata: {}, extra: {}, thread_id: null,
    name: `task-${n}`, run_type: "chain", run_type_raw: null,
    start_time: `2026-11-05T02:${String(n).padStart(2, "0")}:00.000000Z`, end_time: null, error: null,
    status: "running", latency_ms: null, model: null, input_tokens: null, output_tokens: null, total_tokens: null,
    dropped: {}, warnings: [], payload_sizes: { inputs: 10, outputs: 0, events: 0 }, child_count: 0,
    first_seq: n, last_seq: n, event_count: 1, ...over,
  };
}

/** Routes fetch calls to handlers by path prefix. */
export function mockFetch(routes: Record<string, (url: URL) => unknown>) {
  const calls: string[] = [];
  vi.stubGlobal("fetch", vi.fn(async (input: string) => {
    const url = new URL(input, "http://localhost");
    calls.push(url.pathname + url.search);
    const key = Object.keys(routes).find((k) => url.pathname.startsWith(k));
    if (!key) return new Response(JSON.stringify({ error: { message: "not found" } }), { status: 404 });
    return new Response(JSON.stringify(routes[key](url)), { status: 200, headers: { "content-type": "application/json" } });
  }));
  return calls;
}

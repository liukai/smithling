import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TraceView } from "./TraceView";
import { FakeEventSource, header, mockFetch } from "./testutil";

const root = header(1, { start_time: "2026-11-05T02:00:00.000Z", end_time: "2026-11-05T02:01:00.000Z", status: "ok", latency_ms: 60000, dotted_order: "a", child_count: 2 });
const kid = (n: number, order: string, over = {}) =>
  header(n, { is_root: false, parent_run_id: root.run_id, trace_id: root.trace_id, dotted_order: order, start_time: "2026-11-05T02:00:10.000Z", end_time: "2026-11-05T02:00:41.000Z", status: "ok", latency_ms: 31000, ...over });
const fetchUrl = kid(2, "a.1", { name: "fetch_url", run_type: "tool", status: "error", error: "HTTPError: 429 Too Many Requests", payload_sizes: { inputs: 30, outputs: 40000, events: 0 } });
const search = kid(3, "a.2", { name: "web_search" });

beforeEach(() => vi.stubGlobal("EventSource", FakeEventSource));
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const treeNames = () => screen.getAllByRole("treeitem").map((r) => r.querySelector(".tname")?.textContent);

describe("trace view", () => {
  it("renders the tree in order and opens a step's full payload", async () => {
    const calls = mockFetch({
      "/api/traces/": () => ({ root, runs: [root, fetchUrl, search], run_count: 3, has_running: false }),
      "/api/runs/": () => ({ ...fetchUrl, inputs: { url: "https://example.com" }, outputs: { status_code: 429, body: "x".repeat(100) } }),
    });
    const { rerender } = render(<TraceView traceId={root.trace_id} />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => expect(treeNames()).toEqual(["task-1", "fetch_url", "web_search"]));
    expect(FakeEventSource.last!.url).toContain(`trace=${root.trace_id}`);
    expect(calls.filter((c) => c.startsWith("/api/runs/"))).toEqual([]); // no payloads until a step is opened

    act(() => (screen.getAllByRole("treeitem")[1] as HTMLElement).click());
    expect(window.location.search).toContain(`run=${fetchUrl.run_id}`);
    rerender(<TraceView traceId={root.trace_id} selected={fetchUrl.run_id} />);
    await waitFor(() => screen.getByText("HTTPError: 429 Too Many Requests"));
    await waitFor(() => screen.getByText('"https://example.com"'));
    expect(screen.getByText("39.1 KB")).toBeTruthy();
  });

  it("collapses a branch and adds live steps", async () => {
    mockFetch({ "/api/traces/": () => ({ root, runs: [root, fetchUrl], run_count: 2, has_running: false }) });
    render(<TraceView traceId={root.trace_id} />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => expect(treeNames()).toEqual(["task-1", "fetch_url"]));

    act(() => FakeEventSource.last!.emit("runs", [search]));
    await waitFor(() => expect(treeNames()).toEqual(["task-1", "fetch_url", "web_search"]));

    act(() => (screen.getAllByRole("treeitem")[0].querySelector(".toggle") as HTMLElement).click());
    await waitFor(() => expect(treeNames()).toEqual(["task-1"]));
  });
});

import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { FakeEventSource, header, mockFetch, queryPage } from "../testutil";

beforeEach(() => { FakeEventSource.last = null; vi.stubGlobal("EventSource", FakeEventSource); });
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

type Body = Record<string, unknown>;
const id = (n: number) => header(n).run_id;

function setup(path: string, answer: (b: Body) => unknown = () => queryPage([header(2), header(1)])) {
  history.replaceState(null, "", path);
  const bodies: Body[] = [];
  const calls = mockFetch({
    "/api/query": (_u, b) => { bodies.push(b); return answer(b); },
    "/api/projects": () => [],
    "/api/runs/": (u) => header(Number(u.pathname.slice(-12)), { status: "ok", last_seq: 50, latency_ms: 2000 }),
  });
  render(<App />);
  return { bodies, calls };
}

const es = () => FakeEventSource.last!;
const params = () => Object.fromEntries(new URLSearchParams(window.location.search));
const rows = () => screen.getAllByRole("row").slice(1).map((r) =>
  [r.querySelector("td.name")?.textContent, r.querySelector(".status")?.textContent, r.classList.contains("left") ? "left" : ""].join(" ").trim());

describe("live toggle", () => {
  it("subscribes with the current query, keeps live=1 in the URL, and loads after hello", async () => {
    const filter = 'eq(status, "error")';
    const { bodies } = setup(`/?filter=${encodeURIComponent(filter)}&root=1`);
    await waitFor(() => expect(bodies.length).toBe(1));
    expect(FakeEventSource.last).toBeNull();

    act(() => screen.getByRole("button", { name: "live" }).click());
    expect(params().live).toBe("1");
    const q = JSON.parse(new URL(es().url, "http://x").searchParams.get("q")!);
    expect(new URL(es().url, "http://x").pathname).toBe("/api/tail");
    expect(q).toMatchObject({ filter, is_root: true, order: { by: "start_time", dir: "desc" } });
    expect(q.cursor).toBeUndefined();
    expect(bodies.length).toBe(1);
    act(() => es().emit("hello", { server_seq: 9 }));
    await waitFor(() => expect(bodies.length).toBe(2));
    expect(screen.getByText("● live")).toBeTruthy();

    const first = es();
    act(() => screen.getByRole("button", { name: "live" }).click());
    expect(first.closed).toBe(true);
    expect(params().live).toBeUndefined();
  });

  it("adds matching runs as they arrive and updates the ones shown", async () => {
    const { bodies } = setup("/?live=1");
    act(() => es().emit("hello"));
    await waitFor(() => expect(rows()).toEqual(["task-2 ● run", "task-1 ● run"]));
    expect(bodies.length).toBe(1);

    act(() => es().emit("runs", [header(3)]));
    await waitFor(() => expect(rows()).toEqual(["task-3 ● run", "task-2 ● run", "task-1 ● run"]));
    act(() => es().emit("runs", [header(1, { status: "ok", last_seq: 10 }), header(2, { status: "ok", last_seq: 1 })]));
    await waitFor(() => expect(rows()).toEqual(["task-3 ● run", "task-2 ● run", "task-1 ✓ ok"]));
  });

  it("leaves out new runs that sort past the loaded page", async () => {
    setup("/?live=1", (b) => queryPage(b.cursor ? [] : [header(5), header(4)], b.cursor ? null : "c1"));
    act(() => es().emit("hello"));
    await waitFor(() => expect(rows()).toEqual(["task-5 ● run", "task-4 ● run"]));
    act(() => es().emit("runs", [header(1), header(6)]));
    await waitFor(() => expect(rows()).toEqual(["task-6 ● run", "task-5 ● run", "task-4 ● run"]));
  });

  it("shows a run that stopped matching in its new state, marked no longer matching", async () => {
    const { calls } = setup("/?live=1");
    act(() => es().emit("hello"));
    await waitFor(() => expect(rows()).toHaveLength(2));

    act(() => es().emit("left", [id(1), id(9)]));
    await waitFor(() => expect(rows()).toEqual(["task-2 ● run", "task-1 ✓ ok left"]));
    expect(screen.getByText("no longer matching")).toBeTruthy();
    expect(calls.filter((c) => c.startsWith("/api/runs/"))).toEqual([`/api/runs/${id(1)}`]);

    act(() => es().emit("runs", [header(1, { status: "error", last_seq: 60 })]));
    await waitFor(() => expect(rows()).toEqual(["task-2 ● run", "task-1 ✕ err"]));
  });

  it("reloads on resync and clears the marks", async () => {
    const { bodies } = setup("/?live=1");
    act(() => es().emit("hello"));
    await waitFor(() => expect(rows()).toHaveLength(2));
    act(() => es().emit("left", [id(2)]));
    await waitFor(() => expect(rows()[0]).toBe("task-2 ✓ ok left"));

    act(() => es().emit("resync"));
    await waitFor(() => expect(bodies.length).toBe(2));
    await waitFor(() => expect(rows()).toEqual(["task-2 ● run", "task-1 ● run"]));
  });

  it("still shows the query's error when the stream can't open", async () => {
    const { bodies } = setup(`/?live=1&filter=${encodeURIComponent('eq(name, "x")')}`, () => new Response(
      JSON.stringify({ error: { code: "bad_filter", slot: "filter", start: 3, end: 7, message: "no" } }), { status: 400 }));
    act(() => { es().readyState = 2; es().onerror!(); });
    await waitFor(() => expect(bodies.length).toBe(1));
    await waitFor(() => expect(screen.getByText("The query has an error; open Text to see it.")).toBeTruthy());
    expect(screen.getByText("○ live unavailable")).toBeTruthy();
  });

  it("breaks ties by run id as the server does, in either direction", async () => {
    setup("/?live=1&order=latency.desc");
    act(() => es().emit("hello"));
    await waitFor(() => expect(rows()).toEqual(["task-2 ● run", "task-1 ● run"]));
    act(() => es().emit("runs", [header(3), header(1, { last_seq: 9, status: "ok", latency_ms: 5 })]));
    await waitFor(() => expect(rows()).toEqual(["task-1 ✓ ok", "task-2 ● run", "task-3 ● run"]));
  });

  it("applies changes that arrive while the page loads", async () => {
    let resolve: (v: unknown) => void = () => {};
    setup("/?live=1", () => new Promise((r) => { resolve = r; }));
    act(() => es().emit("hello"));
    act(() => es().emit("runs", [header(1, { status: "ok", last_seq: 10 }), header(3)]));
    act(() => es().emit("left", [id(2)]));
    await act(async () => resolve(queryPage([header(2), header(1)])));
    await waitFor(() => expect(rows()).toEqual(["task-3 ● run", "task-2 ✓ ok left", "task-1 ✓ ok"]));
  });
});

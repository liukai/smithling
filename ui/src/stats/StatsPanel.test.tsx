import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import type { Cells } from "../api";
import { DEFAULT_METRICS, fromUrl, toUrl } from "../query/url";
import { header, mockFetch, queryPage } from "../testutil";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

type Body = Record<string, unknown>;
const cells = (over: Partial<Cells>): Cells => ({
  runs: 0, finished: 0, running: 0, errors: 0, error_rate: null, p50: null, p95: null, p99: null, n_latency: 0,
  input_tokens: null, output_tokens: null, total_tokens: null, cost_pico: null, no_price_runs: 0,
  feedback_avg: null, feedback_n: 0, estimated: false, approximate: false, ...over,
});
const AGG = {
  query_id: "q1", partial: false, cut: true, overlap: false,
  totals: cells({ runs: 4210, finished: 4210, errors: 56, error_rate: 0.0133, p95: 12000, n_latency: 4210, cost_pico: 4_120_000_000_000, no_price_runs: 12 }),
  groups: [
    { key: ["fetch_url"], cells: cells({ runs: 300, errors: 42, error_rate: 0.14, p95: 31000, n_latency: 300 }), where: 'eq(name, "fetch_url")' },
    { key: ["llm_call"], cells: cells({ runs: 1112, error_rate: 0, p95: 2400, n_latency: 1112, cost_pico: 4_115_000_000_000, no_price_runs: 12 }), where: 'eq(name, "llm_call")' },
    { key: ["(other)"], cells: cells({ runs: 2798, errors: 14, error_rate: 0.005, p95: 900, n_latency: 2798, approximate: true }), where: null },
  ],
  stats: { files_scanned: 3, clean_rows: 4000, dirty_runs: 210, ms: 12 },
};

function setup(path: string, agg: (b: Body) => unknown = () => AGG) {
  history.replaceState(null, "", path);
  const aggs: Body[] = [];
  const queries: Body[] = [];
  mockFetch({
    "/api/aggregate": (_u, b) => { aggs.push(b); return agg(b); },
    "/api/query": (_u, b) => { queries.push(b); return queryPage([header(1)]); },
    "/api/projects": () => [],
  });
  render(<App />);
  return { aggs, queries };
}

const last = (bodies: Body[]) => bodies[bodies.length - 1];
const params = () => Object.fromEntries(new URLSearchParams(window.location.search));
const table = () => screen.getByRole("table", { name: "stats" });
const lines = () => within(table()).getAllByRole("row").map((r) => [...r.querySelectorAll("th, td")].map((c) => c.textContent));
const rowOf = (key: string) => within(table()).getAllByRole("row").find((r) => r.querySelector("td")?.textContent === key)!;
const groupChips = () => [...screen.getByRole("group", { name: "Group by" }).querySelectorAll(".chip-label")].map((c) => c.textContent);
const metric = (name: string) => within(screen.getByRole("group", { name: "Metrics" })).getByRole("button", { name });
const filterChips = () => [...screen.getByRole("group", { name: "This run" }).querySelectorAll(".chip-label")].map((c) => c.textContent);
const BATCH = encodeURIComponent('has(tags, "batch=a")');

describe("stats view", () => {
  it("asks for the groups and metrics the pickers show, and shows n, cost and no-price counts", async () => {
    const { aggs } = setup(`/?filter=${BATCH}&view=stats&by=name`);
    await waitFor(() => expect(aggs.length).toBe(1));
    expect(last(aggs)).toEqual({
      filter: 'has(tags, "batch=a")', group_by: ["name"], metrics: DEFAULT_METRICS, top: 10, view: "table",
    });
    await waitFor(() => screen.getByRole("table", { name: "stats" }));
    expect(lines()).toEqual([
      ["name", "runs", "error rate", "p95 latency", "cost"],
      ["fetch_url", "300", "14.0%", "31.0 sn=300", "—"],
      ["llm_call", "1,112", "0.0%", "2.40 sn=1,112", "$4.12 12 runs had no price"],
      ["(other)", "2,798", "0.5%", "≈900 msn=2,798", "—"],
      ["all", "4,210", "1.3%", "12.0 sn=4,210", "$4.12 12 runs had no price"],
    ]);
    expect(screen.getByText("Top 10 groups shown; the rest are in (other).")).toBeTruthy();

    expect(groupChips()).toEqual(["name"]);
    expect(metric("p95 latency").getAttribute("aria-pressed")).toBe("true");
    act(() => metric("p99 latency").click());
    await waitFor(() => expect(last(aggs).metrics).toEqual([...DEFAULT_METRICS, "p99(latency)"]));
    expect(params().m).toBe("count,error_rate,p95(latency),sum(cost),p99(latency)");
    act(() => metric("cost").click());
    await waitFor(() => expect(last(aggs).metrics).toEqual(["count", "error_rate", "p95(latency)", "p99(latency)"]));

    fireEvent.change(screen.getByLabelText("add group"), { target: { value: "metadata.prompt_version" } });
    fireEvent.submit(screen.getByLabelText("add group"));
    await waitFor(() => expect(last(aggs).group_by).toEqual(["name", "metadata.prompt_version"]));
    expect(params().by).toBe("name,metadata.prompt_version");
    expect(screen.queryByLabelText("add group")).toBeNull();
    act(() => screen.getByRole("button", { name: "remove group name" }).click());
    await waitFor(() => expect(last(aggs).group_by).toEqual(["metadata.prompt_version"]));

    fireEvent.change(screen.getByLabelText("top"), { target: { value: "5" } });
    await waitFor(() => expect(last(aggs).top).toBe(5));
    expect(params()).toMatchObject({ view: "stats", by: "metadata.prompt_version", top: "5" });
  });

  it("doesn't draw an old result under new headers while the next one loads", async () => {
    let release: (v: unknown) => void = () => {};
    const { aggs } = setup("/?view=stats&by=name", (b) => ((b.group_by as string[]).length === 2
      ? new Promise((r) => { release = r; })
      : AGG));
    await waitFor(() => screen.getByRole("table", { name: "stats" }));
    fireEvent.change(screen.getByLabelText("add group"), { target: { value: "model" } });
    fireEvent.submit(screen.getByLabelText("add group"));
    await waitFor(() => expect(aggs.length).toBe(2));
    expect(lines()[0]).toEqual(["name", "runs", "error rate", "p95 latency", "cost"]);
    await act(async () => release({ ...AGG, cut: false, groups: [{ ...AGG.groups[0], key: ["fetch_url", "gpt-4o"] }] }));
    await waitFor(() => expect(lines()[0]).toEqual(["name", "model", "runs", "error rate", "p95 latency", "cost"]));
    expect(lines()[1].slice(0, 3)).toEqual(["fetch_url", "gpt-4o", "300"]);
  });

  it("refuses a group key the stats line doesn't know", async () => {
    const { aggs } = setup("/?view=stats");
    await waitFor(() => expect(aggs.length).toBe(1));
    fireEvent.change(screen.getByLabelText("add group"), { target: { value: "colour" } });
    fireEvent.submit(screen.getByLabelText("add group"));
    expect(screen.getByRole("alert").textContent).toBe(
      'unknown group key "colour"; expected name, run_type, model, status, tag, feedback_key or metadata.<key>');
    expect(aggs.length).toBe(1);
  });

  it("edits the same spec as a stats line in Text", async () => {
    const { aggs } = setup("/?view=stats&by=name");
    await waitFor(() => expect(aggs.length).toBe(1));
    act(() => within(screen.getByRole("group", { name: "Stats" })).getByRole("button", { name: "Text" }).click());
    const line = screen.getByLabelText("stats line") as HTMLInputElement;
    expect(line.value).toBe("by name | count, error_rate, p95(latency), sum(cost)");

    fireEvent.change(line, { target: { value: "by name | p42(latency)" } });
    fireEvent.submit(line);
    expect(screen.getByText('unknown metric "p42"; expected p50, p95 or p99')).toBeTruthy();
    expect(document.querySelector(".stats-line .err-mirror mark")!.textContent).toBe("p42");
    expect(aggs.length).toBe(1);

    fireEvent.change(line, { target: { value: "by  name,metadata.v | count,p99( latency ) | top 5" } });
    fireEvent.submit(line);
    await waitFor(() => expect(last(aggs)).toMatchObject({ group_by: ["name", "metadata.v"], metrics: ["count", "p99(latency)"], top: 5 }));
    expect(params()).toMatchObject({ by: "name,metadata.v", m: "count,p99(latency)", top: "5" });
    expect(line.value).toBe("by name, metadata.v | count, p99(latency) | top 5");

    act(() => within(screen.getByRole("group", { name: "Stats" })).getByRole("button", { name: "Pickers" }).click());
    expect(groupChips()).toEqual(["name", "metadata.v"]);
    expect(metric("p99 latency").getAttribute("aria-pressed")).toBe("true");
    expect(metric("p95 latency").getAttribute("aria-pressed")).toBe("false");
  });

  it("opens a cell's runs as Samples with the group's conditions as chips", async () => {
    const { queries } = setup(`/?filter=${BATCH}&q=429&view=stats&by=name`);
    await waitFor(() => screen.getByRole("table", { name: "stats" }));
    act(() => within(rowOf("fetch_url")).getByRole("button", { name: "300" }).click());
    await waitFor(() => expect(last(queries)?.filter).toBe('and(has(tags, "batch=a"), eq(name, "fetch_url"), search("429"))'));
    expect(params().view).toBeUndefined();
    expect(params()).toMatchObject({ filter: 'and(has(tags, "batch=a"), eq(name, "fetch_url"))', q: "429", by: "name" });
    expect(filterChips()).toEqual(["tags has batch=a", 'search "429"', "name = fetch_url"]);
  });

  it("adds status = error when an error-rate cell is clicked; (other) without a filter isn't clickable", async () => {
    const { queries } = setup(`/?filter=${BATCH}&view=stats&by=name`);
    await waitFor(() => screen.getByRole("table", { name: "stats" }));
    expect(within(rowOf("(other)")).queryAllByRole("button")).toEqual([]);
    act(() => within(rowOf("fetch_url")).getByRole("button", { name: "14.0%" }).click());
    await waitFor(() => expect(last(queries)?.filter).toBe('and(has(tags, "batch=a"), eq(name, "fetch_url"), eq(status, "error"))'));
    expect(filterChips()).toEqual(["tags has batch=a", "name = fetch_url", "status = error"]);
  });

  it("opens the whole query from the totals row", async () => {
    const { queries } = setup(`/?filter=${BATCH}&view=stats&by=name`);
    await waitFor(() => screen.getByRole("table", { name: "stats" }));
    act(() => within(rowOf("all")).getByRole("button", { name: "4,210" }).click());
    await waitFor(() => expect(last(queries)?.filter).toBe('has(tags, "batch=a")'));
    expect(params().view).toBeUndefined();
  });

  it("keeps the query across view tabs", async () => {
    const { aggs, queries } = setup(`/?filter=${BATCH}&q=429&root=1`);
    await waitFor(() => expect(queries.length).toBe(1));
    act(() => screen.getByRole("button", { name: "Stats" }).click());
    await waitFor(() => expect(aggs.length).toBe(1));
    expect(last(aggs)).toMatchObject({ filter: 'and(has(tags, "batch=a"), search("429"))', is_root: true, group_by: [] });
    act(() => screen.getByRole("button", { name: "Table" }).click());
    await waitFor(() => expect(queries.length).toBe(2));
    expect(last(queries)).toMatchObject({ filter: 'and(has(tags, "batch=a"), search("429"))', is_root: true });
    expect(params()).toMatchObject({ filter: 'has(tags, "batch=a")', q: "429", root: "1", view: "table" });
  });

  it("shows the server's stats error and points a filter error at its slot", async () => {
    setup("/?view=stats&by=metadata.a.b.c.d", () => new Response(
      JSON.stringify({ error: { code: "bad_stats", message: "metadata.a.b.c.d is deeper than the levels kept" } }), { status: 400 }));
    await waitFor(() => screen.getByText("metadata.a.b.c.d is deeper than the levels kept"));
    cleanup();
    setup(`/?view=stats&filter=${encodeURIComponent('eq(metadata.k, "x")')}`, () => new Response(
      JSON.stringify({ error: { code: "bad_filter", slot: "filter", start: 3, end: 13, message: "no such key" } }), { status: 400 }));
    await waitFor(() => expect(document.querySelector(".err-mirror mark")?.textContent).toBe("metadata.k"));
  });

  it("round-trips view, by, m and top in the URL", () => {
    const url = "/?view=stats&by=name,tag&m=count,error_rate,p50(latency),avg(feedback.correctness)&top=20";
    const { q } = fromUrl(url.slice(1));
    expect(q).toMatchObject({ view: "stats", by: ["name", "tag"], m: ["count", "error_rate", "p50(latency)", "avg(feedback.correctness)"], top: 20 });
    expect(toUrl(q)).toBe(url);
  });
});

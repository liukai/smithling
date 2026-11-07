import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { DEFAULT_METRICS, fromUrl, toUrl } from "../query/url";
import { cells, header, mockFetch, queryPage, stubAggStream } from "../testutil";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

type Body = Record<string, unknown>;
const at = (m: number) => `2026-11-05T02:${String(m).padStart(2, "0")}:00.000000Z`;
const bucketWhere = (m: number) => `and(gte(start_time, "${at(m)}"), lt(start_time, "${at(m + 5)}"))`;
const rate = (r: number, runs = 50) => cells({ runs, finished: runs, errors: Math.round(r * runs), error_rate: r, n_latency: runs });
const GROUPS = [
  { key: ["fetch_url"], cells: rate(0.14, 300), where: 'eq(name, "fetch_url")' },
  { key: ["llm_call"], cells: rate(0, 1112), where: 'eq(name, "llm_call")' },
  { key: ["(other)"], cells: rate(0.005, 2798), where: null },
];
const AGG = {
  query_id: "q1", partial: false, cut: true, overlap: false, totals: rate(0.0133, 4210), groups: GROUPS,
  stats: { files_scanned: 3, clean_rows: 4000, dirty_runs: 210, ms: 12 },
};
const SERIES = {
  ...AGG,
  series: {
    step_ms: 300_000, offset: false,
    buckets: [
      { t: at(25), per_group: [rate(0.02), rate(0), rate(0.01)], where: bucketWhere(25) },
      { t: at(30), per_group: [rate(0.4), null, rate(0.01)], where: bucketWhere(30) },
      { t: at(35), per_group: [rate(0.03), rate(0), rate(0)], where: bucketWhere(35) },
    ],
  },
};
const DIST = {
  ...AGG,
  dist: {
    bins_ms: [8000, 16000, 32000],
    per_group: [[1, 5, 120], [300, 10, 0], [50, 2, 1]],
    where_per_bin: ["and(gte(latency, 8), lt(latency, 16))", "and(gte(latency, 16), lt(latency, 32))", "gte(latency, 32)"],
  },
};
const BATCH = encodeURIComponent('has(tags, "batch=a")');

function setup(path: string) {
  history.replaceState(null, "", path);
  const aggs: Body[] = [];
  const queries: Body[] = [];
  stubAggStream((b) => (b.view === "series" ? SERIES : b.view === "dist" ? DIST : AGG), aggs);
  mockFetch({
    "/api/query": (_u, b) => { queries.push(b); return queryPage([header(1)]); },
    "/api/projects": () => [],
  });
  render(<App />);
  return { aggs, queries };
}

const last = (bodies: Body[]) => bodies[bodies.length - 1];
const params = () => Object.fromEntries(new URLSearchParams(window.location.search));
const chart = (name: RegExp) => screen.getByRole("group", { name });
const filterChips = () => [...screen.getByRole("group", { name: "This run" }).querySelectorAll(".chip-label")].map((c) => c.textContent);

describe("time series", () => {
  it("asks for a series and draws one line per group, other included, with gaps where a bucket is null", async () => {
    const { aggs } = setup(`/?filter=${BATCH}&view=series&by=name`);
    await waitFor(() => expect(aggs.length).toBe(1));
    expect(last(aggs)).toEqual({ filter: 'has(tags, "batch=a")', group_by: ["name"], metrics: DEFAULT_METRICS, top: 10, view: "series" });
    const svg = await waitFor(() => chart(/^Time series: error rate by name/));
    expect(svg.querySelector("title")?.textContent).toMatch(/^Time series: error rate by name/);
    const paths = [...svg.querySelectorAll("path.line")];
    expect(paths.map((p) => p.getAttribute("data-series"))).toEqual(["fetch_url", "llm_call", "(other)"]);
    expect(paths.map((p) => (p.getAttribute("d")!.match(/M/g) ?? []).length)).toEqual([1, 2, 1]);
    expect(paths[0].getAttribute("stroke")).not.toBe(paths[1].getAttribute("stroke"));
    expect(within(svg).getByText("start time")).toBeTruthy();
    expect(within(svg).getByText("error rate")).toBeTruthy();
    expect(within(svg).getAllByRole("button", { name: /^fetch_url · .+: error rate / }).map((b) => b.getAttribute("aria-label")!.split(": ")[1]))
      .toEqual(["error rate 2.0%", "error rate 40.0%", "error rate 3.0%"]);
    expect(within(svg).queryAllByRole("button", { name: /^\(other\)/ })).toEqual([]);
    expect(svg.querySelectorAll("circle").length).toBe(8);
    // the table shows with the chart
    expect(screen.getByRole("table", { name: "stats" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Time series" }).getAttribute("aria-pressed")).toBe("true");
  });

  it("charts another metric, and shows a point's figures on hover and focus", async () => {
    setup(`/?view=series&by=name`);
    const svg = await waitFor(() => chart(/^Time series: error rate/));
    const point = within(svg).getAllByRole("button", { name: /^fetch_url/ })[1];
    fireEvent.focus(point);
    expect(screen.getByRole("tooltip").textContent).toMatch(/^fetch_url · .+: error rate 40\.0% · 50 runs$/);
    fireEvent.change(screen.getByLabelText("chart metric"), { target: { value: "count" } });
    await waitFor(() => chart(/^Time series: runs by name/));
    expect(within(chart(/^Time series: runs/)).getAllByRole("button", { name: /^fetch_url/ })[0].getAttribute("aria-label")).toMatch(/: runs 50$/);
  });

  it("opens a point's runs as Samples with the group's and the bucket's conditions", async () => {
    const { queries } = setup(`/?filter=${BATCH}&view=series&by=name`);
    const svg = await waitFor(() => chart(/^Time series/));
    act(() => within(svg).getAllByRole("button", { name: /^fetch_url/ })[1].dispatchEvent(new MouseEvent("click", { bubbles: true })));
    await waitFor(() => expect(last(queries)?.filter)
      .toBe(`and(has(tags, "batch=a"), eq(name, "fetch_url"), gte(start_time, "${at(30)}"), lt(start_time, "${at(35)}"))`));
    expect(params().view).toBeUndefined();
    expect(filterChips().slice(0, 2)).toEqual(["tags has batch=a", "name = fetch_url"]);
    expect(filterChips().length).toBe(4);
  });

  it("opens a point from the keyboard", async () => {
    const { queries } = setup(`/?view=series`);
    const svg = await waitFor(() => chart(/^Time series/));
    const point = within(svg).getAllByRole("button")[0];
    expect(point.getAttribute("tabindex")).toBe("0");
    fireEvent.keyDown(point, { key: "Enter" });
    await waitFor(() => expect(last(queries)?.filter).toBe(bucketWhere(25)));
  });

  it("puts bucket in the URL and the request, and round-trips it", async () => {
    const { aggs } = setup(`/?view=series&by=name`);
    await waitFor(() => chart(/^Time series/));
    fireEvent.change(screen.getByLabelText("bucket"), { target: { value: "5m" } });
    await waitFor(() => expect(last(aggs).bucket).toBe("5m"));
    expect(params()).toMatchObject({ view: "series", bucket: "5m" });
    fireEvent.change(screen.getByLabelText("bucket"), { target: { value: "auto" } });
    await waitFor(() => expect(last(aggs).bucket).toBeUndefined());
    expect(params().bucket).toBeUndefined();

    const url = "/?view=series&by=name&bucket=15m";
    const { q } = fromUrl(url.slice(1));
    expect(q).toMatchObject({ view: "series", bucket: "15m" });
    expect(toUrl(q)).toBe(url);
    expect(fromUrl("view=series&bucket=7m").q.bucket).toBeUndefined();
  });

  it("doesn't send bucket outside the time series", async () => {
    const { aggs } = setup(`/?view=dist&bucket=5m`);
    await waitFor(() => expect(aggs.length).toBe(1));
    expect(last(aggs)).toMatchObject({ view: "dist" });
    expect(last(aggs).bucket).toBeUndefined();
  });
});

describe("empty charts", () => {
  it("says so when a grouped series has no groups or a metric no values", async () => {
    history.replaceState(null, "", "/?view=series&by=name&m=count,p95(latency)");
    stubAggStream(() => ({ ...AGG, groups: [], series: { step_ms: 60_000, offset: false, buckets: [] } }));
    mockFetch({ "/api/projects": () => [] });
    render(<App />);
    await waitFor(() => screen.getByText("No runs to chart."));
    cleanup();
    history.replaceState(null, "", "/?view=series&m=count,p95(latency)");
    stubAggStream(() => SERIES);
    render(<App />);
    await waitFor(() => screen.getByText("No p95 latency to chart."));
  });

  it("keeps count ticks whole", async () => {
    history.replaceState(null, "", "/?view=dist");
    stubAggStream(() => ({ ...DIST, dist: { ...DIST.dist, per_group: [[1, 2, 1]] } }));
    mockFetch({ "/api/projects": () => [] });
    render(<App />);
    const svg = await waitFor(() => chart(/^Distribution/));
    const labels = [...svg.querySelectorAll(".grid text")].map((t) => t.textContent);
    expect(labels).toEqual(["0", "1", "2", "3", "4"]);
  });
});

describe("distribution", () => {
  it("draws one bar per latency bin for the chosen group; a bar opens its runs", async () => {
    const { aggs, queries } = setup(`/?filter=${BATCH}&view=dist&by=name`);
    await waitFor(() => expect(last(aggs)).toMatchObject({ view: "dist", group_by: ["name"] }));
    const svg = await waitFor(() => chart(/^Distribution: latency of fetch_url/));
    expect(within(svg).getByText("latency")).toBeTruthy();
    expect(within(svg).getByText("runs")).toBeTruthy();
    const bars = within(svg).getAllByRole("button");
    expect(bars.map((b) => b.getAttribute("aria-label"))).toEqual([
      "fetch_url · 8.00 s – 16.0 s: 1 run", "fetch_url · 16.0 s – 32.0 s: 5 runs", "fetch_url · 32.0 s and up: 120 runs",
    ]);
    const heights = bars.map((b) => Number(b.getAttribute("height")));
    expect(heights[2]).toBeGreaterThan(heights[1]);

    fireEvent.change(screen.getByLabelText("chart group"), { target: { value: "1" } });
    await waitFor(() => chart(/^Distribution: latency of llm_call/));
    expect(within(chart(/llm_call/)).getAllByRole("button").length).toBe(2);
    fireEvent.change(screen.getByLabelText("chart group"), { target: { value: "0" } });

    act(() => within(chart(/fetch_url/)).getAllByRole("button")[2].dispatchEvent(new MouseEvent("click", { bubbles: true })));
    await waitFor(() => expect(last(queries)?.filter).toBe('and(has(tags, "batch=a"), eq(name, "fetch_url"), gte(latency, 32))'));
  });

  it("draws the whole query when not grouped", async () => {
    setup(`/?view=dist`);
    const svg = await waitFor(() => chart(/^Distribution: latency$/));
    expect(within(svg).getAllByRole("button")[0].getAttribute("aria-label")).toBe("8.00 s – 16.0 s: 1 run");
    expect(screen.queryByLabelText("chart group")).toBeNull();
  });
});

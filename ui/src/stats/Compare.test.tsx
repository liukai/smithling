import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { fromUrl, toUrl } from "../query/url";
import { cells, header, mockFetch, queryPage, stubAggStream } from "../testutil";
import { SIDE_COLORS } from "./TimeSeries";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

type Body = Record<string, unknown>;
const at = (h: number, m: number) => new Date(Date.UTC(2026, 10, 5, h, m)).toISOString().replace(".000Z", ".000000Z");
const span = (h: number, m: number) => `and(gte(start_time, "${at(h, m)}"), lt(start_time, "${at(h, m + 30)}"))`;
const c = (rate: number, p95: number, runs = 300) =>
  cells({ runs, finished: runs, errors: Math.round(rate * runs), error_rate: rate, p95, n_latency: runs });
const STATS = { files_scanned: 3, clean_rows: 4000, dirty_runs: 210, ms: 12 };
const SIDE_B = {
  query_id: "q1", partial: false, cut: true, overlap: false, stats: STATS, totals: c(0.004, 7000, 4100),
  groups: [
    { key: ["fetch_url"], cells: c(0.02, 9000), where: 'eq(name, "fetch_url")' },
    { key: ["llm_call"], cells: null, where: 'eq(name, "llm_call")' },
  ],
  series: {
    step_ms: 1_800_000, offset: true,
    buckets: [
      { offset_ms: 0, per_group: [c(0.02, 9000, 40), null], where: span(26, 0) },
      { offset_ms: 1_800_000, per_group: [c(0.03, 9000, 40), null], where: span(26, 30) },
    ],
  },
};
const AGG = {
  query_id: "q1", partial: false, cut: true, overlap: false, stats: STATS, totals: c(0.0133, 12000, 4210),
  groups: [
    { key: ["fetch_url"], cells: c(0.14, 31000), where: 'eq(name, "fetch_url")' },
    { key: ["llm_call"], cells: c(0, 2400, 1112), where: 'eq(name, "llm_call")' },
  ],
  series: {
    step_ms: 1_800_000, offset: true,
    buckets: [
      { offset_ms: 0, per_group: [c(0.05, 20000, 40), c(0, 2400, 100)], where: span(2, 0) },
      { offset_ms: 1_800_000, per_group: [c(0.4, 31000, 40), c(0, 2400, 100)], where: span(2, 30) },
    ],
  },
  compare: { side_b: SIDE_B },
};
const BATCH_A = 'has(tags, "batch=a")';
const BATCH_B = 'has(tags, "batch=b")';
const M = "m=count,error_rate,p95(latency)";

function setup(path: string, agg: (b: Body) => unknown = (b) => (b.compare ? AGG : { ...AGG, compare: undefined })) {
  history.replaceState(null, "", path);
  const aggs: Body[] = [];
  const queries: Body[] = [];
  stubAggStream(agg, aggs);
  mockFetch({
    "/api/query": (_u, b) => { queries.push(b); return queryPage([header(1)]); },
    "/api/projects": () => [],
  });
  render(<App />);
  return { aggs, queries };
}

const last = (bodies: Body[]) => bodies[bodies.length - 1];
const params = () => Object.fromEntries(new URLSearchParams(window.location.search));
const compareRow = () => screen.getByRole("group", { name: "Compare" });
const table = () => screen.getByRole("table", { name: "stats" });
const lines = () => within(table()).getAllByRole("row").map((r) => [...r.querySelectorAll("th, td")].map((x) => x.textContent));
const rowOf = (key: string) => within(table()).getAllByRole("row").find((r) => r.querySelector("td")?.textContent === key)!;
const filterChips = () => [...screen.getByRole("group", { name: "This run" }).querySelectorAll(".chip-label")].map((x) => x.textContent);
const enc = encodeURIComponent;

describe("compare", () => {
  it("writes a compare filter to the URL and the request, and removes it", async () => {
    const { aggs } = setup(`/?filter=${enc(BATCH_A)}&view=stats&by=name`);
    await waitFor(() => expect(aggs.length).toBe(1));
    expect(last(aggs).compare).toBeUndefined();
    const input = within(compareRow()).getByLabelText("compare filter");
    fireEvent.change(input, { target: { value: "has(tags," } });
    fireEvent.submit(input);
    expect(within(compareRow()).getByRole("alert")).toBeTruthy();
    expect(aggs.length).toBe(1);

    fireEvent.change(input, { target: { value: BATCH_B } });
    fireEvent.submit(input);
    await waitFor(() => expect(last(aggs).compare).toEqual({ filter: BATCH_B }));
    expect(params()).toMatchObject({ cmp: BATCH_B, filter: BATCH_A });
    expect(within(compareRow()).getByText("b: tags has batch=b")).toBeTruthy();

    act(() => within(compareRow()).getByRole("button", { name: "remove comparison" }).click());
    await waitFor(() => expect(last(aggs).compare).toBeUndefined());
    expect(params().cmp).toBeUndefined();
  });

  it("compares with the same window shifted back", async () => {
    setup(`/?view=stats&by=name`);
    expect((within(compareRow()).getByLabelText("compare shift") as HTMLSelectElement).disabled).toBe(true);
    cleanup();
    const { aggs } = setup(`/?time=12h&view=stats&by=name`);
    await waitFor(() => expect(aggs.length).toBe(1));
    fireEvent.change(within(compareRow()).getByLabelText("compare shift"), { target: { value: "1d" } });
    await waitFor(() => expect(last(aggs).compare).toEqual({ shift: "1d" }));
    expect(params()).toMatchObject({ shift: "1d", time: "12h" });
    expect(within(compareRow()).getByText("b: same window, 1d earlier")).toBeTruthy();
    expect(within(compareRow()).queryByLabelText("compare filter")).toBeNull();
  });

  it("round-trips cmp and shift in the URL", () => {
    for (const url of [`/?view=series&by=name&cmp=${enc(BATCH_B)}`, "/?time=12h&view=series&shift=7d"]) {
      const { q } = fromUrl(url.slice(2));
      expect(toUrl(q)).toBe(url.replace("%2C", ","));
    }
    expect(fromUrl(`cmp=${enc(BATCH_B)}`).q).toMatchObject({ cmp: BATCH_B, shift: undefined });
    expect(fromUrl("shift=1d").q.shift).toBe("1d");
  });

  it("keeps the search box on both sides", async () => {
    const { aggs } = setup(`/?filter=${enc(BATCH_A)}&q=429&view=stats&cmp=${enc(BATCH_B)}`);
    await waitFor(() => expect(aggs.length).toBe(1));
    expect(last(aggs)).toMatchObject({
      filter: 'and(has(tags, "batch=a"), search("429"))', compare: { filter: 'and(has(tags, "batch=b"), search("429"))' },
    });
  });

  it("shows side b's columns per group; a group b lacks is empty, not zero; a b cell opens b's runs", async () => {
    const { queries } = setup(`/?filter=${enc(BATCH_A)}&view=stats&by=name&${M}&cmp=${enc(BATCH_B)}`);
    await waitFor(() => table());
    expect(lines()).toEqual([
      ["name", "runs", "error rate", "p95 latency", "b: runs", "b: error rate", "b: p95 latency"],
      ["fetch_url", "300", "14.0%", "31.0 sn=300", "300", "2.0%", "9.00 sn=300"],
      ["llm_call", "1,112", "0.0%", "2.40 sn=1,112", "—", "—", "—"],
      ["all", "4,210", "1.3%", "12.0 sn=4,210", "4,100", "0.4%", "7.00 sn=4,100"],
    ]);
    expect(within(rowOf("llm_call")).getAllByRole("button").length).toBe(3);
    act(() => within(rowOf("fetch_url")).getByRole("button", { name: "2.0%" }).click());
    await waitFor(() => expect(last(queries)?.filter).toBe('and(has(tags, "batch=b"), eq(name, "fetch_url"), eq(status, "error"))'));
    expect(params().cmp).toBeUndefined();
    expect(filterChips()).toEqual(["tags has batch=b", "name = fetch_url", "status = error"]);
  });

  it("overlays a and b for one group on an offset axis, in each side's colour", async () => {
    const { queries } = setup(`/?filter=${enc(BATCH_A)}&view=series&by=name&cmp=${enc(BATCH_B)}`);
    const svg = await waitFor(() => screen.getByRole("group", { name: /^Time series: error rate of fetch_url, a vs b/ }));
    const paths = [...svg.querySelectorAll("path.line")];
    expect(paths.map((p) => p.getAttribute("data-series"))).toEqual(["a", "b"]);
    expect(paths.map((p) => p.getAttribute("stroke"))).toEqual([SIDE_COLORS.a, SIDE_COLORS.b]);
    expect(within(svg).getByText("time since start")).toBeTruthy();
    const legend = [...document.querySelectorAll(".chart .legend li")].map((li) => [li.textContent, (li.querySelector(".swatch") as HTMLElement).style.background]);
    expect(legend.map((l) => l[0])).toEqual(["a: tags has batch=a", "b: tags has batch=b"]);
    expect(legend[0][1]).not.toBe(legend[1][1]);
    expect(within(svg).getByText("+30m")).toBeTruthy();
    expect(within(svg).getAllByRole("button").map((b) => b.getAttribute("aria-label"))).toEqual([
      "a · fetch_url · +0: error rate 5.0%", "a · fetch_url · +30m: error rate 40.0%",
      "b · fetch_url · +0: error rate 2.0%", "b · fetch_url · +30m: error rate 3.0%",
    ]);
    // the table's b columns wear side b's colour too
    expect((table().querySelector("th.side-b .swatch") as HTMLElement).style.background).toBeTruthy();

    fireEvent.change(screen.getByLabelText("chart group"), { target: { value: "1" } });
    const llm = await waitFor(() => screen.getByRole("group", { name: /of llm_call, a vs b/ }));
    expect([...llm.querySelectorAll("path.line")].map((p) => p.getAttribute("d"))[1]).toBe("");
    fireEvent.change(screen.getByLabelText("chart group"), { target: { value: "0" } });

    const b = await waitFor(() => screen.getByRole("button", { name: "b · fetch_url · +30m: error rate 3.0%" }));
    act(() => b.dispatchEvent(new MouseEvent("click", { bubbles: true })));
    await waitFor(() => expect(last(queries)?.filter)
      .toBe(`and(has(tags, "batch=b"), eq(name, "fetch_url"), gte(start_time, "${at(26, 30)}"), lt(start_time, "${at(26, 60)}"))`));
  });

  it("opens a shifted side b point over all time; its table cells aren't clickable", async () => {
    const { queries } = setup(`/?time=12h&view=series&by=name&${M}&shift=1d`);
    await waitFor(() => table());
    expect(within(rowOf("fetch_url")).getAllByRole("button").length).toBe(3);
    const b = await waitFor(() => screen.getByRole("button", { name: "b · fetch_url · +0: error rate 2.0%" }));
    act(() => b.dispatchEvent(new MouseEvent("click", { bubbles: true })));
    await waitFor(() => expect(last(queries)?.filter).toBe(`and(eq(name, "fetch_url"), ${span(26, 0).slice(4, -1)})`));
    expect(last(queries)?.time).toBeUndefined();
    expect(params().time).toBeUndefined();
    expect(params().shift).toBeUndefined();
  });
});

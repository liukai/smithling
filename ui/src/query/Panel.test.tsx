import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import type { RunHeader } from "../api";
import { header, mockFetch } from "../testutil";
import { fromUrl, toUrl } from "./url";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

type Body = Record<string, unknown>;
const page = (runs: RunHeader[], next_cursor: string | null = null, warnings: unknown[] = []) =>
  ({ runs, next_cursor, warnings, stats: { files_total: 0, files_scanned: 0, rows_read: 0, rows_merged: 0, matched: 0, ms: 1 } });

function setup(path: string, answer: (b: Body) => unknown = () => page([header(1), header(2)])) {
  history.replaceState(null, "", path);
  const bodies: Body[] = [];
  mockFetch({ "/api/query": (_u, b) => { bodies.push(b); return answer(b); }, "/api/projects": () => [{ name: "p", runs: 3 }] });
  render(<App />);
  return bodies;
}

const group = (name: string) => screen.getByRole("group", { name });
const chips = (name: string) => [...group(name).querySelectorAll(".chip")].map((c) =>
  (c.classList.contains("off") ? "[off] " : "") + c.querySelector(".chip-label")!.textContent);
const last = (bodies: Body[]) => bodies[bodies.length - 1];
const params = () => Object.fromEntries(new URLSearchParams(window.location.search));
const rowNames = () => screen.getAllByRole("row").slice(1).map((r) => r.querySelector("td.name")?.textContent);

describe("query panel", () => {
  it("shows text as chips and back, losslessly", async () => {
    const filter = 'and(has(tags, "batch=a"), not(eq(name, "plan")), or(eq(status, "error"), gt(latency, 30)))';
    const bodies = setup(`/?filter=${encodeURIComponent(filter)}&tree=${encodeURIComponent('eq(status, "error")')}&root=1`);
    await waitFor(() => expect(bodies.length).toBe(1));
    expect(last(bodies)).toMatchObject({ filter, tree_filter: 'eq(status, "error")', is_root: true });
    expect(chips("This run")).toEqual(["tags has batch=a", "not name = plan", 'or(eq(status, "error"), gt(latency, 30))']);
    expect(group("This run").querySelector(".chip.expr")).toBeTruthy();
    expect(chips("Its root")).toEqual([]);
    expect(chips("Any run in its trace")).toEqual(["status = error"]);
    expect(screen.getByRole("button", { name: "root only" }).getAttribute("aria-pressed")).toBe("true");

    act(() => screen.getByRole("button", { name: "Text" }).click());
    const text = screen.getByLabelText("This run filter") as HTMLTextAreaElement;
    expect(text.value).toBe(filter);
    expect((screen.getByLabelText("Any run in its trace filter") as HTMLTextAreaElement).value).toBe('eq(status, "error")');

    fireEvent.change(text, { target: { value: 'and(eq(status,"ok"),  gt(latency, 5))' } });
    act(() => screen.getByRole("button", { name: "Run" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('and(eq(status, "ok"), gt(latency, 5))'));
    expect(params().filter).toBe('and(eq(status, "ok"), gt(latency, 5))');
    act(() => screen.getByRole("button", { name: "Panel" }).click());
    expect(chips("This run")).toEqual(["status = ok", "latency > 5"]);
  });

  it("negates, disables and removes chips, and the results follow", async () => {
    const bodies = setup(`/?filter=${encodeURIComponent('and(eq(status, "error"), has(tags, "x"))')}`, (b) =>
      page(b.filter === 'has(tags, "x")' ? [header(1), header(2), header(3)] : [header(1)]));
    await waitFor(() => expect(rowNames()).toEqual(["task-1"]));

    act(() => screen.getByRole("button", { name: "negate status = error" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('and(not(eq(status, "error")), has(tags, "x"))'));
    expect(chips("This run")).toEqual(["not status = error", "tags has x"]);

    act(() => screen.getByRole("button", { name: "disable not status = error" }).click());
    await waitFor(() => expect(rowNames()).toEqual(["task-1", "task-2", "task-3"]));
    expect(last(bodies).filter).toBe('has(tags, "x")');
    expect(chips("This run")).toEqual(["[off] not status = error", "tags has x"]);

    act(() => screen.getByRole("button", { name: "enable not status = error" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('and(not(eq(status, "error")), has(tags, "x"))'));

    act(() => screen.getByRole("button", { name: "remove not status = error" }).click());
    await waitFor(() => expect(rowNames()).toEqual(["task-1", "task-2", "task-3"]));
    expect(chips("This run")).toEqual(["tags has x"]);
    expect(params().filter).toBe('has(tags, "x")');
  });

  it("adds and edits chips without typing syntax", async () => {
    const bodies = setup(`/?filter=${encodeURIComponent('eq(status, "error")')}`);
    await waitFor(() => expect(bodies.length).toBe(1));

    act(() => screen.getByRole("button", { name: "edit status = error" }).click());
    fireEvent.change(screen.getByLabelText("value"), { target: { value: "ok" } });
    act(() => screen.getByRole("button", { name: "save" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('eq(status, "ok")'));

    act(() => within(group("Any run in its trace")).getByRole("button", { name: "add condition" }).click());
    fireEvent.change(screen.getByLabelText("field"), { target: { value: "status" } });
    fireEvent.change(screen.getByLabelText("value"), { target: { value: "error" } });
    act(() => screen.getByRole("button", { name: "save" }).click());
    await waitFor(() => expect(last(bodies).tree_filter).toBe('eq(status, "error")'));

    act(() => within(group("This run")).getByRole("button", { name: "add condition" }).click());
    fireEvent.change(screen.getByLabelText("field"), { target: { value: "staus" } });
    fireEvent.change(screen.getByLabelText("value"), { target: { value: "x" } });
    act(() => screen.getByRole("button", { name: "save" }).click());
    expect(screen.getByRole("alert").textContent).toBe('unknown field "staus" (did you mean "status"?)');

    fireEvent.change(screen.getByLabelText("field"), { target: { value: "metadata.code" } });
    fireEvent.change(screen.getByLabelText("operator"), { target: { value: "gte" } });
    fireEvent.change(screen.getByLabelText("value"), { target: { value: "400" } });
    act(() => screen.getByRole("button", { name: "save" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('and(eq(status, "ok"), gte(metadata.code, 400))'));
    expect(chips("This run")).toEqual(["status = ok", "metadata.code ≥ 400"]);
  });

  it("adds a chip when a status, tag or name is clicked; Alt-click adds its negation", async () => {
    const bodies = setup("/", () => page([header(1, { status: "error", tags: ["batch=a"] })]));
    await waitFor(() => expect(rowNames()).toEqual(["task-1"]));
    const cells = () => screen.getAllByRole("row")[1];

    act(() => within(cells()).getByText("batch=a").click());
    await waitFor(() => expect(last(bodies).filter).toBe('has(tags, "batch=a")'));
    expect(window.location.pathname).toBe("/");

    fireEvent.click(within(cells()).getByText("✕ err"), { altKey: true });
    await waitFor(() => expect(last(bodies).filter).toBe('and(has(tags, "batch=a"), not(eq(status, "error")))'));

    act(() => within(cells()).getByText("task-1").click());
    await waitFor(() => expect(chips("This run")).toEqual(["tags has batch=a", "not status = error", "name = task-1"]));
    expect(last(bodies).filter).toBe('and(has(tags, "batch=a"), not(eq(status, "error")), eq(name, "task-1"))');
  });

  it("underlines the error a bad_filter response points at", async () => {
    // 12..20 are UTF-8 bytes; "ключ" is 4 UTF-16 units but 8 bytes.
    const bodies = setup(`/?filter=${encodeURIComponent('eq(metadata.ключ, "x")')}`, () => new Response(
      JSON.stringify({ error: { code: "bad_filter", slot: "filter", start: 12, end: 20, message: "no such key" } }),
      { status: 400, headers: { "content-type": "application/json" } }));
    await waitFor(() => expect(screen.getByText("no such key")).toBeTruthy());
    expect(bodies.length).toBe(1);
    expect(document.querySelector(".err-mirror mark")!.textContent).toBe("ключ");
    expect((screen.getByLabelText("This run filter") as HTMLTextAreaElement).value).toBe('eq(metadata.ключ, "x")');
    expect(screen.getByText("The query has an error; open Text to see it.")).toBeTruthy();
    act(() => screen.getByRole("button", { name: "Panel" }).click());
    act(() => screen.getByRole("button", { name: "Text" }).click());
    expect(document.querySelector(".err-mirror mark")!.textContent).toBe("ключ");
  });

  it("checks text before sending it", async () => {
    const bodies = setup("/");
    await waitFor(() => expect(bodies.length).toBe(1));
    act(() => screen.getByRole("button", { name: "Text" }).click());
    fireEvent.change(screen.getByLabelText("This run filter"), { target: { value: 'eq(staus, "error")' } });
    act(() => screen.getByRole("button", { name: "Run" }).click());
    expect(document.querySelector(".err-mirror mark")!.textContent).toBe("staus");
    expect(screen.getByText('unknown field "staus" (did you mean "status"?)')).toBeTruthy();
    act(() => screen.getByRole("button", { name: "Panel" }).click());
    expect(screen.getByLabelText("This run filter")).toBeTruthy();
    expect(bodies.length).toBe(1);
  });

  it("clears a slot whose text doesn't parse when its text is emptied", async () => {
    setup(`/?filter=${encodeURIComponent("eq(staus, 1)")}`);
    await waitFor(() => expect(document.querySelector(".err-mirror mark")!.textContent).toBe("staus"));
    fireEvent.change(screen.getByLabelText("This run filter"), { target: { value: "" } });
    act(() => screen.getByRole("button", { name: "Run" }).click());
    expect(params().filter).toBeUndefined();
    expect((screen.getByLabelText("This run filter") as HTMLTextAreaElement).value).toBe("");
  });

  it("keeps disabled chips across Text and back when the text wasn't changed", async () => {
    setup(`/?filter=${encodeURIComponent('and(eq(status, "error"), has(tags, "x"))')}`);
    await waitFor(() => expect(chips("This run")).toHaveLength(2));
    act(() => screen.getByRole("button", { name: "disable status = error" }).click());
    act(() => screen.getByRole("button", { name: "Text" }).click());
    act(() => screen.getByRole("button", { name: "Run" }).click());
    act(() => screen.getByRole("button", { name: "Panel" }).click());
    expect(chips("This run")).toEqual(["[off] status = error", "tags has x"]);
  });

  it("keeps unsent text drafts when a value is clicked in Text mode", async () => {
    const bodies = setup("/", () => page([header(1)]));
    await waitFor(() => expect(rowNames()).toEqual(["task-1"]));
    act(() => screen.getByRole("button", { name: "Text" }).click());
    fireEvent.change(screen.getByLabelText("Its root filter"), { target: { value: "gt(latency," } });
    act(() => within(screen.getAllByRole("row")[1]).getByText("batch=a").click());
    await waitFor(() => expect(last(bodies).filter).toBe('has(tags, "batch=a")'));
    expect((screen.getByLabelText("This run filter") as HTMLTextAreaElement).value).toBe('has(tags, "batch=a")');
    expect((screen.getByLabelText("Its root filter") as HTMLTextAreaElement).value).toBe("gt(latency,");
  });

  it("keeps a JSON path's string value a string through the chip editor", async () => {
    const bodies = setup(`/?filter=${encodeURIComponent('eq(metadata.v, "123")')}`);
    await waitFor(() => expect(bodies.length).toBe(1));
    expect(chips("This run")).toEqual(['metadata.v = "123"']);
    act(() => screen.getByRole("button", { name: 'edit metadata.v = "123"' }).click());
    expect((screen.getByLabelText("value") as HTMLInputElement).value).toBe('"123"');
    act(() => screen.getByRole("button", { name: "save" }).click());
    expect(params().filter).toBe('eq(metadata.v, "123")');
    act(() => screen.getByRole("button", { name: 'edit metadata.v = "123"' }).click());
    fireEvent.change(screen.getByLabelText("value"), { target: { value: "123" } });
    act(() => screen.getByRole("button", { name: "save" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe("eq(metadata.v, 123)"));
    expect(chips("This run")).toEqual(["metadata.v = 123"]);
  });

  it("translates M1 links on load", async () => {
    const bodies = setup("/?project=p&status=error");
    await waitFor(() => expect(bodies.length).toBe(1));
    expect(last(bodies)).toMatchObject({ project: "p", is_root: true, filter: 'eq(status, "error")' });
    expect(params()).toEqual({ project: "p", filter: 'eq(status, "error")', root: "1" });
    expect(chips("This run")).toEqual(["status = error"]);
    cleanup();

    const roots = setup("/?project=p");
    await waitFor(() => expect(roots.length).toBe(1));
    expect(last(roots)).toMatchObject({ project: "p", is_root: true });
  });

  it("sends project, time, order and limit, and pages by cursor", async () => {
    const bodies = setup("/?project=p&root=0", (b) => b.cursor
      ? page([header(1)])
      : page([header(3), header(2)], "c1", [{ kind: "mixed_types", path: "outputs.code", counts: { number: 4, string: 1 } }]));
    await waitFor(() => expect(rowNames()).toEqual(["task-3", "task-2"]));
    expect(last(bodies)).toMatchObject({ project: "p", order: { by: "start_time", dir: "desc" }, limit: 50 });
    expect(last(bodies).is_root).toBeUndefined();
    expect(screen.getByText(/outputs.code: mixed types \(number 4, string 1\)/)).toBeTruthy();

    act(() => screen.getByRole("button", { name: "more ▸" }).click());
    await waitFor(() => expect(rowNames()).toEqual(["task-3", "task-2", "task-1"]));
    expect(last(bodies).cursor).toBe("c1");
    expect(screen.queryByRole("button", { name: "more ▸" })).toBeNull();
    expect(screen.getByText("3 runs")).toBeTruthy();

    fireEvent.change(screen.getByLabelText("time"), { target: { value: "12h" } });
    fireEvent.change(screen.getByLabelText("order"), { target: { value: "latency.asc" } });
    fireEvent.change(screen.getByLabelText("limit"), { target: { value: "200" } });
    await waitFor(() => expect(last(bodies)).toMatchObject({ time: { last: "12h" }, order: { by: "latency", dir: "asc" }, limit: 200 }));
    expect(last(bodies).cursor).toBeUndefined();
    expect(params()).toMatchObject({ project: "p", time: "12h", order: "latency.asc", limit: "200" });
  });

  it("explains setup when there are no runs at all", async () => {
    setup("/", () => page([]));
    await waitFor(() => screen.getByText("No traces yet"));
    expect(screen.getByText(/LANGSMITH_ENDPOINT=/)).toBeTruthy();
    cleanup();
    setup(`/?filter=${encodeURIComponent('eq(name, "x")')}`, () => page([]));
    await waitFor(() => screen.getByText("No runs match this query."));
  });
});

describe("search", () => {
  const box = () => screen.getByLabelText("search") as HTMLInputElement;
  const searchFor = (text: string) => {
    fireEvent.change(box(), { target: { value: text } });
    fireEvent.submit(box());
  };

  it("adds a search chip in This run from the search box, and the box follows the chip", async () => {
    const bodies = setup(`/?filter=${encodeURIComponent('has(tags, "batch=a")')}`);
    await waitFor(() => expect(bodies.length).toBe(1));

    searchFor('"429 Too Many Requests"');
    await waitFor(() => expect(last(bodies).filter).toBe('and(has(tags, "batch=a"), search("429 Too Many Requests"))'));
    expect(chips("This run")).toEqual(["tags has batch=a", 'search "429 Too Many Requests"']);
    expect(params()).toEqual({ filter: 'has(tags, "batch=a")', q: "429 Too Many Requests" });
    expect(box().value).toBe("429 Too Many Requests");

    searchFor("rate limit");
    await waitFor(() => expect(last(bodies).filter).toBe('and(has(tags, "batch=a"), search("rate limit"))'));
    expect(chips("This run")).toEqual(["tags has batch=a", 'search "rate limit"']);

    act(() => screen.getByRole("button", { name: 'remove search "rate limit"' }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('has(tags, "batch=a")'));
    expect(params().q).toBeUndefined();
    expect(box().value).toBe("");
  });

  it("keeps the box's search when other chips are added", async () => {
    const bodies = setup("/?q=429", () => page([header(1, { name: "fetch_url" })]));
    await waitFor(() => expect(rowNames()).toEqual(["fetch_url"]));
    act(() => within(screen.getAllByRole("row")[1]).getByText("fetch_url").click());
    await waitFor(() => expect(last(bodies).filter).toBe('and(eq(name, "fetch_url"), search("429"))'));
    expect(chips("This run")).toEqual(['search "429"', "name = fetch_url"]);
    expect(params()).toEqual({ filter: 'eq(name, "fetch_url")', q: "429" });
    expect(box().value).toBe("429");
  });

  it("round-trips the search chip through the text form", async () => {
    const bodies = setup(`/?q=${encodeURIComponent("429 Too Many Requests")}`);
    await waitFor(() => expect(last(bodies)?.filter).toBe('search("429 Too Many Requests")'));
    expect(chips("This run")).toEqual(['search "429 Too Many Requests"']);
    expect(box().value).toBe("429 Too Many Requests");

    act(() => screen.getByRole("button", { name: "Text" }).click());
    const text = screen.getByLabelText("This run filter") as HTMLTextAreaElement;
    expect(text.value).toBe('search("429 Too Many Requests")');
    fireEvent.change(text, { target: { value: 'and(eq(name, "fetch_url"), search(outputs, "rate limit"), search("HTTPError"))' } });
    act(() => screen.getByRole("button", { name: "Run" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('and(eq(name, "fetch_url"), search(outputs, "rate limit"), search("HTTPError"))'));
    expect(params()).toEqual({ filter: 'and(eq(name, "fetch_url"), search(outputs, "rate limit"))', q: "HTTPError" });
    expect(box().value).toBe("HTTPError");

    act(() => screen.getByRole("button", { name: "Panel" }).click());
    expect(chips("This run")).toEqual(["name = fetch_url", 'search outputs "rate limit"', 'search "HTTPError"']);
    act(() => screen.getByRole("button", { name: "Text" }).click());
    expect((screen.getByLabelText("This run filter") as HTMLTextAreaElement).value)
      .toBe('and(eq(name, "fetch_url"), search(outputs, "rate limit"), search("HTTPError"))');
  });

  it("replaces the box's own search, not another one", async () => {
    const bodies = setup(`/?q=a&filter=${encodeURIComponent('search("b")')}`);
    await waitFor(() => expect(last(bodies)?.filter).toBe('and(search("b"), search("a"))'));
    searchFor("c");
    await waitFor(() => expect(last(bodies).filter).toBe('and(search("b"), search("c"))'));
    expect(params()).toEqual({ filter: 'search("b")', q: "c" });
  });

  it("moves a negated search chip out of the box", async () => {
    const bodies = setup("/?q=429");
    await waitFor(() => expect(bodies.length).toBe(1));
    act(() => screen.getByRole("button", { name: 'negate search "429"' }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('not(search("429"))'));
    expect(params()).toEqual({ filter: 'not(search("429"))' });
    expect(box().value).toBe("");
  });

  it("refuses a search with no words", async () => {
    const bodies = setup("/");
    await waitFor(() => expect(bodies.length).toBe(1));
    searchFor("!!");
    expect(screen.getByRole("alert").textContent).toBe("search needs at least one word");
    expect(bodies.length).toBe(1);
    expect(params().q).toBeUndefined();
  });

  it("shows each match's excerpt with the matched characters marked, in Samples and Table", async () => {
    // Offsets count characters: the emoji is one, though two UTF-16 units.
    const hit = { ...header(1, { name: "fetch_url" }), match: { path: "error", excerpt: "…🙂 HTTPError: 429 Too Many", start: 14, end: 17 } };
    setup("/?q=429", () => page([hit, header(2)], null, [{ kind: "not_indexed", files: ["a", "b"] }, { kind: "skipped_files", ids: ["s1"] }]));
    await waitFor(() => expect(document.querySelector("td.excerpt mark")?.textContent).toBe("429"));
    const cell = document.querySelector("td.excerpt")!;
    expect(cell.textContent).toBe("error…🙂 HTTPError: 429 Too Many");
    expect(screen.getByText("2 files have no text index yet; the search read them whole")).toBeTruthy();
    expect(screen.getByText("1 damaged file was skipped: s1")).toBeTruthy();

    act(() => screen.getByRole("button", { name: "Table" }).click());
    await waitFor(() => expect(params().view).toBe("table"));
    await waitFor(() => expect(document.querySelector("td.excerpt mark")?.textContent).toBe("429"));
  });
});

describe("url state", () => {
  it("round-trips every parameter", () => {
    const url = '/?project=p&time=12h&filter=eq(status,%20%22ok%22)&q=429%20Too%20Many%20Requests&trace=gt(latency,%2030)&tree=eq(status,%20%22error%22)&root=1&order=latency.desc&limit=10&cols=name,metadata.v&view=stats&by=name,metadata.v&m=count,p99(latency)&top=5&bucket=5m&live=1';
    const { q, m1 } = fromUrl(url.slice(1));
    expect(m1).toBe(false);
    expect(q).toEqual({
      project: "p", time: "12h", filter: 'eq(status, "ok")', q: "429 Too Many Requests", trace: "gt(latency, 30)", tree: 'eq(status, "error")', root: true,
      order: { by: "latency", dir: "desc" }, limit: 10, cols: ["name", "metadata.v"], view: "stats",
      by: ["name", "metadata.v"], m: ["count", "p99(latency)"], top: 5, bucket: "5m", live: true,
    });
    expect(toUrl(q)).toBe(url);
  });

  it("reads every view, and defaults the stats", () => {
    for (const v of ["samples", "table", "stats", "series", "dist"]) expect(fromUrl(`?view=${v}`).q.view).toBe(v);
    const { q } = fromUrl("?view=nope&top=99");
    expect(q).toMatchObject({ view: "samples", by: [], m: ["count", "error_rate", "p95(latency)", "sum(cost)"], top: 10 });
    expect(toUrl({ ...q, view: "stats" })).toBe("/?view=stats");
  });

  it("marks project-only links as M2, so they don't read as M1", () => {
    const { q } = fromUrl("?project=p&root=0");
    expect(q.root).toBe(false);
    expect(toUrl(q)).toBe("/?project=p&root=0");
    expect(toUrl(fromUrl("").q)).toBe("/");
  });
});

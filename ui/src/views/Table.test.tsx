import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { FakeEventSource, header, mockFetch, queryPage } from "../testutil";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

type Body = Record<string, unknown>;
const row = (n: number, columns: Record<string, unknown>) => ({ ...header(n), columns });

function setup(path: string, answer: (b: Body) => unknown = (b) => queryPage([
  row(1, (b.columns as string[] | undefined)?.includes("metadata.prompt_version") ? { "metadata.prompt_version": "v7", tags: ["a", "b"], latency: 41.5 } : {}),
])) {
  history.replaceState(null, "", path);
  const bodies: Body[] = [];
  mockFetch({
    "/api/query": (_u, b) => { bodies.push(b); return answer(b); },
    "/api/projects": () => [],
    "/api/fields/values": () => ({ values: [], other_count: 0, approximate: false }),
    "/api/fields": () => ({ fields: [{ path: "metadata.prompt_version", types: [{ type: "string", count: 3 }], complete: true }], complete: true }),
  });
  render(<App />);
  return bodies;
}

const last = (bodies: Body[]) => bodies[bodies.length - 1];
const params = () => Object.fromEntries(new URLSearchParams(window.location.search));
const heads = () => screen.getAllByRole("columnheader").map((h) => h.querySelector(".col-name")?.textContent ?? h.textContent);
const cells = () => [...screen.getAllByRole("row")[1].querySelectorAll("td")].map((td) => td.textContent);
const chips = () => [...screen.getByRole("group", { name: "This run" }).querySelectorAll(".chip-label")].map((c) => c.textContent);

describe("table view", () => {
  it("adds a column, sends it as columns, keeps it in the URL, and removes it", async () => {
    const bodies = setup("/");
    await waitFor(() => expect(bodies.length).toBe(1));
    expect(last(bodies).columns).toBeUndefined();

    act(() => screen.getByRole("button", { name: "Table" }).click());
    await waitFor(() => expect(params().view).toBe("table"));
    fireEvent.change(screen.getByLabelText("add column"), { target: { value: "metadata.prompt_version" } });
    fireEvent.submit(screen.getByLabelText("add column"));
    await waitFor(() => expect(last(bodies).columns).toEqual(["metadata.prompt_version"]));
    expect(params().cols).toBe("metadata.prompt_version");
    await waitFor(() => expect(cells()).toContain("v7"));
    expect(heads()).toEqual(["status", "name", "metadata.prompt_version", ""]);

    act(() => screen.getByRole("button", { name: "remove column metadata.prompt_version" }).click());
    await waitFor(() => expect(last(bodies).columns).toBeUndefined());
    expect(params().cols).toBeUndefined();
  });

  it("refuses a column it can't show", async () => {
    const bodies = setup("/?view=table");
    await waitFor(() => expect(bodies.length).toBe(1));
    fireEvent.change(screen.getByLabelText("add column"), { target: { value: "start_time" } });
    fireEvent.submit(screen.getByLabelText("add column"));
    expect(screen.getByRole("alert").textContent).toBe("start_time can't be a column; try a metadata, inputs or outputs path");
    for (const bad of ["metadata.a,b", "metadata.a, 1) or eq(name"]) {
      fireEvent.change(screen.getByLabelText("add column"), { target: { value: bad } });
      fireEvent.submit(screen.getByLabelText("add column"));
      expect(screen.getByRole("alert")).toBeTruthy();
    }
    expect(bodies.length).toBe(1);
  });

  it("adds a clicked cell value to This run; Alt-click excludes; lists filter element by element", async () => {
    const bodies = setup("/?view=table&cols=metadata.prompt_version,tags,latency");
    await waitFor(() => expect(cells()).toContain("v7"));
    expect(last(bodies).columns).toEqual(["metadata.prompt_version", "tags", "latency"]);

    act(() => within(screen.getAllByRole("row")[1]).getByRole("button", { name: "v7" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('eq(metadata.prompt_version, "v7")'));
    expect(window.location.pathname).toBe("/");

    await waitFor(() => expect(cells()).toContain("v7"));
    fireEvent.click(within(screen.getAllByRole("row")[1]).getByRole("button", { name: "b" }), { altKey: true });
    await waitFor(() => expect(last(bodies).filter).toBe('and(eq(metadata.prompt_version, "v7"), not(has(tags, "b")))'));

    await waitFor(() => expect(cells()).toContain("41.5"));
    act(() => within(screen.getAllByRole("row")[1]).getByRole("button", { name: "41.5" }).click());
    await waitFor(() => expect(chips()).toEqual(["metadata.prompt_version = v7", "not tags has b", "latency = 41.5"]));
  });

  it("adds a column from the field browser and opens the Table", async () => {
    const bodies = setup("/");
    await waitFor(() => expect(bodies.length).toBe(1));
    act(() => screen.getByRole("button", { name: "fields" }).click());
    await waitFor(() => screen.getByRole("button", { name: "column metadata.prompt_version" }));
    act(() => screen.getByRole("button", { name: "column metadata.prompt_version" }).click());
    await waitFor(() => expect(last(bodies).columns).toEqual(["metadata.prompt_version"]));
    expect(params()).toMatchObject({ view: "table", cols: "metadata.prompt_version" });
    await waitFor(() => expect(heads()).toContain("metadata.prompt_version"));
  });

  it("keeps columns out of the Samples query", async () => {
    const bodies = setup("/?cols=metadata.prompt_version");
    await waitFor(() => expect(bodies.length).toBe(1));
    expect(last(bodies).columns).toBeUndefined();
    act(() => screen.getByRole("button", { name: "Table" }).click());
    await waitFor(() => expect(last(bodies).columns).toEqual(["metadata.prompt_version"]));
    act(() => screen.getByRole("button", { name: "Samples" }).click());
    await waitFor(() => expect(params().view).toBeUndefined());
    expect(params().cols).toBe("metadata.prompt_version");
  });

  it("shows a run that stops matching a live Table view with its new column values", async () => {
    vi.stubGlobal("EventSource", FakeEventSource);
    const id = header(1).run_id;
    const bodies = setup("/?view=table&cols=metadata.prompt_version&live=1", (b) => queryPage(
      b.filter === `eq(run_id, "${id}")`
        ? [{ ...row(1, { "metadata.prompt_version": "v8" }), status: "ok", last_seq: 50 }]
        : [row(1, { "metadata.prompt_version": "v7" })]));
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => expect(cells()).toContain("v7"));

    act(() => FakeEventSource.last!.emit("left", [id]));
    await waitFor(() => expect(cells()).toContain("v8"));
    expect(screen.getByText("no longer matching")).toBeTruthy();
    expect(screen.getByText("✓ ok")).toBeTruthy();
    expect(last(bodies)).toEqual({ filter: `eq(run_id, "${id}")`, columns: ["metadata.prompt_version"], limit: 1 });
  });
});

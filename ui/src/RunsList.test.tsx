import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RunsList } from "./RunsList";
import { FakeEventSource, header, mockFetch } from "./testutil";

beforeEach(() => {
  vi.stubGlobal("EventSource", FakeEventSource);
  window.scrollY = 0;
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const rowNames = () => screen.getAllByRole("row").slice(1).map((r) => r.querySelector("td.name")?.textContent);

describe("runs list", () => {
  it("loads after hello, newest first, then shows live additions and updates", async () => {
    mockFetch({ "/api/roots": () => ({ runs: [header(2), header(1)], next_cursor: null }), "/api/projects": () => [] });
    render(<RunsList />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => expect(rowNames()).toEqual(["task-2", "task-1"]));

    act(() => FakeEventSource.last!.emit("runs", [header(3)]));
    await waitFor(() => expect(rowNames()).toEqual(["task-3", "task-2", "task-1"]));

    act(() => FakeEventSource.last!.emit("runs", [header(1, { status: "error", error: "boom", last_seq: 99, latency_ms: 31076 })]));
    await waitFor(() => expect(screen.getAllByRole("row")[3].textContent).toContain("✕ err"));
    expect(screen.getAllByRole("row")[3].textContent).toContain("31.1 s");
  });

  it("ignores stale versions of a run", async () => {
    mockFetch({ "/api/roots": () => ({ runs: [header(1, { status: "ok", last_seq: 10 })], next_cursor: null }), "/api/projects": () => [] });
    render(<RunsList />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => expect(rowNames()).toEqual(["task-1"]));
    act(() => FakeEventSource.last!.emit("runs", [header(1, { status: "running", last_seq: 5 })]));
    expect(screen.getAllByRole("row")[1].textContent).toContain("✓ ok");
  });

  it("shows a 'new runs' pill instead of moving the list when scrolled down", async () => {
    mockFetch({ "/api/roots": () => ({ runs: [header(1)], next_cursor: null }), "/api/projects": () => [] });
    render(<RunsList />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => expect(rowNames()).toEqual(["task-1"]));
    window.scrollY = 500;
    act(() => { window.dispatchEvent(new Event("scroll")); });
    act(() => FakeEventSource.last!.emit("runs", [header(2)]));
    await waitFor(() => screen.getByText("1 new run ↑"));
    expect(rowNames()).toEqual(["task-1"]);
    act(() => screen.getByText("1 new run ↑").click());
    await waitFor(() => expect(rowNames()).toEqual(["task-2", "task-1"]));
  });

  it("drops live runs that don't match the status filter", async () => {
    const calls = mockFetch({ "/api/roots": () => ({ runs: [header(1, { status: "error" })], next_cursor: null }), "/api/projects": () => [] });
    render(<RunsList status="error" />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => expect(rowNames()).toEqual(["task-1"]));
    expect(calls.some((c) => c.includes("status=error"))).toBe(true);
    act(() => FakeEventSource.last!.emit("runs", [header(2, { status: "ok" }), header(1, { status: "ok", last_seq: 50 })]));
    await waitFor(() => screen.getByText("No runs match these filters."));
  });

  it("pages older runs with the cursor", async () => {
    const calls = mockFetch({
      "/api/roots": (u) => u.searchParams.get("cursor")
        ? { runs: [header(1)], next_cursor: null }
        : { runs: [header(3), header(2)], next_cursor: "c1" },
      "/api/projects": () => [],
    });
    render(<RunsList />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => screen.getByText("older ▸"));
    act(() => screen.getByText("older ▸").click());
    await waitFor(() => expect(rowNames()).toEqual(["task-3", "task-2", "task-1"]));
    expect(calls.some((c) => c.includes("cursor=c1"))).toBe(true);
    expect(screen.queryByText("older ▸")).toBeNull();
  });

  it("explains setup when there are no runs at all", async () => {
    mockFetch({ "/api/roots": () => ({ runs: [], next_cursor: null }), "/api/projects": () => [] });
    render(<RunsList />);
    act(() => FakeEventSource.last!.emit("hello"));
    await waitFor(() => screen.getByText("No traces yet"));
    expect(screen.getByText(/LANGSMITH_ENDPOINT=/)).toBeTruthy();
  });
});

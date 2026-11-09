import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { StorageLine } from "./StorageLine";
import { mockFetch } from "./testutil";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

const ago = (s: number) => new Date(Date.now() - s * 1000).toISOString();
const health = (files: object, compaction: object) => () => ({
  ok: true,
  files: { l0: 0, l1: 0, replaced: 0, ...files },
  compaction: { enabled: true, running: false, last_run_at: null, last_error: null, stopped: false, ...compaction },
});

describe("StorageLine", () => {
  it("shows the file count, how many are compacted, when compaction last ran and old files waiting", async () => {
    mockFetch({ "/healthz": health({ l0: 3, l1: 20, replaced: 5 }, { last_run_at: ago(150) }) });
    render(<StorageLine />);
    const line = await screen.findByText(/23 data files/);
    expect(line.textContent).toBe("23 data files (20 compacted, 3 new) · compacted 2 min ago · 5 old files waiting for deletion");
  });

  it("says when compaction failed, stopped, is running, is off or hasn't run", async () => {
    for (const [c, text] of [
      [{ last_error: "storage: disk full" }, "compaction failed: storage: disk full"],
      [{ stopped: true }, "compaction stopped; restart to resume"],
      [{ running: true }, "compacting…"],
      [{ enabled: false }, "compaction off"],
      [{}, "not compacted yet"],
    ] as const) {
      mockFetch({ "/healthz": health({ l0: 1 }, c) });
      render(<StorageLine />);
      expect((await screen.findByText(text)).className).toBe(text.startsWith("compaction failed") || text.includes("stopped") ? "warn" : "");
      cleanup();
    }
  });

  it("shows nothing for a server without the M4 fields, or when /healthz fails", async () => {
    mockFetch({ "/healthz": () => ({ ok: true, orphan_feedback: 0 }) });
    const { container } = render(<StorageLine />);
    await new Promise((r) => setTimeout(r, 20));
    expect(container.textContent).toBe("");
    cleanup();
    mockFetch({});
    const second = render(<StorageLine />);
    await new Promise((r) => setTimeout(r, 20));
    expect(second.container.textContent).toBe("");
  });
});

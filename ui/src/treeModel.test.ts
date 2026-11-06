import { describe, expect, it } from "vitest";
import { bar, buildTree, flatten, timeWindow } from "./treeModel";
import { header } from "./testutil";

const root = header(1, { start_time: "2026-11-05T02:00:00.000Z", end_time: "2026-11-05T02:00:10.000Z", status: "ok", dotted_order: "a" });
const kid = (n: number, order: string, parent = root.run_id, over = {}) =>
  header(n, { is_root: false, parent_run_id: parent, trace_id: root.trace_id, dotted_order: order, ...over });

describe("tree model", () => {
  it("orders by dotted_order and nests by parent", () => {
    const a = kid(2, "a.1"), b = kid(3, "a.2"), a1 = kid(4, "a.1.1", a.run_id);
    const rows = flatten(buildTree([b, a1, root, a]), new Set());
    expect(rows.map((r) => [r.node.run.name, r.depth])).toEqual([["task-1", 0], ["task-2", 1], ["task-4", 2], ["task-3", 1]]);
  });

  it("collapsing a node hides its descendants only", () => {
    const a = kid(2, "a.1"), b = kid(3, "a.2"), a1 = kid(4, "a.1.1", a.run_id);
    const rows = flatten(buildTree([root, a, a1, b]), new Set([a.run_id]));
    expect(rows.map((r) => r.node.run.name)).toEqual(["task-1", "task-2", "task-3"]);
    expect(rows[1].collapsed && rows[1].hasChildren).toBe(true);
  });

  it("shows a child whose parent hasn't arrived at the top level", () => {
    const orphan = kid(5, "z.1", "00000000-0000-4000-8000-999999999999");
    expect(flatten(buildTree([root, orphan]), new Set()).map((r) => r.depth)).toEqual([0, 0]);
  });

  it("places bars in the trace's time window; running runs extend to now", () => {
    const w = timeWindow([root], 0);
    expect(w).toEqual({ start: Date.parse("2026-11-05T02:00:00Z"), end: Date.parse("2026-11-05T02:00:10Z") });
    const half = kid(2, "a.1", root.run_id, { start_time: "2026-11-05T02:00:05.000Z", end_time: "2026-11-05T02:00:10.000Z" });
    expect(bar(half, w, 0)).toEqual([0.5, 0.5]);
    const running = kid(3, "a.2", root.run_id, { start_time: "2026-11-05T02:00:00.000Z", end_time: null });
    const now = Date.parse("2026-11-05T02:00:20Z");
    const w2 = timeWindow([root, running], now);
    expect(w2.end).toBe(now);
    expect(bar(running, w2, now)).toEqual([0, 1]);
  });
});

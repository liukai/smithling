import type { RunHeader } from "./api";

export interface TreeNode {
  run: RunHeader;
  children: TreeNode[];
}

export interface Row {
  node: TreeNode;
  depth: number;
  hasChildren: boolean;
  collapsed: boolean;
}

/** Builds the tree from runs; a run whose parent hasn't arrived yet is shown at the top level. */
export function buildTree(runs: RunHeader[]): TreeNode[] {
  const sorted = [...runs].sort((a, b) => (a.dotted_order < b.dotted_order ? -1 : a.dotted_order > b.dotted_order ? 1 : 0));
  const byId = new Map<string, TreeNode>();
  for (const run of sorted) byId.set(run.run_id, { run, children: [] });
  const roots: TreeNode[] = [];
  for (const run of sorted) {
    const node = byId.get(run.run_id)!;
    const parent = run.parent_run_id ? byId.get(run.parent_run_id) : undefined;
    if (parent) parent.children.push(node);
    else roots.push(node);
  }
  return roots;
}

/** Visible rows, depth-first; the tree and the waterfall share this list so they stay aligned. */
export function flatten(roots: TreeNode[], collapsed: Set<string>): Row[] {
  const rows: Row[] = [];
  const walk = (n: TreeNode, depth: number) => {
    const isCollapsed = collapsed.has(n.run.run_id);
    rows.push({ node: n, depth, hasChildren: n.children.length > 0, collapsed: isCollapsed });
    if (!isCollapsed) for (const c of n.children) walk(c, depth + 1);
  };
  for (const r of roots) walk(r, 0);
  return rows;
}

export interface Span {
  start: number;
  end: number;
}

/** Time window covering every run; running runs extend to `now`. */
export function timeWindow(runs: RunHeader[], now: number): Span {
  let start = Infinity, end = -Infinity;
  for (const r of runs) {
    if (!r.start_time) continue;
    const s = Date.parse(r.start_time);
    const e = r.end_time ? Date.parse(r.end_time) : now;
    start = Math.min(start, s);
    end = Math.max(end, e, s);
  }
  if (!Number.isFinite(start)) return { start: now, end: now + 1 };
  return { start, end: Math.max(end, start + 1) };
}

/** Bar position as fractions of the window: [left, width]. */
export function bar(run: RunHeader, w: Span, now: number): [number, number] | null {
  if (!run.start_time) return null;
  const s = Date.parse(run.start_time);
  const e = run.end_time ? Date.parse(run.end_time) : now;
  const total = w.end - w.start;
  const left = (s - w.start) / total;
  const width = Math.max((Math.max(e, s) - s) / total, 0.002);
  return [Math.min(Math.max(left, 0), 1), Math.min(width, 1 - Math.min(Math.max(left, 0), 1))];
}

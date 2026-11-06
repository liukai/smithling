import { useVirtualizer } from "@tanstack/react-virtual";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { type RunHeader, getTrace } from "./api";
import { fmtLatency } from "./format";
import { subscribe, upsert } from "./live";
import { navigate, withParams } from "./route";
import { StatusBadge } from "./StatusBadge";
import { StepPanel } from "./StepPanel";
import { type Row, bar, buildTree, flatten, timeWindow } from "./treeModel";

const ROW_HEIGHT = 24;

export function TraceView({ traceId, selected }: { traceId: string; selected?: string }) {
  const [runs, setRuns] = useState<Map<string, RunHeader>>(new Map());
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());

  const reload = useCallback(async () => {
    try {
      const t = await getTrace(traceId);
      setRuns(new Map(t.runs.map((r) => [r.run_id, r])));
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, [traceId]);

  useEffect(() => subscribe({ trace: traceId }, {
    onHello: reload,
    onResync: reload,
    onRuns: (rs) => setRuns((m) => { const next = new Map(m); return upsert(next, rs) ? next : m; }),
  }), [traceId, reload]);

  const list = useMemo(() => [...runs.values()], [runs]);
  const running = list.some((r) => r.status === "running");
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [running]);

  const tree = useMemo(() => buildTree(list), [list]);
  const rows = useMemo(() => flatten(tree, collapsed), [tree, collapsed]);
  const window_ = useMemo(() => timeWindow(list, now), [list, now]);
  const root = list.find((r) => r.is_root);
  const sel = selected ? runs.get(selected) : undefined;

  const toggle = (id: string) => setCollapsed((s) => { const n = new Set(s); n.has(id) ? n.delete(id) : n.add(id); return n; });
  const select = (id?: string) => navigate(withParams(`/trace/${traceId}`, { run: id }), true);

  return (
    <div className="trace">
      <div className="trace-head">
        <a href="/" onClick={(e) => { e.preventDefault(); navigate("/"); }}>← runs</a>
        <strong>{root?.name ?? "trace"}</strong>
        {root && <StatusBadge status={root.status} />}
        <span className="muted">{list.length} steps · {fmtLatency(root?.latency_ms ?? null)}</span>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <div className={sel ? "trace-body with-panel" : "trace-body"}>
        <TreeRows rows={rows} window_={window_} now={now} selected={selected} onToggle={toggle} onSelect={select} />
        {sel && <StepPanel header={sel} onClose={() => select(undefined)} />}
      </div>
    </div>
  );
}

function TreeRows(p: {
  rows: Row[]; window_: { start: number; end: number }; now: number; selected?: string;
  onToggle: (id: string) => void; onSelect: (id: string) => void;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  const virtual = typeof ResizeObserver !== "undefined";
  const v = useVirtualizer({ count: p.rows.length, getScrollElement: () => scroller.current, estimateSize: () => ROW_HEIGHT, overscan: 20 });
  const items = virtual ? v.getVirtualItems().map((i) => ({ index: i.index, top: i.start })) : p.rows.map((_, i) => ({ index: i, top: i * ROW_HEIGHT }));
  return (
    <div className="tree" ref={scroller}>
      <div style={{ height: p.rows.length * ROW_HEIGHT, position: "relative" }}>
        {items.map(({ index, top }) => {
          const r = p.rows[index];
          const run = r.node.run;
          const b = bar(run, p.window_, p.now);
          return (
            <div key={run.run_id} role="treeitem" aria-selected={run.run_id === p.selected}
              className={run.run_id === p.selected ? "trow selected" : "trow"} style={{ top, height: ROW_HEIGHT }}
              onClick={() => p.onSelect(run.run_id)}>
              <div className="tcell" style={{ paddingLeft: 6 + r.depth * 14 }}>
                <span className="toggle" onClick={(e) => { e.stopPropagation(); if (r.hasChildren) p.onToggle(run.run_id); }}>
                  {r.hasChildren ? (r.collapsed ? "▸" : "▾") : " "}
                </span>
                <span className={`dot status-${run.status}`}>●</span>
                <span className="tname">{run.name ?? "(pending)"}</span>
                <span className="ttype muted">{run.run_type}</span>
              </div>
              <div className="wcell">
                {b && <div className={`wbar bar-${run.status}`} style={{ left: `${b[0] * 100}%`, width: `${b[1] * 100}%` }} />}
              </div>
              <div className="dcell muted">{run.status === "running" ? "…" : fmtLatency(run.latency_ms)}</div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

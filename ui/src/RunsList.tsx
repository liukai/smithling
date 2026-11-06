import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { type ProjectInfo, type RunHeader, getProjects, getRoots, rootOrder } from "./api";
import { EmptyState } from "./EmptyState";
import { elapsedSince, fmtLatency, fmtTime, fmtTokens } from "./format";
import { subscribe, upsert } from "./live";
import { navigate, withParams } from "./route";
import { StatusBadge } from "./StatusBadge";

const PAGE = 50;

export function RunsList({ project, status }: { project?: string; status?: string }) {
  const [rows, setRows] = useState<Map<string, RunHeader>>(new Map());
  const [cursor, setCursor] = useState<string | null>(null);
  const [incoming, setIncoming] = useState<Map<string, RunHeader>>(new Map());
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());
  const atTop = useRef(true);
  const rowsRef = useRef(rows);
  rowsRef.current = rows;

  const matches = useCallback(
    (r: RunHeader) => (!project || r.project === project) && (!status || r.status === status),
    [project, status],
  );

  const reload = useCallback(async () => {
    try {
      const page = await getRoots({ project, status, limit: PAGE });
      setRows(new Map(page.runs.map((r) => [r.run_id, r])));
      setCursor(page.next_cursor);
      setIncoming(new Map());
      setError(null);
      getProjects().then(setProjects).catch(() => {});
    } catch (e) {
      setError(String(e));
    }
  }, [project, status]);

  useEffect(() => {
    setRows(new Map());
    return subscribe(
      { project },
      {
        onHello: reload,
        onResync: reload,
        onConnected: setConnected,
        onRuns: (runs) => {
          const known = runs.filter((r) => rowsRef.current.has(r.run_id));
          const fresh = runs.filter((r) => !rowsRef.current.has(r.run_id) && matches(r));
          if (known.length) {
            setRows((m) => {
              const next = new Map(m);
              for (const r of known) if (!matches(r)) next.delete(r.run_id);
              return upsert(next, known.filter(matches)) || next.size !== m.size ? next : m;
            });
          }
          if (fresh.length) {
            if (atTop.current) setRows((m) => { const next = new Map(m); upsert(next, fresh); return next; });
            else setIncoming((m) => { const next = new Map(m); upsert(next, fresh); return next; });
          }
          if (fresh.length) getProjects().then(setProjects).catch(() => {});
        },
      },
    );
  }, [project, reload, matches]);

  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    const onScroll = () => { atTop.current = window.scrollY < 40; };
    window.addEventListener("scroll", onScroll);
    return () => { clearInterval(t); window.removeEventListener("scroll", onScroll); };
  }, []);

  const sorted = useMemo(() => [...rows.values()].sort(rootOrder), [rows]);

  const loadOlder = async () => {
    if (!cursor) return;
    const page = await getRoots({ project, status, cursor, limit: PAGE });
    setRows((m) => { const next = new Map(m); upsert(next, page.runs); return next; });
    setCursor(page.next_cursor);
  };

  const showIncoming = () => {
    setRows((m) => { const next = new Map(m); upsert(next, [...incoming.values()]); return next; });
    setIncoming(new Map());
    window.scrollTo({ top: 0 });
  };

  const setFilter = (k: "project" | "status", v: string) =>
    navigate(withParams("/", { project, status, [k]: v || undefined }), true);

  const filtered = Boolean(project || status);
  return (
    <div>
      <div className="toolbar">
        <label>
          project{" "}
          <select value={project ?? ""} onChange={(e) => setFilter("project", e.target.value)}>
            <option value="">all</option>
            {projects.map((p) => (
              <option key={p.name} value={p.name}>{p.name} ({p.runs})</option>
            ))}
            {project && !projects.some((p) => p.name === project) && <option value={project}>{project}</option>}
          </select>
        </label>
        <label>
          status{" "}
          <select value={status ?? ""} onChange={(e) => setFilter("status", e.target.value)}>
            <option value="">all</option>
            <option value="running">running</option>
            <option value="ok">ok</option>
            <option value="error">error</option>
          </select>
        </label>
        <span className={connected ? "live on" : "live off"}>{connected ? "● live" : "○ reconnecting"}</span>
      </div>

      {incoming.size > 0 && (
        <button className="pill" onClick={showIncoming}>{incoming.size} new run{incoming.size > 1 ? "s" : ""} ↑</button>
      )}
      {error && <div className="error-banner">{error}</div>}

      {sorted.length === 0 && !filtered && connected ? (
        <EmptyState />
      ) : (
        <table className="runs">
          <thead>
            <tr><th>status</th><th>name</th><th>start</th><th>latency</th><th>tokens</th><th>project</th><th>tags</th></tr>
          </thead>
          <tbody>
            {sorted.map((r) => (
              <tr key={r.run_id} onClick={() => navigate(`/trace/${r.trace_id}`)}>
                <td><StatusBadge status={r.status} /></td>
                <td className="name">{r.name}</td>
                <td>{fmtTime(r.start_time)}</td>
                <td>{r.status === "running" ? `${fmtLatency(elapsedSince(r.start_time, now))}…` : fmtLatency(r.latency_ms)}</td>
                <td>{fmtTokens(r.total_tokens)}</td>
                <td>{r.project}</td>
                <td>{r.tags.map((t) => <span key={t} className="tag">{t}</span>)}</td>
              </tr>
            ))}
            {sorted.length === 0 && (
              <tr><td colSpan={7} className="muted">No runs match these filters.</td></tr>
            )}
          </tbody>
        </table>
      )}
      {cursor && <button className="older" onClick={loadOlder}>older ▸</button>}
    </div>
  );
}

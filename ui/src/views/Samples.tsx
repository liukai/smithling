import { type MouseEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, type QueryRun, type QueryWarning, postQuery } from "../api";
import { EmptyState } from "../EmptyState";
import { elapsedSince, fmtLatency, fmtTime, fmtTokens } from "../format";
import type { Expr } from "../query/ast";
import { type QueryState, toBody } from "../query/url";
import { navigate } from "../route";
import { StatusBadge } from "../StatusBadge";

function warningText(w: QueryWarning): string {
  switch (w.kind) {
    case "mixed_types":
      return `${w.path}: mixed types (${Object.entries(w.counts).map(([t, n]) => `${t} ${n}`).join(", ")}); only values of the compared type match`;
    case "not_indexed":
      return `${w.path} isn't indexed; read the payloads of ${w.runs} runs`;
    case "identity_conflict":
      return `run ${w.run_id} has conflicting versions`;
  }
}

/** Runs matching the query, newest first by default, paged by cursor as you scroll. */
export function Samples({ q, onFilter, onFilterError }: {
  q: QueryState;
  /** Click adds the condition; Alt-click adds its negation. */
  onFilter: (e: Expr, negate: boolean) => void;
  /** A filter slot the server rejected (`bad_filter`, `unsupported`). */
  onFilterError: (e: ApiError) => void;
}) {
  const bodyKey = useMemo(() => JSON.stringify(toBody(q)), [q]);
  const [rows, setRows] = useState<QueryRun[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<QueryWarning[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [filterError, setFilterError] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [now, setNow] = useState(Date.now());
  const gen = useRef(0);
  const busy = useRef(false);
  const sentinel = useRef<HTMLDivElement>(null);
  const onFilterErrorRef = useRef(onFilterError);
  onFilterErrorRef.current = onFilterError;

  const fetchPage = useCallback(async (after: string | null) => {
    const g = after ? gen.current : ++gen.current;
    busy.current = true;
    try {
      const page = await postQuery({ ...JSON.parse(bodyKey), cursor: after ?? undefined });
      if (g !== gen.current) return;
      setRows((rs) => {
        if (!after) return page.runs;
        const seen = new Set(rs.map((r) => r.run_id));
        return [...rs, ...page.runs.filter((r) => !seen.has(r.run_id))];
      });
      setCursor(page.next_cursor);
      if (!after) setWarnings(page.warnings);
      setError(null);
      setFilterError(false);
    } catch (e) {
      if (g !== gen.current) return;
      if (!after) { setRows([]); setCursor(null); setWarnings([]); }
      const inSlot = e instanceof ApiError && e.slot;
      setFilterError(Boolean(inSlot));
      if (inSlot) onFilterErrorRef.current(e);
      else setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (g === gen.current) { busy.current = false; setLoaded(true); }
    }
  }, [bodyKey]);

  useEffect(() => { fetchPage(null); }, [fetchPage]);

  const more = useCallback(() => { if (cursor && !busy.current) fetchPage(cursor); }, [cursor, fetchPage]);

  useEffect(() => {
    const el = sentinel.current;
    if (!el || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver((es) => { if (es.some((e) => e.isIntersecting)) more(); });
    io.observe(el);
    return () => io.disconnect();
  }, [more]);

  const running = rows.some((r) => r.status === "running");
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [running]);

  const click = (e: MouseEvent, expr: Expr) => {
    e.stopPropagation();
    onFilter(expr, e.altKey);
  };
  const hint = "click to filter, Alt-click to exclude";
  const unfiltered = !q.project && !q.time && !q.filter && !q.trace && !q.tree && !q.root;

  if (loaded && !error && !filterError && rows.length === 0 && unfiltered) return <EmptyState />;
  return (
    <div className="samples">
      <div className="samples-head">
        <span className="count">{rows.length}{cursor ? "+" : ""} run{rows.length === 1 && !cursor ? "" : "s"}</span>
      </div>
      {error && <div className="error-banner">{error}</div>}
      {warnings.map((w, i) => <div key={i} className="warning">{warningText(w)}</div>)}
      <table className="runs">
        <thead>
          <tr><th>status</th><th>name</th><th>start</th><th>latency</th><th>tokens</th><th>project</th><th>tags</th></tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.run_id} tabIndex={0} onClick={() => navigate(`/trace/${r.trace_id}`)}
              onKeyDown={(e) => { if (e.key === "Enter" && e.target === e.currentTarget) navigate(`/trace/${r.trace_id}`); }}>
              <td>
                <button className="clickable" title={hint} onClick={(e) => click(e, { t: "cmp", op: "eq", path: "status", value: r.status })}>
                  <StatusBadge status={r.status} />
                </button>
              </td>
              <td className="name">
                {r.name != null && (
                  <button className="clickable" title={hint} onClick={(e) => click(e, { t: "cmp", op: "eq", path: "name", value: r.name })}>{r.name}</button>
                )}
              </td>
              <td>{fmtTime(r.start_time)}</td>
              <td>{r.status === "running" ? `${fmtLatency(elapsedSince(r.start_time, now))}…` : fmtLatency(r.latency_ms)}</td>
              <td>{fmtTokens(r.total_tokens)}</td>
              <td>{r.project}</td>
              <td>
                {r.tags.map((t) => (
                  <button key={t} className="tag clickable" title={hint} onClick={(e) => click(e, { t: "has", path: "tags", value: t })}>{t}</button>
                ))}
              </td>
            </tr>
          ))}
          {loaded && !error && rows.length === 0 && (
            <tr><td colSpan={7} className="muted">{filterError ? "The query has an error; open Text to see it." : "No runs match this query."}</td></tr>
          )}
        </tbody>
      </table>
      <div ref={sentinel} />
      {cursor && <button className="older" onClick={more}>more ▸</button>}
    </div>
  );
}

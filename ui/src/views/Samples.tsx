import { type MouseEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, type QueryRun, type QueryWarning, getRunHeader, postQuery } from "../api";
import { EmptyState } from "../EmptyState";
import { elapsedSince, fmtLatency, fmtTime, fmtTokens } from "../format";
import { compareRuns, mergeLive, subscribe } from "../live";
import type { Expr } from "../query/ast";
import { type Order, type QueryState, toBody } from "../query/url";
import { navigate } from "../route";
import { StatusBadge } from "../StatusBadge";
import { Table } from "./Table";

type Conn = "on" | "retry" | "closed";
const CONN: Record<Conn, string> = { on: "● live", retry: "○ reconnecting", closed: "○ live unavailable" };
type Live = { kind: "runs"; runs: QueryRun[] } | { kind: "left"; ids: string[] };

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

/**
 * Runs matching the query, newest first by default, paged by cursor as you scroll; as samples or as a table.
 * Live: loads after the stream's `hello`, merges `runs`, marks shown runs that `left`, reloads on `resync`.
 */
export function Samples({ q, onCols, onFilter, onFilterError }: {
  q: QueryState;
  onCols: (cols: string[]) => void;
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
  const [left, setLeft] = useState<ReadonlySet<string>>(new Set());
  const [conn, setConn] = useState<Conn>("retry");
  const gen = useRef(0);
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  const cursorRef = useRef(cursor);
  cursorRef.current = cursor;
  /** Live events that arrive while the first page loads, applied to it when it lands. */
  const queued = useRef<Live[] | null>(null);
  /** Per run, the latest `left` lookup; a newer `runs` for it cancels the lookup. */
  const leaving = useRef(new Map<string, number>());
  const token = useRef(0);
  const busy = useRef(false);
  const sentinel = useRef<HTMLDivElement>(null);
  const onFilterErrorRef = useRef(onFilterError);
  onFilterErrorRef.current = onFilterError;
  const order: Order = useMemo(() => JSON.parse(bodyKey).order, [bodyKey]);

  /** Shows the new state of shown runs that no longer match, marked so. */
  const leave = useCallback((ids: Iterable<string>, shown: QueryRun[]) => {
    const g = gen.current;
    const on = new Set(shown.map((r) => r.run_id));
    for (const id of [...ids].filter((i) => on.has(i))) {
      const t = ++token.current;
      leaving.current.set(id, t);
      getRunHeader(id).catch(() => null).then((r) => {
        if (g !== gen.current || leaving.current.get(id) !== t) return;
        leaving.current.delete(id);
        setLeft((l) => new Set(l).add(id));
        if (r) setRows((rs) => rs.map((x) => (x.run_id === id && x.last_seq <= r.last_seq ? { ...r, columns: x.columns } : x)).sort(compareRuns(order)));
      });
    }
  }, [order]);

  const fetchPage = useCallback(async (after: string | null) => {
    const g = after ? gen.current : ++gen.current;
    if (!after) queued.current = [];
    busy.current = true;
    try {
      const page = await postQuery({ ...JSON.parse(bodyKey), cursor: after ?? undefined });
      if (g !== gen.current) return;
      if (!after) {
        let next = page.runs;
        const gone = new Set<string>();
        for (const ev of queued.current ?? []) {
          if (ev.kind === "left") ev.ids.forEach((id) => gone.add(id));
          else {
            ev.runs.forEach((r) => gone.delete(r.run_id));
            next = mergeLive(next, ev.runs, order, page.next_cursor !== null);
          }
        }
        queued.current = null;
        setLeft(new Set());
        leaving.current.clear();
        setRows(next);
        leave(gone, next);
      } else {
        setRows((rs) => {
          const seen = new Set(rs.map((r) => r.run_id));
          return [...rs, ...page.runs.filter((r) => !seen.has(r.run_id))].sort(compareRuns(order));
        });
      }
      setCursor(page.next_cursor);
      if (!after) setWarnings(page.warnings);
      setError(null);
      setFilterError(false);
    } catch (e) {
      if (g !== gen.current) return;
      if (!after) { queued.current = null; setRows([]); setCursor(null); setWarnings([]); }
      const inSlot = e instanceof ApiError && e.slot;
      setFilterError(Boolean(inSlot));
      if (inSlot) onFilterErrorRef.current(e);
      else setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (g === gen.current) { busy.current = false; setLoaded(true); }
    }
  }, [bodyKey, order, leave]);

  useEffect(() => { if (!q.live) fetchPage(null); }, [fetchPage, q.live]);

  useEffect(() => {
    if (!q.live) return;
    let loaded = false;
    const load = () => { loaded = true; fetchPage(null); };
    setConn("retry");
    return subscribe({ q: JSON.parse(bodyKey) }, {
      onHello: load,
      onResync: load,
      // A stream that can't open (a bad query, say) still gets the view loaded, so its error shows.
      onConnected: (c, closed) => { setConn(c ? "on" : closed ? "closed" : "retry"); if (!c && !loaded) load(); },
      onRuns: (runs) => {
        if (queued.current) return void queued.current.push({ kind: "runs", runs });
        const ids = new Set(runs.map((r) => r.run_id));
        for (const id of ids) leaving.current.delete(id);
        setLeft((l) => ([...l].some((id) => ids.has(id)) ? new Set([...l].filter((id) => !ids.has(id))) : l));
        setRows((rs) => mergeLive(rs, runs, order, cursorRef.current !== null));
      },
      onLeft: (ids) => {
        if (queued.current) return void queued.current.push({ kind: "left", ids });
        leave(ids, rowsRef.current);
      },
    });
  }, [q.live, bodyKey, order, fetchPage, leave]);

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

  const empty = loaded && !error && rows.length === 0
    ? (filterError ? "The query has an error; open Text to see it." : "No runs match this query.")
    : null;

  if (loaded && !error && !filterError && rows.length === 0 && unfiltered) return <EmptyState />;
  return (
    <div className="samples">
      <div className="samples-head">
        {q.live && <span className={conn === "on" ? "live on" : "live off"}>{CONN[conn]}</span>}
        <span className="count">{rows.length}{cursor ? "+" : ""} run{rows.length === 1 && !cursor ? "" : "s"}</span>
      </div>
      {error && <div className="error-banner">{error}</div>}
      {warnings.map((w, i) => <div key={i} className="warning">{warningText(w)}</div>)}
      {q.view === "table" ? (
        <Table rows={rows} cols={q.cols} left={left} empty={empty} onCell={click} onCols={onCols} />
      ) : (
        <table className="runs">
          <thead>
            <tr><th>status</th><th>name</th><th>start</th><th>latency</th><th>tokens</th><th>project</th><th>tags</th></tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.run_id} tabIndex={0} className={left.has(r.run_id) ? "left" : undefined} onClick={() => navigate(`/trace/${r.trace_id}`)}
                onKeyDown={(e) => { if (e.key === "Enter" && e.target === e.currentTarget) navigate(`/trace/${r.trace_id}`); }}>
                <td>
                  <button className="clickable" title={hint} onClick={(e) => click(e, { t: "cmp", op: "eq", path: "status", value: r.status })}>
                    <StatusBadge status={r.status} />
                  </button>
                  {left.has(r.run_id) && <span className="left-mark">no longer matching</span>}
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
            {empty && <tr><td colSpan={7} className="muted">{empty}</td></tr>}
          </tbody>
        </table>
      )}
      <div ref={sentinel} />
      {cursor && <button className="older" onClick={more}>more ▸</button>}
    </div>
  );
}

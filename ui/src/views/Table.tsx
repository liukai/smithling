import { type FormEvent, type MouseEvent, useState } from "react";
import type { QueryRun } from "../api";
import { ATTR_ROOTS, type Expr, type Value } from "../query/ast";
import { parse } from "../query/parse";
import { navigate } from "../route";
import { StatusBadge } from "../StatusBadge";
import { Excerpt } from "./Excerpt";

/** Fields the server fills in `columns`; any metadata, inputs or outputs path works too. */
const COLUMN_FIELDS = ["status", "name", "model", "latency", "total_tokens", "tags"];
const SUGGEST = [...COLUMN_FIELDS, ...ATTR_ROOTS.map((r) => `${r}.`)];
const HINT = "click to filter, Alt-click to exclude";

/** `cols=` is comma-separated, so a path with a comma can't be a column. */
export function canBeColumn(c: string): boolean {
  if (COLUMN_FIELDS.includes(c)) return true;
  if (c.includes(",") || !ATTR_ROOTS.some((r) => c.startsWith(`${r}.`))) return false;
  const r = parse(`eq(${c}, null)`);
  return r.ok && r.expr.t === "cmp" && r.expr.path === c;
}

function Cell({ path, value, onCell }: { path: string; value: unknown; onCell: (e: MouseEvent, x: Expr) => void }) {
  if (value == null) return null;
  if (Array.isArray(value)) {
    return value.map((v, i) => (
      <button key={i} className="tag clickable" title={HINT} onClick={(e) => onCell(e, { t: "has", path, value: String(v) })}>{String(v)}</button>
    ));
  }
  if (typeof value === "object") return <span className="muted">{JSON.stringify(value)}</span>;
  return (
    <button className="clickable" title={HINT} onClick={(e) => onCell(e, { t: "cmp", op: "eq", path, value: value as Value })}>{String(value)}</button>
  );
}

function AddColumn({ cols, onCols }: { cols: string[]; onCols: (cols: string[]) => void }) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const add = (e: FormEvent) => {
    e.preventDefault();
    const c = text.trim();
    if (!c) return;
    if (!canBeColumn(c)) return setError(`${c} can't be a column; try a metadata, inputs or outputs path`);
    if (!cols.includes(c)) onCols([...cols, c]);
    setText("");
  };
  return (
    <form className="add-column" onSubmit={add}>
      <input aria-label="add column" list="tracedb-columns" placeholder="+ column" value={text}
        onChange={(e) => { setText(e.target.value); setError(null); }} />
      <datalist id="tracedb-columns">{SUGGEST.map((f) => <option key={f} value={f} />)}</datalist>
      {error && <span role="alert" className="chip-error">{error}</span>}
    </form>
  );
}

/** Runs with the columns the user chose; any cell value filters on click. */
export function Table({ rows, cols, left, empty, matched, onCell, onCols }: {
  rows: QueryRun[];
  cols: string[];
  left: ReadonlySet<string>;
  empty: string | null;
  /** Show the search's excerpts. */
  matched: boolean;
  onCell: (e: MouseEvent, x: Expr) => void;
  onCols: (cols: string[]) => void;
}) {
  return (
    <table className="runs">
      <thead>
        <tr>
          <th>status</th><th>name</th>{matched && <th>match</th>}
          {cols.map((c) => (
            <th key={c}>
              <span className="col-name">{c}</span>
              <button className="col-remove" aria-label={`remove column ${c}`} onClick={() => onCols(cols.filter((x) => x !== c))}>×</button>
            </th>
          ))}
          <th><AddColumn cols={cols} onCols={onCols} /></th>
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.run_id} tabIndex={0} className={left.has(r.run_id) ? "left" : undefined} onClick={() => navigate(`/trace/${r.trace_id}`)}
            onKeyDown={(e) => { if (e.key === "Enter" && e.target === e.currentTarget) navigate(`/trace/${r.trace_id}`); }}>
            <td>
              <button className="clickable" title={HINT} onClick={(e) => onCell(e, { t: "cmp", op: "eq", path: "status", value: r.status })}>
                <StatusBadge status={r.status} />
              </button>
              {left.has(r.run_id) && <span className="left-mark">no longer matching</span>}
            </td>
            <td className="name"><Cell path="name" value={r.name} onCell={onCell} /></td>
            {matched && <td className="excerpt">{r.match && <Excerpt m={r.match} />}</td>}
            {cols.map((c) => <td key={c}><Cell path={c} value={r.columns?.[c]} onCell={onCell} /></td>)}
            <td />
          </tr>
        ))}
        {empty && <tr><td colSpan={cols.length + (matched ? 4 : 3)} className="muted">{empty}</td></tr>}
      </tbody>
    </table>
  );
}

import type { ReactNode } from "react";
import type { AggResult, Cells } from "../api";
import { plural } from "../format";
import type { Expr } from "../query/ast";
import { type Side, SIDE_COLORS } from "./chart";
import { metricOf } from "./metric";

interface Column { head: string; has: (c: Cells) => boolean; show: (c: Cells) => ReactNode; extra?: Expr }

const ERRORS: Expr = { t: "cmp", op: "eq", path: "status", value: "error" };
const n = (k: number) => <span className="n">n={k.toLocaleString()}</span>;

/** How a stats-line metric reads as a column; an error-rate cell opens its errors. */
function column(m: string): Column {
  const def = metricOf(m);
  const text = (c: Cells) => def.fmt(def.value(c));
  const col = (show: Column["show"], extra?: Expr): Column => ({ head: def.head, has: (c) => def.value(c) != null, show, extra });
  if (m === "error_rate") return col(text, ERRORS);
  if (/^p\d\d\(latency\)$/.test(m)) return col((c) => <>{c.approximate && !c.estimated && def.value(c) != null ? "≈" : ""}{text(c)}{n(c.n_latency)}</>);
  if (m === "sum(cost)") {
    return col((c) => <>{text(c)}{c.no_price_runs > 0 && <span className="no-price"> {plural(c.no_price_runs, "run")} had no price</span>}</>);
  }
  if (m.startsWith("avg(feedback.")) return col((c) => <>{text(c)}{n(c.feedback_n)}</>);
  return col(text);
}

const Head = ({ side, children }: { side?: Side; children: ReactNode }) => (
  <th className={side ? `side-${side}` : undefined}>
    {side && <span className="swatch" style={{ background: SIDE_COLORS[side] }} />}{children}
  </th>
);

/**
 * One row per group, then the totals; a cell click opens its runs (no conditions: the whole query). Compared,
 * side b's columns follow, paired by position; a group b lacks is empty there. `openB`: b's cells are clickable.
 */
export function StatsTable({ res, by, metrics, top, openB, onOpen }: {
  res: AggResult;
  by: string[];
  metrics: string[];
  top: number;
  openB: boolean;
  onOpen: (wheres: string[], extra?: Expr, side?: Side) => void;
}) {
  const cols = metrics.map(column);
  const b = res.compare?.side_b;
  const cell = (c: Cells | null | undefined, col: Column, open?: () => void) => {
    if (!c) return "—";
    const v = <>{c.estimated && col.has(c) ? "≈" : ""}{col.show(c)}</>;
    return open ? <button className="cell-link" onClick={open}>{v}</button> : v;
  };
  const cellsB = (cs: Cells | null | undefined, wheres: string[] | null) => cols.map((col, j) => (
    <td key={`b${j}`} className="side-b">{cell(cs, col, openB && wheres ? () => onOpen(wheres, col.extra, "b") : undefined)}</td>
  ));
  return (
    <>
      <table className="runs stats-table" aria-label="stats">
        <thead>
          <tr>
            {by.length ? by.map((k) => <th key={k}>{k}</th>) : <th />}
            {cols.map((c, i) => <Head key={i} side={b ? "a" : undefined}>{c.head}</Head>)}
            {b && cols.map((c, i) => <Head key={`b${i}`} side="b">b: {c.head}</Head>)}
          </tr>
        </thead>
        <tbody>
          {by.length > 0 && res.groups.map((g, i) => {
            const where = g.where;
            return (
              <tr key={i}>
                {g.key.map((k, j) => <td key={j} className="name">{String(k)}</td>)}
                {cols.map((col, j) => (
                  <td key={j}>{cell(g.cells, col, where === null ? undefined : () => onOpen([where], col.extra))}</td>
                ))}
                {b && cellsB(b.groups[i]?.cells, b.groups[i]?.where ? [b.groups[i].where!] : null)}
              </tr>
            );
          })}
          <tr className="totals">
            <td colSpan={Math.max(by.length, 1)} className="name">all</td>
            {cols.map((col, j) => <td key={j}>{cell(res.totals, col, () => onOpen([], col.extra))}</td>)}
            {b && cellsB(b.totals, [])}
          </tr>
        </tbody>
      </table>
      {res.cut && <div className="stats-note">Top {top} groups shown; the rest are in (other).</div>}
      {res.overlap && <div className="stats-note">Groups overlap: a run with several tags is in each of them.</div>}
    </>
  );
}

import type { ReactNode } from "react";
import type { AggResult, Cells } from "../api";
import { fmtCost, fmtLatency, fmtPct, fmtTokens, plural } from "../format";
import type { Expr } from "../query/ast";

interface Column { head: string; show: (c: Cells) => ReactNode; extra?: Expr }

const ERRORS: Expr = { t: "cmp", op: "eq", path: "status", value: "error" };
const n = (k: number) => <span className="n">n={k.toLocaleString()}</span>;

/** How a stats-line metric reads as a column; an error-rate cell opens its errors. */
function column(m: string): Column {
  if (m === "count") return { head: "runs", show: (c) => c.runs.toLocaleString() };
  if (m === "error_rate") return { head: "error rate", show: (c) => fmtPct(c.error_rate), extra: ERRORS };
  const pct = /^(p50|p95|p99)\(latency\)$/.exec(m)?.[1] as "p50" | "p95" | "p99" | undefined;
  if (pct) return { head: `${pct} latency`, show: (c) => <>{c.approximate && c[pct] != null ? "≈" : ""}{fmtLatency(c[pct])}{n(c.n_latency)}</> };
  if (m === "sum(cost)") {
    return {
      head: "cost",
      show: (c) => <>{fmtCost(c.cost_pico)}{c.no_price_runs > 0 && <span className="no-price"> {plural(c.no_price_runs, "run")} had no price</span>}</>,
    };
  }
  const tok = /^sum\(((?:input|output|total)_tokens)\)$/.exec(m)?.[1] as "input_tokens" | "output_tokens" | "total_tokens" | undefined;
  if (tok) return { head: tok === "total_tokens" ? "tokens" : tok.replace("_", " "), show: (c) => fmtTokens(c[tok]) };
  const fb = /^avg\((feedback\..+)\)$/.exec(m)?.[1];
  if (fb) return { head: `${fb} avg`, show: (c) => <>{c.feedback_avg == null ? "—" : c.feedback_avg.toFixed(2)}{n(c.feedback_n)}</> };
  return { head: m, show: () => "—" };
}

/** One row per group, then the totals; a cell click opens its runs (`where` null: the whole query). */
export function StatsTable({ res, by, metrics, top, onOpen }: {
  res: AggResult;
  by: string[];
  metrics: string[];
  top: number;
  onOpen: (where: string | null, extra?: Expr) => void;
}) {
  const cols = metrics.map(column);
  const cell = (c: Cells | null, col: Column, open?: () => void) => {
    const v = c ? col.show(c) : "—";
    return open && c ? <button className="cell-link" onClick={open}>{v}</button> : v;
  };
  return (
    <>
      <table className="runs stats-table" aria-label="stats">
        <thead>
          <tr>
            {by.length ? by.map((k) => <th key={k}>{k}</th>) : <th />}
            {cols.map((c, i) => <th key={i}>{c.head}</th>)}
          </tr>
        </thead>
        <tbody>
          {by.length > 0 && res.groups.map((g, i) => {
            const where = g.where;
            return (
              <tr key={i}>
                {g.key.map((k, j) => <td key={j} className="name">{String(k)}</td>)}
                {cols.map((col, j) => (
                  <td key={j}>{cell(g.cells, col, where === null ? undefined : () => onOpen(where, col.extra))}</td>
                ))}
              </tr>
            );
          })}
          <tr className="totals">
            <td colSpan={Math.max(by.length, 1)} className="name">all</td>
            {cols.map((col, j) => <td key={j}>{cell(res.totals, col, () => onOpen(null, col.extra))}</td>)}
          </tr>
        </tbody>
      </table>
      {res.cut && <div className="stats-note">Top {top} groups shown; the rest are in (other).</div>}
      {res.overlap && <div className="stats-note">Groups overlap: a run with several tags is in each of them.</div>}
    </>
  );
}

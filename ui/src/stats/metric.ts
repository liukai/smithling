import type { Cells } from "../api";
import { fmtCost, fmtLatency, fmtPct, fmtTokens } from "../format";

/** A stats-line metric: its column head, the number it reads from a group's cells, and how that number prints. */
export interface MetricDef { head: string; value: (c: Cells) => number | null; fmt: (v: number | null) => string; known: boolean }

const PCT = /^(p50|p95|p99)\(latency\)$/;
const TOK = /^sum\(((?:input|output|total)_tokens)\)$/;
const FB = /^avg\((feedback\..+)\)$/;

export function metricOf(m: string): MetricDef {
  const def = (head: string, value: MetricDef["value"], fmt: MetricDef["fmt"]): MetricDef => ({ head, value, fmt, known: true });
  if (m === "count") return def("runs", (c) => c.runs, (v) => (v == null ? "—" : Math.round(v).toLocaleString()));
  if (m === "error_rate") return def("error rate", (c) => c.error_rate, fmtPct);
  const pct = PCT.exec(m)?.[1] as "p50" | "p95" | "p99" | undefined;
  if (pct) return def(`${pct} latency`, (c) => c[pct], fmtLatency);
  if (m === "sum(cost)") return def("cost", (c) => c.cost_pico, fmtCost);
  const tok = TOK.exec(m)?.[1] as "input_tokens" | "output_tokens" | "total_tokens" | undefined;
  if (tok) return def(tok === "total_tokens" ? "tokens" : tok.replace("_", " "), (c) => c[tok], fmtTokens);
  const fb = FB.exec(m)?.[1];
  if (fb) return def(`${fb} avg`, (c) => c.feedback_avg, (v) => (v == null ? "—" : v.toFixed(2)));
  return { head: m, value: () => null, fmt: () => "—", known: false };
}

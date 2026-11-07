import type { AggGroup, AggResult } from "../api";
import { fmtLatency, plural } from "../format";
import { Frame, M, type Side, SIDE_COLORS, SERIES_COLORS, keyText, markProps, plotH, plotW, sideLegend, wheresOf, yTicks } from "./chart";

const UPPER = /lt\(latency, ([0-9.e+-]+)\)/;

/** A bin's range as its `where` says it (the last fixed bin has no upper bound). */
function binLabel(lo: number, where: string): string {
  const hi = UPPER.exec(where)?.[1];
  return hi ? `${fmtLatency(lo)} – ${fmtLatency(Math.round(Number(hi) * 1000))}` : `${fmtLatency(lo)} and up`;
}

/**
 * Finished runs per log-latency bin for one group (or the whole query), with side b's bars beside side a's when
 * compared. A bar opens its runs. `openB`: side b's conditions select its runs (a compare filter, not a shift).
 */
export function Distribution({ res, by, focus, openB, sides: names, onOpen }: {
  res: AggResult;
  by: string[];
  focus: number;
  openB: boolean;
  sides: { a: string; b: string };
  onOpen: (wheres: string[], side: Side) => void;
}) {
  const dist = res.dist;
  const b = res.compare?.side_b;
  const at = by.length && focus < res.groups.length ? focus : 0;
  const g: AggGroup | undefined = by.length ? res.groups[at] : undefined;
  if (!dist || dist.bins_ms.length === 0) return <div className="muted chart-empty">No finished runs to chart.</div>;
  const sides: { side: Side; counts: number[]; group: string | null | undefined; color: string }[] = [
    { side: "a", counts: dist.per_group[at] ?? [], group: g?.where, color: b ? SIDE_COLORS.a : SERIES_COLORS[0] },
    ...(b?.dist ? [{ side: "b" as Side, counts: b.dist.per_group[at] ?? [], group: openB ? (b.groups[at]?.where ?? g?.where) : null, color: SIDE_COLORS.b }] : []),
  ];
  const n = dist.bins_ms.length;
  const band = plotW / n;
  const ticks = yTicks(Math.max(0, ...sides.flatMap((s) => s.counts)), true);
  const py = (v: number) => M.t + plotH - (v / (ticks[4] || 1)) * plotH;
  const every = Math.max(1, Math.ceil(n / 8));
  const xTicks = dist.bins_ms.flatMap((lo, i) => (i % every === 0 ? [{ x: M.l + i * band, label: fmtLatency(lo) }] : []));
  const width = Math.max(1, (band - 2) / sides.length - (sides.length > 1 ? 1 : 0));
  const title = `Distribution: latency${g ? ` of ${keyText(g)}` : ""}${b ? ", a vs b" : ""}`;
  return (
    <Frame key={res.query_id + String(res.partial)} title={title} legend={b ? sideLegend(names) : undefined} yLabel="runs" xLabel="latency" ticks={ticks} fmtY={(v) => v.toLocaleString()} xTicks={xTicks}>
      {(tip) => sides.map((s, k) => (
        <g key={s.side} className={res.partial ? "estimated" : undefined}>
          {s.counts.map((count, i) => {
            if (!count) return null;
            const where = dist.where_per_bin[i];
            const prefix = `${b ? `${s.side} · ` : ""}${g ? `${keyText(g)} · ` : ""}`;
            const label = `${prefix}${binLabel(dist.bins_ms[i], where)}: ${res.partial ? "≈" : ""}${plural(count, "run")}`;
            const w = wheresOf(s.group, where);
            const top = py(count);
            return (
              <rect key={i} x={M.l + i * band + 1 + k * (width + 2)} y={top} width={width} height={M.t + plotH - top}
                rx={2} fill={s.color} fillOpacity={res.partial ? 0.5 : 1} strokeDasharray={res.partial ? "3 2" : undefined}
                stroke={res.partial ? s.color : undefined}
                {...markProps({ label, tip: label, open: w ? () => onOpen(w, s.side) : undefined }, tip)} />
            );
          })}
        </g>
      ))}
    </Frame>
  );
}

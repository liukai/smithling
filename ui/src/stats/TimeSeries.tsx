import type { AggGroup, AggResult, Cells, Series } from "../api";
import { plural } from "../format";
import { Frame, M, type Side, SIDE_COLORS, colorOf, keyText, markProps, plotH, plotW, sideLegend, wheresOf, yTicks } from "./chart";
import { metricOf } from "./metric";

export { SIDE_COLORS } from "./chart";

interface Line { id: string; color: string; prefix: string; series: Series; at: number; group: string | null | undefined; side: Side }

const MIN = 60_000;
/** RFC 3339 with microseconds, trimmed to what every `Date.parse` takes. */
const parseTime = (t?: string) => Date.parse((t ?? "").replace(/(\.\d{3})\d+/, "$1"));
export function fmtOffset(ms: number): string {
  if (ms === 0) return "+0";
  const m = Math.round(ms / MIN);
  if (m % 1440 === 0) return `+${m / 1440}d`;
  if (m >= 60) return `+${Math.floor(m / 60)}h${m % 60 ? `${m % 60}m` : ""}`;
  return `+${m}m`;
}
function fmtClock(ms: number, long: boolean): string {
  const d = new Date(ms);
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return long ? `${d.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}` : time;
}

/**
 * One line per group (≤ top + `(other)`), or, compared, side a and side b of one group on an offset axis.
 * A null cell is a gap. A point opens its runs: the group's and the bucket's conditions.
 */
export function TimeSeries({ res, by, metric, focus, sides, onOpen }: {
  res: AggResult;
  /** What side a and side b are, for the legend. */
  sides: { a: string; b: string };
  by: string[];
  metric: string;
  /** The group shown when compared. */
  focus: number;
  onOpen: (wheres: string[], side: Side) => void;
}) {
  const def = metricOf(metric);
  const b = res.compare?.side_b;
  const groups: (AggGroup | undefined)[] = by.length ? res.groups : [undefined];
  const name = (g: AggGroup | undefined) => (g ? keyText(g) : "");
  let lines: Line[];
  if (b) {
    const at = focus < groups.length ? focus : 0;
    const g = groups[at];
    const pre = (s: Side) => `${s} · ${g ? `${name(g)} · ` : ""}`;
    lines = [
      { id: "a", color: SIDE_COLORS.a, prefix: pre("a"), series: res.series!, at, group: g?.where, side: "a" },
      ...(b.series ? [{ id: "b", color: SIDE_COLORS.b, prefix: pre("b"), series: b.series, at, group: b.groups[at]?.where ?? g?.where, side: "b" as Side }] : []),
    ];
  } else {
    lines = groups.map((g, i) => ({
      id: g ? name(g) : "all", color: colorOf(g, i), prefix: g ? `${name(g)} · ` : "", series: res.series!, at: i, group: g?.where, side: "a",
    }));
  }
  if (lines.length === 0 || lines.every((l) => l.series.buckets.length === 0)) return <div className="muted chart-empty">No runs to chart.</div>;
  const offset = Boolean(res.series?.offset);
  const xOf = (s: Series, i: number) => (offset ? s.buckets[i].offset_ms ?? 0 : parseTime(s.buckets[i].t));
  const xs = lines.flatMap((l) => l.series.buckets.map((_, i) => xOf(l.series, i)));
  const step = res.series?.step_ms ?? MIN;
  const x0 = Math.min(...xs), x1 = Math.max(...xs, x0 + step);
  const px = (x: number) => M.l + ((x - x0) / (x1 - x0)) * plotW;
  const values = lines.flatMap((l) => l.series.buckets.map((bk) => (bk.per_group[l.at] ? def.value(bk.per_group[l.at]!) : null)));
  const nums = values.filter((v): v is number => v != null);
  if (nums.length === 0) return <div className="muted chart-empty">No {def.head} to chart.</div>;
  const ticks = yTicks(Math.max(0, ...nums), metric === "count");
  const py = (v: number) => M.t + plotH - (v / (ticks[4] || 1)) * plotH;
  const long = !offset && x1 - x0 > 24 * 60 * MIN;
  const xLabel = (x: number) => (offset ? fmtOffset(x) : fmtClock(x, long));
  const longest = lines.reduce((a, l) => (l.series.buckets.length > a.series.buckets.length ? l : a), lines[0]).series;
  const every = Math.max(1, Math.ceil(longest.buckets.length / 7));
  const xTicks = longest.buckets.filter((_, i) => i % every === 0).map((_, j) => {
    const x = xOf(longest, j * every);
    return { x: px(x), label: xLabel(x) };
  });

  const title = `Time series: ${def.head}${b ? `${by.length ? ` of ${name(groups[focus] ?? groups[0])}` : ""}, a vs b` : by.length ? ` by ${by.join(", ")}` : ""}`;
  const legend = b ? sideLegend(sides) : by.length ? lines.map((l) => ({ label: l.id, color: l.color })) : [];
  return (
    <Frame key={res.query_id + String(res.partial)} title={title} yLabel={def.head} xLabel={offset ? "time since start" : "start time"} ticks={ticks}
      fmtY={(v) => def.fmt(v)} xTicks={xTicks} legend={legend}>
      {(tip) => lines.map((l, li) => {
        const pts = l.series.buckets.map((bk, i) => {
          const c: Cells | null = bk.per_group[l.at] ?? null;
          const v = c ? def.value(c) : null;
          return v == null ? null : { x: px(xOf(l.series, i)), y: py(v), v, c: c!, where: bk.where, i };
        });
        const d = pts.map((p, i) => (p ? `${pts[i - 1] ? "L" : "M"}${p.x.toFixed(1)} ${p.y.toFixed(1)}` : "")).join("");
        return (
          <g key={li}>
            <path className="line" data-series={l.id} d={d} fill="none" stroke={l.color} strokeWidth={2}
              strokeDasharray={res.partial ? "5 4" : undefined} />
            {pts.map((p) => {
              if (!p) return null;
              const label = `${l.prefix}${xLabel(xOf(l.series, p.i))}: ${def.head} ${res.partial ? "≈" : ""}${def.fmt(p.v)}`;
              const w = wheresOf(l.group, p.where);
              return (
                <circle key={p.i} cx={p.x} cy={p.y} r={4} fill={l.color} stroke="#fff" strokeWidth={1.5}
                  {...markProps({ label, tip: `${label} · ${plural(p.c.runs, "run")}`, open: w ? () => onOpen(w, l.side) : undefined }, tip)} />
              );
            })}
          </g>
        );
      })}
    </Frame>
  );
}

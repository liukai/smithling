import { type KeyboardEvent, type ReactNode, useState } from "react";
import type { AggGroup } from "../api";

/** Categorical slots in fixed order (validated light palette); groups past them and `(other)` are grey. */
export const SERIES_COLORS = ["#006ddd", "#d85522", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#7e65ae", "#e34948"];
export const REST_COLOR = "#8b9cad";
/** Side a and side b of a comparison, in tables and charts. */
export const SIDE_COLORS = { a: "#006ddd", b: "#d85522" } as const;
export type Side = "a" | "b";

export const colorOf = (g: AggGroup | undefined, i: number) =>
  g?.key.includes("(other)") || i >= SERIES_COLORS.length ? REST_COLOR : SERIES_COLORS[i];
export const sideLegend = (s: { a: string; b: string }) =>
  [{ label: `a: ${s.a}`, color: SIDE_COLORS.a }, { label: `b: ${s.b}`, color: SIDE_COLORS.b }];
export const keyText = (g: AggGroup) => g.key.map(String).join(", ");

export const W = 720;
export const H = 240;
export const M = { l: 64, r: 16, t: 12, b: 40 };
export const plotW = W - M.l - M.r;
export const plotH = H - M.t - M.b;

/** A round top for the y axis, and 4 ticks up to it. */
export function yTicks(max: number, whole = false): number[] {
  if (!(max > 0)) return whole ? [0, 1, 2, 3, 4] : [0, 0.25, 0.5, 0.75, 1];
  const raw = whole ? Math.max(1, max / 4) : max / 4;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((k) => k * mag).find((s) => s >= raw && (!whole || Number.isInteger(s)))!;
  return [0, 1, 2, 3, 4].map((i) => i * step);
}

/**
 * A group's click-through conditions with a bucket's or bin's: `undefined` group condition = the whole query
 * (not grouped), `null` = no filter can say it (not clickable).
 */
export const wheresOf = (group: string | null | undefined, at: string): string[] | null =>
  group === null ? null : group === undefined ? [at] : [group, at];

export interface Mark { label: string; tip: string; open?: () => void }

/** Focusable, clickable mark props: Enter or Space opens like a click; hover and focus show the tip. */
export function markProps(m: Mark, show: (tip: string | null) => void) {
  const key = (e: KeyboardEvent) => {
    if (m.open && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); m.open(); }
  };
  return {
    role: m.open ? "button" : "img",
    tabIndex: m.open ? 0 : undefined,
    "aria-label": m.label,
    className: m.open ? "mark open" : "mark",
    onClick: m.open,
    onKeyDown: key,
    onMouseEnter: () => show(m.tip),
    onMouseLeave: () => show(null),
    onFocus: () => show(m.tip),
    onBlur: () => show(null),
  };
}

/** The frame: title, y grid and labels, x labels, axis titles; `children` draw the marks. */
export function Frame({ title, yLabel, xLabel, ticks, fmtY, xTicks, legend, note, children }: {
  title: string;
  yLabel: string;
  xLabel: string;
  ticks: number[];
  fmtY: (v: number) => string;
  xTicks: { x: number; label: string }[];
  legend?: { label: string; color: string }[];
  note?: ReactNode;
  children: (tip: (t: string | null) => void) => ReactNode;
}) {
  const [tip, setTip] = useState<string | null>(null);
  const top = ticks[ticks.length - 1] || 1;
  const y = (v: number) => M.t + plotH - (v / top) * plotH;
  return (
    <figure className="chart">
      <svg viewBox={`0 0 ${W} ${H}`} role="group" aria-label={title}>
        <title>{title}</title>
        {ticks.map((t) => (
          <g key={t} className="grid">
            <line x1={M.l} x2={W - M.r} y1={y(t)} y2={y(t)} />
            <text x={M.l - 6} y={y(t)} dy="0.32em" textAnchor="end">{fmtY(t)}</text>
          </g>
        ))}
        {xTicks.map((t, i) => <text key={i} className="tick" x={t.x} y={H - M.b + 14} textAnchor="middle">{t.label}</text>)}
        <text className="axis-label" x={M.l + plotW / 2} y={H - 6} textAnchor="middle">{xLabel}</text>
        <text className="axis-label" transform={`translate(12 ${M.t + plotH / 2}) rotate(-90)`} textAnchor="middle">{yLabel}</text>
        {children(setTip)}
      </svg>
      <div className="chart-foot">
        {legend && legend.length > 0 && (
          <ul className="legend">
            {legend.map((l, i) => <li key={i}><span className="swatch" style={{ background: l.color }} />{l.label}</li>)}
          </ul>
        )}
        {note}
        {tip && <div className="chart-tip" role="tooltip">{tip}</div>}
      </div>
    </figure>
  );
}

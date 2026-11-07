import { type FormEvent, useState } from "react";
import { chipsOf, label } from "../query/chips";
import { parse } from "../query/parse";
import type { QueryState } from "../query/url";
import { SIDE_COLORS } from "./chart";

const SHIFTS = ["1h", "6h", "1d", "7d"];

export function cmpLabel(text: string): string {
  const r = parse(text);
  if (!r.ok) return text;
  return chipsOf(r.expr).map((c) => `${c.negated ? "not " : ""}${label(c.expr)}`).join(" and ");
}

/** What side b is, in words; null when not comparing. */
export const sideB = (q: QueryState) => (q.cmp ? cmpLabel(q.cmp) : q.shift ? `same window, ${q.shift} earlier` : null);

/** Side b: a filter in place of the query's `filter` (e.g. batch b), or the same window shifted back. */
export function ComparePicker({ q, onQuery }: { q: QueryState; onQuery: (patch: Partial<QueryState>) => void }) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const b = sideB(q);
  const on = b && `b: ${b}`;
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const t = text.trim();
    if (!t) return;
    const r = parse(t);
    if (!r.ok) return setError(r.error.message);
    setText("");
    onQuery({ cmp: t, shift: undefined });
  };
  return (
    <div className="where" role="group" aria-label="Compare">
      <span className="where-label">Compare</span>
      {on ? (
        <span className="chip side-b">
          <span className="swatch" style={{ background: SIDE_COLORS.b }} />
          <span className="chip-label" title={q.cmp}>{on}</span>
          <button aria-label="remove comparison" title="remove" onClick={() => onQuery({ cmp: undefined, shift: undefined })}>×</button>
        </span>
      ) : (
        <>
          <form className="add-column compare-filter" onSubmit={submit}>
            <input aria-label="compare filter" spellCheck={false} placeholder={'+ side b filter, e.g. has(tags, "batch=…")'} value={text}
              onChange={(e) => { setText(e.target.value); setError(null); }} />
            {error && <span role="alert" className="chip-error">{error}</span>}
          </form>
          <select aria-label="compare shift" value="" disabled={!q.time} title={q.time ? undefined : "pick a time window first"} onChange={(e) => e.target.value && onQuery({ shift: e.target.value, cmp: undefined })}>
            <option value="">or the same window…</option>
            {SHIFTS.map((s) => <option key={s} value={s}>{s} earlier</option>)}
          </select>
        </>
      )}
    </div>
  );
}

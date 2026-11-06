import { useEffect, useState } from "react";
import { type FieldScope, type FieldValues, getFieldValues } from "../api";
import type { Expr, Value } from "../query/ast";

export const DEBOUNCE_MS = 150;
export const fmtCount = (n: number) => n.toLocaleString("en-US");

/** A path's most common values with counts; a click adds the condition, Alt-click its negation. */
export function ValueList({ scope, path, list, onPick }: {
  scope: FieldScope;
  path: string;
  /** Tags-like: filter with `has`. */
  list: boolean;
  onPick: (e: Expr, negate: boolean) => void;
}) {
  const [prefix, setPrefix] = useState("");
  const [data, setData] = useState<FieldValues | null>(null);
  const [error, setError] = useState<string | null>(null);
  const { project, time } = scope;

  useEffect(() => {
    let on = true;
    const t = setTimeout(() => getFieldValues({ project, time }, path, prefix)
      .then((v) => { if (on) { setData(v); setError(null); } })
      .catch((e) => { if (on) setError(e instanceof Error ? e.message : String(e)); }), prefix ? DEBOUNCE_MS : 0);
    return () => { on = false; clearTimeout(t); };
  }, [project, time, path, prefix]);

  const expr = (v: Value): Expr => (list ? { t: "has", path, value: String(v) } : { t: "cmp", op: "eq", path, value: v });
  const values = (data?.values ?? []).filter((v) => v.value !== null && typeof v.value !== "object");
  return (
    <div className="value-list">
      <input aria-label="search values" placeholder="search values" value={prefix} onChange={(e) => setPrefix(e.target.value)} />
      {error && <div className="error-banner">{error}</div>}
      <ul>
        {values.map((v) => (
          <li key={JSON.stringify(v.value)} className="value">
            <button className="clickable" title="click to filter, Alt-click to exclude"
              onClick={(e) => onPick(expr(v.value as Value), e.altKey)}>{String(v.value)}</button>
            <span className="muted">{fmtCount(v.count)}</span>
          </li>
        ))}
      </ul>
      {data && values.length === 0 && <div className="muted">no values{prefix ? ` starting with ${prefix}` : ""}</div>}
      {data && data.other_count > 0 && <div className="muted">{fmtCount(data.other_count)} other</div>}
      {data?.approximate && <div className="muted">counts are approximate</div>}
    </div>
  );
}

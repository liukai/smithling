import { useEffect, useState } from "react";
import { type FieldInfo, type FieldScope, getFields } from "../api";
import type { Expr } from "../query/ast";
import { canBeColumn } from "../views/Table";
import { DEBOUNCE_MS, ValueList, fmtCount } from "./ValueList";

/** The paths runs in scope have, most common first, with their types; open one to see its values. */
export function FieldBrowser({ scope, onPick, onColumn }: {
  scope: FieldScope;
  onPick: (e: Expr, negate: boolean) => void;
  onColumn: (path: string) => void;
}) {
  const [prefix, setPrefix] = useState("");
  const [fields, setFields] = useState<FieldInfo[] | null>(null);
  const [complete, setComplete] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const { project, time } = scope;

  useEffect(() => {
    let on = true;
    const t = setTimeout(() => getFields({ project, time }, prefix)
      .then((r) => { if (on) { setFields(r.fields); setComplete(r.complete); setError(null); } })
      .catch((e) => { if (on) setError(e instanceof Error ? e.message : String(e)); }), prefix ? DEBOUNCE_MS : 0);
    return () => { on = false; clearTimeout(t); };
  }, [project, time, prefix]);

  return (
    <section className="fields" aria-label="fields">
      <input aria-label="search fields" placeholder="search fields" value={prefix} onChange={(e) => setPrefix(e.target.value)} />
      {error && <div className="error-banner">{error}</div>}
      <ul>
        {fields?.map((f) => (
          <li key={f.path} className="field">
            <div className="field-head">
              <button className="field-path" aria-expanded={open === f.path} onClick={() => setOpen(open === f.path ? null : f.path)}>{f.path}</button>
              <span className="field-types muted">{f.types.map((t) => `${t.type} ${fmtCount(t.count)}`).join(" · ")}</span>
              {canBeColumn(f.path) && (
                <button className="add-col" aria-label={`column ${f.path}`} title="add as a Table column" onClick={() => onColumn(f.path)}>+col</button>
              )}
            </div>
            {open === f.path && (
              <ValueList scope={scope} path={f.path} list={f.types.some((t) => t.type === "list")} onPick={onPick} />
            )}
          </li>
        ))}
      </ul>
      {fields?.length === 0 && !error && <div className="muted">no fields{prefix ? ` starting with ${prefix}` : ""}</div>}
      {!complete && <div className="muted">some files list only their first 10,000 paths</div>}
    </section>
  );
}

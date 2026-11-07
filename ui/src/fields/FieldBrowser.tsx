import { useEffect, useState } from "react";
import { type FeedbackKey, type FieldInfo, type FieldScope, getFeedbackKeys, getFields, getOrphanFeedbackCount } from "../api";
import { parse } from "../query/parse";
import type { Expr } from "../query/ast";
import { canBeColumn } from "../views/Table";
import { DEBOUNCE_MS, ValueList, fmtCount } from "./ValueList";

/** `exists(feedback.<key>)`, or null when the key can't be written as a filter path. */
function feedbackExists(key: string): Expr | null {
  const r = parse(`exists(feedback.${key})`);
  return r.ok && r.expr.t === "exists" && r.expr.path === `feedback.${key}` ? r.expr : null;
}

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

  const [feedback, setFeedback] = useState<{ keys: FeedbackKey[]; orphaned: number } | null>(null);

  useEffect(() => {
    let on = true;
    Promise.all([getFeedbackKeys({ project, time }), getOrphanFeedbackCount()])
      .then(([k, orphaned]) => { if (on) setFeedback({ keys: k.keys, orphaned }); })
      .catch(() => { if (on) setFeedback(null); });
    return () => { on = false; };
  }, [project, time]);

  useEffect(() => {
    let on = true;
    const t = setTimeout(() => getFields({ project, time }, prefix)
      // feedback.<key> paths are listed in their own section below.
      .then((r) => { if (on) { setFields(r.fields.filter((f) => !f.path.startsWith("feedback."))); setComplete(r.complete); setError(null); } })
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
      {feedback && (feedback.keys.length > 0 || feedback.orphaned > 0) && (
        <section className="feedback-keys" aria-label="feedback">
          <div className="muted">feedback.{feedback.orphaned > 0 && ` (${fmtCount(feedback.orphaned)} orphaned)`}</div>
          <ul>
            {feedback.keys.map((k) => {
              const pick = k.avg === null ? null : feedbackExists(k.key);
              return (
                <li key={k.key} className="field">
                  <div className="field-head">
                    {pick
                      ? <button className="field-path" title="runs with a score for this key" onClick={() => onPick(pick, false)}>feedback.{k.key}</button>
                      : <span className="field-path" title={k.avg === null ? "no scores, only values" : "not a filterable key"}>feedback.{k.key}</span>}
                    <span className="field-types muted">{k.avg === null ? "value" : "number"} {fmtCount(k.count)}{k.avg !== null && ` · avg ${k.avg.toFixed(2)}`}</span>
                  </div>
                </li>
              );
            })}
          </ul>
        </section>
      )}
      {fields?.length === 0 && !error && <div className="muted">no fields{prefix ? ` starting with ${prefix}` : ""}</div>}
      {!complete && <div className="muted">some files list only their first 10,000 paths</div>}
    </section>
  );
}

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ApiError } from "../api";
import type { Expr } from "../query/ast";
import { type Rows, chip, chipsOf, rowsOf, textOf } from "../query/chips";
import { type Mode, Panel } from "../query/Panel";
import { parse, utf16Offset } from "../query/parse";
import { print } from "../query/print";
import type { SlotError } from "../query/TextEditor";
import { API_SLOT, type QueryState, SLOTS, type Slot, fromUrl, toUrl } from "../query/url";
import { navigate } from "../route";
import { Samples } from "./Samples";

const slotKey = (q: QueryState) => JSON.stringify(SLOTS.map((s) => q[s] ?? ""));
const textsOf = (q: QueryState) => ({ filter: q.filter ?? "", trace: q.trace ?? "", tree: q.tree ?? "" });

function parseErrors(q: QueryState): Partial<Record<Slot, SlotError>> {
  const { errors } = rowsOf(q);
  return Object.fromEntries(Object.entries(errors).map(([s, e]) => [s, { ...e, text: q[s as Slot]! }]));
}

/** The runs view: the query lives in the URL; chips (incl. disabled ones) live here while the URL matches them. */
export function RunsPage({ search }: { search: string }) {
  const { q, m1 } = useMemo(() => fromUrl(search), [search]);
  const [rows, setRows] = useState<Rows>(() => rowsOf(q).rows);
  const [errors, setErrors] = useState(() => parseErrors(q));
  const [mode, setMode] = useState<Mode>(() => (Object.keys(errors).length ? "text" : "panel"));
  const [drafts, setDrafts] = useState(() => textsOf(q));
  const synced = useRef(slotKey(q));

  useEffect(() => { if (m1) navigate(toUrl(q), true); }, [m1, q]);

  useEffect(() => {
    if (slotKey(q) === synced.current) return;
    synced.current = slotKey(q);
    const errs = parseErrors(q);
    setRows(rowsOf(q).rows);
    setErrors(errs);
    setDrafts(textsOf(q));
    if (Object.keys(errs).length) setMode("text");
  }, [q]);

  /** `keepUnparsed`: panel edits keep slot text the panel can't show (it doesn't parse). */
  const apply = (patch: Partial<QueryState>, next?: Rows, keepUnparsed = false): QueryState => {
    const s = { ...q, ...patch };
    if (next) {
      for (const slot of SLOTS) {
        const keep = keepUnparsed && next[slot].length === 0 && q[slot] && !parse(q[slot]).ok;
        if (!keep) s[slot] = textOf(next[slot]);
      }
      setRows(next);
      const before = textsOf(q), after = textsOf(s);
      setDrafts((d) => Object.fromEntries(SLOTS.map((k) => [k, d[k] === before[k] ? after[k] : d[k]])) as Record<Slot, string>);
    }
    setErrors({});
    synced.current = slotKey(s);
    const url = toUrl(s);
    if (url !== location.pathname + location.search) navigate(url);
    return s;
  };

  const runText = (): boolean => {
    const current = textsOf(q);
    const parses = (t: string) => !t.trim() || parse(t).ok;
    if (SLOTS.every((s) => drafts[s] === current[s] && parses(drafts[s]))) return true;
    const next = { filter: [], trace: [], tree: [] } as Rows;
    const errs: Partial<Record<Slot, SlotError>> = {};
    for (const s of SLOTS) {
      if (!drafts[s].trim()) continue;
      const r = parse(drafts[s]);
      if (r.ok) next[s] = chipsOf(r.expr);
      else errs[s] = { ...r.error, text: drafts[s] };
    }
    if (Object.keys(errs).length) {
      setErrors(errs);
      return false;
    }
    setDrafts(textsOf(apply({}, next)));
    return true;
  };

  const onMode = (m: Mode) => {
    if (m === "text") {
      setDrafts(textsOf(q));
      setMode("text");
    } else if (runText()) {
      setMode("panel");
    }
  };

  const onFilter = (expr: Expr, negate: boolean) => {
    const dup = rows.filter.some((c) => c.negated === negate && print(c.expr) === print(expr));
    if (!dup) apply({}, { ...rows, filter: [...rows.filter, chip(expr, negate)] }, true);
  };

  const onFilterError = useCallback((e: ApiError) => {
    const slot = SLOTS.find((s) => API_SLOT[s] === e.slot);
    if (!slot) return;
    const text = q[slot] ?? "";
    const at = (b?: number) => utf16Offset(text, b ?? 0);
    setErrors({ [slot]: { start: at(e.start), end: at(e.end ?? e.start), message: e.message, text } });
    setDrafts(textsOf(q));
    setMode("text");
  }, [q]);

  return (
    <div>
      <Panel q={q} rows={rows} mode={mode} drafts={drafts} errors={errors}
        onQuery={(p) => apply(p)} onRows={(r) => apply({}, r, true)} onMode={onMode} onDrafts={setDrafts} onRunText={runText} />
      <Samples q={q} onFilter={onFilter} onFilterError={onFilterError} />
    </div>
  );
}

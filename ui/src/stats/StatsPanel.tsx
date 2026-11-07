import { type FormEvent, useEffect, useRef, useState } from "react";
import { ApiError, type AggResult, streamAggregate } from "../api";
import type { Expr } from "../query/ast";
import type { ParseError } from "../query/parse";
import { type Stats, parseStats, printStats } from "../query/stats";
import { BUCKETS, type QueryState, aggBody } from "../query/url";
import { warningText } from "../warnings";
import { type Side, keyText } from "./chart";
import { ComparePicker, cmpLabel, sideB } from "./Compare";
import { Distribution } from "./Distribution";
import { metricOf } from "./metric";
import { StatsTable } from "./StatsTable";
import { TimeSeries } from "./TimeSeries";

const METRICS: [string, string][] = [
  ["count", "runs"], ["error_rate", "error rate"], ["p50(latency)", "p50 latency"], ["p95(latency)", "p95 latency"],
  ["p99(latency)", "p99 latency"], ["sum(cost)", "cost"], ["sum(total_tokens)", "tokens"],
];
const GROUP_SUGGEST = ["name", "run_type", "model", "status", "tag", "metadata."];
const TOPS = [5, 10, 20, 50];
const MAX_KEYS = 2;

/** A one-off input checked by the stats-line parser; `read` turns its text into the change to make. */
function AddInput({ label, placeholder, list, read, onAdd }: {
  label: string;
  placeholder: string;
  list?: string[];
  read: (text: string) => ReturnType<typeof parseStats>;
  onAdd: (s: Stats) => void;
}) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const listId = `stats-${label.replace(/\W+/g, "-")}`;
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!text.trim()) return;
    const r = read(text.trim());
    if (!r.ok) return setError(r.error.message);
    onAdd(r.stats);
    setText("");
  };
  return (
    <form className="add-column" onSubmit={submit}>
      <input aria-label={label} list={list && listId} placeholder={placeholder} value={text}
        onChange={(e) => { setText(e.target.value); setError(null); }} />
      {list && <datalist id={listId}>{list.map((f) => <option key={f} value={f} />)}</datalist>}
      {error && <span role="alert" className="chip-error">{error}</span>}
    </form>
  );
}

function StatsLine({ line, onApply }: { line: string; onApply: (s: Stats) => void }) {
  const [draft, setDraft] = useState(line);
  const [error, setError] = useState<(ParseError & { text: string }) | null>(null);
  useEffect(() => { setDraft(line); setError(null); }, [line]);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const r = parseStats(draft);
    if (!r.ok) return setError({ ...r.error, text: draft });
    setDraft(printStats(r.stats));
    onApply(r.stats);
  };
  const err = error?.text === draft ? error : null;
  return (
    <form className="stats-line slot" onSubmit={submit}>
      <input aria-label="stats line" spellCheck={false} className={err ? "bad" : undefined} value={draft}
        onChange={(e) => setDraft(e.target.value)} />
      {err && (
        <>
          <div className="err-mirror" aria-hidden>
            {err.text.slice(0, err.start)}<mark>{err.text.slice(err.start, err.end) || " "}</mark>{err.text.slice(err.end)}
          </div>
          <div className="err-msg">{err.message}</div>
        </>
      )}
    </form>
  );
}

const pctText = (p?: number) => (p == null ? "?" : p < 1 ? p.toFixed(1) : String(Math.round(p)));

/**
 * The Stats, Time series and Distribution tabs: group by (≤ 2 keys), metrics and top N as pickers or as the
 * stats line, a comparison, the chart and the table. An estimate shows first, marked, until the exact result.
 */
export function StatsPanel({ q, onQuery, onOpen, onFilterError }: {
  q: QueryState;
  onQuery: (patch: Partial<QueryState>) => void;
  /** A cell's, point's or bar's runs: its conditions (none: the whole query) on side a or b. */
  onOpen: (wheres: string[], extra?: Expr, side?: Side) => void;
  /** A filter slot the server rejected. */
  onFilterError: (e: ApiError) => void;
}) {
  const line = printStats({ groupBy: q.by, metrics: q.m, top: q.top });
  const valid = parseStats(line);
  const [text, setText] = useState(!valid.ok);
  /** The result with the spec that produced it, so headers and rows always agree. */
  const [shown, setShown] = useState<{ res: AggResult; by: string[]; m: string[]; top: number; view: string; openB: boolean; key: string } | null>(null);
  const [chartM, setChartM] = useState<string | null>(null);
  /** The chart's group when one is shown at a time, by its key text. */
  const [focus, setFocus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const onFilterErrorRef = useRef(onFilterError);
  onFilterErrorRef.current = onFilterError;
  const bodyKey = valid.ok ? JSON.stringify(aggBody(q)) : null;

  useEffect(() => {
    if (!bodyKey) return setLoading(false);
    setLoading(true);
    const body = JSON.parse(bodyKey);
    const show = (res: AggResult) => {
      setShown({ res, by: body.group_by, m: body.metrics, top: body.top, view: body.view, openB: Boolean(body.compare?.filter), key: bodyKey });
      setError(null);
    };
    return streamAggregate(body, {
      onEstimate: show,
      onResult: (res) => { show(res); setLoading(false); },
      onError: (e) => {
        setShown(null);
        setLoading(false);
        if (e.slot === "compare.filter") setError(`Compare: ${e.message}`);
        else if (e.slot) {
          setError("The query has an error; open Text to see it.");
          onFilterErrorRef.current(e);
        } else setError(e.message);
      },
    });
  }, [bodyKey]);

  const apply = (s: Stats) => onQuery({ by: s.groupBy, m: s.metrics, top: s.top });
  const toggle = (m: string) => {
    const on = q.m.includes(m);
    if (on && q.m.length === 1) return;
    onQuery({ m: on ? q.m.filter((x) => x !== m) : [...q.m, m] });
  };
  const chartable = q.m.filter((m) => metricOf(m).known);
  const metric = chartM && chartable.includes(chartM) ? chartM : chartable.find((m) => m !== "count") ?? chartable[0] ?? "count";
  const kind = q.view === "series" || q.view === "dist" ? q.view : null;
  const res = shown?.res;
  const compared = Boolean(res?.compare);
  const at = Math.max(0, res?.groups.findIndex((g) => keyText(g) === focus) ?? 0);
  const current = shown?.key === bodyKey;
  const sides = { a: q.filter ? cmpLabel(q.filter) : "this query", b: sideB(q) ?? "" };
  const pickGroup = res && shown.by.length > 0 && (kind === "dist" || compared);
  const open = (wheres: string[], side: Side) => onOpen(wheres, undefined, side);
  const picks = [...METRICS, ...q.m.filter((m) => !METRICS.some(([v]) => v === m)).map((m): [string, string] => [m, m])];

  return (
    <div className="stats">
      <div className="query-panel stats-panel" role="group" aria-label="Stats">
        {text ? (
          <div className="where">
            <span className="where-label">Stats</span>
            <StatsLine line={line} onApply={apply} />
          </div>
        ) : (
          <>
            <div className="where" role="group" aria-label="Group by">
              <span className="where-label">Group by</span>
              {q.by.map((k) => (
                <span key={k} className="chip">
                  <span className="chip-label">{k}</span>
                  <button aria-label={`remove group ${k}`} title="remove" onClick={() => onQuery({ by: q.by.filter((x) => x !== k) })}>×</button>
                </span>
              ))}
              {q.by.length < MAX_KEYS && (
                <AddInput label="add group" placeholder="+ group" list={GROUP_SUGGEST} read={(t) => parseStats(`by ${t} | count`)}
                  onAdd={(s) => { if (!q.by.includes(s.groupBy[0])) onQuery({ by: [...q.by, s.groupBy[0]] }); }} />
              )}
            </div>
            <div className="where" role="group" aria-label="Metrics">
              <span className="where-label">Metrics</span>
              {picks.map(([m, l]) => {
                const on = q.m.includes(m);
                return (
                  <button key={m} className={`toggle-chip${on ? " on" : ""}`} aria-pressed={on} onClick={() => toggle(m)}>{l}</button>
                );
              })}
              <AddInput label="add metric" placeholder="+ metric, e.g. avg(feedback.correctness)" read={parseStats}
                onAdd={(s) => onQuery({ m: [...q.m, ...s.metrics.filter((m) => !q.m.includes(m))] })} />
              <label>
                Top{" "}
                <select aria-label="top" value={q.top} onChange={(e) => onQuery({ top: Number(e.target.value) })}>
                  {[...new Set([...TOPS, q.top])].sort((a, b) => a - b).map((t) => <option key={t} value={t}>{t}</option>)}
                </select>
              </label>
            </div>
          </>
        )}
        <ComparePicker q={q} onQuery={onQuery} />
        <button className="mode" onClick={() => setText(!text)}>{text ? "Pickers" : "Text"}</button>
      </div>
      {!valid.ok && !text && <div className="error-banner">{valid.error.message}</div>}
      {error && <div className="error-banner">{error}</div>}
      {shown?.res.warnings?.map((w, i) => <div key={i} className="warning">{warningText(w)}</div>)}
      {shown?.res.skipped_files?.length ? <div className="warning">{warningText({ kind: "skipped_files", ids: shown.res.skipped_files })}</div> : null}
      {loading && !(current && res?.partial) && <div className="muted">{shown ? "updating…" : "…"}</div>}
      {current && res?.partial && <div className="estimate-mark" role="status">≈ estimate · {pctText(res.sampled_pct)}% sampled; exact figures follow</div>}
      {kind && (
        <div className="chart-bar">
          {kind === "series" && (
            <>
              <label>Metric <select aria-label="chart metric" value={metric} onChange={(e) => setChartM(e.target.value)}>
                {chartable.map((m) => <option key={m} value={m}>{metricOf(m).head}</option>)}
              </select></label>
              <label>Bucket <select aria-label="bucket" value={q.bucket ?? "auto"} onChange={(e) => onQuery({ bucket: e.target.value === "auto" ? undefined : e.target.value })}>
                {["auto", ...BUCKETS].map((b) => <option key={b} value={b}>{b}</option>)}
              </select></label>
            </>
          )}
          {pickGroup && (
            <label>Group <select aria-label="chart group" value={at} onChange={(e) => setFocus(keyText(res.groups[Number(e.target.value)]))}>
              {res.groups.map((g, i) => <option key={i} value={i}>{keyText(g)}</option>)}
            </select></label>
          )}
        </div>
      )}
      {shown && !error && kind === "series" && shown.view === "series" && res?.series && (
        <TimeSeries res={res} by={shown.by} metric={metric} focus={at} sides={sides} onOpen={open} />
      )}
      {shown && !error && kind === "dist" && shown.view === "dist" && (
        <Distribution res={res!} by={shown.by} focus={at} openB={shown.openB} sides={sides} onOpen={open} />
      )}
      {shown && !error && (
        <StatsTable res={shown.res} by={shown.by} metrics={shown.m} top={shown.top} openB={shown.openB} onOpen={onOpen} />
      )}
    </div>
  );
}

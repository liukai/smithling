import { useEffect, useState } from "react";
import { type ProjectInfo, getProjects } from "../api";
import { ChipEditor, ChipView } from "./Chip";
import { type Rows, chip } from "./chips";
import { ROW_LABEL, type SlotError, TextEditor } from "./TextEditor";
import { DEFAULT_LIMIT, type Order, type QueryState, SLOTS, type Slot } from "./url";

const TIMES: [string, string][] = [
  ["", "all time"], ["15m", "last 15m"], ["1h", "last 1h"], ["12h", "last 12h"], ["24h", "last 24h"], ["7d", "last 7d"], ["30d", "last 30d"],
];
const ORDERS: [string, string][] = [
  ["start_time.desc", "start time, newest"], ["start_time.asc", "start time, oldest"],
  ["latency.desc", "latency, slowest"], ["latency.asc", "latency, fastest"],
];
const LIMITS = [10, 25, DEFAULT_LIMIT, 100, 200];

export type Mode = "panel" | "text";

export function Panel({ q, rows, mode, drafts, errors, onQuery, onRows, onMode, onDrafts, onRunText }: {
  q: QueryState;
  rows: Rows;
  mode: Mode;
  drafts: Record<Slot, string>;
  errors: Partial<Record<Slot, SlotError>>;
  onQuery: (patch: Partial<QueryState>) => void;
  onRows: (rows: Rows) => void;
  onMode: (m: Mode) => void;
  onDrafts: (d: Record<Slot, string>) => void;
  onRunText: () => void;
}) {
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [editing, setEditing] = useState<{ slot: Slot; id?: number } | null>(null);
  useEffect(() => { getProjects().then(setProjects).catch(() => {}); }, []);

  const setRow = (slot: Slot, chips: Rows[Slot]) => onRows({ ...rows, [slot]: chips });
  const order = `${q.order.by}.${q.order.dir}`;
  const rootOnly = (
    <button className={`toggle-chip${q.root ? " on" : ""}`} aria-pressed={q.root} aria-label="root only"
      onClick={() => onQuery({ root: !q.root })}>root only{q.root ? " ×" : ""}</button>
  );

  return (
    <div className="query-panel">
      <div className="toolbar">
        <label>
          Project{" "}
          <select aria-label="project" value={q.project ?? ""} onChange={(e) => onQuery({ project: e.target.value || undefined })}>
            <option value="">all</option>
            {projects.map((p) => <option key={p.name} value={p.name}>{p.name} ({p.runs})</option>)}
            {q.project && !projects.some((p) => p.name === q.project) && <option value={q.project}>{q.project}</option>}
          </select>
        </label>
        <label>
          Time{" "}
          <select aria-label="time" value={q.time ?? ""} onChange={(e) => onQuery({ time: e.target.value || undefined })}>
            {TIMES.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
            {q.time && !TIMES.some(([v]) => v === q.time) && <option value={q.time}>last {q.time}</option>}
          </select>
        </label>
        <button className="mode" onClick={() => onMode(mode === "panel" ? "text" : "panel")}>{mode === "panel" ? "Text" : "Panel"}</button>
      </div>

      {mode === "text" ? (
        <TextEditor drafts={drafts} errors={errors} onDrafts={onDrafts} onRun={onRunText} extra={{ filter: rootOnly }} />
      ) : (
        SLOTS.map((slot) => (
          <div key={slot} className="where" role="group" aria-label={ROW_LABEL[slot]}>
            <span className="where-label">{ROW_LABEL[slot]}</span>
            {rows[slot].map((c, i) => (editing?.slot === slot && editing.id === c.id ? (
              <ChipEditor key={c.id} initial={c.expr} onCancel={() => setEditing(null)} onSave={(expr) => {
                setEditing(null);
                setRow(slot, rows[slot].map((x, j) => (j === i ? { ...x, expr } : x)));
              }} />
            ) : (
              <ChipView key={c.id} c={c}
                onChange={(n) => setRow(slot, rows[slot].map((x, j) => (j === i ? n : x)))}
                onRemove={() => setRow(slot, rows[slot].filter((_, j) => j !== i))}
                onEdit={() => setEditing({ slot, id: c.id })} />
            )))}
            {slot === "filter" && rootOnly}
            {editing?.slot === slot && editing.id === undefined ? (
              <ChipEditor onCancel={() => setEditing(null)} onSave={(expr) => {
                setEditing(null);
                setRow(slot, [...rows[slot], chip(expr)]);
              }} />
            ) : (
              <button className="add" aria-label="add condition" onClick={() => setEditing({ slot })}>+</button>
            )}
          </div>
        ))
      )}

      <div className="toolbar">
        <label>
          Order{" "}
          <select aria-label="order" value={order} onChange={(e) => {
            const [by, dir] = e.target.value.split(".") as [Order["by"], Order["dir"]];
            onQuery({ order: { by, dir } });
          }}>
            {ORDERS.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
          </select>
        </label>
        <label>
          Limit{" "}
          <select aria-label="limit" value={q.limit} onChange={(e) => onQuery({ limit: Number(e.target.value) })}>
            {[...new Set([...LIMITS, q.limit])].sort((a, b) => a - b).map((n) => <option key={n} value={n}>{n}</option>)}
          </select>
        </label>
      </div>
    </div>
  );
}

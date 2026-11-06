import { useEffect, useState } from "react";
import { type RunHeader, type RunView, getRun } from "./api";
import { fmtBytes, fmtLatency, fmtTime, fmtTokens } from "./format";
import { JsonView } from "./JsonView";
import { StatusBadge } from "./StatusBadge";

/** Full detail of one step; payloads are fetched only when the step is selected (or updated). */
export function StepPanel({ header, onClose }: { header: RunHeader; onClose: () => void }) {
  const [run, setRun] = useState<RunView | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    getRun(header.run_id).then((r) => live && setRun(r)).catch((e) => live && setError(String(e)));
    return () => { live = false; };
  }, [header.run_id, header.last_seq]);

  const h = header;
  const dropped = Object.entries(h.dropped);
  return (
    <aside className="panel">
      <div className="panel-head">
        <strong>{h.name}</strong> <span className="muted">({h.run_type_raw ?? h.run_type})</span> <StatusBadge status={h.status} />
        <button className="close" onClick={onClose} aria-label="close">×</button>
      </div>
      {h.error && <section><h4>error</h4><pre className="err">{h.error}</pre></section>}
      <section className="facts">
        <div><span>start</span>{fmtTime(h.start_time)}</div>
        <div><span>end</span>{fmtTime(h.end_time)}</div>
        <div><span>latency</span>{fmtLatency(h.latency_ms)}</div>
        <div><span>tokens</span>{fmtTokens(h.total_tokens)}{h.input_tokens != null && ` (${h.input_tokens} in / ${h.output_tokens ?? 0} out)`}</div>
        {h.model && <div><span>model</span>{h.model}</div>}
        <div><span>project</span>{h.project}</div>
        {h.tags.length > 0 && <div><span>tags</span>{h.tags.join(", ")}</div>}
        {h.warnings.length > 0 && <div><span>warnings</span>{h.warnings.join(", ")}</div>}
      </section>
      {error && <div className="error-banner">{error}</div>}
      <section><h4>inputs <span className="muted">{fmtBytes(h.payload_sizes.inputs)}</span></h4>
        {run ? <JsonView value={run.inputs} bytes={h.payload_sizes.inputs} /> : <span className="muted">loading…</span>}</section>
      <section><h4>outputs <span className="muted">{fmtBytes(h.payload_sizes.outputs)}</span></h4>
        {run ? <JsonView value={run.outputs} bytes={h.payload_sizes.outputs} /> : <span className="muted">loading…</span>}</section>
      {h.payload_sizes.events > 0 && (
        <section><h4>events</h4>{run ? <JsonView value={run.events} bytes={h.payload_sizes.events} /> : null}</section>
      )}
      {Object.keys(h.metadata).length > 0 && <section><h4>metadata</h4><JsonView value={h.metadata} /></section>}
      {dropped.length > 0 && (
        <section className="muted">not stored in this version: {dropped.map(([k, v]) => `${k} (${fmtBytes(v)})`).join(", ")}</section>
      )}
    </aside>
  );
}

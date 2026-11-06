import { useState } from "react";

const RAW_FIRST_BYTES = 2 * 1024 * 1024;

/** Collapsible JSON; payloads over 2 MB start as collapsed raw text. */
export function JsonView({ value, bytes }: { value: unknown; bytes?: number }) {
  const [raw, setRaw] = useState((bytes ?? 0) > RAW_FIRST_BYTES);
  if (value === undefined) return <span className="muted">—</span>;
  if (raw) {
    return (
      <details className="raw">
        <summary>large payload ({Math.round((bytes ?? 0) / 1024)} KB) — show raw <button onClick={() => setRaw(false)}>as tree</button></summary>
        <pre>{JSON.stringify(value, null, 2)}</pre>
      </details>
    );
  }
  return <div className="json"><Node value={value} depth={0} /></div>;
}

function Node({ value, depth, name }: { value: unknown; depth: number; name?: string }) {
  const [open, setOpen] = useState(depth < 2);
  const label = name !== undefined ? <span className="key">{name}: </span> : null;
  if (value === null || typeof value !== "object") {
    return <div className="leaf">{label}<Scalar v={value} /></div>;
  }
  const entries = Array.isArray(value) ? value.map((v, i) => [String(i), v] as const) : Object.entries(value as object);
  const [o, c] = Array.isArray(value) ? ["[", "]"] : ["{", "}"];
  return (
    <div>
      <span className="toggle" onClick={() => setOpen(!open)}>{open ? "▾" : "▸"}</span> {label}
      {open ? (
        <>
          {o}
          <div className="children">{entries.map(([k, v]) => <Node key={k} name={k} value={v} depth={depth + 1} />)}</div>
          {c}
        </>
      ) : (
        <span className="muted" onClick={() => setOpen(true)}>{o} {entries.length} {Array.isArray(value) ? "items" : "keys"} {c}</span>
      )}
    </div>
  );
}

function Scalar({ v }: { v: unknown }) {
  if (typeof v === "string") return <span className="str">"{v}"</span>;
  if (typeof v === "number") return <span className="num">{v}</span>;
  if (typeof v === "boolean") return <span className="bool">{String(v)}</span>;
  return <span className="null">null</span>;
}

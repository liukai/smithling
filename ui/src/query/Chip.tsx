import { type FormEvent, useState } from "react";
import { type CmpOp, type Expr, FIELDS } from "./ast";
import { type Chip, isSimple, label, toValue, valueText } from "./chips";
import { parse } from "./parse";
import { print } from "./print";

export function ChipView({ c, onChange, onRemove, onEdit }: {
  c: Chip;
  onChange: (c: Chip) => void;
  onRemove: () => void;
  onEdit: () => void;
}) {
  const l = label(c.expr);
  const simple = isSimple(c.expr);
  const text = `${c.negated ? "not " : ""}${l}`;
  return (
    <span className={`chip${c.negated ? " neg" : ""}${c.disabled ? " off" : ""}${simple ? "" : " expr"}`}>
      <button aria-label={`negate ${text}`} aria-pressed={c.negated} title="negate" onClick={() => onChange({ ...c, negated: !c.negated })}>¬</button>
      {simple
        ? <button className="chip-label" aria-label={`edit ${text}`} onClick={onEdit}>{text}</button>
        : <span className="chip-label" title={print(c.expr)}>{text}</span>}
      <button aria-label={`${c.disabled ? "enable" : "disable"} ${text}`} title={c.disabled ? "enable" : "disable"}
        onClick={() => onChange({ ...c, disabled: !c.disabled })}>{c.disabled ? "○" : "●"}</button>
      <button aria-label={`remove ${text}`} title="remove" onClick={onRemove}>×</button>
    </span>
  );
}

type EditOp = CmpOp | "has" | "in" | "exists" | "missing";
const OPS: [EditOp, string][] = [
  ["eq", "="], ["neq", "≠"], ["gt", ">"], ["gte", "≥"], ["lt", "<"], ["lte", "≤"],
  ["has", "has"], ["in", "in"], ["exists", "exists"], ["missing", "missing"],
];
const SUGGEST = [...Object.keys(FIELDS), "metadata.", "inputs.", "outputs."];

function build(path: string, op: EditOp, text: string): Expr {
  switch (op) {
    case "exists": return { t: "cmp", op: "neq", path, value: null };
    case "missing": return { t: "cmp", op: "eq", path, value: null };
    case "has": return { t: "has", path, value: text };
    case "in": return { t: "in", path, values: text.split(",").filter((v) => v.trim()).map((v) => toValue(path, v.trim())) };
    default: return { t: "cmp", op, path, value: toValue(path, text) };
  }
}

function fromExpr(e?: Expr): [string, EditOp, string] {
  if (e?.t === "cmp") {
    if (e.value === null) return [e.path, e.op === "eq" ? "missing" : "exists", ""];
    return [e.path, e.op, valueText(e.path, e.value)];
  }
  if (e?.t === "has") return [e.path, "has", e.value];
  if (e?.t === "in") return [e.path, "in", e.values.map((v) => valueText(e.path, v)).join(", ")];
  return ["", "eq", ""];
}

/** Field, operator and value, checked by the same parser as the text form. */
export function ChipEditor({ initial, onSave, onCancel }: { initial?: Expr; onSave: (e: Expr) => void; onCancel: () => void }) {
  const [init] = useState(() => fromExpr(initial));
  const [field, setField] = useState(init[0]);
  const [op, setOp] = useState<EditOp>(init[1]);
  const [value, setValue] = useState(init[2]);
  const [error, setError] = useState<string | null>(null);

  const save = (ev: FormEvent) => {
    ev.preventDefault();
    const r = parse(print(build(field.trim(), op, value)));
    if (r.ok) onSave(r.expr);
    else setError(r.error.message);
  };

  return (
    <form className="chip-editor" onSubmit={save}>
      <input aria-label="field" list="tracedb-fields" placeholder="field" value={field} autoFocus
        onChange={(e) => { setField(e.target.value); setError(null); }} />
      <datalist id="tracedb-fields">{SUGGEST.map((f) => <option key={f} value={f} />)}</datalist>
      <select aria-label="operator" value={op} onChange={(e) => { setOp(e.target.value as EditOp); setError(null); }}>
        {OPS.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
      </select>
      {op !== "exists" && op !== "missing" && (
        <input aria-label="value" placeholder={op === "in" ? "a, b, c" : "value"} value={value}
          onChange={(e) => { setValue(e.target.value); setError(null); }} />
      )}
      <button type="submit">save</button>
      <button type="button" onClick={onCancel}>cancel</button>
      {error && <span role="alert" className="chip-error">{error}</span>}
    </form>
  );
}

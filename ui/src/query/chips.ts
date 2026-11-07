import { type CmpOp, type Expr, type Value, kindOf } from "./ast";
import { type ParseError, parse } from "./parse";
import { print, printValue } from "./print";
import { SLOTS, type Slot, boxSearch } from "./url";

/** One condition in a where-row. The row means `and(...)` of its enabled chips. */
export interface Chip { id: number; expr: Expr; negated: boolean; disabled: boolean }
export type Rows = Record<Slot, Chip[]>;

let nextId = 1;
export const chip = (expr: Expr, negated = false): Chip => ({ id: nextId++, expr, negated, disabled: false });

/** Top-level `and` arguments become chips; `not(x)` becomes a negated chip of `x`. */
export function chipsOf(e: Expr | undefined): Chip[] {
  if (!e) return [];
  return (e.t === "and" ? e.args : [e]).map((a) => (a.t === "not" ? chip(a.arg, true) : chip(a)));
}

export function exprOf(chips: Chip[]): Expr | undefined {
  const on = chips.filter((c) => !c.disabled).map((c): Expr => (c.negated ? { t: "not", arg: c.expr } : c.expr));
  return on.length === 0 ? undefined : on.length === 1 ? on[0] : { t: "and", args: on };
}

export function textOf(chips: Chip[]): string | undefined {
  const e = exprOf(chips);
  return e && print(e);
}

/** The search box's chip: the last enabled, non-negated `search("…")` over all text (`filterOf` puts it last). */
const isBox = (c: Chip) => !c.negated && !c.disabled && c.expr.t === "search" && c.expr.side === "any";
function boxIndex(chips: Chip[]): number {
  for (let i = chips.length - 1; i >= 0; i--) if (isBox(chips[i])) return i;
  return -1;
}

/** A filter row as the URL keeps it: the box's search as `q`, the rest as `filter`. */
export function splitBox(chips: Chip[]): { filter?: string; q?: string } {
  const i = boxIndex(chips);
  const box = chips[i]?.expr;
  if (box?.t !== "search") return { filter: textOf(chips), q: undefined };
  return { filter: textOf(chips.filter((_, j) => j !== i)), q: box.text };
}

/** The row with the box's search set to `text`, or removed. */
export function withBox(chips: Chip[], text?: string): Chip[] {
  const i = boxIndex(chips);
  if (!text) return i < 0 ? chips : chips.filter((_, j) => j !== i);
  const expr = boxSearch(text);
  return i < 0 ? [...chips, chip(expr)] : chips.map((c, j) => (j === i ? { ...c, expr } : c));
}

/** The row plus `add`, leaving out conditions it already applies. */
export function addChips(chips: Chip[], add: Chip[]): Chip[] {
  const key = (c: Chip) => `${c.negated}${print(c.expr)}`;
  const on = new Set(chips.filter((c) => !c.disabled).map(key));
  return [...chips, ...add.filter((c) => !on.has(key(c)))];
}

/** Chips for each slot's text; slots whose text doesn't parse get no chips and an error. */
export function rowsOf(texts: Partial<Record<Slot, string>>): { rows: Rows; errors: Partial<Record<Slot, ParseError>> } {
  const rows = { filter: [], trace: [], tree: [] } as Rows;
  const errors: Partial<Record<Slot, ParseError>> = {};
  for (const s of SLOTS) {
    const t = texts[s];
    if (!t?.trim()) continue;
    const r = parse(t);
    if (r.ok) rows[s] = chipsOf(r.expr);
    else errors[s] = r.error;
  }
  return { rows, errors };
}

/** Conditions the chip editor can show; anything else is an expression chip. */
export const isSimple = (e: Expr) => e.t === "cmp" || e.t === "has" || e.t === "in";

const SYM: Record<CmpOp, string> = { eq: "=", neq: "≠", gt: ">", gte: "≥", lt: "<", lte: "≤" };

/** Editor text to a value. On JSON paths numbers and true/false are typed; quote to keep a string. */
export function toValue(path: string, s: string): Value {
  const kind = kindOf(path);
  const t = s.trim();
  if (kind === "num") return t !== "" && Number.isFinite(Number(t)) ? Number(t) : s;
  if (kind === "any") {
    if (/^-?\d+(\.\d+)?([eE][+-]?\d+)?$/.test(t)) return Number(t);
    if (t === "true" || t === "false") return t === "true";
    if (/^".*"$/s.test(t)) {
      try {
        const v = JSON.parse(t);
        if (typeof v === "string") return v;
      } catch { /* not JSON: keep the text */ }
    }
  }
  return s;
}

/** A value as the editor and chips show it; `toValue` reads it back. */
export function valueText(path: string, v: Value): string {
  if (typeof v !== "string") return printValue(v);
  return toValue(path, v) === v ? v : JSON.stringify(v);
}

export function label(e: Expr): string {
  switch (e.t) {
    case "cmp":
      if (e.value === null) return `${e.path} ${e.op === "eq" ? "missing" : "exists"}`;
      return `${e.path} ${SYM[e.op]} ${valueText(e.path, e.value)}`;
    case "has":
      return `${e.path} has ${e.value}`;
    case "in":
      return `${e.path} in ${e.values.map((v) => valueText(e.path, v)).join(", ")}`;
    case "search":
      return `search ${e.side === "any" ? "" : `${e.side} `}${JSON.stringify(e.text)}`;
    default:
      return print(e);
  }
}

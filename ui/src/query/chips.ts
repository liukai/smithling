import { type CmpOp, type Expr, type Value, kindOf } from "./ast";
import { type ParseError, parse } from "./parse";
import { print, printValue } from "./print";
import { SLOTS, type Slot } from "./url";

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
    default:
      return print(e);
  }
}

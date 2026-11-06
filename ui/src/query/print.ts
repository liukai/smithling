import type { Expr, Value } from "./ast";

/** Canonical text, byte-for-byte the same as crates/model/src/filter/print.rs. */
export function print(e: Expr): string {
  switch (e.t) {
    case "and":
    case "or":
      return `${e.t}(${e.args.map(print).join(", ")})`;
    case "not":
      return `not(${print(e.arg)})`;
    case "cmp":
      return `${e.op}(${e.path}, ${printValue(e.value)})`;
    case "has":
      return `has(${e.path}, ${printValue(e.value)})`;
    case "in":
      return `in(${e.path}, [${e.values.map(printValue).join(", ")}])`;
  }
}

export function printValue(v: Value): string {
  if (typeof v === "string") return JSON.stringify(v);
  if (typeof v === "number") return formatNum(v);
  return String(v);
}

/** Rust's f64 Display: shortest round-trip digits, never in exponent form. */
export function formatNum(n: number): string {
  if (Number.isInteger(n) && Math.abs(n) < 1e15) return String(Math.trunc(n) || 0);
  const m = /^(-?)(\d+)(?:\.(\d+))?e([+-]\d+)$/.exec(String(n));
  if (!m) return String(n);
  const [, sign, int, frac = "", exp] = m;
  const digits = int + frac;
  const point = int.length + Number(exp);
  if (point <= 0) return `${sign}0.${"0".repeat(-point)}${digits}`;
  if (point >= digits.length) return sign + digits + "0".repeat(point - digits.length);
  return `${sign}${digits.slice(0, point)}.${digits.slice(point)}`;
}

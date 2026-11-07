import { describe, expect, it } from "vitest";
import golden from "../../../tests/fixtures/filter-golden.json";
import type { CmpOp, Expr, Value } from "./ast";
import { byteOffset, parse, utf16Offset } from "./parse";
import { print } from "./print";
import { type Stats, parseStats, printStats } from "./stats";

interface Case { slot?: "filter" | "stats"; input: string; ok?: string; error?: { start: number; end: number; message: string } }

describe("filter golden cases (shared with crates/model/tests/filter.rs)", () => {
  for (const c of golden.cases as Case[]) {
    it(`${c.slot ?? "filter"} ${JSON.stringify(c.input)}`, () => {
      const r = c.slot === "stats" ? parseStats(c.input) : parse(c.input);
      if (r.ok) {
        expect(c.ok, `parsed, but expected ${JSON.stringify(c.error)}`).toBeDefined();
        if ("expr" in r) {
          expect(print(r.expr)).toBe(c.ok);
          expect(parse(print(r.expr))).toEqual(r);
        } else {
          expect(printStats(r.stats)).toBe(c.ok);
          expect(parseStats(printStats(r.stats))).toEqual(r);
        }
      } else {
        expect(c.error, `failed: ${r.error.message}`).toBeDefined();
        const { start, end, message } = r.error;
        expect({ start: byteOffset(c.input, start), end: byteOffset(c.input, end), message }).toEqual(c.error);
      }
    });
  }
});

describe("offsets", () => {
  it("converts between UTF-16 indices and UTF-8 byte offsets", () => {
    const s = 'eq(metadata.ключ, "😀")';
    expect(byteOffset(s, 16)).toBe(20);
    expect(utf16Offset(s, 20)).toBe(16);
    expect(utf16Offset(s, byteOffset(s, s.length))).toBe(s.length);
    expect(utf16Offset(s, 999)).toBe(s.length);
  });
});

describe("print/parse round trip", () => {
  let seed = 7;
  const rnd = (n: number) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % n; };
  const pick = <T,>(xs: T[]): T => xs[rnd(xs.length)];
  const str = () => pick(["", "a", "plan", 'q"uote', "back\\slash", "ключ", "😀 x", "tab\tnl\n", "\u0001"]);
  const num = () => pick([0, 1, -1, 0.5, 1e-7, 123456.789, 1e20, -2.5, 429]);
  const anyValue = (): Value => pick<() => Value>([str, num, () => true, () => false, () => null])();
  const ops: CmpOp[] = ["eq", "neq", "gt", "gte", "lt", "lte"];
  const fbKey = () => pick(["correctness", "user_score", "Helpful-1", "a.b", "ключ"]);
  const leaf = (): Expr => {
    switch (rnd(9)) {
      case 6: return { t: "search", side: pick(["any", "inputs", "outputs", "error"] as const), text: pick(["429 Too Many Requests", "rate limit", 'q"uote 😀', "ключ", "a\\b\n"]) };
      case 7: return { t: "exists", path: pick(["name", "tags", "metadata.v", "outputs.a.b", `feedback.${fbKey()}`]) };
      case 8: return { t: "cmp", op: pick(ops), path: `feedback.${fbKey()}`, value: num() };
      case 0: return { t: "cmp", op: pick(["eq", "neq"]), path: "status", value: pick(["running", "ok", "error"]) };
      case 1: return { t: "cmp", op: pick(ops), path: pick(["latency", "total_tokens"]), value: num() };
      case 2: return { t: "has", path: "tags", value: str() };
      case 3: return { t: "in", path: "name", values: [str(), str()] };
      case 4: return { t: "cmp", op: pick(ops), path: "start_time", value: pick(["-12h", "2026-11-05T00:00:00Z"]) };
      default: {
        const op = pick(ops), value = anyValue();
        const ordered = op !== "eq" && op !== "neq";
        return { t: "cmp", op, path: pick(["metadata.v", "outputs.a.b", "inputs.ключ"]), value: ordered && (value === null || typeof value === "boolean") ? 1 : value };
      }
    }
  };
  const expr = (depth: number): Expr => {
    if (depth === 0 || rnd(3) === 0) return leaf();
    const k = rnd(3);
    if (k === 2) return { t: "not", arg: expr(depth - 1) };
    return { t: k === 0 ? "and" : "or", args: Array.from({ length: 1 + rnd(3) }, () => expr(depth - 1)) };
  };

  it("parse(print(e)) == e and printing is canonical", () => {
    for (let i = 0; i < 500; i++) {
      const e = expr(3);
      const text = print(e);
      const r = parse(text);
      expect(r, text).toEqual({ ok: true, expr: e });
      if (r.ok) expect(print(r.expr)).toBe(text);
    }
  });
});

describe("stats line round trip", () => {
  let seed = 11;
  const rnd = (n: number) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % n; };
  const pick = <T,>(xs: T[]): T => xs[rnd(xs.length)];
  const keys = ["name", "run_type", "model", "status", "tag", "feedback_key", "metadata.prompt_version", "metadata.a.b"];
  const metrics = ["count", "error_rate", "p50(latency)", "p95(latency)", "p99(latency)", "sum(input_tokens)",
    "sum(output_tokens)", "sum(total_tokens)", "sum(cost)", "avg(feedback.correctness)", "avg(feedback.Helpful-1)"];

  it("parseStats(printStats(s)) == s and printing is canonical", () => {
    for (let i = 0; i < 300; i++) {
      const s: Stats = {
        groupBy: [...new Set(Array.from({ length: rnd(3) }, () => pick(keys)))],
        metrics: [...new Set(Array.from({ length: 1 + rnd(4) }, () => pick(metrics)))],
        top: 1 + rnd(50),
      };
      const text = printStats(s);
      const r = parseStats(text);
      expect(r, text).toEqual({ ok: true, stats: s });
      if (r.ok) expect(printStats(r.stats)).toBe(text);
    }
  });
});

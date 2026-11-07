import { FEEDBACK_NEEDS_KEY, Fail, type ParseError, type Token, dbg, fail, lex, resolvePath } from "./parse";

/**
 * The stats line, mirroring crates/model/src/agg.rs: `[by <key>[, <key>] |] <metric>, … [| top <n>]`. Keys and
 * metrics are kept as their canonical text (`name`, `metadata.v`, `p95(latency)`, `avg(feedback.k)`).
 */
export interface Stats { groupBy: string[]; metrics: string[]; top: number }
export type StatsResult = { ok: true; stats: Stats } | { ok: false; error: ParseError };

export const DEFAULT_TOP = 10;
const MAX_TOP = 50;
const MAX_GROUP_KEYS = 2;
const NAMED_KEYS = ["name", "run_type", "model", "status", "tag", "feedback_key"];
const PCTS = ["p50", "p95", "p99"];
const SUMS = ["input_tokens", "output_tokens", "total_tokens", "cost"];
const TOP_RANGE = "top is a whole number from 1 to 50";

export function parseStats(src: string): StatsResult {
  try {
    return { ok: true, stats: new Cursor(src, lex(src, true)).stats() };
  } catch (e) {
    if (e instanceof Fail) return { ok: false, error: e.e };
    throw e;
  }
}

export function printStats(s: Stats): string {
  const by = s.groupBy.length ? `by ${s.groupBy.join(", ")} | ` : "";
  const top = s.top !== DEFAULT_TOP ? ` | top ${s.top}` : "";
  return `${by}${s.metrics.join(", ")}${top}`;
}

function groupKey(text: string, start: number, end: number): string {
  if (NAMED_KEYS.includes(text)) return text;
  if (text.startsWith("metadata.")) return resolvePath(text, start, end);
  return fail(start, end, `unknown group key ${dbg(text)}; expected name, run_type, model, status, tag, feedback_key or metadata.<key>`);
}

class Cursor {
  pos = 0;
  constructor(private src: string, private toks: Token[]) {}

  peek(): Token | undefined {
    return this.toks[this.pos];
  }

  eat(k: Token["k"]): boolean {
    const hit = this.peek()?.k === k;
    if (hit) this.pos++;
    return hit;
  }

  next(expected: string): Token {
    const t = this.peek();
    if (!t) fail(this.src.length, this.src.length, `expected ${expected}`);
    this.pos++;
    return t;
  }

  expect(k: Token["k"], shown: string): Token {
    const t = this.next(shown);
    if (t.k !== k) fail(t.start, t.end, `expected ${shown}`);
    return t;
  }

  stats(): Stats {
    const s: Stats = { groupBy: [], metrics: [], top: DEFAULT_TOP };
    const first = this.peek();
    if (first?.k === "ident" && first.s === "by") {
      this.pos++;
      const keys = [this.keyToken()];
      while (this.eat(",")) keys.push(this.keyToken());
      const extra = keys[MAX_GROUP_KEYS];
      if (extra) fail(extra.start, extra.end, `at most ${MAX_GROUP_KEYS} group keys`);
      s.groupBy = keys.map((k) => groupKey(k.s, k.start, k.end));
      const i = s.groupBy.findIndex((k, j) => s.groupBy.indexOf(k) < j);
      if (i >= 0) fail(keys[i].start, keys[i].end, `${dbg(s.groupBy[i])} is listed twice`);
      this.expect("|", '"|"');
    }
    do {
      const first = this.pos;
      const m = this.metric();
      if (s.metrics.includes(m)) fail(this.toks[first].start, this.toks[this.pos - 1].end, `${dbg(m)} is listed twice`);
      s.metrics.push(m);
    } while (this.eat(","));
    if (this.eat("|")) {
      const t = this.next("top");
      if (t.k !== "ident" || t.s !== "top") fail(t.start, t.end, "expected top");
      const n = this.next("a number");
      if (n.k !== "num" || !Number.isInteger(n.n) || n.n < 1 || n.n > MAX_TOP) fail(n.start, n.end, TOP_RANGE);
      s.top = n.n;
    }
    const t = this.peek();
    if (t) fail(t.start, t.end, "expected end of stats line");
    return s;
  }

  keyToken(): Token & { k: "ident" } {
    const t = this.next("a group key");
    if (t.k !== "ident") return fail(t.start, t.end, "expected a group key");
    return t;
  }

  /** The argument of `f(arg)` as an identifier; anything else is `wrong`. */
  arg(wrong: string): Token & { k: "ident" } {
    this.expect("(", '"("');
    const t = this.peek();
    if (!t) fail(this.src.length, this.src.length, wrong);
    this.pos++;
    if (t.k !== "ident") return fail(t.start, t.end, wrong);
    this.expect(")", '")"');
    return t;
  }

  metric(): string {
    const want = "a metric such as count";
    const t = this.next(want);
    if (t.k !== "ident") return fail(t.start, t.end, `expected ${want}`);
    const name = t.s;
    if (PCTS.includes(name)) {
      const msg = `${name}() works on latency`;
      const a = this.arg(msg);
      if (a.s !== "latency") fail(a.start, a.end, msg);
      return `${name}(latency)`;
    }
    if (name === "count" || name === "error_rate") return name;
    if (name === "sum") {
      const msg = "sum() works on input_tokens, output_tokens, total_tokens or cost";
      const a = this.arg(msg);
      if (!SUMS.includes(a.s)) fail(a.start, a.end, msg);
      return `sum(${a.s})`;
    }
    if (name === "avg") {
      const a = this.arg("avg() works on feedback.<key>");
      if (a.s === "feedback" || a.s === "feedback.") fail(a.start, a.end, FEEDBACK_NEEDS_KEY);
      if (!a.s.startsWith("feedback.")) fail(a.start, a.end, "avg() works on feedback.<key>");
      return `avg(${resolvePath(a.s, a.start, a.end)})`;
    }
    if (/^p\d+$/.test(name)) fail(t.start, t.end, `unknown metric ${dbg(name)}; expected p50, p95 or p99`);
    return fail(t.start, t.end, `unknown metric ${dbg(name)}; expected count, error_rate, p50, p95, p99, sum or avg`);
  }
}

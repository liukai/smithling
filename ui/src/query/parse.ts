import { ATTR_ROOTS, type CmpOp, type Expr, FIELDS, type Kind, STATUSES, type Side, type Value, kindOf } from "./ast";

/**
 * Mirror of crates/model/src/filter/parse.rs: same grammar, checks and messages. Offsets here are UTF-16
 * indices into the text; `byteOffset` gives the server's UTF-8 offsets (tests/fixtures/filter-golden.json).
 */
export interface ParseError { start: number; end: number; message: string }
export type ParseResult = { ok: true; expr: Expr } | { ok: false; error: ParseError };

type Tok =
  | { k: "ident"; s: string }
  | { k: "str"; s: string }
  | { k: "num"; n: number }
  | { k: "(" | ")" | "[" | "]" | "," | "|" };
export type Token = Tok & { start: number; end: number };

export class Fail extends Error {
  constructor(public e: ParseError) { super(e.message); }
}
export function fail(start: number, end: number, message: string): never {
  throw new Fail({ start, end, message });
}

export function parse(src: string): ParseResult {
  try {
    const p = new Parser(src, lex(src, false));
    const expr = p.expr();
    const t = p.toks[p.pos];
    if (t) fail(t.start, t.end, "expected end of filter");
    return { ok: true, expr };
  } catch (e) {
    if (e instanceof Fail) return { ok: false, error: e.e };
    throw e;
  }
}

export function byteOffset(src: string, i: number): number {
  return new TextEncoder().encode(src.slice(0, i)).length;
}

export function utf16Offset(src: string, bytes: number): number {
  let b = 0, i = 0;
  for (const ch of src) {
    if (b >= bytes) break;
    b += new TextEncoder().encode(ch).length;
    i += ch.length;
  }
  return i;
}

const ALPHA = /^\p{Alphabetic}$/u;
const IDENT = /^[\p{Alphabetic}\p{N}_.-]$/u;
/** Rust's `escape_debug` for one char inside `quote`-delimited text. */
function escapeDebug(ch: string, quote: string): string {
  const named: Record<string, string> = { "\0": "\\0", "\t": "\\t", "\r": "\\r", "\n": "\\n", "\\": "\\\\" };
  if (named[ch]) return named[ch];
  if (ch === quote) return `\\${ch}`;
  if (ch !== " " && /[\p{C}\p{Z}\p{Grapheme_Extend}]/u.test(ch)) return `\\u{${ch.codePointAt(0)!.toString(16)}}`;
  return ch;
}

/** Rust's `{:?}` for a string and for a char. */
export const dbg = (s: string) => `"${[...s].map((c) => escapeDebug(c, '"')).join("")}"`;
const dbgChar = (ch: string) => `'${escapeDebug(ch, "'")}'`;

/** Tokens of `src`; `|` is a token only when `pipe` is set (the stats line). */
export function lex(src: string, pipe: boolean): Token[] {
  const out: Token[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const start = i;
    if ("()[],".includes(c) || (pipe && c === "|")) {
      out.push({ k: c as "(", start, end: i + 1 });
      i++;
    } else if (" \t\n\f\r".includes(c)) {
      i++;
    } else if (c === '"') {
      const [s, end] = lexString(src, i);
      out.push({ k: "str", s, start, end });
      i = end;
    } else if (c === "-" || (c >= "0" && c <= "9")) {
      i++;
      while (i < src.length && /[0-9.eE+-]/.test(src[i])) i++;
      const text = src.slice(start, i);
      const n = /^-?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(text) ? Number(text) : NaN;
      if (Number.isNaN(n)) fail(start, i, `bad number ${dbg(text)}`);
      if (!Number.isFinite(n)) fail(start, i, `number ${dbg(text)} is out of range`);
      out.push({ k: "num", n, start, end: i });
    } else if (/[A-Za-z_]/.test(c) || (c > "\x7f" && ALPHA.test(String.fromCodePoint(src.codePointAt(i)!)))) {
      while (i < src.length) {
        const ch = String.fromCodePoint(src.codePointAt(i)!);
        if (!IDENT.test(ch)) break;
        i += ch.length;
      }
      out.push({ k: "ident", s: src.slice(start, i), start, end: i });
    } else {
      const ch = String.fromCodePoint(src.codePointAt(i)!);
      fail(i, i + ch.length, `unexpected character ${dbgChar(ch)}`);
    }
  }
  return out;
}

const ESC: Record<string, string> = { '"': '"', "\\": "\\", "/": "/", n: "\n", t: "\t", r: "\r", b: "\b", f: "\f" };

/** A JSON-style string starting at the opening quote `i`: its value and the index after it. */
function lexString(src: string, i: number): [string, number] {
  let out = "";
  let j = i + 1;
  const hex4 = (at: number) => (/^[0-9a-fA-F]{4}$/.test(src.slice(at, at + 4)) ? parseInt(src.slice(at, at + 4), 16) : null);
  while (j < src.length) {
    const at = j;
    const ch = src[j];
    if (ch === '"') return [out, j + 1];
    if (ch !== "\\") {
      out += ch;
      j++;
      continue;
    }
    if (j + 1 >= src.length) break;
    const e = String.fromCodePoint(src.codePointAt(j + 1)!);
    const escEnd = j + 1 + e.length;
    if (e in ESC) {
      out += ESC[e];
      j = escEnd;
    } else if (e === "u") {
      const hi = hex4(escEnd);
      if (hi === null) fail(at, escEnd, "bad \\u escape; expected 4 hex digits");
      const end = escEnd + 4;
      j = end;
      if (hi >= 0xd800 && hi < 0xdc00) {
        const lo = src.slice(end, end + 2) === "\\u" ? hex4(end + 2) : null;
        if (lo === null || lo < 0xdc00 || lo >= 0xe000) fail(at, end, "bad \\u escape");
        out += String.fromCharCode(hi, lo);
        j = end + 6;
      } else if (hi >= 0xdc00 && hi < 0xe000) {
        fail(at, end, "bad \\u escape");
      } else {
        out += String.fromCharCode(hi);
      }
    } else {
      fail(at, escEnd, `unknown escape \\${e}`);
    }
  }
  return fail(i, src.length, 'unterminated string; expected "');
}

const CMP: CmpOp[] = ["eq", "neq", "gt", "gte", "lt", "lte"];
const FUNCS = ["and", "or", "not", ...CMP, "has", "in", "exists", "search"];
const SIDES: Side[] = ["inputs", "outputs", "error"];
const PAIR = "feedback_key and feedback_score go together in one and(…); or write feedback.<key>";
export const FEEDBACK_NEEDS_KEY = "feedback needs a key: feedback.<key>";

/** An expression, or one half of LangSmith's `and(eq(feedback_key, "k"), op(feedback_score, x))`. */
type Node =
  | { n: "expr"; e: Expr }
  | { n: "key"; key: string; start: number; end: number }
  | { n: "score"; op: CmpOp; value: Value; start: number; end: number };

/** One `and(…)`: one feedback_key and at least one feedback_score become `feedback.<key>` comparisons. */
function feedbackPair(nodes: Node[]): Expr {
  const keys = nodes.filter((n) => n.n === "key");
  if (keys.length > 1) fail(keys[1].start, keys[1].end, PAIR);
  const score = nodes.find((n) => n.n === "score");
  const key = keys[0];
  if (key && !score) fail(key.start, key.end, PAIR);
  if (score && !key) fail(score.start, score.end, PAIR);
  const args: Expr[] = [];
  for (const n of nodes) {
    if (n.n === "expr") args.push(n.e);
    else if (n.n === "score") args.push({ t: "cmp", op: n.op, path: `feedback.${key!.key}`, value: n.value });
  }
  return key && args.length === 1 ? args[0] : { t: "and", args };
}

/** A key that prints and lexes back as `feedback.<key>`. */
const validKey = (key: string) => !key.split(".").some((p) => !p) && [...key].every((c) => IDENT.test(c));

/** Rust's `char::is_alphanumeric`: `text::tokens` is empty exactly when no character is one. */
const hasWord = (text: string) => /[\p{Alphabetic}\p{N}]/u.test(text);

class Parser {
  pos = 0;
  constructor(private src: string, public toks: Token[]) {}

  next(expected: string): Token {
    const t = this.toks[this.pos];
    if (!t) fail(this.src.length, this.src.length, `expected ${expected}`);
    this.pos++;
    return t;
  }

  expect(k: Tok["k"], shown: string): Token {
    const t = this.next(shown);
    if (t.k !== k) fail(t.start, t.end, `expected ${shown}`);
    return t;
  }

  peekIs(k: Tok["k"]): boolean {
    return this.toks[this.pos]?.k === k;
  }

  expr(): Expr {
    const n = this.node();
    if (n.n !== "expr") fail(n.start, n.end, PAIR);
    return n.e;
  }

  node(): Node {
    const t = this.next("a function such as eq( or and(");
    if (t.k !== "ident") return fail(t.start, t.end, "expected a function such as eq( or and(");
    const name = t.s;
    if (!FUNCS.includes(name)) {
      fail(t.start, t.end, `unknown function ${dbg(name)}; expected and, or, not, eq, neq, gt, gte, lt, lte, has, in, exists or search`);
    }
    this.expect("(", '"("');
    let e: Expr;
    if (name === "and") {
      const nodes = [this.node()];
      while (this.peekIs(",")) {
        this.pos++;
        nodes.push(this.node());
      }
      e = feedbackPair(nodes);
    } else if (name === "or") {
      const args = [this.expr()];
      while (this.peekIs(",")) {
        this.pos++;
        args.push(this.expr());
      }
      e = { t: name, args };
    } else if (name === "exists") {
      const n = this.toks[this.pos];
      if (n?.k !== "ident") {
        const end = this.src.length;
        if (!n) fail(end, end, "exists() needs a field path");
        fail(n.start, n.k === ")" ? n.start : n.end, "exists() needs a field path");
      }
      e = { t: "exists", path: this.path()[0] };
    } else if (name === "search") {
      e = this.search();
    } else if (name === "not") {
      e = { t: "not", arg: this.expr() };
    } else if (name === "has") {
      const [path, ps, pe] = this.path();
      this.expect(",", '","');
      const [value, vs, ve] = this.value();
      if (path !== "tags") fail(ps, pe, `has() works on lists; ${dbg(path)} isn't one (use eq)`);
      if (typeof value !== "string") fail(vs, ve, "has(tags, …) needs a string value");
      e = { t: "has", path, value: value as string };
    } else if (name === "in") {
      const [path, ps, pe] = this.path();
      this.expect(",", '","');
      this.expect("[", '"["');
      const values: Value[] = [];
      if (!this.peekIs("]")) {
        for (;;) {
          const [v, vs, ve] = this.value();
          values.push(checkValue(path, ps, pe, "eq", v, vs, ve));
          if (!this.peekIs(",")) break;
          this.pos++;
        }
      }
      this.expect("]", '"]"');
      if (!values.length) fail(ps, pe, "in() needs at least one value");
      e = { t: "in", path, values };
    } else {
      const op = name as CmpOp;
      const part = this.feedbackPairPart(op);
      if (part) {
        this.expect(")", '")"');
        return part;
      }
      const [path, ps, pe] = this.path();
      this.expect(",", '","');
      const [value, vs, ve] = this.value();
      e = { t: "cmp", op, path, value: checkValue(path, ps, pe, op, value, vs, ve) };
    }
    this.expect(")", '")"');
    return { n: "expr", e };
  }

  search(): Expr {
    let side: Side = "any";
    const s = this.toks[this.pos];
    if (s?.k === "ident") {
      if (!SIDES.includes(s.s as Side)) fail(s.start, s.end, "search side is inputs, outputs or error");
      side = s.s as Side;
      this.pos++;
      if (this.toks[this.pos] && !this.peekIs(")")) this.expect(",", '","');
    }
    const t = this.toks[this.pos];
    const [start, end] = !t ? [this.src.length, this.src.length] : t.k === ")" ? [t.start, t.start] : [t.start, t.end];
    if (t?.k !== "str") return fail(start, end, "search needs a string");
    this.pos++;
    if (!hasWord(t.s)) fail(start, end, "search needs at least one word");
    return { t: "search", side, text: t.s };
  }

  feedbackPairPart(op: CmpOp): Node | null {
    const p = this.toks[this.pos];
    if (p?.k !== "ident" || (p.s !== "feedback_key" && p.s !== "feedback_score")) return null;
    this.pos++;
    this.expect(",", '","');
    const [value, vs, ve] = this.value();
    if (p.s === "feedback_score") {
      return { n: "score", op, value: checkValueAs("feedback_score", "num", p.start, p.end, op, value, vs, ve), start: p.start, end: p.end };
    }
    if (op !== "eq" || typeof value !== "string") fail(p.start, p.end, 'feedback_key is compared with eq and a "string"');
    if (!validKey(value)) {
      fail(vs, ve, `feedback key ${dbg(value)} can't be written as feedback.<key>; use letters, digits, _ . or -`);
    }
    return { n: "key", key: value, start: p.start, end: p.end };
  }

  path(): [string, number, number] {
    const t = this.next("a field name");
    if (t.k !== "ident") return fail(t.start, t.end, "expected a field name");
    return [resolvePath(t.s, t.start, t.end), t.start, t.end];
  }

  value(): [Value, number, number] {
    const t = this.next("a value");
    let v: Value;
    if (t.k === "str") v = t.s;
    else if (t.k === "num") v = t.n;
    else if (t.k === "ident" && t.s === "true") v = true;
    else if (t.k === "ident" && t.s === "false") v = false;
    else if (t.k === "ident" && t.s === "null") v = null;
    else return fail(t.start, t.end, 'expected a value: a "string", number, true, false or null');
    return [v, t.start, t.end];
  }
}

export function resolvePath(text: string, start: number, end: number): string {
  if (text === "feedback_key" || text === "feedback_score") fail(start, end, PAIR);
  if (text === "feedback" || text === "feedback.") fail(start, end, FEEDBACK_NEEDS_KEY);
  if (text.startsWith("feedback.")) {
    if (!validKey(text.slice("feedback.".length))) fail(start, end, `bad path ${dbg(text)}`);
    return text;
  }
  const dot = text.indexOf(".");
  if (dot >= 0) {
    if (!ATTR_ROOTS.includes(text.slice(0, dot))) {
      fail(start, end, `unknown field ${dbg(text)}; JSON paths start with metadata., inputs. or outputs.`);
    }
    if (text.slice(dot + 1).split(".").some((p) => !p)) fail(start, end, `bad path ${dbg(text)}`);
    return text;
  }
  if (Object.hasOwn(FIELDS, text)) return text;
  let best: [number, string] | null = null;
  for (const n of [...Object.keys(FIELDS), ...ATTR_ROOTS]) {
    const d = editDistance(n, text);
    if (!best || d < best[0] || (d === best[0] && n < best[1])) best = [d, n];
  }
  const hint = best && best[0] <= 2 ? ` (did you mean ${dbg(best[1])}?)` : "";
  return fail(start, end, `unknown field ${dbg(text)}${hint}`);
}

function editDistance(a: string, b: string): number {
  const bs = [...b];
  let prev = Array.from({ length: bs.length + 1 }, (_, i) => i);
  let i = 0;
  for (const ca of a) {
    const cur = [i + 1];
    bs.forEach((cb, j) => cur.push(Math.min(prev[j] + (ca === cb ? 0 : 1), prev[j + 1] + 1, cur[j] + 1)));
    prev = cur;
    i++;
  }
  return prev[bs.length];
}

const MAX_RELATIVE_US = 100n * 366n * 86_400_000_000n;
const UNIT_US: Record<string, bigint> = { s: 1_000_000n, m: 60_000_000n, h: 3_600_000_000n, d: 86_400_000_000n };

/** `"-12h"`-style relative times or ISO-8601, as `TimeAt::parse` and chrono accept them. */
export function isTime(s: string): boolean {
  const rel = /^[+-](\d+)([smhd])$/.exec(s);
  if (rel) {
    const n = BigInt(rel[1]);
    return n <= 9223372036854775807n && n * UNIT_US[rel[2]] <= MAX_RELATIVE_US;
  }
  const rfc = /^(\d{4})-(\d{2})-(\d{2})[Tt ](\d{2}):(\d{2}):(\d{2})(\.\d+)?([Zz]|[+-](\d{2}):(\d{2}))$/.exec(s);
  if (rfc) return validDate(rfc) && (!rfc[9] || (Number(rfc[9]) < 24 && Number(rfc[10]) < 60));
  // chrono's `%Y-%m-%dT%H:%M:%S%.f`: numeric fields may have leading spaces; years have 1–4 digits or a sign.
  const naive = /^\s*([+-]\d+|\d{1,4})-\s*(\d{1,2})-\s*(\d{1,2})T\s*(\d{1,2}):\s*(\d{1,2}):\s*(\d{1,2})(\.\d+)?$/.exec(s.endsWith("Z") ? s.slice(0, -1) : s);
  return naive !== null && Math.abs(Number(naive[1])) <= 262142 && validDate(naive);
}

function validDate(m: RegExpExecArray): boolean {
  const [y, mo, d, h, mi, sec] = m.slice(1, 7).map(Number);
  const leap = (y % 4 === 0 && y % 100 !== 0) || y % 400 === 0;
  const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][mo - 1];
  return days !== undefined && d >= 1 && d <= days && h < 24 && mi < 60 && sec <= 60;
}

const UUID = /^([0-9a-f]{32}|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|\{[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\}|urn:uuid:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i;

const NEEDS: Partial<Record<Kind, string>> = {
  str: 'a "string" value',
  time: 'a time such as "2026-11-05T00:00:00Z" or "-12h"',
  num: "a number",
};

function checkValue(path: string, ps: number, pe: number, op: CmpOp, value: Value, vs: number, ve: number): Value {
  return checkValueAs(path, kindOf(path), ps, pe, op, value, vs, ve);
}

function checkValueAs(path: string, kind: Kind, ps: number, pe: number, op: CmpOp, value: Value, vs: number, ve: number): Value {
  const ordered = op !== "eq" && op !== "neq";
  if (value === null) {
    return ordered ? fail(vs, ve, `null can only be used with eq or neq, not ${op}()`) : value;
  }
  const mismatch = (what: string) => fail(ps, pe, `${dbg(path)} is ${what}; ${op}() needs ${NEEDS[kind] ?? "a value of the same type"}`);
  switch (kind) {
    case "str":
      return typeof value === "string" ? value : mismatch("a string");
    case "status":
      if (ordered) fail(ps, pe, `status can only be compared with eq, neq or in, not ${op}()`);
      return typeof value === "string" && STATUSES.includes(value) ? value : fail(vs, ve, 'status is one of "running", "ok", "error"');
    case "time":
      if (typeof value !== "string") return mismatch("a time");
      return isTime(value) ? value : fail(vs, ve, `${path} needs an ISO-8601 time or a relative time like "-12h"`);
    case "num":
      return typeof value === "number" ? value : mismatch("a number");
    case "list":
      return fail(ps, pe, `${path} is a list; use has(${path}, …)`);
    case "id":
      if (ordered) fail(ps, pe, `${path} can only be compared with eq, neq or in`);
      return typeof value === "string" && UUID.test(value) ? value : fail(vs, ve, `${path} needs a UUID string`);
    case "any":
      return typeof value === "boolean" && ordered ? fail(vs, ve, `true/false can only be used with eq or neq, not ${op}()`) : value;
  }
}

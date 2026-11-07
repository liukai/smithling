/** Filter AST, mirroring crates/model/src/filter/ast.rs. Time values stay strings, as written. */
export type CmpOp = "eq" | "neq" | "gt" | "gte" | "lt" | "lte";
export type Value = string | number | boolean | null;
export type Side = "any" | "inputs" | "outputs" | "error";

export type Expr =
  | { t: "and" | "or"; args: Expr[] }
  | { t: "not"; arg: Expr }
  | { t: "cmp"; op: CmpOp; path: string; value: Value }
  | { t: "has"; path: string; value: string }
  | { t: "in"; path: string; values: Value[] }
  | { t: "search"; side: Side; text: string }
  | { t: "exists"; path: string };

export type Kind = "str" | "status" | "time" | "num" | "list" | "id" | "any";

export const FIELDS: Record<string, Kind> = {
  status: "status", name: "str", run_type: "str", model: "str", thread_id: "str", project: "str", error: "str",
  start_time: "time", end_time: "time", latency: "num", tags: "list",
  total_tokens: "num", input_tokens: "num", output_tokens: "num",
  trace_id: "id", run_id: "id", parent_run_id: "id",
};

export const ATTR_ROOTS = ["metadata", "inputs", "outputs"];
export const STATUSES = ["running", "ok", "error"];

export function kindOf(path: string): Kind {
  if (path.startsWith("feedback.")) return "num";
  return Object.hasOwn(FIELDS, path) ? FIELDS[path] : "any";
}

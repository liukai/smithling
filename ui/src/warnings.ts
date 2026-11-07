import type { QueryWarning } from "./api";
import { plural } from "./format";

export function warningText(w: QueryWarning): string {
  switch (w.kind) {
    case "mixed_types":
      return `${w.path}: mixed types (${Object.entries(w.counts).map(([t, n]) => `${t} ${n}`).join(", ")}); only values of the compared type match`;
    case "not_indexed":
      if ("files" in w) {
        const one = w.files.length === 1;
        return `${plural(w.files.length, "file")} ${one ? "has" : "have"} no text index yet; the search read ${one ? "it" : "them"} whole`;
      }
      return `${w.path} isn't indexed; read the payloads of ${w.runs} runs`;
    case "skipped_files":
      return `${plural(w.ids.length, "damaged file")} ${w.ids.length === 1 ? "was" : "were"} skipped: ${w.ids.join(", ")}`;
    case "identity_conflict":
      return `run ${w.run_id} has conflicting versions`;
  }
}

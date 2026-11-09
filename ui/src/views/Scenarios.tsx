import { useState } from "react";
import { postAggregate, postQuery } from "../api";
import { type QueryState, aggBody, fromUrl, toBody, toUrl } from "../query/url";
import { navigate } from "../route";

const PROJECT = "overnight-eval";
const A = 'has(tags, "batch=2026-11-05-a")';
const B = 'has(tags, "batch=2026-11-06-b")';

export interface Scenario {
  n: number;
  title: string;
  question: string;
  /** What the demo data planted, so the result can be checked by eye. */
  expect: string;
  /** The runs view's URL parameters; none when smithling can't answer it yet. */
  params?: Record<string, string>;
  missing?: string;
}

/** The SmithDB post's seven questions, asked of `smithling demo`'s overnight-eval data, plus the batch comparison. */
export const SCENARIOS: Scenario[] = [
  {
    n: 1, title: "Show me this one", question: "Open the slowest failed task of batch a, then any step's full input.",
    expect: "One root run; click it for the tree, the waterfall and each step's inputs and outputs.",
    params: { project: PROJECT, root: "1", filter: `and(${A}, eq(status, "error"))`, order: "latency.desc", limit: "1" },
  },
  {
    n: 2, title: "Narrow it down", question: "Failed steps in batch a that took longer than 2 s.",
    expect: "56 runs: fetch_url steps that hit a 429 or a 30 s timeout, and the failed tasks above them.",
    params: { project: PROJECT, filter: `and(${A}, eq(status, "error"), gt(latency, 2))` },
  },
  {
    n: 3, title: "Find the text", question: 'Which runs mention "429 Too Many Requests" anywhere in their payload?',
    expect: "31 runs, all fetch_url, each with the matching excerpt highlighted.",
    params: { project: PROJECT, filter: `or(${A}, ${B})`, q: "429 Too Many Requests" },
  },
  {
    n: 4, title: "Look inside the JSON", question: "Runs on prompt v7 whose output carried status code 429.",
    expect: "19 runs (metadata.prompt_version = v7, outputs.status_code = 429).",
    params: { project: PROJECT, filter: `and(${A}, eq(metadata.prompt_version, "v7"), eq(outputs.status_code, 429))` },
  },
  {
    n: 5, title: "Think in trees", question: "Tasks that finished ok, but where some step underneath failed.",
    expect: "23 quiet failures: the root says ok, a fetch_url below it errored.",
    params: { project: PROJECT, root: "1", filter: `and(${A}, eq(status, "ok"))`, tree: 'eq(status, "error")' },
  },
  {
    n: 6, title: "Rebuild the conversation", question: "All turns of one chat, in order.",
    expect: "", missing: "Threads are in the backlog; the demo data has no chats yet.",
  },
  {
    n: 7, title: "Add it up", question: "Error rate, p95 latency and cost per step name in batch a.",
    expect: "fetch_url: 300 runs, 14% errors, p95 31 s. Batch total $4.12 (12 llm runs have no price).",
    params: { project: PROJECT, filter: A, view: "stats", by: "name" },
  },
  {
    n: 8, title: "Did the fix work?", question: "Batch a against batch b, after the retry fix.",
    expect: "fetch_url errors 14% → 2%, p95 31 s → 9 s.",
    params: { project: PROJECT, filter: A, view: "stats", by: "name", cmp: B },
  },
];

const stateOf = (s: Scenario): QueryState => fromUrl(new URLSearchParams(s.params).toString()).q;
const isStats = (q: QueryState) => q.view === "stats" || q.view === "series" || q.view === "dist";

/** The request the runs view sends for this scenario, and its answer. */
async function answer(s: Scenario) {
  const q = stateOf(s);
  const [endpoint, body] = isStats(q) ? ["/api/aggregate", aggBody(q)] : ["/api/query", toBody(q)];
  const response = endpoint === "/api/aggregate" ? await postAggregate(body) : await postQuery(body);
  return { scenario: s.n, title: s.title, question: s.question, view: toUrl(q), request: { endpoint, body }, response };
}

function save(name: string, data: unknown) {
  const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: "application/json" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  a.click();
  URL.revokeObjectURL(url);
}

const slug = (s: Scenario) => s.title.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/-$/, "");

export function ScenariosPage() {
  const [busy, setBusy] = useState<number | "all" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const run = async (what: number | "all", f: () => Promise<void>) => {
    setBusy(what);
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(`Download failed: ${(e as Error).message}`);
    } finally {
      setBusy(null);
    }
  };
  const ready = SCENARIOS.filter((s) => s.params);
  return (
    <div className="scenarios">
      <div className="scenarios-head">
        <div>
          <h2>Scenarios</h2>
          <p className="muted">
            The questions from LangChain's <a href="https://www.langchain.com/blog/introducing-smithdb">SmithDB post</a>,
            asked of the demo data. Load it once with <code>smithling demo --url {window.location.origin}</code>, then
            open a question or download its answer as JSON.
          </p>
        </div>
        <button className="btn" disabled={busy !== null}
          onClick={() => run("all", async () => save("smithling-scenarios.json", await Promise.all(ready.map(answer))))}>
          {busy === "all" ? "Downloading…" : "Download all"}
        </button>
      </div>
      {error && <div className="error-banner">{error}</div>}
      <ol className="cards">
        {SCENARIOS.map((s) => (
          <li key={s.n} className={s.params ? "card" : "card off"}>
            <div className="card-n">{s.n <= 7 ? `Q${s.n}` : "Extra"}</div>
            <h3>{s.title}</h3>
            <p>{s.question}</p>
            {s.params ? (
              <>
                <p className="muted expect">Expect: {s.expect}</p>
                <div className="card-actions">
                  <a className="btn primary" href={toUrl(stateOf(s))}
                    onClick={(e) => { e.preventDefault(); navigate(toUrl(stateOf(s))); }}>Open</a>
                  <button className="btn" disabled={busy !== null}
                    onClick={() => run(s.n, async () => save(`smithling-scenario-${s.n}-${slug(s)}.json`, await answer(s)))}>
                    {busy === s.n ? "Downloading…" : "Download JSON"}
                  </button>
                </div>
              </>
            ) : (
              <p className="muted expect">{s.missing}</p>
            )}
          </li>
        ))}
      </ol>
    </div>
  );
}

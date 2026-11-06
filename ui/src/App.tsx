import { RunsList } from "./RunsList";
import { TraceView } from "./TraceView";
import { navigate, useLocation } from "./route";

export function App() {
  const url = useLocation();
  const trace = url.pathname.match(/^\/trace\/([0-9a-f-]{36})$/i)?.[1];
  const q = url.searchParams;
  return (
    <>
      <header>
        <a href="/" onClick={(e) => { e.preventDefault(); navigate("/"); }} className="brand">tracedb</a>
        <span className="muted">local traces for AI agents</span>
      </header>
      <main>
        {trace ? (
          <TraceView traceId={trace} selected={q.get("run") ?? undefined} />
        ) : (
          <RunsList project={q.get("project") ?? undefined} status={q.get("status") ?? undefined} />
        )}
      </main>
    </>
  );
}

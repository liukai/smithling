import { navigate, useLocation } from "./route";
import { TraceView } from "./TraceView";
import { RunsPage } from "./views/Runs";
import { ScenariosPage } from "./views/Scenarios";

export function App() {
  const url = useLocation();
  const trace = url.pathname.match(/^\/trace\/([0-9a-f-]{36})$/i)?.[1];
  const q = url.searchParams;
  return (
    <>
      <header>
        <a href="/" onClick={(e) => { e.preventDefault(); navigate("/"); }} className="brand">tracedb</a>
        <span className="muted">local traces for AI agents</span>
        <nav>
          <a href="/" onClick={(e) => { e.preventDefault(); navigate("/"); }}
            className={url.pathname === "/" ? "on" : undefined}>Runs</a>
          <a href="/scenarios" onClick={(e) => { e.preventDefault(); navigate("/scenarios"); }}
            className={url.pathname === "/scenarios" ? "on" : undefined}>Scenarios</a>
        </nav>
      </header>
      <main>
        {trace ? (
          <TraceView traceId={trace} selected={q.get("run") ?? undefined} />
        ) : url.pathname === "/scenarios" ? (
          <ScenariosPage />
        ) : (
          <RunsPage search={url.search} />
        )}
      </main>
    </>
  );
}

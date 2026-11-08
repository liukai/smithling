export function EmptyState() {
  const origin = window.location.origin;
  return (
    <div className="empty">
      <h2>No traces yet</h2>
      <p>Point any agent built on the LangSmith SDK at smithling. Only the endpoint changes; no code changes:</p>
      <pre>{`export LANGSMITH_TRACING=true
export LANGSMITH_ENDPOINT=${origin}
export LANGSMITH_API_KEY=local   # any value; smithling doesn't check it
python your_agent.py`}</pre>
      <p>
        No agent at hand? Generate realistic traces with <code>smithling seed --traces 100</code>.
      </p>
    </div>
  );
}

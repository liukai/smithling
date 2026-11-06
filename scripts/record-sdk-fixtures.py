"""Record real LangSmith SDK upload traffic as contract fixtures for tracedb.

Run with the Python of a venv that has the pinned `langsmith` and `langgraph`, e.g.:

    uv venv /tmp/ls-0.14.17 && VIRTUAL_ENV=/tmp/ls-0.14.17 uv pip install langsmith==0.14.17 langgraph
    /tmp/ls-0.14.17/bin/python scripts/record-sdk-fixtures.py

Writes tests/fixtures/sdk-<version>/requests.jsonl (every request the SDK made) and expected-runs.json
(the merged runs, computed here independently of tracedb). Fails if the SDK logs a backend-version warning.
"""

import json
import logging
import os
import subprocess
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

INFO = {
    "version": "",
    "tracedb_version": "recorder",
    "license_expiration_time": None,
    "batch_ingest_config": {
        "use_multipart_endpoint": False,
        "scale_up_qsize_trigger": 200,
        "scale_up_nthreads_limit": 32,
        "scale_down_nempty_trigger": 4,
        "size_limit": 100,
        "size_limit_bytes": 20971520,
    },
    "instance_flags": {},
}
REQUESTS: list[dict] = []
LOCK = threading.Lock()


class Recorder(BaseHTTPRequestHandler):
    def _record(self):
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n).decode() if n else ""
        with LOCK:
            REQUESTS.append({
                "method": self.command,
                "path": self.path,
                "content_type": self.headers.get("content-type"),
                "body": json.loads(body) if body and "json" in (self.headers.get("content-type") or "") else body,
            })

    def _reply(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._record()
        self._reply(200, INFO) if self.path.split("?")[0] == "/info" else self._reply(404, {})

    def do_POST(self):
        self._record()
        self._reply(202, {"accepted": 0, "rejected": []})

    do_PATCH = do_POST

    def log_message(self, *args):
        pass


class WarningCatcher(logging.Handler):
    def __init__(self):
        super().__init__(logging.WARNING)
        self.messages = []

    def emit(self, record):
        self.messages.append(record.getMessage())


def run_agent():
    """A small LangGraph agent: plan → web_search → fetch_url (fails) → summarize (sub-agent with an LLM)."""
    from typing import TypedDict

    from langchain_core.language_models.fake_chat_models import FakeListChatModel
    from langgraph.graph import END, StateGraph
    from langsmith import traceable

    class State(TypedDict, total=False):
        task: str
        notes: list

    @traceable(run_type="tool", name="web_search")
    def web_search(q: str) -> list:
        return [f"https://example.com/{q}/{i}" for i in range(3)]

    @traceable(run_type="tool", name="fetch_url")
    def fetch_url(url: str) -> dict:
        raise RuntimeError("HTTPError: 429 Too Many Requests")

    llm = FakeListChatModel(responses=["a short summary"])

    def plan(s: State) -> State:
        return {"notes": [f"plan for {s['task']}"]}

    def search(s: State) -> State:
        urls = web_search(s["task"])
        try:
            fetch_url(urls[0])
        except RuntimeError:
            pass
        return {"notes": s["notes"] + urls}

    def summarize(s: State) -> State:
        time.sleep(1.5)  # long enough that the SDK sends the trace's start before its end (exercises patches)
        return {"notes": s["notes"] + [llm.invoke("summarize: " + " ".join(s["notes"])).content]}

    g = StateGraph(State)
    g.add_node("plan", plan)
    g.add_node("search", search)
    g.add_node("summarize", summarize)
    g.set_entry_point("plan")
    g.add_edge("plan", "search")
    g.add_edge("search", "summarize")
    g.add_edge("summarize", END)
    g.compile().invoke({"task": "llm-papers"}, config={"metadata": {"task_id": "t-117", "prompt_version": "v7"}, "tags": ["batch=fixture"]})


def project_of(item: dict):
    if item.get("session_name"):
        return item["session_name"]
    if item.get("session_id"):
        return str(item["session_id"])
    addr = item.get("address")
    if isinstance(addr, str) and addr.startswith("lrn:agents/") and "/environments/" in addr:
        agent, env = addr[len("lrn:agents/"):].split("/environments/", 1)
        return f"{agent}/{env}"
    return None


def expected_runs(requests: list[dict]) -> list[dict]:
    """Independent oracle: fold each run's items in arrival order; present non-null fields win."""
    runs: dict[str, dict] = {}
    for req in requests:
        body = req["body"] if isinstance(req["body"], dict) else {}
        if req["method"] == "POST" and req["path"] == "/runs/batch":
            items = body.get("post", []) + body.get("patch", [])
        elif req["path"] == "/runs" or req["path"].startswith("/runs/"):
            items = [dict(body, id=body.get("id") or req["path"].rsplit("/", 1)[-1])]
        else:
            continue
        for it in items:
            r = runs.setdefault(it["id"], {"project": None})
            for k in ("trace_id", "parent_run_id", "name", "run_type", "start_time", "end_time", "error", "address"):
                if it.get(k) is not None:
                    r[k] = it[k]
            if r["project"] is None:
                r["project"] = project_of(it)
            for k in ("inputs", "outputs"):
                if it.get(k) is not None:
                    r["has_" + k] = True
    out = []
    for rid, r in sorted(runs.items()):
        status = "error" if r.get("error") else ("ok" if r.get("end_time") else "running")
        out.append({
            "run_id": rid,
            "trace_id": r.get("trace_id"),
            "parent_run_id": r.get("parent_run_id"),
            "name": r.get("name"),
            "run_type": r.get("run_type"),
            "project": r["project"] or "default",
            "address": r.get("address"),
            "status": status,
            "has_inputs": r.get("has_inputs", False),
            "has_outputs": r.get("has_outputs", False),
        })
    return out


def child():
    """Runs the agent once in this process (env already set by the parent) and flushes all uploads."""
    from langchain_core.tracers.langchain import wait_for_all_tracers
    from langsmith.run_trees import get_cached_client

    catcher = WarningCatcher()
    logging.getLogger("langsmith").addHandler(catcher)
    run_agent()
    wait_for_all_tracers()
    get_cached_client().flush()
    version_warnings = [m for m in catcher.messages if "Backend version" in m]
    if version_warnings:
        sys.exit(f"SDK logged backend-version warnings: {version_warnings}")


def main():
    import langsmith

    server = ThreadingHTTPServer(("127.0.0.1", 0), Recorder)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = {
        k: v for k, v in os.environ.items() if not k.startswith(("LANGSMITH_", "LANGCHAIN_"))
    } | {
        "LANGSMITH_TRACING": "true",
        "LANGSMITH_ENDPOINT": f"http://127.0.0.1:{server.server_port}",
        "LANGSMITH_API_KEY": "local",
    }
    variants = [{"LANGSMITH_PROJECT": "research-agent"}]
    if tuple(int(x) for x in langsmith.__version__.split(".")[:2]) >= (0, 14):
        variants.append({"LANGSMITH_AGENT_ID": "research-bot", "LANGSMITH_AGENT_ENVIRONMENT": "development"})
    for env in variants:
        subprocess.run([sys.executable, __file__, "--child"], env=base | env, check=True, timeout=120)
    server.shutdown()

    out = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / f"sdk-{langsmith.__version__}"
    out.mkdir(parents=True, exist_ok=True)
    with open(out / "requests.jsonl", "w") as f:
        for r in REQUESTS:
            f.write(json.dumps(r, sort_keys=True) + "\n")
    runs = expected_runs(REQUESTS)
    (out / "expected-runs.json").write_text(json.dumps(runs, indent=2, sort_keys=True) + "\n")
    print(f"langsmith {langsmith.__version__}: {len(REQUESTS)} requests, {len(runs)} runs → {out}")
    print("paths:", sorted({(r["method"], r["path"].split("?")[0]) for r in REQUESTS}))


if __name__ == "__main__":
    child() if "--child" in sys.argv else main()

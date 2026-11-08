"""Record real LangSmith SDK upload traffic as contract fixtures for tracedb.

Run with the Python of a venv that has the pinned `langsmith`, `langgraph` and `zstandard`, e.g.:

    uv venv /tmp/ls-0.14.17 && VIRTUAL_ENV=/tmp/ls-0.14.17 uv pip install langsmith==0.14.17 langgraph zstandard
    /tmp/ls-0.14.17/bin/python scripts/record-sdk-fixtures.py --mode multipart-zstd

`--mode` sets what `/info` advertises: `json` (multipart off; the default), `multipart`, or `multipart-zstd`.
Writes tests/fixtures/sdk-<version>[-<mode>]/requests.jsonl (every request the SDK made; multipart bodies as
base64 with their headers), expected-runs.json (the merged runs, computed here independently of tracedb) and
expected-feedback.json. Fails if the SDK logs a backend-version warning.
"""

import base64
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

MODES = ("json", "multipart", "multipart-zstd")
MP_HEADERS = ("content-type", "content-encoding", "x-pre-compressed-size", "x-post-compressed-size")
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
        raw = self.rfile.read(n) if n else b""
        ct = self.headers.get("content-type") or ""
        req = {"method": self.command, "path": self.path, "content_type": self.headers.get("content-type")}
        if ct.startswith("multipart/"):
            assert raw, "multipart body without Content-Length (chunked); the recorder can't read it"
            req["body"] = None
            req["body_b64"] = base64.b64encode(raw).decode()
            req["headers"] = {k: self.headers[k] for k in MP_HEADERS if self.headers.get(k) is not None}
        else:
            body = raw.decode()
            try:
                req["body"] = json.loads(body) if body else ""
            except ValueError:
                req["body"] = body
        with LOCK:
            REQUESTS.append(req)

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


def run_agent(root_id: str):
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
    g.compile().invoke(
        {"task": "llm-papers"},
        config={"run_id": root_id, "metadata": {"task_id": "t-117", "prompt_version": "v7"}, "tags": ["batch=fixture"]},
    )


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


def multipart_parts(req: dict) -> list[tuple[str, bytes]]:
    """(name, bytes) per part, decoded with the stdlib and `zstandard` (not tracedb's parser)."""
    import zstandard

    raw = base64.b64decode(req["body_b64"])
    if req["headers"].get("content-encoding") == "zstd":
        raw = zstandard.ZstdDecompressor().stream_reader(raw, read_across_frames=True).read()
    boundary = req["headers"]["content-type"].split("boundary=", 1)[1].encode()
    parts = []
    for chunk in raw.split(b"--" + boundary)[1:]:
        if chunk.startswith(b"--"):
            break
        head, _, data = chunk[2:].partition(b"\r\n\r\n")
        name = head.split(b'name="', 1)[1].split(b'"', 1)[0].decode()
        parts.append((name, data[:-2]))
    return parts


def multipart_items(req: dict) -> tuple[list[dict], list[dict]]:
    """Runs (posts, then patches) and feedback, assembled from the parts."""
    runs: dict[tuple[str, str], dict] = {}
    feedback = []
    for name, data in multipart_parts(req):
        bits = name.split(".")
        if bits[0] in ("post", "patch") and len(bits) == 2:
            runs.setdefault((bits[0], bits[1]), {}).update(json.loads(data))
        elif bits[0] in ("post", "patch") and len(bits) == 3:
            runs.setdefault((bits[0], bits[1]), {})[bits[2]] = json.loads(data)
        elif bits[0] == "feedback":
            feedback.append(json.loads(data))
    items = [dict(v, id=v.get("id") or k[1]) for op in ("post", "patch") for k, v in runs.items() if k[0] == op]
    return items, feedback


def expected_runs(requests: list[dict]) -> list[dict]:
    """Independent oracle: fold each run's items in arrival order; present non-null fields win."""
    runs: dict[str, dict] = {}
    for req in requests:
        body = req["body"] if isinstance(req["body"], dict) else {}
        if req["method"] == "POST" and req["path"] == "/runs/batch":
            items = body.get("post", []) + body.get("patch", [])
        elif req["method"] == "POST" and req["path"] == "/runs/multipart":
            items = multipart_items(req)[0]
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


def expected_feedback(requests: list[dict]) -> list[dict]:
    """Feedback by id after every create and update, in arrival order."""
    rows: dict[str, dict] = {}
    for req in requests:
        path = req["path"].split("?")[0]
        if req["method"] == "POST" and path == "/feedback":
            items = [req["body"]]
        elif req["method"] == "PATCH" and path.startswith("/feedback/"):
            items = [dict(req["body"], id=path.rsplit("/", 1)[-1])]
        elif req["method"] == "POST" and path == "/runs/multipart":
            items = multipart_items(req)[1]
        else:
            continue
        for fb in items:
            row = rows.setdefault(fb["id"], {"id": fb["id"]})
            row.update({k: fb[k] for k in ("run_id", "key", "score", "comment") if fb.get(k) is not None})
    return [rows[k] for k in sorted(rows)]


def child():
    """Runs the agent once in this process (env already set by the parent) and flushes all uploads.

    Feedback goes out before the run is uploaded (it arrives first), after it, and as one update.
    """
    from langchain_core.tracers.langchain import wait_for_all_tracers
    from langsmith.run_trees import get_cached_client

    catcher = WarningCatcher()
    logging.getLogger("langsmith").addHandler(catcher)
    client = get_cached_client()
    root_id = str(uuid.uuid4())
    client.create_feedback(root_id, key="correctness", score=0.0, comment="gave up on 429")
    run_agent(root_id)
    wait_for_all_tracers()
    client.flush()
    fb = client.create_feedback(root_id, key="helpfulness", score=1)
    client.update_feedback(fb.id, score=0.5, comment="partly")
    version_warnings = [m for m in catcher.messages if "Backend version" in m]
    if version_warnings:
        sys.exit(f"SDK logged backend-version warnings: {version_warnings}")


def main():
    import langsmith

    mode = sys.argv[sys.argv.index("--mode") + 1] if "--mode" in sys.argv else "json"
    if mode not in MODES:
        sys.exit(f"--mode must be one of {MODES}")
    INFO["batch_ingest_config"]["use_multipart_endpoint"] = mode != "json"
    INFO["instance_flags"] = {"zstd_compression_enabled": True} if mode == "multipart-zstd" else {}

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

    suffix = "" if mode == "json" else f"-{mode}"
    out = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / f"sdk-{langsmith.__version__}{suffix}"
    out.mkdir(parents=True, exist_ok=True)
    # Recorded tracebacks hold this checkout's absolute path; fixtures are published, so replace it.
    root = str(Path(__file__).resolve().parent.parent) + "/"
    scrub = lambda text: text.replace(root, "/work/tracedb/")
    with open(out / "requests.jsonl", "w") as f:
        for r in REQUESTS:
            f.write(scrub(json.dumps(r, sort_keys=True)) + "\n")
    runs = expected_runs(REQUESTS)
    (out / "expected-runs.json").write_text(scrub(json.dumps(runs, indent=2, sort_keys=True)) + "\n")
    feedback = expected_feedback(REQUESTS)
    (out / "expected-feedback.json").write_text(scrub(json.dumps(feedback, indent=2, sort_keys=True)) + "\n")
    print(f"langsmith {langsmith.__version__} ({mode}): {len(REQUESTS)} requests, {len(runs)} runs, "
          f"{len(feedback)} feedback → {out}")
    print("paths:", sorted({(r["method"], r["path"].split("?")[0]) for r in REQUESTS}))


if __name__ == "__main__":
    child() if "--child" in sys.argv else main()

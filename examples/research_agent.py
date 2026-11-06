"""A small LangGraph research agent for trying tracedb. It needs no API keys and makes no network
calls except its traces (a fake LLM; one tool fails with a 429 on purpose).

    pip install -r examples/requirements.txt
    LANGSMITH_TRACING=true LANGSMITH_ENDPOINT=http://localhost:7070 LANGSMITH_API_KEY=local \
      LANGSMITH_PROJECT=research-agent python examples/research_agent.py
"""

import time
from typing import TypedDict

from langchain_core.language_models.fake_chat_models import FakeListChatModel
from langchain_core.tracers.langchain import wait_for_all_tracers
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


@traceable(run_type="tool", name="python_exec")
def python_exec(code: str) -> str:
    return str(eval(code, {"__builtins__": {}}))


llm = FakeListChatModel(responses=["LLM papers in 2026 focus on agents and evaluation."])


def plan(s: State) -> State:
    return {"notes": [f"plan for {s['task']}"]}


def search(s: State) -> State:
    urls = web_search(s["task"])
    for u in urls[:2]:
        try:
            fetch_url(u)
        except RuntimeError:
            pass
    return {"notes": s["notes"] + urls + [python_exec("2 + 2")]}


def summarize(s: State) -> State:
    time.sleep(1.5)  # a slow step: its start shows in tracedb as running before it finishes
    return {"notes": s["notes"] + [llm.invoke("summarize: " + " ".join(s["notes"])).content]}


graph = StateGraph(State)
graph.add_node("plan", plan)
graph.add_node("search", search)
graph.add_node("summarize", summarize)
graph.set_entry_point("plan")
graph.add_edge("plan", "search")
graph.add_edge("search", "summarize")
graph.add_edge("summarize", END)

if __name__ == "__main__":
    out = graph.compile().invoke(
        {"task": "llm-papers"},
        config={"metadata": {"task_id": "t-117", "prompt_version": "v7"}, "tags": ["batch=example"]},
    )
    wait_for_all_tracers()
    print(out["notes"][-1])

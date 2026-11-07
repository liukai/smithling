import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { header, mockFetch, queryPage } from "../testutil";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

type Body = Record<string, unknown>;
const FIELDS = [
  { path: "status", types: [{ type: "string", count: 2000 }], complete: true },
  { path: "metadata.prompt_version", types: [{ type: "string", count: 1840 }], complete: true },
  { path: "metadata.code", types: [{ type: "number", count: 40 }, { type: "string", count: 3 }], complete: true },
  { path: "tags", types: [{ type: "list", count: 200 }], complete: true },
];
const VALUES: Record<string, { value: unknown; count: number }[]> = {
  "metadata.prompt_version": [{ value: "v6", count: 928 }, { value: "v7", count: 912 }],
  "metadata.code": [{ value: 429, count: 30 }],
  tags: [{ value: "batch=2026-11-05-a", count: 200 }],
};

const KEYS = [
  { key: "correctness", count: 2, avg: 0 }, { key: "helpfulness", count: 1, avg: null }, { key: "user rating", count: 1, avg: 1 },
];

function setup(path: string, feedback: { keys: unknown[]; orphans: number } = { keys: [], orphans: 0 }) {
  history.replaceState(null, "", path);
  const bodies: Body[] = [];
  const calls = mockFetch({
    "/api/feedback": () => ({ keys: feedback.keys }),
    "/healthz": () => ({ ok: true, orphan_feedback: feedback.orphans }),
    "/api/query": (_u, b) => { bodies.push(b); return queryPage([header(1)]); },
    "/api/projects": () => [{ name: "p", runs: 3 }],
    "/api/fields/values": (u) => {
      const prefix = u.searchParams.get("prefix") ?? "";
      const values = (VALUES[u.searchParams.get("path")!] ?? []).filter((v) => String(v.value).startsWith(prefix));
      return { values, other_count: prefix ? 0 : 5, approximate: false };
    },
    "/api/fields": (u) => {
      const prefix = u.searchParams.get("prefix") ?? "";
      return { fields: FIELDS.filter((f) => f.path.startsWith(prefix)), complete: true };
    },
  });
  render(<App />);
  return { bodies, calls };
}

const last = (bodies: Body[]) => bodies[bodies.length - 1];
const browser = () => screen.getByRole("region", { name: "fields" });
const fieldRows = () => within(browser()).getAllByRole("listitem").filter((li) => li.classList.contains("field"))
  .map((li) => li.querySelector(".field-path")!.textContent + " " + li.querySelector(".field-types")!.textContent);
const valueRows = () => [...browser().querySelectorAll(".value")].map((li) => li.textContent);
const chips = () => [...screen.getByRole("group", { name: "This run" }).querySelectorAll(".chip-label")].map((c) => c.textContent);

describe("field browser", () => {
  it("lists paths by count with their types, scoped to the project and time, and searches by prefix as you type", async () => {
    const { calls } = setup("/?project=p&time=12h");
    act(() => screen.getByRole("button", { name: "fields" }).click());
    await waitFor(() => expect(fieldRows()).toEqual([
      "status string 2,000", "metadata.prompt_version string 1,840", "metadata.code number 40 · string 3", "tags list 200",
    ]));
    expect(calls).toContain("/api/fields?project=p&time=12h");

    fireEvent.change(screen.getByLabelText("search fields"), { target: { value: "meta" } });
    await waitFor(() => expect(fieldRows()).toEqual(["metadata.prompt_version string 1,840", "metadata.code number 40 · string 3"]));
    expect(calls).toContain("/api/fields?project=p&time=12h&prefix=meta");
  });

  it("shows a path's values with counts, searches them, and adds a clicked value to This run", async () => {
    const { bodies, calls } = setup("/?project=p");
    act(() => screen.getByRole("button", { name: "fields" }).click());
    await waitFor(() => expect(fieldRows()).toHaveLength(4));

    act(() => screen.getByRole("button", { name: "metadata.prompt_version" }).click());
    await waitFor(() => expect(valueRows()).toEqual(["v6928", "v7912"]));
    expect(calls).toContain("/api/fields/values?project=p&path=metadata.prompt_version");
    expect(within(browser()).getByText("5 other")).toBeTruthy();

    fireEvent.change(screen.getByLabelText("search values"), { target: { value: "v7" } });
    await waitFor(() => expect(valueRows()).toEqual(["v7912"]));
    expect(calls).toContain("/api/fields/values?project=p&path=metadata.prompt_version&prefix=v7");

    act(() => within(browser()).getByRole("button", { name: "v7" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('eq(metadata.prompt_version, "v7")'));
    expect(chips()).toEqual(["metadata.prompt_version = v7"]);
  });

  it("has a feedback. section with keys, counts and orphans; a key adds exists(feedback.<key>)", async () => {
    const { bodies, calls } = setup("/?project=p&time=12h", { keys: KEYS, orphans: 1 });
    act(() => screen.getByRole("button", { name: "fields" }).click());
    const section = () => within(browser()).getByRole("region", { name: "feedback" });
    await waitFor(() => expect(section().textContent).toContain("(1 orphaned)"));
    const keyRows = [...section().querySelectorAll(".field")].map((li) => li.textContent);
    expect(keyRows).toEqual([
      "feedback.correctnessnumber 2 · avg 0.00", "feedback.helpfulnessvalue 1", "feedback.user ratingnumber 1 · avg 1.00",
    ]);
    expect(calls).toContain("/api/feedback?keys=true&project=p&time=12h");
    // Only keys with scores and a filterable name are clickable.
    expect(within(section()).getAllByRole("button").map((b) => b.textContent)).toEqual(["feedback.correctness"]);

    act(() => within(section()).getByRole("button", { name: "feedback.correctness" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe("exists(feedback.correctness)"));
  });

  it("shows no feedback section when no feedback is stored", async () => {
    const { calls } = setup("/");
    act(() => screen.getByRole("button", { name: "fields" }).click());
    await waitFor(() => expect(fieldRows()).toHaveLength(4));
    await waitFor(() => expect(calls.filter((c) => c.startsWith("/api/feedback") || c === "/healthz")).toHaveLength(2));
    await act(async () => {});
    expect(within(browser()).queryByRole("region", { name: "feedback" })).toBeNull();
  });

  it("filters list fields with has(), keeps numbers typed, and Alt-click excludes", async () => {
    const { bodies } = setup("/");
    act(() => screen.getByRole("button", { name: "fields" }).click());
    await waitFor(() => expect(fieldRows()).toHaveLength(4));

    act(() => screen.getByRole("button", { name: "tags" }).click());
    await waitFor(() => expect(valueRows()).toEqual(["batch=2026-11-05-a200"]));
    act(() => within(browser()).getByRole("button", { name: "batch=2026-11-05-a" }).click());
    await waitFor(() => expect(last(bodies).filter).toBe('has(tags, "batch=2026-11-05-a")'));

    act(() => screen.getByRole("button", { name: "metadata.code" }).click());
    await waitFor(() => expect(valueRows()).toEqual(["42930"]));
    fireEvent.click(within(browser()).getByRole("button", { name: "429" }), { altKey: true });
    await waitFor(() => expect(last(bodies).filter).toBe('and(has(tags, "batch=2026-11-05-a"), not(eq(metadata.code, 429)))'));
    expect(chips()).toEqual(["tags has batch=2026-11-05-a", "not metadata.code = 429"]);
  });
});

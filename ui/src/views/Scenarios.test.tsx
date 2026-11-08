import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { parse } from "../query/parse";
import { fromUrl } from "../query/url";
import { mockFetch } from "../testutil";
import { SCENARIOS, ScenariosPage } from "./Scenarios";

afterEach(() => vi.unstubAllGlobals());

describe("scenarios", () => {
  it("every filter the runs view would send parses", () => {
    for (const s of SCENARIOS.filter((s) => s.params)) {
      const q = fromUrl(new URLSearchParams(s.params).toString()).q;
      for (const t of [q.filter, q.tree, q.cmp].filter(Boolean) as string[]) {
        expect(parse(t).ok, `${s.title}: ${t}`).toBe(true);
      }
    }
  });

  it("opens a question as the runs view's URL", () => {
    render(<ScenariosPage />);
    const open = screen.getAllByRole("link", { name: "Open" })[4];
    const url = new URL(open.getAttribute("href")!, "http://localhost");
    expect(url.searchParams.get("root")).toBe("1");
    expect(url.searchParams.get("tree")).toBe('eq(status, "error")');
  });

  it("downloads a question's request and answer", async () => {
    const calls = mockFetch({ "/api/aggregate": () => ({ totals: {}, groups: [] }) });
    const click = vi.fn();
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(click);
    URL.createObjectURL = vi.fn(() => "blob:x");
    URL.revokeObjectURL = vi.fn();
    render(<ScenariosPage />);
    fireEvent.click(screen.getAllByRole("button", { name: "Download JSON" })[5]);
    await waitFor(() => expect(click).toHaveBeenCalled());
    expect(calls).toContain("/api/aggregate");
    const blob = (URL.createObjectURL as any).mock.calls[0][0] as Blob;
    const saved = JSON.parse(await blob.text());
    expect(saved.request.body.group_by).toEqual(["name"]);
    expect(saved.response.groups).toEqual([]);
  });
});

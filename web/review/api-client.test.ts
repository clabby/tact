import { expect, test } from "bun:test";
import { ApiClient } from "./api-client";
import type { ReviewPage } from "./protocol";

test("overview requests send trimmed custom instructions only when supplied", async () => {
  const originalFetch = globalThis.fetch;
  const requests: Record<string, unknown>[] = [];
  globalThis.fetch = async (_url, options) => {
    requests.push(JSON.parse(String(options?.body)) as Record<string, unknown>);
    return new Response(JSON.stringify({ overview_mdx: "# Overview" }), { status: 200 });
  };
  const page = {
    generation: 12,
    selected_range: { from: 0, to: 2 },
  } as ReviewPage;

  try {
    const api = new ApiClient();
    await api.overview(page, "  Focus on migrations.  ");
    await api.overview(page, "  \n  ");
    expect(requests).toEqual([
      { generation: 12, range: page.selected_range, instructions: "Focus on migrations." },
      { generation: 12, range: page.selected_range },
    ]);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

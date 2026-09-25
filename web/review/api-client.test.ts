import { afterEach, expect, test } from "bun:test";
import { ApiClient, ApiError } from "./api-client";
import type { ReviewPage } from "./protocol";

const originalFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = originalFetch; });

test("overview requests send trimmed custom instructions only when supplied", async () => {
  const requests: Record<string, unknown>[] = [];
  globalThis.fetch = async (_url, options) => {
    requests.push(JSON.parse(String(options?.body)) as Record<string, unknown>);
    return new Response(JSON.stringify({ overview_mdx: "# Overview" }), { status: 200 });
  };
  const page = {
    generation: 12,
    selected_range: { from: 0, to: 2 },
  } as ReviewPage;

  const api = new ApiClient();
  await api.overview(page, "  Focus on migrations.  ");
  await api.overview(page, "  \n  ");
  expect(requests).toEqual([
    { generation: 12, range: page.selected_range, instructions: "Focus on migrations." },
    { generation: 12, range: page.selected_range },
  ]);
});

test("an active turn response retains its code for the review banner", async () => {
  globalThis.fetch = async () => new Response(JSON.stringify({
    code: "turn_running",
    error: "The agent turn is still running.",
  }), { status: 409, headers: { "content-type": "application/json" } });

  try {
    await new ApiClient().status();
    throw new Error("Expected a turn_running response");
  } catch (error) {
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).code).toBe("turn_running");
  }
});

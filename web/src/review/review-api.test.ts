import { describe, expect, test } from "bun:test";
import { REVIEW_PROTOCOL_VERSION, type QuestionRequest, type ReviewPage, type ReviewSession } from "./protocol";
import { ProtocolMismatch, ReviewApi, errorCode, type ReviewTransport } from "./review-api";

type Call = { method: "get" | "post"; path: string; body?: unknown };

function fakeApi(reply: unknown = {}, checkout: string | null = null) {
  const calls: Call[] = [];
  const transport: ReviewTransport = {
    get: async (path) => { calls.push({ method: "get", path }); return reply as never; },
    post: async (path, body) => { calls.push({ method: "post", path, body }); return reply as never; },
  };
  return { api: new ReviewApi(transport, () => checkout), calls };
}

const page = { generation: 4, selected_range: { from: 0, to: 2 } } as ReviewPage;

describe("review requests are scoped to the session that owns them", () => {
  test("overview, AI review, and questions send the session", async () => {
    const { api, calls } = fakeApi();
    await api.overview("s1", page, " focus on tests ");
    await api.aiReview("s1", page);
    await api.questions("s1", 4);
    await api.cancelQuestion("s1", { operation_id: "op", generation: 4, range: page.selected_range });
    expect(calls.map((call) => [call.path, (call.body as { session: string }).session])).toEqual([
      ["overview", "s1"], ["ai-review", "s1"], ["questions", "s1"], ["question/cancel", "s1"],
    ]);
    expect(calls[0].body).toMatchObject({ generation: 4, instructions: "focus on tests" });
  });

  test("the diff itself is not session scoped", async () => {
    const { api, calls } = fakeApi();
    await api.loadRange(4, page.selected_range);
    expect(calls[0].body).toEqual({ generation: 4, range: page.selected_range });
  });
});

describe("reviewing another checkout", () => {
  test("every request names the checkout, and the snapshot names the session", async () => {
    const { api, calls } = fakeApi({ protocol_version: REVIEW_PROTOCOL_VERSION, markdown: "" }, "/src/tact-ws2");
    await api.review("s1");
    await api.loadRange(4, page.selected_range);
    await api.refresh(4);
    await api.overview("s1", page);
    await api.aiReview("s1", page);
    await api.question("s1", { thread_id: "t" } as QuestionRequest);
    await api.questions("s1", 4);
    await api.compose({ generation: 4, range: page.selected_range, decision: "approve", summary: "", comments: [] });
    expect(calls[0]).toEqual({ method: "get", path: "review?session=s1&checkout=%2Fsrc%2Ftact-ws2" });
    for (const call of calls.slice(1)) expect(call.body).toMatchObject({ checkout: "/src/tact-ws2" });
  });

  test("the session's own workspace is implied", async () => {
    const { api, calls } = fakeApi({ protocol_version: REVIEW_PROTOCOL_VERSION });
    await api.review(null);
    await api.refresh(4);
    expect(calls[0]!.path).toBe("review");
    expect(calls[1]!.body).toEqual({ generation: 4 });
  });
});

describe("sending a review to the chat", () => {
  test("composing posts the decision and returns the canonical markdown", async () => {
    const { api, calls } = fakeApi({ markdown: "Requested changes\n" });
    const decision = {
      generation: 4, range: page.selected_range, decision: "request_changes" as const,
      summary: "overall", comments: [{ path: "a.rs", side: "additions" as const, start_line: 1, end_line: 2, body: "fix" }],
    };
    expect(await api.compose(decision)).toBe("Requested changes\n");
    expect(calls).toEqual([{ method: "post", path: "review/compose", body: decision }]);
  });
});

describe("protocol checks", () => {
  test("an unsupported protocol version is rejected with a typed error", async () => {
    const { api } = fakeApi({ protocol_version: 99 });
    const failure = await api.review(null).catch((error) => error);
    expect(failure).toBeInstanceOf(ProtocolMismatch);
    expect(errorCode(failure)).toBe("invalid_response");
  });
});

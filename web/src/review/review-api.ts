import {
  REVIEW_PROTOCOL_VERSION,
  type AiReviewResponse,
  type OverviewResponse,
  type QuestionCancelRequest,
  type QuestionListResponse,
  type QuestionRequest,
  type QuestionResponse,
  type ReviewDecision,
  type ReviewPage,
  type ReviewSession,
} from "./protocol";
import type { ReviewRange } from "./range-selection";

/** The authenticated JSON transport the application shell provides; paths are relative to its API root. */
export type ReviewTransport = {
  get<T>(path: string, options?: { signal?: AbortSignal }): Promise<T>;
  post<T>(path: string, body: unknown, options?: { signal?: AbortSignal }): Promise<T>;
};

/** Raised for protocol violations detected on the client; server errors come from the transport. */
export class ProtocolMismatch extends Error {
  readonly code = "invalid_response";
}

/**
 * Review endpoints. Overviews, AI reviews, and question threads run in a chat session, so every
 * request that starts or reads them names the session; the diff and the snapshot (which carries
 * the active session's stored overview and questions) belong to the workspace.
 */
export class ReviewApi {
  constructor(private readonly transport: ReviewTransport) {}

  async review(signal?: AbortSignal): Promise<ReviewSession> {
    const review = await this.transport.get<ReviewSession>("review", { signal });
    if (review.protocol_version !== REVIEW_PROTOCOL_VERSION) {
      throw new ProtocolMismatch(
        `This review UI supports protocol ${REVIEW_PROTOCOL_VERSION}, but Tact returned ${review.protocol_version}.`,
      );
    }
    return review;
  }

  loadRange(generation: number, range: ReviewRange, signal?: AbortSignal): Promise<ReviewPage> {
    return this.transport.post("range", { generation, range }, { signal });
  }

  refresh(generation: number, signal?: AbortSignal): Promise<ReviewSession> {
    return this.transport.post("refresh", { generation }, { signal });
  }

  overview(
    session: string,
    page: ReviewPage,
    instructions?: string,
    signal?: AbortSignal,
  ): Promise<OverviewResponse> {
    return this.transport.post("overview", {
      session,
      generation: page.generation,
      range: page.selected_range,
      ...(instructions?.trim() ? { instructions: instructions.trim() } : {}),
    }, { signal });
  }

  aiReview(session: string, page: ReviewPage, signal?: AbortSignal): Promise<AiReviewResponse> {
    return this.transport.post("ai-review", {
      session,
      generation: page.generation,
      range: page.selected_range,
    }, { signal });
  }

  question(session: string, request: QuestionRequest, signal?: AbortSignal): Promise<QuestionResponse> {
    return this.transport.post("question", { ...request, session }, { signal });
  }

  questions(session: string, generation: number, signal?: AbortSignal): Promise<QuestionListResponse> {
    return this.transport.post("questions", { session, generation }, { signal });
  }

  async cancelQuestion(session: string, request: QuestionCancelRequest): Promise<void> {
    await this.transport.post("question/cancel", { ...request, session });
  }

  /** Returns the canonical markdown for the review, ready to be written into the chat draft. */
  async compose(decision: ReviewDecision): Promise<string> {
    const { markdown } = await this.transport.post<{ markdown: string }>("review/compose", decision);
    return markdown;
  }
}

export function errorCode(error: unknown): string | undefined {
  const code = (error as { code?: unknown } | null)?.code;
  return typeof code === "string" ? code : undefined;
}

export function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

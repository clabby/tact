import {
  REVIEW_PROTOCOL_VERSION,
  type OverviewResponse,
  type QuestionCancelRequest,
  type QuestionListResponse,
  type QuestionRequest,
  type QuestionResponse,
  type ReviewDecision,
  type ReviewErrorCode,
  type ReviewPage,
  type ReviewSession,
  type ReviewStatus,
} from "./protocol";
import type { ReviewRange } from "./range-selection";

type ErrorPayload = { code?: string; error?: string };

export class ApiError extends Error {
  constructor(
    readonly code: ReviewErrorCode,
    message: string,
    readonly status?: number,
  ) {
    super(message);
    this.name = "ApiError";
  }

  get retryable() {
    return this.code === "network_error"
      || this.code === "overview_failed"
      || this.code === "question_failed"
      || this.code === "agent_busy"
      || this.code === "operation_cancelled"
      || (this.status !== undefined && this.status >= 500);
  }
}

export class ApiClient {
  constructor(private readonly base = "./api") {}

  async review(signal?: AbortSignal): Promise<ReviewSession> {
    const session = await this.request<ReviewSession>("review", { signal });
    if (session.protocol_version !== REVIEW_PROTOCOL_VERSION) {
      throw new ApiError(
        "invalid_response",
        `This review UI supports protocol ${REVIEW_PROTOCOL_VERSION}, but Tact returned ${session.protocol_version}.`,
      );
    }
    return session;
  }

  status(signal?: AbortSignal): Promise<ReviewStatus> {
    return this.request("status", { cache: "no-store", signal });
  }

  loadRange(generation: number, range: ReviewRange, signal?: AbortSignal): Promise<ReviewPage> {
    return this.post("range", { generation, range }, signal);
  }

  refresh(generation: number, signal?: AbortSignal): Promise<ReviewSession> {
    return this.post("refresh", { generation }, signal);
  }

  overview(page: ReviewPage, signal?: AbortSignal): Promise<OverviewResponse> {
    return this.post("overview", {
      generation: page.generation,
      range: page.selected_range,
    }, signal);
  }

  question(request: QuestionRequest, signal?: AbortSignal): Promise<QuestionResponse> {
    return this.post("question", request, signal);
  }

  questions(generation: number, signal?: AbortSignal): Promise<QuestionListResponse> {
    return this.post("questions", { generation }, signal);
  }

  async cancelQuestion(request: QuestionCancelRequest): Promise<void> {
    await this.request("question/cancel", this.postOptions(request), false);
  }

  async submit(decision: ReviewDecision): Promise<void> {
    await this.request("decision", this.postOptions(decision), false);
  }

  async cancel(generation: number): Promise<void> {
    await this.request("cancel", this.postOptions({ generation }), false);
  }

  private post<T>(path: string, body: unknown, signal?: AbortSignal): Promise<T> {
    return this.request(path, { ...this.postOptions(body), signal });
  }

  private postOptions(body: unknown): RequestInit {
    return {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    };
  }

  private async request<T>(
    path: string,
    options?: RequestInit,
    expectsJson = true,
  ): Promise<T> {
    let response: Response;
    try {
      response = await fetch(`${this.base}/${path}`, options);
    } catch (error) {
      throw new ApiError("network_error", errorMessage(error));
    }

    if (!response.ok) throw await responseError(response);
    if (!expectsJson) return undefined as T;

    try {
      return await response.json() as T;
    } catch {
      throw new ApiError("invalid_response", `Tact returned an invalid ${path} response.`, response.status);
    }
  }
}

async function responseError(response: Response): Promise<ApiError> {
  let payload: ErrorPayload = {};
  try {
    payload = await response.json() as ErrorPayload;
  } catch {
    // The status still gives the user a recoverable error when an older server has no JSON body.
  }
  const code = isErrorCode(payload.code) ? payload.code : "unknown";
  const message = payload.error?.trim() || `Tact returned HTTP ${response.status}.`;
  return new ApiError(code, message, response.status);
}

function isErrorCode(value: string | undefined): value is ReviewErrorCode {
  return [
    "stale_snapshot", "invalid_range", "workspace_changed", "overview_failed",
    "question_failed", "invalid_thread", "agent_busy", "operation_cancelled",
    "session_cancelled", "invalid_comment_anchor",
  ].includes(value ?? "");
}

export function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

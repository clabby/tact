import type {
  CommandName,
  CommandReplies,
  Commands,
  Instance,
  Queries,
  QueryName,
  SiblingInstance,
  ToolDetail,
  WireEntry,
} from "./wire";

type ArgsOf<Args> = [Args] extends [undefined] ? [] : [Args];

/** Server error codes (see `docs/web.md` and `protocol.ts`) plus the client-side failures. */
export type ApiErrorCode = string;

type ErrorPayload = { code?: unknown; message?: unknown; error?: unknown };

type RequestOptions = { signal?: AbortSignal };

export class ApiError extends Error {
  constructor(
    readonly code: ApiErrorCode,
    message: string,
    readonly status?: number,
  ) {
    super(message);
    this.name = "ApiError";
  }

  get retryable() {
    return this.code === "network_error"
      || this.code === "overview_failed"
      || this.code === "ai_review_failed"
      || this.code === "question_failed"
      || this.code === "agent_busy"
      || this.code === "operation_cancelled"
      || (this.status !== undefined && this.status >= 500);
  }
}

/**
 * The one HTTP client of the web app. Every request carries the session cookie; every POST carries
 * the `X-Tact` header the server requires of mutating requests, and every command carries this
 * tab's `client` id so the tab can recognise the echo of its own writes.
 */
export class ApiClient {
  /** A random per-tab id, below 2^53 so it survives JSON number round trips. */
  readonly client: number;

  constructor(private readonly base = "./api", client?: number) {
    this.client = client ?? randomClientId();
  }

  /** The `origin` the server attaches to draft events caused by this tab. */
  get origin() {
    return `web:${this.client}`;
  }

  get<T>(path: string, options: RequestOptions = {}): Promise<T> {
    return this.request(path, { cache: "no-store", signal: options.signal });
  }

  post<T>(path: string, body: unknown, options: RequestOptions = {}): Promise<T> {
    return this.request(path, {
      method: "POST",
      headers: { "content-type": "application/json", "x-tact": "1" },
      body: JSON.stringify(body),
      signal: options.signal,
    });
  }

  async login(token: string): Promise<void> {
    await this.post("login", { token });
  }

  /** Sends one command through the generic command route (`bridge::Command`). */
  command<Name extends CommandName>(name: Name, ...[args]: ArgsOf<Commands[Name]>): Promise<CommandReplies[Name]> {
    return this.post("cmd", args === undefined ? { client: this.client, cmd: name } : { client: this.client, cmd: name, args });
  }

  /** Reads data through the generic query route (`bridge::Query`). */
  query<Name extends QueryName>(name: Name, ...[args]: ArgsOf<Queries[Name]["args"]>): Promise<Queries[Name]["reply"]> {
    return this.post("query", args === undefined ? { query: name } : { query: name, args });
  }

  instance(options?: RequestOptions) {
    return this.get<Instance>("instance", options);
  }

  /** What a sign-in link for another device needs: the configured public origin and the token. */
  link(options?: RequestOptions) {
    return this.get<{ public_origin: string | null; token: string }>("link", options);
  }

  instances(options?: RequestOptions) {
    return this.get<{ instances: SiblingInstance[] }>("instances", options);
  }

  /** One entry's full tool detail; `agent` addresses a subagent's transcript. */
  toolDetail(session: string, entry: number, agent?: number, options?: RequestOptions) {
    return this.get<ToolDetail>(`${sessionPath(session, agent)}/entries/${entry}`, options);
  }

  /** The URL of the n-th image attached to a user entry. */
  imageUrl(session: string, entry: number, index: number) {
    return `${this.base}/${sessionPath(session)}/entries/${entry}/images/${index}`;
  }

  agentEntries(session: string, agent: number, options?: RequestOptions) {
    return this.get<{ entries: WireEntry[] }>(`${sessionPath(session, agent)}/entries`, options);
  }

  private async request<T>(path: string, init: RequestInit): Promise<T> {
    let response: Response;
    try {
      response = await fetch(`${this.base}/${path}`, { ...init, credentials: "same-origin" });
    } catch (error) {
      if (init.signal?.aborted) throw error;
      throw new ApiError("network_error", errorMessage(error));
    }

    if (!response.ok) throw await responseError(response);
    const body = await response.text();
    if (!body) return {} as T;
    try {
      return JSON.parse(body) as T;
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
    // The status alone still yields a usable error when the body is not JSON.
  }
  const code = typeof payload.code === "string" && payload.code
    ? payload.code
    : response.status === 401 ? "unauthorized" : "unknown";
  const text = [payload.message, payload.error].find((value) => typeof value === "string" && value.trim());
  return new ApiError(code, (text as string | undefined)?.trim() || `Tact returned HTTP ${response.status}.`, response.status);
}

function sessionPath(session: string, agent?: number) {
  const base = `sessions/${encodeURIComponent(session)}`;
  return agent === undefined ? base : `${base}/agents/${agent}`;
}

function randomClientId() {
  const [high, low] = crypto.getRandomValues(new Uint32Array(2));
  return (high! & 0x1f_ffff) * 0x1_0000_0000 + low!;
}

export function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

const REFUSALS: Record<string, string> = {
  draft_changed: "The draft changed in another window. Check it and send again.",
  not_available_remotely: "That only works in the terminal.",
  nothing_running: "Nothing is running.",
  turn_running: "Wait for the current turn to finish, or force it.",
  queue_not_empty: "Clear the queue first.",
  unknown_session: "That session is no longer live.",
  too_many_sessions: "Too many live sessions. Close one first.",
  session_locked: "That session is open in another Tact.",
  stale: "It changed since you opened it. Reload and try again.",
  network_error: "Tact is unreachable. Check the connection and try again.",
  unauthorized: "This browser is no longer signed in.",
};

/** A short sentence for an inline error, preferring a known refusal over the server's wording. */
export function describeError(error: unknown) {
  if (error instanceof ApiError) return REFUSALS[error.code] ?? error.message;
  return errorMessage(error);
}

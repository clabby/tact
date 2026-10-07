import { realTimers, type Timers } from "./draft";
import type { Connection } from "./store";
import { STREAM_EVENTS, type StreamEvent } from "./wire";

/** The part of `EventSource` the client uses, so tests can drive it. */
export type EventSourceLike = {
  onopen: ((event: Event) => void) | null;
  onerror: ((event: Event) => void) | null;
  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void;
  close(): void;
};

export type StreamClientOptions = {
  url: string;
  connect(url: string): EventSourceLike;
  onEvent(event: StreamEvent): void;
  onConnection(connection: Connection): void;
  /** Distinguishes a revoked login from a transient failure after the stream drops. */
  authorized(): Promise<boolean>;
  timers?: Timers;
  random?: () => number;
};

const BASE_DELAY_MS = 500;
const MAX_DELAY_MS = 15_000;

/**
 * One SSE connection with reconnection under client control. The browser's built-in retry is
 * replaced by exponential backoff with jitter so a down server is not hammered, and every drop
 * checks whether the login is still valid so a revoked token locks the app instead of retrying
 * forever. Each reconnect receives a fresh hello/live/active/snapshot sequence; nothing is
 * replayed.
 */
export class StreamClient {
  private source: EventSourceLike | null = null;
  private timer: unknown = null;
  private attempt = 0;
  private stopped = true;
  private readonly timers: Timers;
  private readonly random: () => number;

  constructor(private readonly options: StreamClientOptions) {
    this.timers = options.timers ?? realTimers;
    this.random = options.random ?? Math.random;
  }

  start() {
    this.stopped = false;
    this.open("connecting");
  }

  stop() {
    this.stopped = true;
    this.clearTimer();
    this.source?.close();
    this.source = null;
  }

  /** Skips the remaining backoff, e.g. when the page becomes visible or the network returns. */
  reconnectNow() {
    if (this.stopped || this.source) return;
    this.clearTimer();
    this.open("reconnecting");
  }

  private open(connection: Connection) {
    this.options.onConnection(connection);
    const source = this.options.connect(this.options.url);
    this.source = source;
    for (const type of STREAM_EVENTS) {
      source.addEventListener(type, (message) => {
        if (this.source !== source) return;
        let data: unknown;
        try {
          data = JSON.parse(message.data);
        } catch {
          return;
        }
        if (type === "hello") this.attempt = 0;
        this.options.onEvent({ type, data } as StreamEvent);
      });
    }
    source.onerror = () => {
      if (this.source !== source) return;
      source.close();
      this.source = null;
      void this.recover();
    };
  }

  private async recover() {
    if (this.stopped) return;
    this.options.onConnection("reconnecting");
    let authorized = true;
    try {
      authorized = await this.options.authorized();
    } catch {
      // An unreachable server is a transient failure; keep retrying.
    }
    if (this.stopped) return;
    if (!authorized) {
      this.stopped = true;
      this.options.onConnection("locked");
      return;
    }
    const ceiling = Math.min(MAX_DELAY_MS, BASE_DELAY_MS * 2 ** this.attempt);
    this.attempt += 1;
    const delay = ceiling / 2 + this.random() * (ceiling / 2);
    this.timer = this.timers.set(() => {
      this.timer = null;
      if (!this.stopped) this.open("reconnecting");
    }, delay);
  }

  private clearTimer() {
    if (this.timer === null) return;
    this.timers.clear(this.timer);
    this.timer = null;
  }
}

import type { Message } from "./api";

export type SseMessageHandler = (msg: Message) => void;

/**
 * Connect to /events/{instance_id}?token={token}
 * On message: parse JSON from SSE data line
 * On error/close: reconnect with exponential backoff (1s, 2s, 4s… max 30s)
 */
export class CollabSse {
  private abort: AbortController | null = null;
  private loopPromise: Promise<void> | null = null;
  private backoffMs = 1000;
  private readonly maxBackoffMs = 30_000;
  private closed = false;

  constructor(
    private readonly server: string,
    private readonly token: string,
    private readonly instanceId: string,
    private readonly onMessage: SseMessageHandler,
    private readonly onConnectionChange?: (connected: boolean) => void
  ) {}

  start(): void {
    this.closed = false;
    this.backoffMs = 1000;
    this.loopPromise = this.runLoop();
  }

  private url(): string {
    const base = this.server.replace(/\/$/, "");
    const tokenQ = encodeURIComponent(this.token);
    return `${base}/events/${encodeURIComponent(this.instanceId)}?token=${tokenQ}`;
  }

  private async sleep(ms: number): Promise<void> {
    await new Promise<void>((r) => setTimeout(r, ms));
  }

  private async runLoop(): Promise<void> {
    while (!this.closed) {
      this.abort = new AbortController();
      try {
        const res = await fetch(this.url(), { signal: this.abort.signal });
        if (!res.ok) {
          throw new Error(`SSE HTTP ${res.status}`);
        }
        if (!res.body) {
          throw new Error("SSE: no body");
        }
        this.backoffMs = 1000;
        this.onConnectionChange?.(true);

        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let carry = "";
        while (!this.closed) {
          const { done, value } = await reader.read();
          if (done) {
            break;
          }
          carry += decoder.decode(value, { stream: true });
          const segments = carry.split(/\r?\n/);
          carry = segments.pop() ?? "";
          for (const line of segments) {
            if (line.startsWith("data:")) {
              const payload = line.slice(5).trimStart();
              if (payload.length === 0) {
                continue;
              }
              try {
                const msg = JSON.parse(payload) as Message;
                this.onMessage(msg);
              } catch {
                // ignore non-JSON
              }
            }
          }
        }
      } catch {
        if (this.closed) {
          break;
        }
        this.onConnectionChange?.(false);
        const wait = this.backoffMs;
        this.backoffMs = Math.min(this.backoffMs * 2, this.maxBackoffMs);
        await this.sleep(wait);
        continue;
      }
      if (this.closed) {
        break;
      }
      this.onConnectionChange?.(false);
      const wait = this.backoffMs;
      this.backoffMs = Math.min(this.backoffMs * 2, this.maxBackoffMs);
      await this.sleep(wait);
    }
    this.onConnectionChange?.(false);
  }

  dispose(): void {
    this.closed = true;
    this.abort?.abort();
    this.abort = null;
    void this.loopPromise;
  }
}

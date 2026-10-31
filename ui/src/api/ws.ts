import type { HubWsFrame } from "../types/api";

export type WsFrameHandler<T = unknown> = (frame: HubWsFrame<T>) => void;
export type WsStatusHandler = (connected: boolean) => void;

export interface HubWsClientOptions {
  url?: string;
  token?: string | null;
  webSocketFactory?: (url: string) => WebSocket;
  autoReconnect?: boolean;
  reconnectDelayMs?: number;
  /** Upper bound for the exponential reconnect backoff. Defaults to 30s. */
  maxReconnectDelayMs?: number;
}

export interface SendFrameOptions {
  /**
   * When the socket is not open, keep the frame and replay it after the next
   * connect (default). Pass `false` for frames that are only meaningful on the
   * live connection (interactive terminal input, resize, close): replaying
   * keystrokes into a later shell is unsafe.
   */
  queueWhenDisconnected?: boolean;
}

/** Cap on frames buffered while disconnected so a long outage cannot grow without bound. */
export const MAX_PENDING_FRAMES = 256;

export class HubWsClient {
  private readonly baseUrl?: string;
  private token: string | null;
  private readonly webSocketFactory: (url: string) => WebSocket;
  private readonly autoReconnect: boolean;
  private readonly reconnectDelayMs: number;
  private readonly maxReconnectDelayMs: number;
  private reconnectAttempts = 0;

  private ws: WebSocket | null = null;
  private connected = false;
  private manualClose = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private pendingFrames: string[] = [];

  private readonly channelSubscribers = new Map<string, Set<WsFrameHandler>>();
  private readonly statusSubscribers = new Set<WsStatusHandler>();

  constructor(options: HubWsClientOptions = {}) {
    this.baseUrl = options.url;
    this.token = options.token ?? null;
    this.webSocketFactory = options.webSocketFactory ?? ((url: string) => new WebSocket(url));
    this.autoReconnect = options.autoReconnect ?? true;
    this.reconnectDelayMs = options.reconnectDelayMs ?? 1500;
    this.maxReconnectDelayMs = Math.max(
      this.reconnectDelayMs,
      options.maxReconnectDelayMs ?? 30_000,
    );
  }

  public setToken(token: string | null): void {
    const next = token && token.trim().length > 0 ? token.trim() : null;
    if (next !== this.token) {
      // Frames queued under another identity must never be replayed under this one.
      this.pendingFrames = [];
    }
    this.token = next;
  }

  public isConnected(): boolean {
    return this.connected;
  }

  public buildWsUrl(): string {
    let raw = this.baseUrl;
    if (!raw) {
      if (typeof window !== "undefined" && window.location) {
        const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
        raw = `${proto}//${window.location.host}/ws/v1`;
      } else {
        raw = "ws://127.0.0.1:8420/ws/v1";
      }
    }
    if (this.token) {
      const sep = raw.includes("?") ? "&" : "?";
      return `${raw}${sep}token=${encodeURIComponent(this.token)}`;
    }
    return raw;
  }

  public connect(): void {
    this.manualClose = false;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      const previous = this.ws;
      this.ws = null;
      try {
        previous.close();
      } catch {
        // Ignore close errors.
      }
    }

    const url = this.buildWsUrl();
    const socket = this.webSocketFactory(url);
    this.ws = socket;

    // A superseded socket (replaced by a later connect() or by disconnect())
    // can still deliver open/message/close events. Every handler must ignore
    // them, otherwise a stale close schedules a duplicate reconnect that tears
    // down the live socket, and stale frames are dispatched twice.
    const isCurrent = () => this.ws === socket;

    socket.onopen = () => {
      if (!isCurrent()) {
        return;
      }
      this.reconnectAttempts = 0;
      this.setConnected(true);
      this.flushPendingFrames();
    };

    socket.onmessage = (event: MessageEvent) => {
      if (!isCurrent()) {
        return;
      }
      const rawData = typeof event.data === "string" ? event.data : String(event.data ?? "");
      if (!rawData) {
        return;
      }
      let frame: HubWsFrame | null = null;
      try {
        const parsed = JSON.parse(rawData) as HubWsFrame;
        if (parsed && typeof parsed.channel === "string" && typeof parsed.event === "string") {
          frame = parsed;
        }
      } catch {
        // Ignore malformed frames.
      }
      if (frame) {
        this.dispatchFrame(frame);
      }
    };

    socket.onerror = () => {
      // Handled by onclose.
    };

    socket.onclose = () => {
      if (!isCurrent()) {
        return;
      }
      this.ws = null;
      this.setConnected(false);
      if (!this.manualClose && this.autoReconnect) {
        const delay = Math.min(
          this.maxReconnectDelayMs,
          this.reconnectDelayMs * 2 ** Math.min(this.reconnectAttempts, 16),
        );
        this.reconnectAttempts += 1;
        this.reconnectTimer = setTimeout(() => {
          this.reconnectTimer = null;
          if (!this.manualClose) {
            this.connect();
          }
        }, delay);
      }
    };
  }

  public disconnect(): void {
    this.manualClose = true;
    this.reconnectAttempts = 0;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      const sock = this.ws;
      this.ws = null;
      try {
        sock.close();
      } catch {
        // Ignore close errors.
      }
    }
    this.setConnected(false);
  }

  public subscribe<T = unknown>(channel: string, handler: WsFrameHandler<T>): () => void {
    const key = channel.trim();
    let set = this.channelSubscribers.get(key);
    if (!set) {
      set = new Set();
      this.channelSubscribers.set(key, set);
    }
    const wrapped = handler as WsFrameHandler;
    set.add(wrapped);
    return () => {
      const current = this.channelSubscribers.get(key);
      if (current) {
        current.delete(wrapped);
        if (current.size === 0) {
          this.channelSubscribers.delete(key);
        }
      }
    };
  }

  public onStatusChange(handler: WsStatusHandler): () => void {
    this.statusSubscribers.add(handler);
    return () => {
      this.statusSubscribers.delete(handler);
    };
  }

  public sendFrame<T = unknown>(
    channel: string,
    event: string,
    data: T,
    options: SendFrameOptions = {},
  ): boolean {
    const payload = JSON.stringify({
      channel,
      event,
      data,
    } satisfies HubWsFrame<T>);

    if (this.ws && this.ws.readyState === 1 /* WebSocket.OPEN */) {
      this.ws.send(payload);
      return true;
    }

    if (options.queueWhenDisconnected !== false) {
      this.pendingFrames.push(payload);
      if (this.pendingFrames.length > MAX_PENDING_FRAMES) {
        this.pendingFrames.splice(0, this.pendingFrames.length - MAX_PENDING_FRAMES);
      }
    }
    return false;
  }

  private flushPendingFrames(): void {
    if (!this.ws || this.ws.readyState !== 1 /* WebSocket.OPEN */) {
      return;
    }
    while (this.pendingFrames.length > 0) {
      const next = this.pendingFrames.shift();
      if (next !== undefined) {
        this.ws.send(next);
      }
    }
  }

  private dispatchFrame(frame: HubWsFrame): void {
    if (frame.channel === "system" && frame.event === "connected") {
      this.setConnected(true);
      this.flushPendingFrames();
    }

    // One misbehaving subscriber must not starve the others.
    const direct = this.channelSubscribers.get(frame.channel);
    if (direct) {
      for (const cb of Array.from(direct)) {
        this.safely(() => cb(frame));
      }
    }

    const wildcard = this.channelSubscribers.get("*");
    if (wildcard) {
      for (const cb of Array.from(wildcard)) {
        this.safely(() => cb(frame));
      }
    }
  }

  private setConnected(state: boolean): void {
    if (this.connected === state) {
      return;
    }
    this.connected = state;
    for (const cb of Array.from(this.statusSubscribers)) {
      this.safely(() => cb(state));
    }
  }

  private safely(fn: () => void): void {
    try {
      fn();
    } catch (err) {
      console.error("HubWsClient subscriber threw", err);
    }
  }
}

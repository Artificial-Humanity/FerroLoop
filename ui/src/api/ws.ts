import type { HubWsFrame } from "../types/api";

export type WsFrameHandler<T = unknown> = (frame: HubWsFrame<T>) => void;
export type WsStatusHandler = (connected: boolean) => void;

export interface HubWsClientOptions {
  url?: string;
  token?: string | null;
  webSocketFactory?: (url: string) => WebSocket;
  autoReconnect?: boolean;
  reconnectDelayMs?: number;
}

export class HubWsClient {
  private readonly baseUrl?: string;
  private token: string | null;
  private readonly webSocketFactory: (url: string) => WebSocket;
  private readonly autoReconnect: boolean;
  private readonly reconnectDelayMs: number;

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
  }

  public setToken(token: string | null): void {
    this.token = token && token.trim().length > 0 ? token.trim() : null;
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
      try {
        this.ws.close();
      } catch {
        // Ignore close errors.
      }
      this.ws = null;
    }

    const url = this.buildWsUrl();
    const socket = this.webSocketFactory(url);
    this.ws = socket;

    socket.onopen = () => {
      this.setConnected(true);
      this.flushPendingFrames();
    };

    socket.onmessage = (event: MessageEvent) => {
      const rawData = typeof event.data === "string" ? event.data : String(event.data ?? "");
      if (!rawData) {
        return;
      }
      try {
        const frame = JSON.parse(rawData) as HubWsFrame;
        if (frame && typeof frame.channel === "string" && typeof frame.event === "string") {
          this.dispatchFrame(frame);
        }
      } catch {
        // Ignore malformed frames.
      }
    };

    socket.onerror = () => {
      // Handled by onclose.
    };

    socket.onclose = () => {
      if (this.ws === socket) {
        this.ws = null;
      }
      this.setConnected(false);
      if (!this.manualClose && this.autoReconnect) {
        this.reconnectTimer = setTimeout(() => {
          this.reconnectTimer = null;
          if (!this.manualClose) {
            this.connect();
          }
        }, this.reconnectDelayMs);
      }
    };
  }

  public disconnect(): void {
    this.manualClose = true;
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

  public sendFrame<T = unknown>(channel: string, event: string, data: T): boolean {
    const payload = JSON.stringify({
      channel,
      event,
      data,
    } satisfies HubWsFrame<T>);

    if (this.ws && this.ws.readyState === 1 /* WebSocket.OPEN */) {
      this.ws.send(payload);
      return true;
    }

    this.pendingFrames.push(payload);
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

    const direct = this.channelSubscribers.get(frame.channel);
    if (direct) {
      for (const cb of Array.from(direct)) {
        cb(frame);
      }
    }

    const wildcard = this.channelSubscribers.get("*");
    if (wildcard) {
      for (const cb of Array.from(wildcard)) {
        cb(frame);
      }
    }
  }

  private setConnected(state: boolean): void {
    if (this.connected === state) {
      return;
    }
    this.connected = state;
    for (const cb of Array.from(this.statusSubscribers)) {
      cb(state);
    }
  }
}

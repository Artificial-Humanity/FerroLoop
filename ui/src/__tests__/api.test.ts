import { beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient, HubApiError } from "../api/client";
import { HubWsClient } from "../api/ws";
import { DEFAULT_WORKSPACE_AGENTS, resolveLivenessState } from "../types/api";

describe("HubApiClient", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("exchanges a pairing code via POST /api/v1/auth/pair and persists session_token", async () => {
    const mockFetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/api/v1/auth/pair")) {
        expect(init?.method).toBe("POST");
        const body = JSON.parse(String(init?.body));
        expect(body).toEqual({ code: "pair-12345" });
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
            session_token: "sess-tok-abc",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.endsWith("/api/v1/agents")) {
        const headers = new Headers(init?.headers);
        expect(headers.get("Authorization")).toBe("Bearer sess-tok-abc");
        return new Response(
          JSON.stringify({
            agents: DEFAULT_WORKSPACE_AGENTS,
            areas: [],
            routes: [],
          }),
          { status: 200 },
        );
      }
      return new Response(JSON.stringify({ error: "not found" }), { status: 404 });
    });

    const client = new HubApiClient({
      baseUrl: "http://127.0.0.1:8420",
      fetchImpl: mockFetch as unknown as typeof fetch,
    });

    const pairRes = await client.pair({ code: "pair-12345" });
    expect(pairRes.authenticated).toBe(true);
    expect(pairRes.authority).toEqual({ tier: "owner", login: "lmcfarlin" });
    expect(client.getToken()).toBe("sess-tok-abc");

    const agentsRes = await client.getAgents();
    expect(agentsRes.agents.length).toBeGreaterThanOrEqual(7);
  });

  it("URL-encodes session IDs and worktree IDs and surfaces HubApiError on non-200 responses", async () => {
    const seenUrls: string[] = [];
    const mockFetch = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      seenUrls.push(url);
      if (url.includes("/transcript")) {
        return new Response(
          JSON.stringify({
            session_id: "urn:uuid:01965500-0000-7000-8000-000000000001",
            turns: [{ index: 0, role: "assistant", content: "Ready", tool_calls: [] }],
          }),
          { status: 200 },
        );
      }
      return new Response(
        JSON.stringify({
          error: "unauthorized",
          detail: "pairing code has expired",
        }),
        { status: 401 },
      );
    });

    const client = new HubApiClient({
      fetchImpl: mockFetch as unknown as typeof fetch,
    });

    const transcript = await client.getSessionTranscript(
      "urn:uuid:01965500-0000-7000-8000-000000000001",
    );
    expect(transcript.turns).toHaveLength(1);
    expect(seenUrls[0]).toBe(
      "/api/v1/sessions/urn%3Auuid%3A01965500-0000-7000-8000-000000000001/transcript",
    );

    await expect(client.pair({ code: "expired" })).rejects.toThrowError(HubApiError);
    await expect(client.pair({ code: "expired" })).rejects.toMatchObject({
      status: 401,
      message: "pairing code has expired",
    });
  });

  it("calls the FerroGate vault and connector endpoints with encoded secret names", async () => {
    const calls: Array<{ url: string; method: string; body?: unknown }> = [];
    const mockFetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      const body = init?.body ? JSON.parse(String(init.body)) : undefined;
      calls.push({ url, method, body });

      if (url.endsWith("/api/v1/gate/secrets") && method === "GET") {
        return new Response(
          JSON.stringify({
            secrets: [
              {
                name: "DISCORD_BOT_TOKEN",
                allowed_hosts: ["discord.com"],
                allowed_methods: ["GET"],
                allowed_agents: ["*"],
                allow_private_network: false,
                created_at: "2026-10-27T04:00:00Z",
                updated_at: "2026-10-27T04:00:00Z",
              },
            ],
          }),
          { status: 200 },
        );
      }
      if (url.endsWith("/api/v1/gate/secrets") && method === "POST") {
        return new Response(
          JSON.stringify({ secret: { name: body.name } }),
          { status: 200 },
        );
      }
      if (url.includes("/api/v1/gate/secrets/") && method === "DELETE") {
        return new Response(
          JSON.stringify({ removed: true, name: "DISCORD_BOT_TOKEN" }),
          { status: 200 },
        );
      }
      if (url.endsWith("/api/v1/gate/connectors")) {
        return new Response(
          JSON.stringify({
            connectors: [
              {
                id: "discord",
                title: "Discord",
                version: "0.1.0",
                description: "",
                base_url: "https://discord.com/api/v10",
                allowed_hosts: ["discord.com"],
                tools: [],
                bridges: [],
              },
            ],
          }),
          { status: 200 },
        );
      }
      return new Response(JSON.stringify({ error: "not found" }), { status: 404 });
    });

    const client = new HubApiClient({
      fetchImpl: mockFetch as unknown as typeof fetch,
    });

    const secrets = await client.listGateSecrets();
    expect(secrets.secrets[0].name).toBe("DISCORD_BOT_TOKEN");
    // Metadata payloads never carry a plaintext `value` field.
    expect(secrets.secrets[0]).not.toHaveProperty("value");

    await client.setGateSecret({
      name: "DISCORD_BOT_TOKEN",
      value: "tok",
      allowed_hosts: ["discord.com"],
      allowed_methods: ["GET", "POST"],
      allowed_agents: ["*"],
    });

    const deleted = await client.deleteGateSecret("DISCORD_BOT_TOKEN");
    expect(deleted).toEqual({ removed: true, name: "DISCORD_BOT_TOKEN" });

    const connectors = await client.listGateConnectors();
    expect(connectors.connectors[0].title).toBe("Discord");

    expect(calls).toEqual([
      { url: "/api/v1/gate/secrets", method: "GET", body: undefined },
      {
        url: "/api/v1/gate/secrets",
        method: "POST",
        body: {
          name: "DISCORD_BOT_TOKEN",
          value: "tok",
          allowed_hosts: ["discord.com"],
          allowed_methods: ["GET", "POST"],
          allowed_agents: ["*"],
        },
      },
      { url: "/api/v1/gate/secrets/DISCORD_BOT_TOKEN", method: "DELETE", body: undefined },
      { url: "/api/v1/gate/connectors", method: "GET", body: undefined },
    ]);
  });

  it("normalizes liveness verdicts across fl-core status and UI state strings", () => {
    expect(resolveLivenessState({ status: "live", pid: 1234 }, false)).toBe("alive");
    expect(resolveLivenessState({ state: "alive" }, false)).toBe("alive");
    expect(
      resolveLivenessState({ status: "unverifiable", reason: "stale_status" }, false),
    ).toBe("stale");
    expect(
      resolveLivenessState({ status: "unverifiable", reason: "missing_status" }, true),
    ).toBe("alive");
    expect(resolveLivenessState({ status: "exited", outcome: "completed" }, false)).toBe(
      "dead",
    );
  });
});

describe("HubWsClient", () => {
  it("connects with bearer token query param, routes frames by channel, and sends frames", () => {
    let createdUrl = "";
    const sentPayloads: string[] = [];
    let mockSocketInstance: {
      readyState: number;
      onopen: (() => void) | null;
      onmessage: ((ev: MessageEvent) => void) | null;
      onclose: (() => void) | null;
      onerror: (() => void) | null;
      send: (data: string) => void;
      close: () => void;
    } | null = null;

    const wsClient = new HubWsClient({
      url: "ws://127.0.0.1:8420/ws/v1",
      token: "tok-xyz",
      autoReconnect: false,
      webSocketFactory: (url: string) => {
        createdUrl = url;
        mockSocketInstance = {
          readyState: 1,
          onopen: null,
          onmessage: null,
          onclose: null,
          onerror: null,
          send: (data: string) => {
            sentPayloads.push(data);
          },
          close: () => {},
        };
        return mockSocketInstance as unknown as WebSocket;
      },
    });

    const wireFrames: unknown[] = [];
    const terminalFrames: unknown[] = [];
    const statuses: boolean[] = [];

    const unsubWire = wsClient.subscribe("wire", (f) => wireFrames.push(f));
    wsClient.subscribe("terminal", (f) => terminalFrames.push(f));
    wsClient.onStatusChange((c) => statuses.push(c));

    // Queue a frame before connect opens
    expect(wsClient.sendFrame("terminal", "open", { session_id: "t1" })).toBe(false);

    wsClient.connect();
    expect(createdUrl).toBe("ws://127.0.0.1:8420/ws/v1?token=tok-xyz");
    expect(mockSocketInstance).not.toBeNull();

    // Trigger onopen -> flushes queued frame
    mockSocketInstance!.onopen?.();
    expect(wsClient.isConnected()).toBe(true);
    expect(statuses).toEqual([true]);
    expect(sentPayloads).toHaveLength(1);
    expect(JSON.parse(sentPayloads[0])).toEqual({
      channel: "terminal",
      event: "open",
      data: { session_id: "t1" },
    });

    // Receive wire & terminal frames
    mockSocketInstance!.onmessage?.({
      data: JSON.stringify({
        channel: "wire",
        event: "message_sent",
        data: { id: "m1" },
      }),
    } as MessageEvent);
    mockSocketInstance!.onmessage?.({
      data: JSON.stringify({
        channel: "terminal",
        event: "output",
        data: { session_id: "t1", stream: "stdout", data: "hello\n" },
      }),
    } as MessageEvent);

    expect(wireFrames).toHaveLength(1);
    expect(terminalFrames).toHaveLength(1);

    unsubWire();
    mockSocketInstance!.onmessage?.({
      data: JSON.stringify({
        channel: "wire",
        event: "message_acked",
        data: { id: "m1" },
      }),
    } as MessageEvent);
    expect(wireFrames).toHaveLength(1);

    wsClient.disconnect();
    expect(wsClient.isConnected()).toBe(false);
    expect(statuses).toEqual([true, false]);
  });
});

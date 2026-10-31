// Regression tests from the 2026-10-31 FerroDeck UI cloud audit
// (docs/audits/2026-10-31-ferrodeck-ui-cloud-audit.md). Every test here failed
// against the pre-audit source and pins the corresponding fix.
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { HubWsClient, MAX_PENDING_FRAMES } from "../api/ws";
import { App } from "../App";
import { ArtifactPreviewPane } from "../components/ArtifactPreviewPane";
import { ChatPane, sessionDraftMemory } from "../components/ChatPane";
import { parseUnifiedDiff } from "../components/DiffViewer";
import { FerroStatePane } from "../components/FerroStatePane";
import { GitPrPane, parsePrNumber } from "../components/GitPrPane";
import { NodesModal } from "../components/NodesModal";
import { RichMarkdown } from "../components/RichMarkdown";
import {
  MAX_TERMINAL_LOG_CHARS,
  TerminalPane,
  appendTerminalLog,
} from "../components/TerminalPane";
import { TurnCard } from "../components/TurnCard";
import { WireMailPane } from "../components/WireMailPane";
import { canAttachTokenTo, isSafeInlineImageUrl, isSafeLinkUrl } from "../lib/safeUrl";
import type {
  ArtifactPreviewResponse,
  HubWsFrame,
  StateEntry,
  TranscriptTurn,
  WireInboxItem,
} from "../types/api";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

class FakeSocket {
  static instances: FakeSocket[] = [];
  readyState = 0;
  onopen: (() => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  sent: string[] = [];
  closed = false;
  constructor(public readonly url: string) {
    FakeSocket.instances.push(this);
  }
  send(data: string) {
    this.sent.push(data);
  }
  close() {
    this.closed = true;
  }
  open() {
    this.readyState = 1;
    this.onopen?.();
  }
  receive(frame: unknown) {
    this.onmessage?.({ data: JSON.stringify(frame) } as MessageEvent);
  }
  drop() {
    this.readyState = 3;
    this.onclose?.();
  }
}

function makeWs(options: ConstructorParameters<typeof HubWsClient>[0] = {}) {
  FakeSocket.instances = [];
  return new HubWsClient({
    url: "ws://hub.test/ws/v1",
    webSocketFactory: (url) => new FakeSocket(url) as unknown as WebSocket,
    ...options,
  });
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function turn(content: string, role = "assistant"): TranscriptTurn {
  return {
    index: 0,
    role,
    content,
    thinking: null,
    tool_calls: [],
    created_at: "2026-10-31T00:00:00Z",
  };
}

beforeEach(() => {
  window.localStorage.clear();
  sessionDraftMemory.clear();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// safeUrl allow-lists
// ---------------------------------------------------------------------------

describe("safeUrl allow-lists", () => {
  it("accepts only http(s) and same-origin relative link targets", () => {
    expect(isSafeLinkUrl("https://example.com/a")).toBe(true);
    expect(isSafeLinkUrl("/api/v1/x?y=1")).toBe(true);
    expect(isSafeLinkUrl("javascript:alert(1)")).toBe(false);
    expect(isSafeLinkUrl(" JaVa\tScRiPt:alert(1)")).toBe(false);
    expect(isSafeLinkUrl("data:text/html,<script>1</script>")).toBe(false);
    expect(isSafeLinkUrl("vbscript:x")).toBe(false);
    expect(isSafeLinkUrl("//evil.example/x")).toBe(false);
    expect(isSafeLinkUrl("/\\evil.example/x")).toBe(false);
    expect(isSafeLinkUrl("")).toBe(false);
  });

  it("auto-loads only same-origin, blob and raster/svg data images", () => {
    const origin = "http://hub.local:8420";
    expect(isSafeInlineImageUrl("/api/v1/attachments/a.png", origin)).toBe(true);
    expect(isSafeInlineImageUrl("http://hub.local:8420/a.png", origin)).toBe(true);
    expect(isSafeInlineImageUrl("https://evil.example/p.png?q=secret", origin)).toBe(false);
    expect(isSafeInlineImageUrl("data:image/png;base64,AAAA", origin)).toBe(true);
    expect(isSafeInlineImageUrl("data:text/html;base64,AAAA", origin)).toBe(false);
    expect(isSafeInlineImageUrl("javascript:void0", origin)).toBe(false);
  });

  it("only attaches the bearer token to the hub origin", () => {
    expect(canAttachTokenTo("/api/v1/artifacts/preview?x=1", "")).toBe(true);
    expect(canAttachTokenTo("https://hub.example/a", "https://hub.example")).toBe(true);
    expect(canAttachTokenTo("https://evil.example/a", "https://hub.example")).toBe(false);
    expect(canAttachTokenTo("//evil.example/a", "https://hub.example")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// api/ws.ts
// ---------------------------------------------------------------------------

describe("HubWsClient lifecycle", () => {
  it("ignores events from a socket that connect() has already superseded", () => {
    vi.useFakeTimers();
    const ws = makeWs({ reconnectDelayMs: 100 });
    const statuses: boolean[] = [];
    ws.onStatusChange((c) => statuses.push(c));
    const frames: HubWsFrame[] = [];
    ws.subscribe("session", (f) => frames.push(f));

    ws.connect();
    const first = FakeSocket.instances[0];
    first.open();
    ws.connect(); // re-pair / token refresh replaces the socket
    const second = FakeSocket.instances[1];
    second.open();
    expect(ws.isConnected()).toBe(true);

    // Late events from the dead socket.
    first.receive({ channel: "session", event: "x", data: 1 });
    first.drop();

    expect(frames).toHaveLength(0);
    expect(ws.isConnected()).toBe(true);
    vi.advanceTimersByTime(60_000);
    // No duplicate reconnect that would tear the live socket down.
    expect(FakeSocket.instances).toHaveLength(2);
    expect(second.closed).toBe(false);
    ws.disconnect();
  });

  it("backs off exponentially, caps the delay, and resets after a successful open", () => {
    vi.useFakeTimers();
    const ws = makeWs({ reconnectDelayMs: 100, maxReconnectDelayMs: 400 });
    ws.connect();
    const dropAndWait = (expectedDelay: number, expectedSockets: number) => {
      FakeSocket.instances[FakeSocket.instances.length - 1].drop();
      vi.advanceTimersByTime(expectedDelay - 1);
      expect(FakeSocket.instances).toHaveLength(expectedSockets - 1);
      vi.advanceTimersByTime(1);
      expect(FakeSocket.instances).toHaveLength(expectedSockets);
    };
    dropAndWait(100, 2);
    dropAndWait(200, 3);
    dropAndWait(400, 4);
    dropAndWait(400, 5); // capped
    FakeSocket.instances[4].open();
    dropAndWait(100, 6); // reset
    ws.disconnect();
  });

  it("isolates throwing subscribers and status handlers", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const ws = makeWs();
    const seen: string[] = [];
    ws.subscribe("wire", () => {
      throw new Error("boom");
    });
    ws.subscribe("wire", () => seen.push("second"));
    ws.subscribe("*", () => seen.push("wildcard"));
    ws.onStatusChange(() => {
      throw new Error("status boom");
    });
    const statusSeen: boolean[] = [];
    ws.onStatusChange((c) => statusSeen.push(c));

    ws.connect();
    const sock = FakeSocket.instances[0];
    sock.open();
    sock.receive({ channel: "wire", event: "new", data: {} });

    expect(seen).toEqual(["second", "wildcard"]);
    expect(statusSeen).toEqual([true]);
    ws.disconnect();
  });

  it("does not replay queued frames under a different identity, and bounds the queue", () => {
    const ws = makeWs();
    ws.setToken("alice");
    ws.sendFrame("terminal", "input", { data: "secret-for-alice" });
    ws.setToken("bob");
    ws.connect();
    FakeSocket.instances[0].open();
    expect(FakeSocket.instances[0].sent).toHaveLength(0);

    const ws2 = makeWs();
    for (let i = 0; i < MAX_PENDING_FRAMES + 50; i++) {
      ws2.sendFrame("c", "e", { i });
    }
    ws2.connect();
    FakeSocket.instances[0].open();
    expect(FakeSocket.instances[0].sent).toHaveLength(MAX_PENDING_FRAMES);
    expect(JSON.parse(FakeSocket.instances[0].sent[0]).data.i).toBe(50);
  });

  it("can send live-only frames without queuing them for replay", () => {
    const ws = makeWs();
    expect(ws.sendFrame("terminal", "input", { data: "rm -rf x\n" }, { queueWhenDisconnected: false })).toBe(
      false,
    );
    ws.connect();
    FakeSocket.instances[0].open();
    expect(FakeSocket.instances[0].sent).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// App.tsx
// ---------------------------------------------------------------------------

function appFetch(extra?: (url: string) => Response | undefined) {
  const json = (body: unknown, status = 200) =>
    new Response(JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    });
  return vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    const custom = extra?.(url);
    if (custom) return custom;
    if (url.includes("/api/v1/auth/session")) {
      return json({ authenticated: true, authority: { tier: "owner", login: "lmcfarlin" } });
    }
    return json({});
  });
}

function wsStub() {
  return {
    setToken: vi.fn(),
    connect: vi.fn(),
    disconnect: vi.fn(),
    onStatusChange: () => () => {},
    subscribe: () => () => {},
  };
}

describe("App", () => {
  it("keeps the WebSocket connected across agent switches", async () => {
    const fetchImpl = appFetch();
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    const ws = wsStub();
    const { unmount } = render(<App apiClient={apiClient} wsClient={ws as unknown as HubWsClient} />);

    await screen.findByTestId("roster-sidebar");
    await waitFor(() => expect(fetchImpl).toHaveBeenCalled());
    const rows = screen
      .getAllByTestId(/^agent-row-/)
      .filter((row) => !row.getAttribute("data-testid")?.endsWith("ferris"));
    expect(rows.length).toBeGreaterThan(0);
    const connectsBefore = ws.connect.mock.calls.length;

    fireEvent.click(rows[0]);
    await act(async () => {
      await Promise.resolve();
    });

    // Before the fix every switch re-ran the socket effect: disconnect()+connect().
    expect(ws.disconnect).not.toHaveBeenCalled();
    expect(ws.connect.mock.calls.length).toBe(connectsBefore);
    unmount();
    expect(ws.disconnect).toHaveBeenCalledTimes(1);
  });

  it("strips the single-use pair code from the URL immediately and pairs exactly once under StrictMode", async () => {
    window.history.replaceState({}, "", "/?pair=ABC123&keep=1#frag");
    const pairCalls: string[] = [];
    const fetchImpl = appFetch((url) => {
      if (url.includes("/api/v1/auth/pair")) {
        pairCalls.push(url);
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
            session_token: "tok",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return undefined;
    });
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    const ws = wsStub();
    const { unmount } = render(
      <React.StrictMode>
        <App apiClient={apiClient} wsClient={ws as unknown as HubWsClient} />
      </React.StrictMode>,
    );

    // Synchronously after mount, before any network await resolves.
    expect(window.location.search).toBe("?keep=1");
    expect(window.location.hash).toBe("#frag");

    await waitFor(() => expect(pairCalls.length).toBeGreaterThan(0));
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20));
    });
    expect(pairCalls).toHaveLength(1);
    expect(screen.queryByText(/rejected/i)).toBeNull();
    unmount();
    window.history.replaceState({}, "", "/");
  });

  it("clears in-memory session, transcript and node state on logout", async () => {
    const json = (body: unknown) =>
      new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    const fetchImpl = appFetch((url) => {
      if (url.includes("/api/v1/sessions/") && url.endsWith("/transcript")) {
        return json({
          turns: [turn("confidential transcript from ferris")],
          queue: [],
          is_busy: false,
        });
      }
      if (url.endsWith("/api/v1/sessions")) {
        return json({
          sessions: [{ id: "sess-ferris", persona: "ferris", adapter: "claude", status: "idle" }],
        });
      }
      if (url.endsWith("/api/v1/nodes")) {
        return json({
          nodes: [{ node_id: "n1", label: "node-1", status: "online" }],
        });
      }
      return undefined;
    });
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    const ws = wsStub();
    render(<App apiClient={apiClient} wsClient={ws as unknown as HubWsClient} />);

    await screen.findByText("confidential transcript from ferris");
    expect(screen.getByTestId("nodes-modal-btn")).toHaveTextContent("Nodes (1)");
    expect(screen.getByTestId("session-id-pill")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Sign Out/i }));

    await waitFor(() =>
      expect(screen.queryByText("confidential transcript from ferris")).toBeNull(),
    );
    expect(screen.queryByTestId("session-id-pill")).toBeNull();
    expect(screen.getByTestId("nodes-modal-btn")).toHaveTextContent(/^Nodes$/);
    expect(ws.setToken).toHaveBeenLastCalledWith(null);
    expect(ws.disconnect).toHaveBeenCalled();
  });

  it("coalesces a refresh requested while another refresh is in flight into a follow-up run", async () => {
    const json = (body: unknown) =>
      new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    let holdNextSessions = false;
    let refreshSessionsCalls = 0;
    const firstRefreshSessions = deferred<Response>();
    const fetchImpl = appFetch((url) => {
      if (url.endsWith("/api/v1/sessions")) {
        if (holdNextSessions) {
          holdNextSessions = false;
          refreshSessionsCalls += 1;
          return firstRefreshSessions.promise as unknown as Response;
        }
        if (refreshSessionsCalls > 0) {
          refreshSessionsCalls += 1;
        }
        return json({ sessions: [] });
      }
      if (url.includes("/api/v1/sessions/") && url.endsWith("/transcript")) {
        const text =
          refreshSessionsCalls === 0 ? "initial-turn" : `refresh-turn-${refreshSessionsCalls}`;
        return json({
          turns: [turn(text)],
          queue: [],
          is_busy: false,
        });
      }
      return undefined;
    });
    const apiClient = new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    let sessionListener: (() => void) | null = null;
    const ws = {
      ...wsStub(),
      subscribe: vi.fn((channel: string, cb: () => void) => {
        if (channel === "session") {
          sessionListener = cb;
        }
        return () => {};
      }),
    };
    render(<App apiClient={apiClient} wsClient={ws as unknown as HubWsClient} />);

    await screen.findByText("initial-turn");
    expect(sessionListener).not.toBeNull();

    // Start refresh #1 and hold its getSessions() call in flight.
    holdNextSessions = true;
    act(() => {
      sessionListener!();
    });
    expect(refreshSessionsCalls).toBe(1);

    // Two more session events arrive while refresh #1 is in flight; they
    // should coalesce into a single trailing refresh (#2) after refresh #1 settles.
    act(() => {
      sessionListener!();
      sessionListener!();
    });
    expect(refreshSessionsCalls).toBe(1);

    await act(async () => {
      firstRefreshSessions.resolve(json({ sessions: [] }));
      await firstRefreshSessions.promise;
    });

    await screen.findByText("refresh-turn-2");
    expect(refreshSessionsCalls).toBe(2);
  });
});

// ---------------------------------------------------------------------------
// ChatPane
// ---------------------------------------------------------------------------

describe("ChatPane send race", () => {
  it("does not wipe the next agent's draft when an earlier send finishes after a switch", async () => {
    const pending = deferred<unknown>();
    const onSendPrompt = vi.fn(() => pending.promise);
    const { rerender } = render(
      <ChatPane agentName="alpha" sessionId="sess-a" turns={[]} onSendPrompt={onSendPrompt} />,
    );

    const input = () => screen.getByPlaceholderText(/Send a message/i) as HTMLTextAreaElement;
    fireEvent.change(input(), { target: { value: "message for alpha" } });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(onSendPrompt).toHaveBeenCalledTimes(1));

    // Operator switches to another agent and starts a different draft.
    rerender(
      <ChatPane agentName="beta" sessionId="sess-b" turns={[]} onSendPrompt={onSendPrompt} />,
    );
    fireEvent.change(input(), { target: { value: "draft for beta" } });

    await act(async () => {
      pending.resolve({ queued: false });
      await pending.promise;
    });

    expect(input().value).toBe("draft for beta");
    expect(sessionDraftMemory.get("sess-b")?.promptText).toBe("draft for beta");
    // The finished send still clears its own target's draft.
    expect(sessionDraftMemory.has("sess-a")).toBe(false);
  });

  it("does not show a failed send's error on the agent the operator switched to", async () => {
    const pending = deferred<unknown>();
    const onSendPrompt = vi.fn(() => pending.promise);
    const { rerender } = render(
      <ChatPane agentName="alpha" sessionId="sess-a" turns={[]} onSendPrompt={onSendPrompt} />,
    );
    fireEvent.change(screen.getByPlaceholderText(/Send a message/i), {
      target: { value: "hello" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Send Message/i }));
    await waitFor(() => expect(onSendPrompt).toHaveBeenCalled());
    rerender(
      <ChatPane agentName="beta" sessionId="sess-b" turns={[]} onSendPrompt={onSendPrompt} />,
    );
    await act(async () => {
      pending.reject(new Error("alpha is offline"));
      await pending.promise.catch(() => {});
    });
    expect(screen.queryByText(/alpha is offline/)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// TurnCard / RichMarkdown
// ---------------------------------------------------------------------------

describe("TurnCard attachments", () => {
  it("does not auto-load third-party images referenced by transcript text", () => {
    const { container } = render(
      <TurnCard
        turn={turn("see\n[Attached file: shot.png (https://evil.example/p.png?q=conversation-secret)]")}
      />,
    );
    expect(container.querySelector('img[src*="evil.example"]')).toBeNull();
  });

  it("still renders same-origin attachment images", () => {
    const { container } = render(
      <TurnCard turn={turn("see\n[Attached file: /tmp/shot.png (/api/v1/attachments/shot.png)]")} />,
    );
    expect(container.querySelector('img[src="/api/v1/attachments/shot.png"]')).not.toBeNull();
  });

  it("never emits a javascript: href or src from an attachment url", () => {
    const { container } = render(
      <TurnCard turn={turn("x\n[Attached file: a.png (javascript:void0#.png)]")} />,
    );
    expect(container.querySelector('a[href^="javascript" i]')).toBeNull();
    expect(container.querySelector('img[src^="javascript" i]')).toBeNull();
  });
});

describe("RichMarkdown links", () => {
  it("exposes the real destination so a spoofed label cannot hide it", () => {
    render(<RichMarkdown text="[https://bank.example/login](https://evil.example/phish)" />);
    const link = screen.getByRole("link");
    expect(link).toHaveAttribute("href", "https://evil.example/phish");
    expect(link).toHaveAttribute("title", "https://evil.example/phish");
    expect(link).toHaveAttribute("rel", expect.stringContaining("noopener"));
  });

  it("renders javascript: links as inert text", () => {
    const { container } = render(<RichMarkdown text="[click](javascript:alert1)" />);
    expect(container.querySelector("a")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// ArtifactPreviewPane
// ---------------------------------------------------------------------------

function artifact(overrides: Partial<ArtifactPreviewResponse>): ArtifactPreviewResponse {
  return {
    path: "/work/a.bin",
    name: "a.bin",
    ext: "bin",
    mime_type: "application/octet-stream",
    size_bytes: 3,
    kind: "binary",
    raw_url: "/api/v1/artifacts/preview?path=%2Fwork%2Fa.bin&mode=raw",
    download_url: "/api/v1/artifacts/preview?path=%2Fwork%2Fa.bin&mode=raw&download=1",
    ...overrides,
  } as ArtifactPreviewResponse;
}

function artifactClient(
  getArtifactPreview: (path: string) => Promise<ArtifactPreviewResponse>,
  token: string | null = null,
) {
  return {
    getArtifactPreview: vi.fn(getArtifactPreview),
    listSessionArtifacts: vi.fn(async () => ({ artifacts: [] })),
    pruneArtifacts: vi.fn(async () => ({})),
    getToken: () => token,
    getBaseUrl: () => "",
    getArtifactRawUrl: (path: string) => `/api/v1/artifacts/preview?path=${encodeURIComponent(path)}`,
  } as unknown as HubApiClient;
}

describe("ArtifactPreviewPane URL handling", () => {
  it("refuses a javascript: raw_url for the PDF iframe and the open-raw link", async () => {
    const client = artifactClient(async () =>
      artifact({
        kind: "pdf",
        name: "a.pdf",
        path: "/work/a.pdf",
        raw_url: "javascript:alert(document.domain)",
        download_url: "javascript:alert(1)",
      }),
    );
    render(<ArtifactPreviewPane apiClient={client} activePath="/work/a.pdf" />);
    await screen.findByTestId("artifact-unsafe-url");
    expect(screen.queryByTestId("artifact-pdf-frame")).toBeNull();
    expect(screen.getByTestId("artifact-open-raw-btn")).not.toHaveAttribute("href");
    expect(screen.getByTestId("artifact-download-btn")).not.toHaveAttribute("href");
  });

  it("does not append the session token to a cross-origin url, but does for the hub's own", async () => {
    const cross = artifactClient(
      async () =>
        artifact({
          raw_url: "https://evil.example/f",
          download_url: "https://evil.example/f?download=1",
        }),
      "SESSION-SECRET",
    );
    const { unmount } = render(<ArtifactPreviewPane apiClient={cross} activePath="/work/a.bin" />);
    const dl = await screen.findByTestId("artifact-download-btn");
    expect(dl.getAttribute("href")).toBe("https://evil.example/f?download=1");
    expect(screen.getByTestId("artifact-open-raw-btn").getAttribute("href")).not.toContain(
      "SESSION-SECRET",
    );
    unmount();

    const same = artifactClient(async () => artifact({}), "SESSION-SECRET");
    render(<ArtifactPreviewPane apiClient={same} activePath="/work/a.bin" />);
    const dl2 = await screen.findByTestId("artifact-download-btn");
    expect(dl2.getAttribute("href")).toContain("token=SESSION-SECRET");
  });

  it("does not build a bare '?token=' link when raw_url is empty", async () => {
    const client = artifactClient(
      async () => artifact({ raw_url: "", download_url: "" }),
      "SESSION-SECRET",
    );
    render(<ArtifactPreviewPane apiClient={client} activePath="/work/a.bin" />);
    const raw = await screen.findByTestId("artifact-open-raw-btn");
    expect(raw.getAttribute("href")).toMatch(/^\/api\/v1\/artifacts\/preview\?path=/);
  });

  it("drops the previous artifact while the next one loads", async () => {
    const second = deferred<ArtifactPreviewResponse>();
    const client = artifactClient((path) =>
      path === "/work/one.bin"
        ? Promise.resolve(artifact({ name: "one.bin", path }))
        : second.promise,
    );
    const { rerender } = render(<ArtifactPreviewPane apiClient={client} activePath="/work/one.bin" />);
    await waitFor(() => expect(screen.getByTestId("artifact-filename")).toHaveTextContent("one.bin"));

    rerender(<ArtifactPreviewPane apiClient={client} activePath="/work/two.bin" />);
    await waitFor(() =>
      expect(screen.getByTestId("artifact-filename")).not.toHaveTextContent("one.bin"),
    );
    expect(screen.queryByTestId("artifact-download-btn")).toBeNull();

    await act(async () => {
      second.resolve(artifact({ name: "two.bin", path: "/work/two.bin" }));
      await second.promise;
    });
    await waitFor(() => expect(screen.getByTestId("artifact-filename")).toHaveTextContent("two.bin"));
  });

  it("ignores a prune refresh that resolves after the session changed", async () => {
    const refresh = deferred<{ artifacts: Array<{ path: string; name: string; status: string }> }>();
    let call = 0;
    const client = {
      ...artifactClient(async () => artifact({})),
      listSessionArtifacts: vi.fn(async (sid: string | null) => {
        call += 1;
        // 1: session A mount, 2: session A prune refresh (slow), 3: session B mount.
        if (call === 2) return refresh.promise;
        return {
          artifacts: [
            { path: `/work/${sid}.md`, name: `${sid}.md`, ext: "md", kind: "markdown", size_bytes: 1, status: "archived" },
          ],
        };
      }),
    } as unknown as HubApiClient;

    const { rerender } = render(<ArtifactPreviewPane apiClient={client} sessionId="A" />);
    await waitFor(() => expect(screen.getByText("A.md")).toBeInTheDocument());
    fireEvent.click(screen.getByTestId("prune-artifacts-btn"));
    await waitFor(() => expect(call).toBe(2));

    rerender(<ArtifactPreviewPane apiClient={client} sessionId="B" />);
    await waitFor(() => expect(screen.getByText("B.md")).toBeInTheDocument());
    expect(screen.queryByText("A.md")).toBeNull();

    await act(async () => {
      refresh.resolve({
        artifacts: [{ path: "/work/A.md", name: "A.md", status: "archived" }],
      });
      await refresh.promise;
    });
    expect(screen.queryByText("A.md")).toBeNull();
    expect(screen.getByText("B.md")).toBeInTheDocument();
  });
});

// ---------------------------------------------------------------------------
// TerminalPane
// ---------------------------------------------------------------------------

const OWNER = { tier: "owner", login: "lmcfarlin" } as const;

function terminalWs() {
  let handler: ((frame: HubWsFrame) => void) | null = null;
  let statusCb: ((connected: boolean) => void) | null = null;
  return {
    emit: (frame: HubWsFrame) => handler?.(frame),
    setStatus: (connected: boolean) => statusCb?.(connected),
    client: {
      subscribe: vi.fn((_channel: string, cb: (frame: HubWsFrame) => void) => {
        handler = cb;
        return () => {
          handler = null;
        };
      }),
      onStatusChange: vi.fn((cb: (connected: boolean) => void) => {
        statusCb = cb;
        return () => {
          statusCb = null;
        };
      }),
      sendFrame: vi.fn(() => true),
    },
  };
}

describe("TerminalPane", () => {
  it("bounds the output buffer", () => {
    const big = "x".repeat(MAX_TERMINAL_LOG_CHARS + 5000);
    expect(appendTerminalLog("", big)).toHaveLength(MAX_TERMINAL_LOG_CHARS);
    expect(appendTerminalLog("old", "new")).toBe("oldnew");

    const ws = terminalWs();
    render(<TerminalPane authority={OWNER} sessionId="s1" wsClient={ws.client as never} />);
    act(() => {
      ws.emit({ channel: "terminal", event: "output", data: { session_id: "s1", data: big } });
    });
    expect(screen.getByTestId("terminal-output-log").textContent!.length).toBeLessThanOrEqual(
      MAX_TERMINAL_LOG_CHARS,
    );
  });

  it("clears the previous session's scrollback when the session changes", () => {
    const ws = terminalWs();
    const { rerender } = render(
      <TerminalPane authority={OWNER} sessionId="s1" wsClient={ws.client as never} />,
    );
    act(() => {
      ws.emit({ channel: "terminal", event: "output", data: { session_id: "s1", data: "from-s1" } });
    });
    expect(screen.getByTestId("terminal-output-log")).toHaveTextContent("from-s1");
    rerender(<TerminalPane authority={OWNER} sessionId="s2" wsClient={ws.client as never} />);
    expect(screen.getByTestId("terminal-output-log")).not.toHaveTextContent("from-s1");
  });

  it("never queues input, resize or close for replay, but lets open wait", () => {
    const ws = terminalWs();
    const { unmount } = render(
      <TerminalPane authority={OWNER} sessionId="s1" wsClient={ws.client as never} />,
    );
    const calls = () => ws.client.sendFrame.mock.calls as unknown as unknown[][];
    const open = calls().find((c) => c[1] === "open")!;
    expect(open).toHaveLength(3);

    fireEvent.change(screen.getByPlaceholderText(/Enter shell command/i), {
      target: { value: "rm -rf build" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Send Command/i }));
    const input = calls().find((c) => c[1] === "input")!;
    expect(input[3]).toEqual({ queueWhenDisconnected: false });

    unmount();
    const close = calls().find((c) => c[1] === "close")!;
    expect(close[3]).toEqual({ queueWhenDisconnected: false });
  });

  it("does not hard-code a developer home directory as the default cwd or render an empty cwd badge", () => {
    const onSend = vi.fn();
    const { container } = render(
      <TerminalPane authority={OWNER} sessionId="s1" cwd={null} onSendTerminalFrame={onSend} />,
    );
    const open = onSend.mock.calls.find((c) => c[0] === "open")!;
    expect((open[1] as { cwd?: string }).cwd).toBeUndefined();
    expect(JSON.stringify(open[1])).not.toContain("/home/");
    const monoBadges = Array.from(container.querySelectorAll(".fd-badge.fd-mono")).map((el) =>
      el.textContent?.trim(),
    );
    expect(monoBadges).toEqual(["s1"]);
  });

  it("survives a terminal frame with null data", () => {
    const ws = terminalWs();
    render(<TerminalPane authority={OWNER} sessionId="s1" wsClient={ws.client as never} />);
    expect(() =>
      act(() => {
        ws.emit({ channel: "terminal", event: "output", data: null });
        ws.emit({ channel: "terminal", event: "exited", data: null });
      }),
    ).not.toThrow();
  });

  it("shows the socket dropping instead of claiming the shell is still connected", () => {
    const ws = terminalWs();
    render(<TerminalPane authority={OWNER} sessionId="s1" wsClient={ws.client as never} />);
    act(() => {
      ws.emit({ channel: "terminal", event: "opened", data: { session_id: "s1", pid: 42 } });
    });
    expect(screen.getByTestId("terminal-status-badge")).toHaveTextContent(/connected/);
    act(() => ws.setStatus(false));
    expect(screen.getByTestId("terminal-status-badge")).toHaveTextContent(/disconnected/);
    act(() => ws.setStatus(true));
    expect(screen.getByTestId("terminal-status-badge")).toHaveTextContent(/Reconnect/);
  });
});

// ---------------------------------------------------------------------------
// FerroStatePane
// ---------------------------------------------------------------------------

function stateHit(slug: string) {
  return {
    entry: stateEntry(slug),
    score_millis: 1000,
    matched_terms: [slug],
    snippet: `snippet ${slug}`,
    estimated_tokens: 10,
  };
}

function stateEntry(slug: string): StateEntry {
  return {
    id: `urn:fl:state:${slug}`,
    scope: "project:ferroloop",
    concern: "memory",
    slug,
    sensitive: false,
    ever_sensitive: false,
    payload: {
      memory: {
        category: "note",
        basis: "inference",
        lifecycle: "active",
        superseded_by: null,
        pinned: false,
        summary: `summary ${slug}`,
        body: "",
      },
    },
    head_revision: "r1",
    updated_at: "2026-10-31T00:00:00Z",
    by: "ferris",
  } as StateEntry;
}

describe("FerroStatePane search ordering", () => {
  it("keeps the newest query's results when an older response arrives late", async () => {
    const first = deferred<{ hits: Array<{ entry: StateEntry }> }>();
    const second = deferred<{ hits: Array<{ entry: StateEntry }> }>();
    const searchStateEntries = vi
      .fn()
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    const client = {
      listStateEntries: vi.fn(async () => ({ entries: [] })),
      searchStateEntries,
    } as unknown as HubApiClient;
    render(<FerroStatePane authority={OWNER as never} apiClient={client} />);
    await waitFor(() => expect(client.listStateEntries).toHaveBeenCalled());

    const box = screen.getByPlaceholderText(/BM25 recall/i);
    fireEvent.change(box, { target: { value: "older" } });
    fireEvent.click(screen.getByRole("button", { name: /^Search$/i }));
    // Search is disabled while loading; changing a filter re-queries regardless.
    fireEvent.change(screen.getByLabelText(/Concern Filter/i), { target: { value: "memory" } });
    await waitFor(() => expect(searchStateEntries).toHaveBeenCalledTimes(2));

    await act(async () => {
      second.resolve({ hits: [stateHit("newer-hit")] });
      await second.promise;
    });
    await act(async () => {
      first.resolve({ hits: [stateHit("older-hit")] });
      await first.promise;
    });

    expect(screen.getByTestId("state-entry-newer-hit")).toBeInTheDocument();
    expect(screen.queryByTestId("state-entry-older-hit")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// NodesModal
// ---------------------------------------------------------------------------

describe("NodesModal secrets and keyboard", () => {
  const token = () => ({
    code: "FLJOIN-1",
    expires_at_ms: Date.now() + 600_000,
    ttl_secs: 600,
    join_command: "fl hub join https://hub --code FLJOIN-1",
  });

  it("forgets the enrollment command when the modal closes", async () => {
    const onCreate = vi.fn(async () => token());
    const props = { nodes: [], onCreateEnrollToken: onCreate, onClose: vi.fn() };
    const { rerender } = render(<NodesModal isOpen {...props} />);
    fireEvent.click(screen.getByTestId("enroll-node-btn"));
    await screen.findByTestId("enroll-join-command");

    rerender(<NodesModal isOpen={false} {...props} />);
    rerender(<NodesModal isOpen {...props} />);
    expect(screen.queryByTestId("enroll-join-command")).toBeNull();
  });

  it("does not claim 'Copied' when the clipboard write fails", async () => {
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText: vi.fn().mockRejectedValue(new Error("denied")) },
      configurable: true,
    });
    render(
      <NodesModal isOpen nodes={[]} onCreateEnrollToken={async () => token()} onClose={vi.fn()} />,
    );
    fireEvent.click(screen.getByTestId("enroll-node-btn"));
    await screen.findByTestId("enroll-join-command");
    const copy = screen.getByTestId("copy-join-command-btn");
    fireEvent.click(copy);
    await waitFor(() => expect(copy).toHaveTextContent(/Copy failed/i));
    expect(copy).not.toHaveTextContent(/^Copied$/);
  });

  it("closes on Escape", () => {
    const onClose = vi.fn();
    render(<NodesModal isOpen nodes={[]} onClose={onClose} />);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

// ---------------------------------------------------------------------------
// GitPrPane / DiffViewer / WireMailPane
// ---------------------------------------------------------------------------

describe("GitPrPane PR number parsing", () => {
  it("accepts plain positive integers only", () => {
    expect(parsePrNumber("7")).toBe(7);
    expect(parsePrNumber(" 42 ")).toBe(42);
    expect(parsePrNumber("1e3")).toBeNull();
    expect(parsePrNumber("2.5")).toBeNull();
    expect(parsePrNumber("12abc")).toBeNull();
    expect(parsePrNumber("0")).toBeNull();
    expect(parsePrNumber("-3")).toBeNull();
    expect(parsePrNumber("")).toBeNull();
  });

  it("refuses to merge when the field holds 1e3 (parseInt would merge PR #1)", () => {
    const onMergePr = vi.fn(async () => undefined);
    render(<GitPrPane defaultSubView="prs" prs={[]} onMergePr={onMergePr} />);
    const field = screen.getByLabelText("PR Number");
    fireEvent.change(field, { target: { value: "1e3" } });
    fireEvent.click(screen.getByRole("button", { name: /Merge Approved PR/i }));
    expect(onMergePr).not.toHaveBeenCalled();
    expect(screen.getByText(/valid positive PR number/i)).toBeInTheDocument();

    fireEvent.change(field, { target: { value: "7" } });
    fireEvent.click(screen.getByRole("button", { name: /Merge Approved PR/i }));
    expect(onMergePr).toHaveBeenCalledWith(7, expect.anything());
  });
});

describe("DiffViewer parser", () => {
  it("does not invent a context line from the diff's trailing newline", () => {
    const diff = [
      "diff --git a/x.txt b/x.txt",
      "--- a/x.txt",
      "+++ b/x.txt",
      "@@ -1,2 +1,2 @@",
      " keep",
      "-old",
      "+new",
      "",
    ].join("\n");
    const [file] = parseUnifiedDiff(diff);
    expect(file.hunks[0].lines.map((l) => l.type)).toEqual(["context", "del", "add"]);
  });

  it("keeps genuine blank context lines inside a hunk", () => {
    const diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
    const [file] = parseUnifiedDiff(diff);
    expect(file.hunks[0].lines.map((l) => l.type)).toEqual(["context", "context", "del", "add"]);
  });
});

describe("WireMailPane acknowledgements", () => {
  const item = {
    message: {
      id: "m1",
      thread_id: "t1",
      sender_name: "jarvis",
      to: "all",
      kind: "mail",
      urgency: "normal",
      durable: true,
      subject: "Broadcast",
      body: "hello everyone",
      created_at: "2026-10-31T00:00:00Z",
    },
  } as unknown as WireInboxItem;

  it("scopes the optimistic ack to the agent that acknowledged", async () => {
    const onAck = vi.fn(async () => undefined);
    const { rerender } = render(
      <WireMailPane messages={[item]} selectedAgentName="ferris" onAckMessage={onAck} />,
    );
    fireEvent.click(screen.getByTestId("ack-btn-m1"));
    await waitFor(() => expect(screen.queryByTestId("ack-btn-m1")).toBeNull());

    rerender(<WireMailPane messages={[item]} selectedAgentName="kacey" onAckMessage={onAck} />);
    // kacey never acknowledged it.
    expect(screen.getByTestId("ack-btn-m1")).toBeInTheDocument();

    rerender(<WireMailPane messages={[item]} selectedAgentName="ferris" onAckMessage={onAck} />);
    expect(screen.queryByTestId("ack-btn-m1")).toBeNull();
  });
});

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../api";
import type { HubWsClient } from "../api/ws";
import { NodesModal } from "../components/NodesModal";
import type { EnrolledNode, NodeEnrollTokenResponse } from "../types";

const SAMPLE_NODES: EnrolledNode[] = [
  {
    node_id: "node-hub-ai-lab-0",
    display_name: "ai-lab-0",
    role: "hub",
    hostname: "ai-lab-0.local",
    os: "linux",
    arch: "x86_64",
    listen_addr: "127.0.0.1:8420",
    capabilities: ["claude", "agy", "codex", "muse", "grok", "tmux"],
    labels: ["gpu-rocm", "primary"],
    status: "online",
    enrolled_at_ms: 1700000000000,
    last_seen_at_ms: 1700000010000,
  },
  {
    node_id: "node-worker-mac-studio",
    display_name: "mac-studio",
    role: "worker",
    hostname: "mac-studio.local",
    os: "macos",
    arch: "aarch64",
    listen_addr: null,
    capabilities: ["claude", "xcode", "tmux"],
    labels: ["macos-xcode"],
    status: "stale",
    enrolled_at_ms: 1700000002000,
    last_seen_at_ms: 1700000008000,
  },
];

describe("NodesModal", () => {
  it("renders enrolled hub and worker nodes, capabilities, labels, and hides Revoke on hub node", () => {
    render(
      <NodesModal
        isOpen
        onClose={vi.fn()}
        nodes={SAMPLE_NODES}
      />,
    );

    const modal = screen.getByTestId("nodes-modal");
    expect(modal).toBeInTheDocument();

    const rows = screen.getAllByTestId("enrolled-node-row");
    expect(rows).toHaveLength(2);

    // Hub node row: shows HUB badge, online status dot, capabilities, listen_addr, and NO Revoke button
    const hubRow = rows[0];
    expect(hubRow).toHaveTextContent("ai-lab-0");
    expect(hubRow).toHaveTextContent("HUB");
    expect(hubRow).toHaveTextContent("linux/x86_64");
    expect(hubRow).toHaveTextContent("127.0.0.1:8420");
    expect(hubRow).toHaveTextContent("claude");
    expect(hubRow).toHaveTextContent("agy");
    expect(hubRow).toHaveTextContent("codex");
    expect(hubRow).toHaveTextContent("muse");
    expect(hubRow).toHaveTextContent("grok");
    expect(hubRow).toHaveTextContent("tmux");
    expect(hubRow).toHaveTextContent("#gpu-rocm");
    expect(within(hubRow).getByTestId("node-status-dot-node-hub-ai-lab-0")).toHaveClass(
      "fd-dot-alive",
    );
    expect(within(hubRow).queryByTestId("revoke-node-btn")).toBeNull();

    // Worker node row: shows WORKER badge, stale status dot, xcode capability, and Revoke button
    const workerRow = rows[1];
    expect(workerRow).toHaveTextContent("mac-studio");
    expect(workerRow).toHaveTextContent("WORKER");
    expect(workerRow).toHaveTextContent("macos/aarch64");
    expect(workerRow).toHaveTextContent("xcode");
    expect(workerRow).toHaveTextContent("#macos-xcode");
    expect(
      within(workerRow).getByTestId("node-status-dot-node-worker-mac-studio"),
    ).toHaveClass("fd-dot-stale");
    expect(within(workerRow).getByTestId("revoke-node-btn")).toBeInTheDocument();
  });

  it("generates and copies a one-time fl hub join enrollment command with expiry countdown", async () => {
    const writeText = vi.fn(async () => undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });

    const onCreateEnrollToken = vi.fn(
      async (label?: string): Promise<NodeEnrollTokenResponse> => ({
        code: "FLJOIN-123456",
        expires_at_ms: Date.now() + 595_000,
        ttl_secs: 600,
        join_command: `fl hub join https://ferrodeck.ai-lab-0.mcfarlin.family --code FLJOIN-123456 --label ${label ?? "worker"}`,
      }),
    );

    render(
      <NodesModal
        isOpen
        onClose={vi.fn()}
        nodes={SAMPLE_NODES}
        onCreateEnrollToken={onCreateEnrollToken}
      />,
    );

    const labelInput = screen.getByTestId("enroll-node-label-input");
    fireEvent.change(labelInput, { target: { value: "macos-xcode" } });
    fireEvent.click(screen.getByTestId("enroll-node-btn"));

    await waitFor(() => {
      expect(onCreateEnrollToken).toHaveBeenCalledWith("macos-xcode");
    });

    const joinCmd = await screen.findByTestId("enroll-join-command");
    expect(joinCmd).toHaveTextContent(
      "fl hub join https://ferrodeck.ai-lab-0.mcfarlin.family --code FLJOIN-123456 --label macos-xcode",
    );
    expect(screen.getByTestId("enroll-token-expiry")).toHaveTextContent(/Expires in 9m/i);

    const copyBtn = screen.getByTestId("copy-join-command-btn");
    fireEvent.click(copyBtn);

    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(
        "fl hub join https://ferrodeck.ai-lab-0.mcfarlin.family --code FLJOIN-123456 --label macos-xcode",
      );
    });
    expect(copyBtn).toHaveTextContent("Copied");
  });

  it("revokes a worker node when clicking Revoke", async () => {
    const onRevokeNode = vi.fn(async () => undefined);

    render(
      <NodesModal
        isOpen
        onClose={vi.fn()}
        nodes={SAMPLE_NODES}
        onRevokeNode={onRevokeNode}
      />,
    );

    const revokeBtn = screen.getByTestId("revoke-node-btn");
    fireEvent.click(revokeBtn);

    await waitFor(() => {
      expect(onRevokeNode).toHaveBeenCalledWith("node-worker-mac-studio");
    });
  });

  it("opens from the header Nodes button (showing online count) and updates when a worker node is revoked", async () => {
    let currentNodes = [...SAMPLE_NODES];

    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      const json = (body: unknown) =>
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });

      if (url.includes("/api/v1/auth/session")) {
        return json({
          authenticated: true,
          authority: { tier: "owner", login: "lmcfarlin" },
        });
      }
      if (url.endsWith("/api/v1/nodes") && method === "GET") {
        return json({ nodes: currentNodes });
      }
      if (url.includes("/api/v1/nodes/") && method === "DELETE") {
        const id = decodeURIComponent(url.split("/api/v1/nodes/")[1]);
        currentNodes = currentNodes.filter((n) => n.node_id !== id);
        return json({ revoked: true });
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const wsStub = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: () => () => {},
    } as unknown as HubWsClient;

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);

    const headerBtn = await screen.findByTestId("nodes-modal-btn");
    await waitFor(() => {
      expect(headerBtn).toHaveTextContent("Nodes (1)");
    });

    fireEvent.click(headerBtn);
    expect(await screen.findByTestId("nodes-modal")).toBeInTheDocument();
    expect(screen.getAllByTestId("enrolled-node-row")).toHaveLength(2);

    fireEvent.click(screen.getByTestId("revoke-node-btn"));
    await waitFor(() => {
      expect(screen.getAllByTestId("enrolled-node-row")).toHaveLength(1);
    });

    unmount();
  });
});

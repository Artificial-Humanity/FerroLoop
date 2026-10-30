import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../lib/api";
import type { HubWsClient } from "../api/ws";
import { SpinUpAgentModal } from "../components/SpinUpAgentModal";
import type { SpinUpSessionRequest, SpinUpSessionResponse } from "../types/api";

describe("SpinUpAgentModal", () => {
  it("renders all required inputs with defaults and submits spinUpSession payload", async () => {
    const onSpinUp = vi.fn(
      async (req: SpinUpSessionRequest): Promise<SpinUpSessionResponse> => ({
        session_id: "sess-kacey-99",
        persona: req.persona,
        project: req.project,
        role: req.role,
        vendor: req.vendor,
        model: req.model ?? null,
        node_id: req.node_id ?? "ai-lab-0",
        hosted: Boolean(req.hosted),
        status: "running",
        tmux_target: "kacey:0.0",
        hydration_dispatched: true,
      }),
    );
    const onClose = vi.fn();

    render(<SpinUpAgentModal isOpen onClose={onClose} onSpinUp={onSpinUp} />);

    expect(screen.getByTestId("spin-up-agent-modal")).toBeInTheDocument();

    const personaInput = screen.getByTestId("spin-up-persona-input") as HTMLInputElement;
    const projectSelect = screen.getByTestId("spin-up-project-select") as HTMLSelectElement;
    const roleSelect = screen.getByTestId("spin-up-role-select") as HTMLSelectElement;
    const vendorSelect = screen.getByTestId("spin-up-vendor-select") as HTMLSelectElement;
    const modelInput = screen.getByTestId("spin-up-model-input") as HTMLInputElement;
    const nodeSelect = screen.getByTestId("spin-up-node-select") as HTMLInputElement;
    const hostedCheckbox = screen.getByTestId("spin-up-hosted-checkbox") as HTMLInputElement;

    expect(projectSelect.value).toBe("ferroloop");
    expect(roleSelect.value).toBe("assistant");
    expect(vendorSelect.value).toBe("claude");
    expect(nodeSelect.value).toBe("ai-lab-0");
    expect(hostedCheckbox.checked).toBe(true);

    // Submitting without persona shows validation error
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));
    expect(await screen.findByRole("alert")).toHaveTextContent(/persona name/i);
    expect(onSpinUp).not.toHaveBeenCalled();

    fireEvent.change(personaInput, { target: { value: "Kacey" } });
    fireEvent.change(projectSelect, { target: { value: "kaggle-gemma4" } });
    fireEvent.change(roleSelect, { target: { value: "assistant" } });
    fireEvent.change(vendorSelect, { target: { value: "antigravity" } });
    fireEvent.change(modelInput, { target: { value: "gemini-4-argon" } });
    fireEvent.change(nodeSelect, { target: { value: "ai-lab-0" } });

    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpinUp).toHaveBeenCalledWith({
        persona: "kacey",
        project: "kaggle-gemma4",
        role: "assistant",
        vendor: "antigravity",
        model: "gemini-4-argon",
        node_id: "ai-lab-0",
        hosted: true,
      });
    });
    expect(onClose).toHaveBeenCalled();
  });

  it("clears validation/API errors when closed and reopened", async () => {
    const onClose = vi.fn();
    const { rerender } = render(<SpinUpAgentModal isOpen onClose={onClose} />);

    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));
    expect(await screen.findByRole("alert")).toHaveTextContent(/persona name/i);

    rerender(<SpinUpAgentModal isOpen={false} onClose={onClose} />);
    rerender(<SpinUpAgentModal isOpen onClose={onClose} />);

    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("clears transcript state and avoids re-fetching torn-down session transcript in App on teardown", async () => {
    const transcriptFetches: string[] = [];

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
      if (url.endsWith("/api/v1/sessions")) {
        return json({
          sessions: [
            {
              id: "sess-ferris-live",
              persona: "ferris",
              adapter: "claude",
              status: "running",
            },
          ],
        });
      }
      if (url.includes("/api/v1/sessions/") && url.endsWith("/transcript")) {
        transcriptFetches.push(url);
        return json({
          turns: [
            {
              index: 0,
              role: "assistant",
              content: "Active session turn before teardown",
              thinking: null,
              tool_calls: [],
            },
          ],
          is_busy: true,
        });
      }
      if (url.endsWith("/api/v1/sessions/sess-ferris-live/decommission") && method === "POST") {
        return json({
          session_id: "sess-ferris-live",
          persona: "ferris",
          mode: "teardown",
          status: "decommissioned",
          unregistered: true,
          runtime_stopped: true,
        });
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

    expect(
      await screen.findByText("Active session turn before teardown"),
    ).toBeInTheDocument();

    transcriptFetches.length = 0;

    fireEvent.click(screen.getByTestId("decommission-session-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));

    await waitFor(() => {
      expect(
        screen.queryByText("Active session turn before teardown"),
      ).toBeNull();
    });
    expect(transcriptFetches).toEqual([]);

    unmount();
  });

  it("opens from the sidebar + Spin Up button in App and dispatches POST /api/v1/sessions/spin-up", async () => {
    const spinUpCalls: unknown[] = [];

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
      if (url.endsWith("/api/v1/sessions/spin-up") && method === "POST") {
        const body = JSON.parse(String(init?.body));
        spinUpCalls.push(body);
        return json({
          session_id: "sess-kacey-new",
          persona: body.persona,
          project: body.project,
          role: body.role,
          vendor: body.vendor,
          model: body.model ?? null,
          node_id: body.node_id ?? "ai-lab-0",
          hosted: Boolean(body.hosted),
          status: "running",
          hydration_dispatched: true,
        });
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

    const spinUpBtn = await screen.findByTestId("spin-up-agent-btn");
    fireEvent.click(spinUpBtn);

    expect(await screen.findByTestId("spin-up-agent-modal")).toBeInTheDocument();

    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "kacey" },
    });
    fireEvent.change(screen.getByTestId("spin-up-project-select"), {
      target: { value: "kaggle-gemma4" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(spinUpCalls).toEqual([
        {
          persona: "kacey",
          project: "kaggle-gemma4",
          role: "assistant",
          vendor: "claude",
          node_id: "ai-lab-0",
          hosted: true,
        },
      ]);
    });
    await waitFor(() => {
      expect(screen.queryByTestId("spin-up-agent-modal")).toBeNull();
    });

    unmount();
  });
});

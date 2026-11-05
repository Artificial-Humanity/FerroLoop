import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, act } from "@testing-library/react";
import "@testing-library/jest-dom/vitest";
import {
  AgentSettingsModal,
  type AgentSettingsModalProps,
} from "../components/AgentSettingsModal";
import { ChatPane, getVendorModelOptions, resolveSessionVendor } from "../components/ChatPane";
import type { HubApiClient } from "../api/client";

function modalProps(overrides: Partial<AgentSettingsModalProps> = {}): AgentSettingsModalProps {
  return {
    isOpen: true,
    onClose: () => {},
    agentName: "ferris",
    agentDisplayName: "Ferris",
    sessionId: "sess-1",
    adapter: "claude",
    selectedModel: "claude-haiku-4-5",
    selectedThinkingLevel: "medium",
    onChangeModelAndThinking: () => {},
    contextEstimate: { usedTokens: 1000, maxTokens: 200000, percent: 1 },
    canCompact: false,
    compacting: false,
    onCompactContext: () => {},
    canClear: true,
    clearing: false,
    onOpenClearModal: () => {},
    canDecommission: true,
    decommissioning: false,
    onOpenDecommissionModal: () => {},
    ...overrides,
  };
}

function clientWith(getSessions: unknown): HubApiClient {
  return { getSessions } as unknown as HubApiClient;
}

function session(overrides: Record<string, unknown> = {}) {
  return {
    id: "sess-1",
    source: "route",
    adapter: "claude",
    status: "active",
    ...overrides,
  };
}

const badge = () => screen.getByTestId("chat-active-model-badge");
const mode = () => screen.getByTestId("execution-mode-badge");
const modelSelectOptions = () =>
  Array.from(
    (document.getElementById("fd-agent-settings-model") as HTMLSelectElement).options,
  ).map((o) => o.value);

describe("AgentSettingsModal refreshes the session on every open", () => {
  it("fetches on open even with an executionMode prop, and again after a close and reopen", async () => {
    const getSessions = vi.fn().mockResolvedValue({ sessions: [session()] });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(
      <AgentSettingsModal {...modalProps({ executionMode: "auto", apiClient })} />,
    );
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(1));

    rerender(
      <AgentSettingsModal {...modalProps({ executionMode: "auto", apiClient, isOpen: false })} />,
    );
    expect(getSessions).toHaveBeenCalledTimes(1);

    rerender(
      <AgentSettingsModal {...modalProps({ executionMode: "auto", apiClient })} />,
    );
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(2));
  });

  it("does not refetch when an unrelated prop changes while open", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ model: "claude-sonnet-5-5", thinking_level: "low" })],
    });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(badge()).toHaveTextContent("claude-sonnet-5-5 (low)"));

    rerender(
      <AgentSettingsModal
        {...modalProps({ apiClient, contextEstimate: { usedTokens: 5, maxTokens: 10, percent: 50 } })}
      />,
    );
    rerender(<AgentSettingsModal {...modalProps({ apiClient: clientWith(getSessions) })} />);
    expect(getSessions).toHaveBeenCalledTimes(1);
    expect(badge()).toHaveTextContent("claude-sonnet-5-5 (low)");
  });

  it("shows the model, effort and mode that changed on the backend between two opens", async () => {
    const getSessions = vi
      .fn()
      .mockResolvedValueOnce({
        sessions: [
          session({ model: "claude-sonnet-5-5", thinking_level: "low", execution_mode: "plan" }),
        ],
      })
      .mockResolvedValueOnce({
        sessions: [
          session({ model: "claude-opus-5-5", thinking_level: "high", execution_mode: "yolo" }),
        ],
      });
    const apiClient = clientWith(getSessions);
    const props = { apiClient, executionMode: "auto" };
    const { rerender } = render(<AgentSettingsModal {...modalProps(props)} />);

    // Props are the initial values until the fetch lands.
    expect(badge()).toHaveTextContent("claude · claude-haiku-4-5 (medium)");
    expect(mode()).toHaveTextContent("Auto");
    await waitFor(() => expect(badge()).toHaveTextContent("claude · claude-sonnet-5-5 (low)"));
    expect(mode()).toHaveTextContent("Plan");

    rerender(<AgentSettingsModal {...modalProps({ ...props, isOpen: false })} />);
    rerender(<AgentSettingsModal {...modalProps(props)} />);
    // Stale fetched state is gone on reopen: the props show until the new fetch lands.
    expect(badge()).toHaveTextContent("claude-haiku-4-5 (medium)");
    await waitFor(() => expect(badge()).toHaveTextContent("claude · claude-opus-5-5 (high)"));
    expect(mode()).toHaveTextContent("Yolo");
  });

  it("lets a prop that changed after the fetch (an operator switch) win over the fetched value", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ model: "claude-sonnet-5-5", thinking_level: "low" })],
    });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(badge()).toHaveTextContent("claude-sonnet-5-5 (low)"));

    rerender(
      <AgentSettingsModal
        {...modalProps({
          apiClient,
          selectedModel: "claude-opus-5-5",
          selectedThinkingLevel: "high",
        })}
      />,
    );
    expect(badge()).toHaveTextContent("claude-opus-5-5 (high)");
  });

  it("ignores a response for a previous target that lands after the target changed", async () => {
    let resolveFirst: (value: unknown) => void = () => {};
    const first = new Promise((resolve) => {
      resolveFirst = resolve;
    });
    const getSessions = vi
      .fn()
      .mockReturnValueOnce(first)
      .mockResolvedValueOnce({
        sessions: [
          session({ id: "sess-2", persona: "other", model: "claude-opus-5-5", thinking_level: "high" }),
        ],
      });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(1));

    rerender(
      <AgentSettingsModal
        {...modalProps({ apiClient, sessionId: "sess-2", agentName: "other" })}
      />,
    );
    await waitFor(() => expect(badge()).toHaveTextContent("claude-opus-5-5 (high)"));

    await act(async () => {
      resolveFirst({
        sessions: [session({ model: "claude-sonnet-5-5", thinking_level: "low" })],
      });
      await first;
    });
    expect(badge()).toHaveTextContent("claude-opus-5-5 (high)");
    expect(badge()).not.toHaveTextContent("claude-sonnet-5-5");
  });

  it("renders the props and does not crash without an apiClient or getSessions", () => {
    const { unmount } = render(<AgentSettingsModal {...modalProps({ executionMode: "plan" })} />);
    expect(badge()).toHaveTextContent("claude · claude-haiku-4-5 (medium)");
    expect(mode()).toHaveTextContent("Plan");
    unmount();

    render(<AgentSettingsModal {...modalProps({ apiClient: {} as unknown as HubApiClient })} />);
    expect(badge()).toHaveTextContent("claude · claude-haiku-4-5 (medium)");
  });
});

describe("AgentSettingsModal trusts the session adapter over the model id", () => {
  const antigravityModels = ["claude-sonnet-5-5", "claude-opus-5-5", "gpt-oss-120b-medium"];

  it.each(antigravityModels)("keeps an antigravity session on %s as Antigravity", (model) => {
    render(
      <AgentSettingsModal
        {...modalProps({ adapter: "antigravity", selectedModel: model })}
      />,
    );
    expect(badge()).toHaveTextContent(`antigravity · ${model}`);
    const options = modelSelectOptions();
    expect(options).toContain("gemini-4-argon");
    expect(options).not.toContain("claude-haiku-4-5");
    expect(options).not.toContain("gpt-5.4");
  });

  it("takes an antigravity adapter that only the session list reports", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ adapter: "antigravity", model: "claude-opus-5-5" })],
    });
    render(
      <AgentSettingsModal
        {...modalProps({
          adapter: "",
          selectedModel: "claude-opus-5-5",
          apiClient: clientWith(getSessions),
        })}
      />,
    );
    await waitFor(() => expect(badge()).toHaveTextContent("antigravity · claude-opus-5-5"));
    expect(modelSelectOptions()).toContain("gemini-4-argon");
  });

  it.each([
    ["", "claude-sonnet-5-5", "claude · claude-sonnet-5-5"],
    ["unknown", "claude-sonnet-5-5", "claude · claude-sonnet-5-5"],
    ["unknown", "grok-4.7", "grok · grok-4.7"],
  ])("still infers the vendor from the model for adapter %j", (adapter, model, text) => {
    render(<AgentSettingsModal {...modalProps({ adapter, selectedModel: model })} />);
    expect(badge()).toHaveTextContent(text);
  });
});

describe("vendor resolution outside the modal", () => {
  it("lets a known adapter decide and infers only for a missing or unknown adapter", () => {
    expect(resolveSessionVendor("antigravity", "claude-sonnet-5-5-high")).toBe("antigravity");
    expect(resolveSessionVendor("antigravity", "gpt-oss-120b-low")).toBe("antigravity");
    expect(resolveSessionVendor("", "claude-sonnet-5-5")).toBe("claude");
    expect(resolveSessionVendor("unknown", "gpt-5.4")).toBe("codex");
    expect(resolveSessionVendor(null, "mystery")).toBe("");
  });

  it("offers the Antigravity model list for Antigravity sessions holding Claude or GPT-OSS ids", () => {
    for (const model of ["claude-sonnet-5-5", "claude-opus-5-5", "gpt-oss-120b-high"]) {
      const options = getVendorModelOptions("antigravity", model);
      expect(options).toContain("gemini-4-argon");
      expect(options).not.toContain("claude-haiku-4-5");
    }
    expect(getVendorModelOptions("", "claude-sonnet-5-5")).toContain("claude-haiku-4-5");
    expect(getVendorModelOptions("unknown", "claude-sonnet-5-5")).toContain("claude-haiku-4-5");
  });

  it("ChatPane shows an antigravity session with a Claude or GPT-OSS model as Antigravity", () => {
    for (const model of ["claude-sonnet-5-5", "claude-opus-5-5", "gpt-oss-120b-medium"]) {
      const { unmount } = render(
        <ChatPane
          agentName="ferris"
          agentDisplayName="Ferris"
          adapter="antigravity"
          model={model}
          sessionId="sess-agy"
          turns={[]}
          agentSettingsOpen={true}
          onSendPrompt={vi.fn()}
        />,
      );
      expect(badge()).toHaveTextContent(`antigravity · ${model}`);
      expect(modelSelectOptions()).toContain("gemini-4-argon");
      expect(modelSelectOptions()).not.toContain("claude-haiku-4-5");
      unmount();
    }
  });
});

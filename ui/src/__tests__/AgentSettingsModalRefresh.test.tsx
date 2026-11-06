import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, act, fireEvent } from "@testing-library/react";
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

function deferred() {
  let resolve: (value: unknown) => void = () => {};
  const promise = new Promise((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

const modelSelect = () => screen.getByTestId("session-model-select") as HTMLSelectElement;
const thinkingSelect = () => screen.getByTestId("session-thinking-select") as HTMLSelectElement;

describe("AgentSettingsModal latches an operator choice over a fetched session value", () => {
  it("keeps a model and mode picked while the request was in flight over the late response", async () => {
    const late = deferred();
    const getSessions = vi.fn().mockReturnValue(late.promise);
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(1));

    rerender(
      <AgentSettingsModal {...modalProps({ apiClient, selectedModel: "claude-opus-5-5" })} />,
    );
    await act(async () => {
      late.resolve({
        sessions: [
          session({ model: "claude-sonnet-5-5", thinking_level: "low", execution_mode: "plan" }),
        ],
      });
      await late.promise;
    });
    // The unrelated, untouched fields prove the response landed.
    await waitFor(() => expect(mode()).toHaveTextContent("Plan"));
    expect(badge()).toHaveTextContent("claude-opus-5-5 (low)");
    expect(badge()).not.toHaveTextContent("claude-sonnet-5-5");
    expect(modelSelect().value).toBe("claude-opus-5-5");
  });

  it("keeps a pick made after a rapid close and reopen when the first, stale response lands last", async () => {
    const first = deferred();
    const second = deferred();
    const getSessions = vi
      .fn()
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(1));
    rerender(<AgentSettingsModal {...modalProps({ apiClient, isOpen: false })} />);
    rerender(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(2));

    // The operator picks a model while both requests are still in flight, and the parent
    // passes the choice back as the prop.
    fireEvent.change(modelSelect(), { target: { value: "claude-opus-5-5" } });
    rerender(
      <AgentSettingsModal {...modalProps({ apiClient, selectedModel: "claude-opus-5-5" })} />,
    );
    expect(modelSelect().value).toBe("claude-opus-5-5");

    await act(async () => {
      second.resolve({
        sessions: [session({ model: "claude-sonnet-5-5", execution_mode: "plan" })],
      });
      await second.promise;
    });
    // The current response landed (its mode shows) and the pick survives it.
    await waitFor(() => expect(mode()).toHaveTextContent("Plan"));
    expect(modelSelect().value).toBe("claude-opus-5-5");

    // The stale response of the first open lands last and is ignored.
    await act(async () => {
      first.resolve({
        sessions: [session({ model: "claude-haiku-4-5", execution_mode: "yolo" })],
      });
      await first.promise;
    });
    expect(modelSelect().value).toBe("claude-opus-5-5");
    expect(badge()).toHaveTextContent("claude-opus-5-5");
    expect(badge()).not.toHaveTextContent("claude-haiku-4-5");
    expect(mode()).toHaveTextContent("Plan");
    expect(mode()).not.toHaveTextContent("Yolo");
  });

  it("keeps a thinking level picked while the request was in flight over the late response", async () => {
    const late = deferred();
    const getSessions = vi.fn().mockReturnValue(late.promise);
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(1));

    rerender(<AgentSettingsModal {...modalProps({ apiClient, selectedThinkingLevel: "high" })} />);
    await act(async () => {
      late.resolve({
        sessions: [session({ model: "claude-sonnet-5-5", thinking_level: "low" })],
      });
      await late.promise;
    });
    await waitFor(() => expect(badge()).toHaveTextContent("claude-sonnet-5-5"));
    expect(badge()).toHaveTextContent("claude-sonnet-5-5 (high)");
  });

  it("keeps an execution mode and adapter changed while the request was in flight", async () => {
    const late = deferred();
    const getSessions = vi.fn().mockReturnValue(late.promise);
    const apiClient = clientWith(getSessions);
    const { rerender } = render(
      <AgentSettingsModal {...modalProps({ apiClient, executionMode: "auto" })} />,
    );
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(1));

    rerender(
      <AgentSettingsModal
        {...modalProps({ apiClient, executionMode: "yolo", adapter: "codex", selectedModel: "gpt-5.4" })}
      />,
    );
    await act(async () => {
      late.resolve({
        sessions: [
          session({ adapter: "claude", execution_mode: "plan", thinking_level: "low" }),
        ],
      });
      await late.promise;
    });
    await waitFor(() => expect(badge()).toHaveTextContent("(low)"));
    expect(mode()).toHaveTextContent("Yolo");
    expect(badge()).toHaveTextContent("codex · gpt-5.4");
  });

  it("keeps the prop when it rolls back to its old value after the fetch landed (model)", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ model: "claude-sonnet-5-5" })],
    });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(badge()).toHaveTextContent("claude-sonnet-5-5"));

    rerender(
      <AgentSettingsModal {...modalProps({ apiClient, selectedModel: "claude-opus-5-5" })} />,
    );
    expect(badge()).toHaveTextContent("claude-opus-5-5");
    rerender(<AgentSettingsModal {...modalProps({ apiClient })} />);
    expect(badge()).toHaveTextContent("claude-haiku-4-5");
    expect(badge()).not.toHaveTextContent("claude-sonnet-5-5");
    expect(modelSelect().value).toBe("claude-haiku-4-5");
  });

  it("keeps the prop when it rolls back to its old value after the fetch landed (thinking level)", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ thinking_level: "low" })],
    });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(badge()).toHaveTextContent("(low)"));

    rerender(<AgentSettingsModal {...modalProps({ apiClient, selectedThinkingLevel: "high" })} />);
    expect(badge()).toHaveTextContent("(high)");
    rerender(<AgentSettingsModal {...modalProps({ apiClient })} />);
    expect(badge()).toHaveTextContent("(medium)");
    expect(thinkingSelect().value).toBe("medium");
  });

  it("shows the model the operator picks in the modal even when it equals the prop", async () => {
    const onChange = vi.fn();
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ model: "claude-sonnet-5-5" })],
    });
    const apiClient = clientWith(getSessions);
    render(
      <AgentSettingsModal {...modalProps({ apiClient, onChangeModelAndThinking: onChange })} />,
    );
    await waitFor(() => expect(modelSelect().value).toBe("claude-sonnet-5-5"));

    fireEvent.change(modelSelect(), { target: { value: "claude-haiku-4-5" } });
    expect(onChange).toHaveBeenCalledWith("claude-haiku-4-5", "medium");
    // The parent's setSelectedModel is a no-op here, so the prop never changes.
    expect(badge()).toHaveTextContent("claude-haiku-4-5");
    expect(badge()).not.toHaveTextContent("claude-sonnet-5-5");
    expect(modelSelect().value).toBe("claude-haiku-4-5");
  });

  it("shows the thinking level the operator picks in the modal even when it equals the prop", async () => {
    const onChange = vi.fn();
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ thinking_level: "low" })],
    });
    const apiClient = clientWith(getSessions);
    render(
      <AgentSettingsModal {...modalProps({ apiClient, onChangeModelAndThinking: onChange })} />,
    );
    await waitFor(() => expect(thinkingSelect().value).toBe("low"));

    fireEvent.change(thinkingSelect(), { target: { value: "medium" } });
    expect(onChange).toHaveBeenCalledWith("claude-haiku-4-5", "medium");
    expect(badge()).toHaveTextContent("(medium)");
    expect(thinkingSelect().value).toBe("medium");
  });

  it("clears the latch when the modal closes and reopens, so a new backend value shows", async () => {
    const getSessions = vi
      .fn()
      .mockResolvedValueOnce({ sessions: [session({ model: "claude-sonnet-5-5" })] })
      .mockResolvedValueOnce({ sessions: [session({ model: "claude-opus-5-5" })] });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(modelSelect().value).toBe("claude-sonnet-5-5"));
    fireEvent.change(modelSelect(), { target: { value: "claude-haiku-4-5" } });
    expect(modelSelect().value).toBe("claude-haiku-4-5");

    rerender(<AgentSettingsModal {...modalProps({ apiClient, isOpen: false })} />);
    rerender(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(modelSelect().value).toBe("claude-opus-5-5"));
  });

  it("clears the latch when the target changes", async () => {
    const getSessions = vi
      .fn()
      .mockResolvedValueOnce({ sessions: [session({ model: "claude-sonnet-5-5" })] })
      .mockResolvedValueOnce({
        sessions: [session({ id: "sess-2", persona: "other", model: "claude-opus-5-5" })],
      });
    const apiClient = clientWith(getSessions);
    const { rerender } = render(<AgentSettingsModal {...modalProps({ apiClient })} />);
    await waitFor(() => expect(modelSelect().value).toBe("claude-sonnet-5-5"));
    fireEvent.change(modelSelect(), { target: { value: "claude-haiku-4-5" } });
    expect(modelSelect().value).toBe("claude-haiku-4-5");

    rerender(
      <AgentSettingsModal {...modalProps({ apiClient, sessionId: "sess-2", agentName: "other" })} />,
    );
    await waitFor(() => expect(modelSelect().value).toBe("claude-opus-5-5"));
  });

  it("still shows the fetched value over a prop-only value, and ignores identical re-renders", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ model: "claude-sonnet-5-5", thinking_level: "low", execution_mode: "plan" })],
    });
    const apiClient = clientWith(getSessions);
    const props = { apiClient, executionMode: "auto" };
    const { rerender } = render(<AgentSettingsModal {...modalProps(props)} />);
    await waitFor(() => expect(badge()).toHaveTextContent("claude-sonnet-5-5 (low)"));
    expect(mode()).toHaveTextContent("Plan");

    rerender(<AgentSettingsModal {...modalProps(props)} />);
    rerender(<AgentSettingsModal {...modalProps(props)} />);
    expect(badge()).toHaveTextContent("claude-sonnet-5-5 (low)");
    expect(mode()).toHaveTextContent("Plan");
  });

  it("shows the props for a null or empty fetched value and for a rejected fetch", async () => {
    const getSessions = vi.fn().mockResolvedValue({
      sessions: [session({ model: null, thinking_level: "  ", execution_mode: "" })],
    });
    const apiClient = clientWith(getSessions);
    const { unmount } = render(
      <AgentSettingsModal {...modalProps({ apiClient, executionMode: "auto" })} />,
    );
    await waitFor(() => expect(getSessions).toHaveBeenCalledTimes(1));
    await act(async () => {
      await Promise.resolve();
    });
    expect(badge()).toHaveTextContent("claude · claude-haiku-4-5 (medium)");
    expect(mode()).toHaveTextContent("Auto");
    unmount();

    const rejecting = vi.fn().mockRejectedValue(new Error("hub down"));
    render(
      <AgentSettingsModal
        {...modalProps({ apiClient: clientWith(rejecting), executionMode: "auto" })}
      />,
    );
    await waitFor(() => expect(rejecting).toHaveBeenCalledTimes(1));
    await act(async () => {
      await Promise.resolve();
    });
    expect(badge()).toHaveTextContent("claude · claude-haiku-4-5 (medium)");
    expect(mode()).toHaveTextContent("Auto");
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

describe("resolveSessionVendor adapter precedence", () => {
  it.each([
    ["antigravity", "claude-sonnet-5-5", "antigravity"],
    ["antigravity", "gpt-oss-120b", "antigravity"],
    ["claude", "gpt-oss-120b", "claude"],
    ["claude", "gemini-4-argon", "claude"],
    ["codex", "claude-sonnet-5-5", "codex"],
    ["unknown", "claude-sonnet-5-5", "claude"],
    ["unknown", "gpt-5.4", "codex"],
    ["", "claude-opus-5-5", "claude"],
    ["", "grok-4.7", "grok"],
  ])("resolves adapter %j with model %j to %j", (adapter, model, expected) => {
    expect(resolveSessionVendor(adapter, model)).toBe(expected);
  });

  it("offers the options of the known adapter, not of a conflicting model id", () => {
    expect(getVendorModelOptions("antigravity", "claude-sonnet-5-5")).toContain("gemini-4-argon");
    expect(getVendorModelOptions("antigravity", "claude-sonnet-5-5")).not.toContain("claude-haiku-4-5");
    expect(getVendorModelOptions("claude", "gpt-oss-120b")).toContain("claude-haiku-4-5");
    expect(getVendorModelOptions("claude", "gpt-oss-120b")).not.toContain("gemini-4-argon");
    expect(getVendorModelOptions("unknown", "claude-sonnet-5-5")).toContain("claude-haiku-4-5");
    expect(getVendorModelOptions("", "claude-sonnet-5-5")).toContain("claude-haiku-4-5");
  });
});

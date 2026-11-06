import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import "@testing-library/jest-dom/vitest";
import {
  AgentSettingsModal,
  type AgentSettingsModalProps,
} from "../components/AgentSettingsModal";
import { estimateSessionContextUsage } from "../components/ChatPane";
import { HubApiClient, HubApiError } from "../api/client";
import type {
  HubAuthority,
  SessionContextUsage,
  SetSessionContextWindowResponse,
} from "../types/api";

const OWNER: HubAuthority = { tier: "owner", login: "lmcfarlin" };
const PEER: HubAuthority = { tier: "peer", agent: "ferris" };
const USAGE: SessionContextUsage = {
  used_tokens: 10_000,
  max_tokens: 256_000,
  source: "reported",
  offered_windows: [256_000, 500_000],
};

function modalProps(
  usage: SessionContextUsage | null = USAGE,
  overrides: Partial<AgentSettingsModalProps> = {},
): AgentSettingsModalProps {
  return {
    isOpen: true,
    onClose: () => {},
    agentName: "ferris",
    agentDisplayName: "Ferris",
    sessionId: "sess-1",
    adapter: "grok",
    selectedModel: "grok-5-code",
    selectedThinkingLevel: "high",
    onChangeModelAndThinking: () => {},
    contextEstimate: estimateSessionContextUsage([], usage),
    canCompact: false,
    compacting: false,
    onCompactContext: () => {},
    canClear: true,
    clearing: false,
    onOpenClearModal: () => {},
    canDecommission: true,
    decommissioning: false,
    onOpenDecommissionModal: () => {},
    authority: OWNER,
    ...overrides,
  };
}

function response(
  overrides: Partial<SetSessionContextWindowResponse> = {},
): SetSessionContextWindowResponse {
  return {
    ok: true,
    session_id: "sess-1",
    vendor: "grok",
    context_window: 500_000,
    persisted: true,
    method: "operator-prompt",
    ...overrides,
  };
}

function clientWith(setSessionContextWindow: unknown): HubApiClient {
  return { setSessionContextWindow } as unknown as HubApiClient;
}

const select = () => screen.getByTestId("context-window-select") as HTMLSelectElement;
const queryControl = () => screen.queryByTestId("context-window-select");
const status = () => screen.getByTestId("context-window-status");
const readonly = () => screen.getByTestId("context-window-readonly");
const pick = (size: number) => fireEvent.change(select(), { target: { value: String(size) } });
const optionValues = () => Array.from(select().options).map((o) => o.value);

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

describe("Agent Settings context window selector: who gets a control", () => {
  const vendors = ["claude", "codex", "muse", "antigravity", "grok"] as const;
  const authorities: Array<[string, HubAuthority | null]> = [
    ["owner", OWNER],
    ["peer", PEER],
    ["unknown", null],
  ];
  const offers: Array<[string, number[] | undefined]> = [
    ["none", undefined],
    ["one size", [256_000]],
    ["two sizes", [256_000, 500_000]],
  ];

  for (const vendor of vendors) {
    for (const [authorityName, authority] of authorities) {
      for (const [offerName, offered] of offers) {
        const expected = vendor === "grok" && authorityName === "owner" && offerName === "two sizes";
        it(`${expected ? "shows" : "hides"} the control for ${vendor} / ${authorityName} / ${offerName}`, () => {
          render(
            <AgentSettingsModal
              {...modalProps(
                { used_tokens: 10_000, max_tokens: 256_000, source: "reported", offered_windows: offered },
                { adapter: vendor, authority, selectedModel: "" },
              )}
            />,
          );
          if (expected) {
            expect(queryControl()).toBeInTheDocument();
            expect(screen.queryByTestId("context-window-readonly")).not.toBeInTheDocument();
          } else {
            expect(queryControl()).not.toBeInTheDocument();
            expect(screen.queryByRole("combobox", { name: "Context window" })).not.toBeInTheDocument();
            expect(readonly()).toBeInTheDocument();
          }
        });
      }
    }
  }

  it("takes the vendor from the model when the adapter is unknown", () => {
    render(
      <AgentSettingsModal
        {...modalProps(USAGE, { adapter: "unknown", selectedModel: "grok-5-code" })}
      />,
    );
    expect(select()).toBeInTheDocument();
  });

  it("needs a session id", () => {
    render(<AgentSettingsModal {...modalProps(USAGE, { sessionId: null })} />);
    expect(queryControl()).not.toBeInTheDocument();
  });
});

describe("Agent Settings context window selector: the control", () => {
  it("lists the offered sizes and selects the effective one, with no empty option", () => {
    render(<AgentSettingsModal {...modalProps()} />);
    expect(optionValues()).toEqual(["256000", "500000"]);
    expect(Array.from(select().options).map((o) => o.textContent)).toEqual(["256k", "500k"]);
    expect(select()).toHaveValue("256000");
    expect(screen.getByRole("combobox", { name: "Context window" })).toBe(select());
    expect(screen.getByTestId("context-window-effective")).toHaveTextContent("256k");
    expect(screen.queryByTestId("context-window-waiting")).not.toBeInTheDocument();
  });

  it("follows the effective window and marks an assumed one", () => {
    render(
      <AgentSettingsModal
        {...modalProps({ ...USAGE, max_tokens: 500_000, source: "assumed" })}
      />,
    );
    expect(select()).toHaveValue("500000");
    expect(screen.getByTestId("context-window-effective")).toHaveTextContent("about 500k, assumed");
  });

  it("keeps the existing meter and offered-sizes text", () => {
    render(<AgentSettingsModal {...modalProps()} />);
    expect(screen.getByTestId("context-level-meter")).toHaveTextContent("Context: 10k / 256k (4%)");
    expect(screen.getByTestId("context-offered-windows")).toHaveTextContent("Offered: 256k, 500k");
  });

  it("sends exactly { context_window: 500000 } to the session's context-window URL", async () => {
    const fetchImpl = vi.fn(async () => new Response(JSON.stringify(response()), { status: 200 }));
    const apiClient = new HubApiClient({
      baseUrl: "http://127.0.0.1:8420",
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient, sessionId: "sess/1 a" })} />);
    pick(500_000);
    await waitFor(() => expect(fetchImpl).toHaveBeenCalledTimes(1));
    const [url, init] = fetchImpl.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("http://127.0.0.1:8420/api/v1/sessions/sess%2F1%20a/context-window");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({ context_window: 500_000 });
  });

  it("disables the control and sets aria-busy while sending, then returns focus to it", async () => {
    const d = deferred<SetSessionContextWindowResponse>();
    const send = vi.fn(() => d.promise);
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    select().focus();
    pick(500_000);
    expect(select()).toBeDisabled();
    expect(select()).toHaveAttribute("aria-busy", "true");
    await act(async () => {
      d.resolve(response());
    });
    expect(select()).toBeEnabled();
    expect(select()).toHaveAttribute("aria-busy", "false");
    expect(select()).toHaveFocus();
  });

  it("describes the select by the status line", async () => {
    const send = vi.fn().mockResolvedValue(response());
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    expect(select().getAttribute("aria-describedby")).toContain(status().id);
    expect(status()).toHaveAttribute("role", "status");
  });

  it("says a prompt was sent and that the session reports the window it uses", async () => {
    const send = vi.fn().mockResolvedValue(response());
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    pick(500_000);
    await waitFor(() =>
      expect(status()).toHaveTextContent(
        "Requested 500k. Sent to the session; it reports the window it actually uses.",
      ),
    );
    expect(status()).not.toHaveTextContent("Not saved");
    expect(screen.getByTestId("context-window-waiting")).toHaveTextContent(
      "waiting for the session to report it",
    );
    expect(send).toHaveBeenCalledWith("sess-1", 500_000);
  });

  it("warns that a prompt that was not persisted will not survive a restart", async () => {
    const send = vi.fn().mockResolvedValue(response({ persisted: false }));
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    pick(500_000);
    await waitFor(() =>
      expect(status()).toHaveTextContent(
        "Requested 500k. Sent to the session; it reports the window it actually uses. Not saved: it will not survive a restart.",
      ),
    );
  });

  it("says a stored size was saved but not applied now", async () => {
    const send = vi.fn().mockResolvedValue(response({ method: "stored" }));
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    pick(500_000);
    await waitFor(() =>
      expect(status()).toHaveTextContent(
        "Saved for this agent. The session's terminal was not reachable, so it was not applied now.",
      ),
    );
    expect(screen.queryByTestId("context-window-waiting")).not.toBeInTheDocument();
  });

  it.each([
    [400, "context window 123 is not offered; offered sizes: 256000, 500000"],
    [403, "owner authority required"],
    [404, "session not found"],
    [409, "not applied: the session has no record"],
  ])("shows the server's %i text in an alert and restores the previous selection", async (code, text) => {
    const send = vi.fn().mockRejectedValue(new HubApiError(code, text));
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    pick(500_000);
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(text);
    expect(select()).toHaveValue("256000");
    expect(select()).toBeEnabled();
    expect(select().getAttribute("aria-describedby")).toContain(alert.id);
    expect(status()).toHaveTextContent("");
  });

  it("shows a network failure in an alert", async () => {
    const send = vi.fn().mockRejectedValue(new TypeError("Failed to fetch"));
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    pick(500_000);
    expect(await screen.findByRole("alert")).toHaveTextContent("Failed to fetch");
    expect(select()).toHaveValue("256000");
  });

  it("clears an old alert when the operator tries again", async () => {
    const send = vi
      .fn()
      .mockRejectedValueOnce(new HubApiError(409, "not applied"))
      .mockResolvedValueOnce(response());
    render(<AgentSettingsModal {...modalProps(USAGE, { apiClient: clientWith(send) })} />);
    pick(500_000);
    await screen.findByRole("alert");
    pick(500_000);
    await waitFor(() => expect(screen.queryByRole("alert")).not.toBeInTheDocument());
    await waitFor(() => expect(status()).toHaveTextContent("Requested 500k"));
  });
});

describe("Agent Settings context window selector: the latch", () => {
  it("keeps the pick in flight and after a late stale update, then follows the server", async () => {
    const d = deferred<SetSessionContextWindowResponse>();
    const send = vi.fn(() => d.promise);
    const props = modalProps(USAGE, { apiClient: clientWith(send) });
    const { rerender } = render(<AgentSettingsModal {...props} />);
    pick(500_000);
    expect(select()).toHaveValue("500000");

    // A stale report (still 256k) arrives while the request is in flight.
    rerender(<AgentSettingsModal {...props} contextEstimate={estimateSessionContextUsage([], { ...USAGE })} />);
    expect(select()).toHaveValue("500000");

    await act(async () => {
      d.resolve(response());
    });
    expect(select()).toHaveValue("500000");
    expect(screen.getByTestId("context-window-waiting")).toBeInTheDocument();
    expect(screen.getByTestId("context-window-effective")).toHaveTextContent("256k");

    // Another stale report after the answer.
    rerender(
      <AgentSettingsModal
        {...props}
        contextEstimate={estimateSessionContextUsage([], { ...USAGE, used_tokens: 20_000 })}
      />,
    );
    expect(select()).toHaveValue("500000");

    // The session now reports 500k: follow the server and drop the hint.
    rerender(
      <AgentSettingsModal
        {...props}
        contextEstimate={estimateSessionContextUsage([], { ...USAGE, max_tokens: 500_000 })}
      />,
    );
    expect(select()).toHaveValue("500000");
    expect(screen.queryByTestId("context-window-waiting")).not.toBeInTheDocument();

    // From here on the server leads, even when it goes back.
    rerender(<AgentSettingsModal {...props} />);
    expect(select()).toHaveValue("256000");
  });

  it("follows a different window than the one requested as soon as the hub reports it", async () => {
    const send = vi.fn().mockResolvedValue(response());
    const props = modalProps(USAGE, { apiClient: clientWith(send) });
    const { rerender } = render(<AgentSettingsModal {...props} />);
    pick(500_000);
    await waitFor(() => expect(status()).toHaveTextContent("Requested 500k"));
    rerender(
      <AgentSettingsModal
        {...props}
        contextEstimate={estimateSessionContextUsage([], {
          ...USAGE,
          max_tokens: 1_000_000,
          offered_windows: [256_000, 500_000, 1_000_000],
        })}
      />,
    );
    expect(select()).toHaveValue("1000000");
    expect(screen.queryByTestId("context-window-waiting")).not.toBeInTheDocument();
  });

  it("rolls back to the effective window after an error even when a stale update arrives", async () => {
    const d = deferred<SetSessionContextWindowResponse>();
    const send = vi.fn(() => d.promise);
    const props = modalProps(USAGE, { apiClient: clientWith(send) });
    const { rerender } = render(<AgentSettingsModal {...props} />);
    pick(500_000);
    expect(select()).toHaveValue("500000");
    await act(async () => {
      d.reject(new HubApiError(409, "not applied"));
    });
    expect(select()).toHaveValue("256000");
    rerender(<AgentSettingsModal {...props} />);
    expect(select()).toHaveValue("256000");
  });

  it("drops the pick and the status when the modal is reopened", async () => {
    const send = vi.fn().mockResolvedValue(response());
    const props = modalProps(USAGE, { apiClient: clientWith(send) });
    const { rerender } = render(<AgentSettingsModal {...props} />);
    pick(500_000);
    await waitFor(() => expect(status()).toHaveTextContent("Requested 500k"));
    rerender(<AgentSettingsModal {...props} isOpen={false} />);
    rerender(<AgentSettingsModal {...props} />);
    expect(select()).toHaveValue("256000");
    expect(status()).toHaveTextContent("");
  });

  it("ignores an answer that arrives after the modal moved to another session", async () => {
    const d = deferred<SetSessionContextWindowResponse>();
    const send = vi.fn(() => d.promise);
    const props = modalProps(USAGE, { apiClient: clientWith(send) });
    const { rerender } = render(<AgentSettingsModal {...props} />);
    pick(500_000);
    rerender(<AgentSettingsModal {...props} sessionId="sess-2" agentName="other" />);
    await act(async () => {
      d.resolve(response());
    });
    expect(select()).toHaveValue("256000");
    expect(select()).toBeEnabled();
    expect(status()).toHaveTextContent("");
  });
});

describe("Agent Settings context window: read-only lines", () => {
  it.each([
    ["claude", "Claude"],
    ["codex", "Codex"],
    ["muse", "Muse"],
    ["antigravity", "Antigravity"],
  ])("says %s's window cannot be selected from FerroDeck", (adapter, name) => {
    render(
      <AgentSettingsModal
        {...modalProps(
          { used_tokens: 1_000, max_tokens: 200_000, source: "reported" },
          { adapter, selectedModel: "" },
        )}
      />,
    );
    expect(readonly()).toHaveTextContent(
      `Context window: 200k. ${name}'s window cannot be selected from FerroDeck.`,
    );
    expect(queryControl()).not.toBeInTheDocument();
  });

  it("folds an antigravity alias into the vendor name", () => {
    render(
      <AgentSettingsModal
        {...modalProps(
          { used_tokens: 1_000, max_tokens: 1_000_000, source: "assumed" },
          { adapter: "agy", selectedModel: "" },
        )}
      />,
    );
    expect(readonly()).toHaveTextContent(
      "Context window: about 1m, assumed. Antigravity's window cannot be selected from FerroDeck.",
    );
  });

  it("says a Grok session with one size has no other sizes", () => {
    render(
      <AgentSettingsModal
        {...modalProps({ ...USAGE, offered_windows: [256_000] })}
      />,
    );
    expect(readonly()).toHaveTextContent("Context window: 256k (no other sizes offered)");
  });

  it("says a Grok session with no offered sizes has no other sizes", () => {
    render(
      <AgentSettingsModal
        {...modalProps({ ...USAGE, offered_windows: undefined })}
      />,
    );
    expect(readonly()).toHaveTextContent("Context window: 256k (no other sizes offered)");
  });

  it("shows no control and no claim about sizes to a peer", () => {
    render(<AgentSettingsModal {...modalProps(USAGE, { authority: PEER })} />);
    expect(queryControl()).not.toBeInTheDocument();
    expect(readonly()).toHaveTextContent("Context window: 256k. Only an owner can change it.");
    expect(readonly()).not.toHaveTextContent("no other sizes");
  });

  it("says so when no window is reported", () => {
    render(<AgentSettingsModal {...modalProps(null, { adapter: "claude", selectedModel: "" })} />);
    expect(readonly()).toHaveTextContent("Context window: not reported.");
  });
});

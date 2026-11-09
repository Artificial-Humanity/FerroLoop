import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../lib/api";
import type { HubWsClient } from "../api/ws";
import { SpinUpAgentModal, SPIN_UP_ROLES } from "../components/SpinUpAgentModal";
import {
  canonicalizeDefaultRole,
  CREATE_CUSTOM_ROLE_VALUE,
  normalizeRecastRole,
  RECAST_ROLE_OPTIONS,
} from "../components/SettingsModal";
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
    expect(
      Array.from(roleSelect.options).map((o) => o.value),
    ).toContain("strategist");
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
        posture: "auto",
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

  it("offers the three real Muse 1.4.14 model IDs and labels when muse vendor is selected", () => {
    render(<SpinUpAgentModal isOpen onClose={() => {}} />);

    const vendorSelect = screen.getByTestId("spin-up-vendor-select") as HTMLSelectElement;
    fireEvent.change(vendorSelect, { target: { value: "muse" } });

    const datalist = screen.getByTestId("spin-up-model-options");
    const options = Array.from(datalist.querySelectorAll("option")).map((opt) => ({
      value: opt.getAttribute("value"),
      label: opt.getAttribute("label"),
      text: opt.textContent,
    }));

    expect(options).toEqual([
      {
        value: "muse-wonder-full-1.0",
        label: "Muse Wonder Full (muse-wonder-full-1.0)",
        text: "Muse Wonder Full (muse-wonder-full-1.0)",
      },
      {
        value: "muse-wonder-1.0",
        label: "Muse Wonder (muse-wonder-1.0)",
        text: "Muse Wonder (muse-wonder-1.0)",
      },
      {
        value: "muse-spark-1.3",
        label: "Muse Spark (muse-spark-1.3)",
        text: "Muse Spark (muse-spark-1.3)",
      },
    ]);
  });

  it("offers all 11 default SPIN_UP_ROLES, matching RECAST_ROLE_OPTIONS, and Codex 6.1 models at the top of Codex options", () => {
    render(<SpinUpAgentModal isOpen onClose={() => {}} />);

    expect(SPIN_UP_ROLES).toEqual([
      { value: "assistant", label: "assistant (Assisting Engineer)" },
      { value: "ui-developer", label: "ui-developer (UI & Full-Stack)" },
      { value: "web-developer", label: "web-developer (Web Developer)" },
      { value: "mobile-developer", label: "mobile-developer (Mobile Developer)" },
      { value: "system-developer", label: "system-developer (System / Backend Developer)" },
      { value: "data-engineer", label: "data-engineer (Data Engineer)" },
      { value: "tester", label: "tester (QA & Tester)" },
      { value: "strategist", label: "strategist (Research & Architecture Strategist)" },
      { value: "journalist", label: "journalist (Journalist / Technical Scribe)" },
      { value: "lead", label: "lead (Project Lead)" },
      { value: "lab-manager", label: "lab-manager (Lab Manager)" },
    ]);

    const roleSelect = screen.getByTestId("spin-up-role-select") as HTMLSelectElement;
    expect(
      Array.from(roleSelect.options).map((o) => ({
        value: o.value,
        label: o.textContent,
      })),
    ).toEqual([...SPIN_UP_ROLES]);

    expect(RECAST_ROLE_OPTIONS).toEqual([
      { value: "lead", label: "lead" },
      { value: "assistant", label: "assistant" },
      { value: "lab_manager", label: "lab_manager" },
      { value: "ui-developer", label: "ui-developer" },
      { value: "web-developer", label: "web-developer" },
      { value: "mobile-developer", label: "mobile-developer" },
      { value: "system-developer", label: "system-developer" },
      { value: "data-engineer", label: "data-engineer" },
      { value: "tester", label: "tester" },
      { value: "strategist", label: "strategist" },
      { value: "journalist", label: "journalist" },
    ]);

    expect(canonicalizeDefaultRole("custom:strategist")).toBe("strategist");
    expect(canonicalizeDefaultRole("custom:ui-developer")).toBe("ui-developer");
    expect(canonicalizeDefaultRole("scribe")).toBe("journalist");
    expect(canonicalizeDefaultRole("custom:scribe")).toBe("journalist");
    expect(canonicalizeDefaultRole("lab-manager")).toBe("lab_manager");
    expect(canonicalizeDefaultRole("custom:reviewer")).toBe("custom:reviewer");
    expect(normalizeRecastRole("custom:strategist", "")).toBe("strategist");
    expect(normalizeRecastRole("scribe", "")).toBe("journalist");
    expect(normalizeRecastRole(CREATE_CUSTOM_ROLE_VALUE, "reviewer")).toBe(
      "custom:reviewer",
    );
    expect(normalizeRecastRole(CREATE_CUSTOM_ROLE_VALUE, "scribe")).toBe(
      "journalist",
    );
    expect(normalizeRecastRole(CREATE_CUSTOM_ROLE_VALUE, "strategist")).toBe(
      "strategist",
    );

    const vendorSelect = screen.getByTestId("spin-up-vendor-select") as HTMLSelectElement;
    fireEvent.change(vendorSelect, { target: { value: "codex" } });

    const datalist = screen.getByTestId("spin-up-model-options");
    const codexOptions = Array.from(datalist.querySelectorAll("option")).map((opt) => ({
      value: opt.getAttribute("value"),
      label: opt.getAttribute("label"),
      text: opt.textContent,
    }));

    expect(codexOptions.slice(0, 2)).toEqual([
      {
        value: "gpt-6.1-terra",
        label: "Codex Terra 6.1 (gpt-6.1-terra)",
        text: "Codex Terra 6.1 (gpt-6.1-terra)",
      },
      {
        value: "gpt-6.1-sol",
        label: "Codex Sol 6.1 (gpt-6.1-sol)",
        text: "Codex Sol 6.1 (gpt-6.1-sol)",
      },
    ]);
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

    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    fireEvent.click(screen.getByTestId("decommission-teardown-btn"));
    // Decommission asks for a second confirmation before it stops the session (Record #128).
    expect(screen.getByTestId("decommission-confirm-dialog")).toBeInTheDocument();
    expect(screen.getByText("Active session turn before teardown")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("decommission-confirm-submit-btn"));

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
          posture: "auto",
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

  it("re-fetches /api/v1/sessions on window focus, visibilitychange, and agent switch so vendor CLI model changes update RosterSidebar immediately", async () => {
    let currentModel = "claude-opus-5-5";
    let sessionsFetchCount = 0;

    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
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
      if (url.endsWith("/api/v1/agents")) {
        return json({
          agents: [
            {
              name_key: "ferris",
              display_name: "Ferris",
              project: "FerroLoop",
              role: "lead",
            },
            {
              name_key: "felix",
              display_name: "Felix",
              project: "FerroLoop",
              role: "ui-developer",
            },
          ],
          routes: [
            {
              id: "sess-ferris-live",
              persona: "ferris",
              adapter: "claude",
              model: "claude-opus-5-5",
              status: "running",
            },
          ],
        });
      }
      if (url.endsWith("/api/v1/sessions")) {
        sessionsFetchCount += 1;
        return json({
          sessions: [
            {
              id: "sess-ferris-live",
              persona: "ferris",
              adapter: "claude",
              model: currentModel,
              status: "running",
            },
          ],
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

    await waitFor(() => {
      expect(sessionsFetchCount).toBeGreaterThanOrEqual(1);
    });

    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    expect(await screen.findByTestId("chat-active-model-badge")).toHaveTextContent(
      /claude · claude-opus-5-5/i,
    );

    // Simulate user changing model in Claude CLI to claude-sonnet-5-5 and focusing FerroDeck window
    currentModel = "claude-sonnet-5-5";
    fireEvent(window, new Event("focus"));

    await waitFor(() => {
      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        /claude · claude-sonnet-5-5/i,
      );
      expect(
        (screen.getByTestId("session-model-select") as HTMLSelectElement).value,
      ).toBe("claude-sonnet-5-5");
    });

    // Simulate switching agent in RosterSidebar after another model change
    currentModel = "claude-haiku-4-5";
    const beforeSwitchCount = sessionsFetchCount;
    fireEvent.click(screen.getByTestId("agent-row-felix"));

    await waitFor(() => {
      expect(sessionsFetchCount).toBeGreaterThan(beforeSwitchCount);
    });

    fireEvent.click(screen.getByTestId("agent-row-ferris"));
    await waitFor(() => {
      expect(screen.getByTestId("chat-active-model-badge")).toHaveTextContent(
        /claude · claude-haiku-4-5/i,
      );
    });

    unmount();
  });

  it("defaults permission posture to auto, requires explicit choice for bypass, resets on reopen, and surfaces posture rejection errors", async () => {
    const onSpinUp = vi.fn(
      async (req: SpinUpSessionRequest): Promise<SpinUpSessionResponse> => {
        if (req.vendor === "antigravity" && req.posture === "accept-edits") {
          throw new Error(
            "Unsupported permission posture 'accept-edits' for vendor 'antigravity'",
          );
        }
        return {
          session_id: "sess-felix-382",
          persona: req.persona,
          project: req.project,
          role: req.role,
          vendor: req.vendor,
          model: req.model ?? null,
          posture: req.posture,
          node_id: req.node_id ?? "ai-lab-0",
          hosted: Boolean(req.hosted),
          status: "running",
          hydration_dispatched: true,
        };
      },
    );
    const onClose = vi.fn();

    const { rerender, unmount } = render(
      <SpinUpAgentModal isOpen onClose={onClose} onSpinUp={onSpinUp} />,
    );

    const postureSelect = screen.getByTestId(
      "spin-up-posture-select",
    ) as HTMLSelectElement;
    expect(postureSelect.value).toBe("auto");
    expect(Array.from(postureSelect.options).map((o) => o.value)).toEqual([
      "ask",
      "accept-edits",
      "auto",
      "plan",
      "bypass",
    ]);

    // Explicitly choose bypass and verify reopening resets back to auto
    fireEvent.change(postureSelect, { target: { value: "bypass" } });
    expect(postureSelect.value).toBe("bypass");

    rerender(<SpinUpAgentModal isOpen={false} onClose={onClose} onSpinUp={onSpinUp} />);
    rerender(<SpinUpAgentModal isOpen onClose={onClose} onSpinUp={onSpinUp} />);

    const reopenedPostureSelect = screen.getByTestId(
      "spin-up-posture-select",
    ) as HTMLSelectElement;
    expect(reopenedPostureSelect.value).toBe("auto");

    // Surface server error when unsupported vendor+posture is rejected
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.change(screen.getByTestId("spin-up-vendor-select"), {
      target: { value: "antigravity" },
    });
    fireEvent.change(reopenedPostureSelect, {
      target: { value: "accept-edits" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /Unsupported permission posture 'accept-edits' for vendor 'antigravity'/i,
    );
    expect(onClose).not.toHaveBeenCalled();

    // Explicit bypass selection submits posture: "bypass"
    fireEvent.change(reopenedPostureSelect, {
      target: { value: "bypass" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpinUp).toHaveBeenLastCalledWith(
        expect.objectContaining({
          persona: "felix",
          vendor: "antigravity",
          posture: "bypass",
        }),
      );
    });
    expect(onClose).toHaveBeenCalledTimes(1);

    unmount();
  });

  it("normalizes hydration_status in SpinUpAgentModal and renders session-hydration-status-badge in App for unconfirmed, failed, and confirmed states (#383 D2b)", async () => {
    const onSpunUp = vi.fn();
    let nextStatus: "unconfirmed" | "failed" | "confirmed" | undefined =
      undefined;

    const onSpinUp = vi.fn(
      async (req: SpinUpSessionRequest): Promise<SpinUpSessionResponse> => ({
        session_id: `sess-${req.persona}`,
        persona: req.persona,
        project: req.project,
        role: req.role,
        vendor: req.vendor,
        model: req.model ?? null,
        posture: req.posture,
        node_id: req.node_id ?? "ai-lab-0",
        hosted: Boolean(req.hosted),
        status: "running",
        hydration_dispatched: nextStatus !== "failed",
        hydration_status: nextStatus,
      }),
    );

    const { unmount: unmountModal } = render(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        onSpinUp={onSpinUp}
        onSpunUp={onSpunUp}
      />,
    );

    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "fiona" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpunUp).toHaveBeenCalledWith(
        expect.objectContaining({
          persona: "fiona",
          hydration_dispatched: true,
          hydration_status: "unconfirmed",
        }),
      );
    });
    const modalHydrationBadge = await screen.findByTestId(
      "spin-up-hydration-status",
    );
    expect(modalHydrationBadge).toHaveAttribute(
      "data-hydration-status",
      "unconfirmed",
    );
    expect(modalHydrationBadge).toHaveAttribute(
      "data-hydration-dispatched",
      "true",
    );
    expect(modalHydrationBadge).toHaveTextContent(
      /Hydration: unconfirmed \(dispatched: true\)/i,
    );
    unmountModal();

    // Verify App session toolbar badge reflects unconfirmed (neutral) and failed (warning) hydration_status
    let spinUpHydrationStatus: "unconfirmed" | "failed" | "confirmed" =
      "unconfirmed";
    let hostedHydrationStatus: "unconfirmed" | "failed" | "confirmed" =
      "unconfirmed";

    const fetchImpl = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit) => {
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
        if (url.endsWith("/api/v1/agents")) {
          return json({
            agents: [
              {
                name_key: "felix",
                display_name: "Felix",
                project: "FerroLoop",
                role: "ui-developer",
              },
            ],
            routes: [],
          });
        }
        if (url.endsWith("/api/v1/hosted-sessions")) {
          return json({
            sessions: [
              {
                session_id: "claude:felix",
                persona: "felix",
                project: "ferroloop",
                role: "ui-developer",
                vendor: "claude",
                status: "running",
                restart_count: 0,
                hydration_dispatched: hostedHydrationStatus !== "failed",
                hydration_status: hostedHydrationStatus,
              },
            ],
          });
        }
        if (url.endsWith("/api/v1/sessions/spin-up") && method === "POST") {
          const body = JSON.parse(String(init?.body));
          hostedHydrationStatus = spinUpHydrationStatus;
          return json({
            session_id: "claude:felix",
            persona: body.persona,
            project: body.project,
            role: body.role,
            vendor: body.vendor,
            model: body.model ?? null,
            node_id: body.node_id ?? "ai-lab-0",
            hosted: Boolean(body.hosted),
            status: "running",
            hydration_dispatched: spinUpHydrationStatus !== "failed",
            hydration_status: spinUpHydrationStatus,
          });
        }
        return json({});
      },
    );

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

    const { unmount } = render(
      <App
        apiClient={apiClient}
        wsClient={wsStub}
        initialSelectedAgentName="felix"
      />,
    );

    const sessionBadge = await screen.findByTestId(
      "session-hydration-status-badge",
    );
    expect(sessionBadge).toHaveAttribute(
      "data-hydration-status",
      "unconfirmed",
    );
    expect(sessionBadge).toHaveAttribute("data-hydration-dispatched", "true");
    expect(sessionBadge).toHaveTextContent("Hydration: unconfirmed");
    expect(sessionBadge).not.toHaveAttribute("role", "status");

    // Spin up with hydration_status: "failed" and verify warning badge in session view
    spinUpHydrationStatus = "failed";
    fireEvent.click(screen.getByTestId("spin-up-agent-btn"));
    expect(
      await screen.findByTestId("spin-up-agent-modal"),
    ).toBeInTheDocument();
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      const updatedBadge = screen.getByTestId("session-hydration-status-badge");
      expect(updatedBadge).toHaveAttribute("data-hydration-status", "failed");
      expect(updatedBadge).toHaveAttribute(
        "data-hydration-dispatched",
        "false",
      );
      expect(updatedBadge).toHaveAttribute("role", "status");
      expect(updatedBadge).toHaveTextContent("Hydration: failed");
    });

    unmount();
  });
});

import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../lib/api";
import type { HubWsClient } from "../api/ws";
import {
  resolveSpinUpProject,
  SpinUpAgentModal,
  SPIN_UP_ROLES,
} from "../components/SpinUpAgentModal";
import {
  canonicalizeDefaultRole,
  CREATE_CUSTOM_ROLE_VALUE,
  normalizeRecastRole,
  RECAST_ROLE_OPTIONS,
} from "../components/SettingsModal";
import type {
  EnrolledNode,
  SpinUpSessionRequest,
  SpinUpSessionResponse,
} from "../types/api";
import { TEST_PROJECTS } from "./fixtures/testAgents";

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

    render(
      <SpinUpAgentModal
        isOpen
        onClose={onClose}
        onSpinUp={onSpinUp}
        projects={TEST_PROJECTS}
      />,
    );

    expect(screen.getByTestId("spin-up-agent-modal")).toBeInTheDocument();

    const personaInput = screen.getByTestId("spin-up-persona-input") as HTMLInputElement;
    const projectSelect = screen.getByTestId("spin-up-project-select") as HTMLSelectElement;
    const roleSelect = screen.getByTestId("spin-up-role-select") as HTMLSelectElement;
    const vendorSelect = screen.getByTestId("spin-up-vendor-select") as HTMLSelectElement;
    const modelInput = screen.getByTestId("spin-up-model-input") as HTMLInputElement;
    const nodeSelect = screen.getByTestId("spin-up-node-select") as HTMLInputElement;
    const hostedCheckbox = screen.getByTestId("spin-up-hosted-checkbox") as HTMLInputElement;

    expect(projectSelect.value).toBe("ai-lab-amd");
    expect(roleSelect.value).toBe("assistant");
    expect(
      Array.from(roleSelect.options).map((o) => o.value),
    ).toContain("strategist");
    expect(vendorSelect.value).toBe("claude");
    expect(nodeSelect.value).toBe("");
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

  it("omits node_id when Target Node is blank and seeds Target Node from defaultNodeId or the hub node in nodes", async () => {
    const onSpinUp = vi.fn(
      async (req: SpinUpSessionRequest): Promise<SpinUpSessionResponse> => ({
        session_id: "sess-node-test",
        persona: req.persona,
        project: req.project,
        role: req.role,
        vendor: req.vendor,
        model: req.model ?? null,
        node_id: req.node_id ?? "hub-default-node",
        hosted: Boolean(req.hosted),
        status: "running",
        hydration_dispatched: true,
      }),
    );

    const { rerender, unmount } = render(
      <SpinUpAgentModal isOpen onClose={vi.fn()} projects={TEST_PROJECTS} onSpinUp={onSpinUp} />,
    );

    const nodeSelect = screen.getByTestId("spin-up-node-select") as HTMLInputElement;
    expect(nodeSelect.value).toBe("");
    const emptyDatalist = document.getElementById("fd-spin-up-node-options");
    expect(emptyDatalist?.querySelectorAll("option").length ?? 0).toBe(0);

    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "kacey" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpinUp).toHaveBeenCalledTimes(1);
    });
    expect(onSpinUp.mock.calls[0][0]).toEqual({
      persona: "kacey",
      project: "ai-lab-amd",
      role: "assistant",
      vendor: "claude",
      posture: "auto",
      hosted: true,
    });
    expect("node_id" in onSpinUp.mock.calls[0][0]).toBe(false);

    // Reopen with enrolled nodes (including a hub node) and verify hub node is seeded and submitted
    const nodes: EnrolledNode[] = [
      {
        node_id: "worker-mac-1",
        display_name: "Mac Worker",
        role: "worker",
        hostname: "mac-1",
        os: "macos",
        arch: "aarch64",
        capabilities: [],
        labels: [],
        status: "online",
        enrolled_at_ms: 1000,
        last_seen_at_ms: 2000,
      },
      {
        node_id: "hub-local-0",
        display_name: "Local Hub",
        role: "hub",
        hostname: "hub-local-0",
        os: "linux",
        arch: "x86_64",
        capabilities: [],
        labels: [],
        status: "online",
        enrolled_at_ms: 1000,
        last_seen_at_ms: 2000,
      },
    ];

    rerender(
      <SpinUpAgentModal
        isOpen={false}
        onClose={vi.fn()}
        projects={TEST_PROJECTS}
        nodes={nodes}
        onSpinUp={onSpinUp}
      />,
    );
    rerender(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        projects={TEST_PROJECTS}
        nodes={nodes}
        onSpinUp={onSpinUp}
      />,
    );

    const seededNodeSelect = screen.getByTestId("spin-up-node-select") as HTMLInputElement;
    expect(seededNodeSelect.value).toBe("hub-local-0");
    const populatedDatalist = document.getElementById("fd-spin-up-node-options");
    expect(
      Array.from(populatedDatalist?.querySelectorAll("option") ?? []).map((o) =>
        o.getAttribute("value"),
      ),
    ).toEqual(["worker-mac-1", "hub-local-0"]);

    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpinUp).toHaveBeenCalledTimes(2);
    });
    expect(onSpinUp.mock.calls[1][0]).toEqual({
      persona: "felix",
      project: "ai-lab-amd",
      role: "assistant",
      vendor: "claude",
      posture: "auto",
      node_id: "hub-local-0",
      hosted: true,
    });

    // Explicit defaultNodeId overrides nodes fallback on reopen
    rerender(
      <SpinUpAgentModal
        isOpen={false}
        onClose={vi.fn()}
        projects={TEST_PROJECTS}
        defaultNodeId="custom-hub-host"
        nodes={nodes}
        onSpinUp={onSpinUp}
      />,
    );
    rerender(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        projects={TEST_PROJECTS}
        defaultNodeId="custom-hub-host"
        nodes={nodes}
        onSpinUp={onSpinUp}
      />,
    );
    expect(
      (screen.getByTestId("spin-up-node-select") as HTMLInputElement).value,
    ).toBe("custom-hub-host");

    unmount();
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
      if (url.endsWith("/api/v1/projects")) {
        return json({ projects: TEST_PROJECTS });
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
      if (url.endsWith("/api/v1/projects")) {
        return json({ projects: TEST_PROJECTS });
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
      if (url.endsWith("/api/v1/projects")) {
        return json({ projects: TEST_PROJECTS });
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
      <SpinUpAgentModal isOpen onClose={onClose} projects={TEST_PROJECTS} onSpinUp={onSpinUp} />,
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

    rerender(<SpinUpAgentModal isOpen={false} onClose={onClose} projects={TEST_PROJECTS} onSpinUp={onSpinUp} />);
    rerender(<SpinUpAgentModal isOpen onClose={onClose} projects={TEST_PROJECTS} onSpinUp={onSpinUp} />);

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
        projects={TEST_PROJECTS}
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
        if (url.endsWith("/api/v1/projects")) {
        return json({ projects: TEST_PROJECTS });
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

  it("syncs hydration state and preserves or clears node_id/role without hardcoded fallbacks on WebSocket sessions lifecycle events", async () => {
    let hostedSessionsResponse: Array<Record<string, unknown>> = [
      {
        session_id: "claude:felix",
        persona: "felix",
        project: "ferroloop",
        role: "ui-developer",
        vendor: "claude",
        node_id: "worker-gpu-1",
        status: "running",
        restart_count: 0,
        updated_at_ms: 1000,
        hydration_dispatched: true,
        hydration_status: "unconfirmed",
      },
      {
        session_id: "claude:felix-old-decommissioned",
        persona: "felix",
        project: "ferroloop",
        role: "ui-developer",
        vendor: "claude",
        node_id: "worker-gpu-1",
        status: "decommissioned",
        restart_count: 0,
        updated_at_ms: 9999,
        hydration_dispatched: false,
        hydration_status: "failed",
      },
    ];

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
          sessions: hostedSessionsResponse,
        });
      }
      if (url.endsWith("/api/v1/projects")) {
        return json({ projects: TEST_PROJECTS });
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const topicSubscribers = new Map<
      string,
      Set<(frame: { event?: string; data?: unknown }) => void>
    >();
    const wsStub = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: (
        topic: string,
        cb: (frame: { event?: string; data?: unknown }) => void,
      ) => {
        if (!topicSubscribers.has(topic)) {
          topicSubscribers.set(topic, new Set());
        }
        topicSubscribers.get(topic)!.add(cb);
        return () => {
          topicSubscribers.get(topic)?.delete(cb);
        };
      },
    } as unknown as HubWsClient;

    const emitSessionsEvent = (event: string, data: Record<string, unknown>) => {
      act(() => {
        for (const cb of topicSubscribers.get("sessions") ?? []) {
          cb({ event, data });
        }
      });
    };

    const { unmount } = render(
      <App
        apiClient={apiClient}
        wsClient={wsStub}
        initialSelectedAgentName="felix"
      />,
    );

    // Initial state: decommissioned record is ignored, running record (unconfirmed) is selected
    const initialBadge = await screen.findByTestId(
      "session-hydration-status-badge",
    );
    expect(initialBadge).toHaveAttribute(
      "data-hydration-status",
      "unconfirmed",
    );
    expect(initialBadge).toHaveAttribute("data-hydration-dispatched", "true");

    // 1. mode === "prepare" is ignored and does not overwrite hydration state
    emitSessionsEvent("session.remodeled", {
      persona: "felix",
      vendor: "codex",
      session_id: "codex:felix",
      mode: "prepare",
      hosted: true,
      hydration_dispatched: false,
      hydration_status: "failed",
    });
    expect(
      screen.getByTestId("session-hydration-status-badge"),
    ).toHaveAttribute("data-hydration-status", "unconfirmed");

    // 2. session.remodeled (mode !== "prepare") updates spunUpByAgent and hostedSessionsByAgent,
    // and activeHydrationStatus prioritizes activeSpunUp over any stale hosted poll
    emitSessionsEvent("session.remodeled", {
      persona: "felix",
      vendor: "codex",
      model: "gpt-5.4",
      session_id: "codex:felix",
      mode: "execute",
      hosted: true,
      hydration_dispatched: false,
      hydration_status: "failed",
    });

    await waitFor(() => {
      const badge = screen.getByTestId("session-hydration-status-badge");
      expect(badge).toHaveAttribute("data-hydration-status", "failed");
      expect(badge).toHaveAttribute("data-hydration-dispatched", "false");
      const workspace = screen.getByTestId("cockpit-workspace");
      expect(workspace).toHaveAttribute(
        "data-active-hosted-node-id",
        "worker-gpu-1",
      );
      expect(workspace).toHaveAttribute(
        "data-active-hosted-role",
        "ui-developer",
      );
    });

    // 3. session.restarted updates hydration status to confirmed
    hostedSessionsResponse = [
      {
        session_id: "codex:felix",
        persona: "felix",
        project: "ferroloop",
        role: "ui-developer",
        vendor: "codex",
        node_id: "worker-gpu-1",
        status: "running",
        restart_count: 1,
        updated_at_ms: 2000,
        hydration_dispatched: true,
        hydration_status: "confirmed",
      },
    ];
    emitSessionsEvent("session.restarted", {
      persona: "felix",
      vendor: "codex",
      session_id: "codex:felix",
      hosted: true,
      hydration_dispatched: true,
      hydration_status: "confirmed",
    });

    await waitFor(() => {
      const badge = screen.getByTestId("session-hydration-status-badge");
      expect(badge).toHaveAttribute("data-hydration-status", "confirmed");
      expect(badge).toHaveAttribute("data-hydration-dispatched", "true");
    });

    // 4. session.decommission clears both spunUpByAgent and hostedSessionsByAgent
    hostedSessionsResponse = [
      {
        session_id: "codex:felix",
        persona: "felix",
        project: "ferroloop",
        role: "ui-developer",
        vendor: "codex",
        node_id: "worker-gpu-1",
        status: "decommissioned",
        restart_count: 1,
        updated_at_ms: 3000,
        hydration_dispatched: true,
        hydration_status: "confirmed",
      },
    ];
    emitSessionsEvent("session.decommission", {
      persona: "felix",
      session_id: "codex:felix",
      status: "decommissioned",
      mode: "teardown",
    });

    await waitFor(() => {
      expect(
        screen.queryByTestId("session-hydration-status-badge"),
      ).not.toBeInTheDocument();
    });

    // 5. Fresh session.spun_up without node_id or role falls back to empty string (not "ai-lab-0" or "assistant")
    hostedSessionsResponse = [];
    emitSessionsEvent("session.spun_up", {
      persona: "felix",
      vendor: "claude",
      session_id: "claude:felix-fresh",
      hosted: true,
      hydration_dispatched: true,
      hydration_status: "unconfirmed",
    });

    await waitFor(() => {
      const workspace = screen.getByTestId("cockpit-workspace");
      expect(workspace).toHaveAttribute("data-active-hosted-node-id", "");
      expect(workspace).toHaveAttribute("data-active-hosted-role", "");
    });

    unmount();
  });

  it("renders plain-text hydration warnings when present and omits them when empty or absent in SpinUpAgentModal and App (#419)", async () => {
    let nextWarnings: string[] | undefined = undefined;
    const onSpunUp = vi.fn();

    const fetchImpl = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? "GET";
        const json = (body: unknown) =>
          new Response(JSON.stringify(body), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });

        if (url.endsWith("/api/v1/sessions/spin-up") && method === "POST") {
          const body = JSON.parse(String(init?.body));
          return json({
            ...(nextWarnings !== undefined ? { warnings: nextWarnings } : {}),
            session_id: `sess-${body.persona}`,
            persona: body.persona,
            project: body.project,
            role: body.role,
            vendor: body.vendor,
            model: body.model ?? null,
            posture: body.posture,
            node_id: body.node_id ?? "ai-lab-0",
            hosted: Boolean(body.hosted),
            status: "running",
            hydration_dispatched: true,
            hydration_status: "confirmed",
          });
        }
        if (url.endsWith("/api/v1/projects")) {
        return json({ projects: TEST_PROJECTS });
      }
      return json({});
      },
    );

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    const { unmount: unmountModal } = render(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        projects={TEST_PROJECTS}
        apiClient={apiClient}
        onSpunUp={onSpunUp}
      />,
    );

    // 1. Absent warnings (undefined) -> normalized to [] and no warning list rendered
    nextWarnings = undefined;
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "fiona" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpunUp).toHaveBeenCalledWith(
        expect.objectContaining({
          persona: "fiona",
          hydration_status: "confirmed",
          warnings: [],
        }),
      );
    });
    expect(screen.getByTestId("spin-up-hydration-status")).toHaveAttribute(
      "data-hydration-status",
      "confirmed",
    );
    expect(screen.queryByTestId("spin-up-warnings")).not.toBeInTheDocument();

    // 2. Empty warnings ([]) -> no warning list rendered
    nextWarnings = [];
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "feira" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(onSpunUp).toHaveBeenLastCalledWith(
        expect.objectContaining({
          persona: "feira",
          hydration_status: "confirmed",
          warnings: [],
        }),
      );
    });
    expect(screen.queryByTestId("spin-up-warnings")).not.toBeInTheDocument();

    // 3. Present warnings -> rendered one line per item as plain text, while launch still reads as succeeded
    nextWarnings = [
      "role file not found: looked in /workspace/.agents/roles/FerroLoop/scribe.md",
      "<img src=x onerror=alert(1)> missing project WORKFLOW.md",
    ];
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    const warningsList = await screen.findByTestId("spin-up-warnings");
    expect(warningsList).toHaveAttribute("role", "status");
    const items = screen.getAllByTestId("spin-up-warning-item");
    expect(items).toHaveLength(2);
    expect(items[0]).toHaveTextContent(
      "role file not found: looked in /workspace/.agents/roles/FerroLoop/scribe.md",
    );
    expect(items[1].textContent).toBe(
      "<img src=x onerror=alert(1)> missing project WORKFLOW.md",
    );
    expect(items[1].querySelector("img")).toBeNull();
    expect(screen.getByTestId("spin-up-hydration-status")).toHaveAttribute(
      "data-hydration-status",
      "confirmed",
    );

    unmountModal();

    // 4. App surfaces spin-up warnings in session-hydration-warnings after modal closes
    let appSpinUpWarnings: string[] | undefined = [
      "cwd is outside project ferroloop",
      "<b>plain text warning</b>",
    ];
    const appFetchImpl = vi.fn(
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
          return json({ sessions: [] });
        }
        if (url.endsWith("/api/v1/sessions/spin-up") && method === "POST") {
          const body = JSON.parse(String(init?.body));
          return json({
            ...(appSpinUpWarnings !== undefined
              ? { warnings: appSpinUpWarnings }
              : {}),
            session_id: "claude:felix",
            persona: body.persona,
            project: body.project,
            role: body.role,
            vendor: body.vendor,
            model: body.model ?? null,
            node_id: body.node_id ?? "ai-lab-0",
            hosted: Boolean(body.hosted),
            status: "running",
            hydration_dispatched: true,
            hydration_status: "confirmed",
          });
        }
        if (url.endsWith("/api/v1/projects")) {
        return json({ projects: TEST_PROJECTS });
      }
      return json({});
      },
    );

    const appApiClient = new HubApiClient({
      fetchImpl: appFetchImpl as unknown as typeof fetch,
    });
    const wsStub = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: () => () => {},
    } as unknown as HubWsClient;

    const { unmount: unmountApp } = render(
      <App
        apiClient={appApiClient}
        wsClient={wsStub}
        initialSelectedAgentName="felix"
      />,
    );

    fireEvent.click(await screen.findByTestId("spin-up-agent-btn"));
    expect(
      await screen.findByTestId("spin-up-agent-modal"),
    ).toBeInTheDocument();
    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    const sessionWarnings = await screen.findByTestId(
      "session-hydration-warnings",
    );
    expect(sessionWarnings).toHaveAttribute("role", "status");
    const sessionWarningItems = screen.getAllByTestId(
      "session-hydration-warning-item",
    );
    expect(sessionWarningItems).toHaveLength(2);
    expect(sessionWarningItems[0]).toHaveTextContent(
      "cwd is outside project ferroloop",
    );
    expect(sessionWarningItems[1].textContent).toBe("<b>plain text warning</b>");
    expect(sessionWarningItems[1].querySelector("b")).toBeNull();
    expect(
      screen.getByTestId("session-hydration-status-badge"),
    ).toHaveAttribute("data-hydration-status", "confirmed");

    // Spin up again with absent warnings -> banner clears
    appSpinUpWarnings = undefined;
    fireEvent.click(screen.getByTestId("spin-up-agent-btn"));
    fireEvent.change(await screen.findByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    await waitFor(() => {
      expect(
        screen.queryByTestId("session-hydration-warnings"),
      ).not.toBeInTheDocument();
    });

    unmountApp();
  });

  it("leaves project unselected when projects is empty, maps initialProject to a real slug or unselected, and updates when projects arrives", async () => {
    expect(resolveSpinUpProject(null, [])).toBe("");
    expect(resolveSpinUpProject("ferroloop", [])).toBe("");
    expect(resolveSpinUpProject(null, TEST_PROJECTS)).toBe("ai-lab-amd");
    expect(resolveSpinUpProject("Sonora", TEST_PROJECTS)).toBe("sonora");
    expect(resolveSpinUpProject("nonexistent-project", TEST_PROJECTS)).toBe("");

    const onSpinUp = vi.fn(
      async (req: SpinUpSessionRequest): Promise<SpinUpSessionResponse> => ({
        session_id: "sess-felix-1",
        persona: req.persona,
        project: req.project,
        role: req.role,
        vendor: req.vendor,
        model: req.model ?? null,
        node_id: req.node_id ?? "ai-lab-0",
        hosted: Boolean(req.hosted),
        status: "running",
        hydration_dispatched: true,
      }),
    );

    const { rerender, unmount } = render(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        initialProject="Sonora"
        projects={[]}
        onSpinUp={onSpinUp}
      />,
    );

    const projectSelect = screen.getByTestId(
      "spin-up-project-select",
    ) as HTMLSelectElement;
    expect(projectSelect.value).toBe("");
    expect(
      Array.from(projectSelect.options).map((o) => o.value),
    ).toEqual([""]);

    fireEvent.change(screen.getByTestId("spin-up-persona-input"), {
      target: { value: "felix" },
    });
    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /select a project/i,
    );
    expect(onSpinUp).not.toHaveBeenCalled();

    // When projects arrive, the untampered project selection updates to match initialProject
    rerender(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        initialProject="Sonora"
        projects={TEST_PROJECTS}
        onSpinUp={onSpinUp}
      />,
    );

    await waitFor(() => {
      expect(projectSelect.value).toBe("sonora");
    });

    fireEvent.click(screen.getByTestId("spin-up-submit-btn"));
    await waitFor(() => {
      expect(onSpinUp).toHaveBeenCalledWith(
        expect.objectContaining({
          persona: "felix",
          project: "sonora",
        }),
      );
    });

    // Unknown initialProject stays unselected even when projects is populated
    rerender(
      <SpinUpAgentModal
        isOpen={false}
        onClose={vi.fn()}
        initialProject="unknown-proj"
        projects={TEST_PROJECTS}
        onSpinUp={onSpinUp}
      />,
    );
    rerender(
      <SpinUpAgentModal
        isOpen
        onClose={vi.fn()}
        initialProject="unknown-proj"
        projects={TEST_PROJECTS}
        onSpinUp={onSpinUp}
      />,
    );

    expect(
      (screen.getByTestId("spin-up-project-select") as HTMLSelectElement).value,
    ).toBe("");

    unmount();
  });
});

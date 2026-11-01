import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { AgentSettingsModal } from "../components/AgentSettingsModal";
import {
  ArtifactPreviewPane,
  formatVersionTime,
} from "../components/ArtifactPreviewPane";
import { FerroStatePane } from "../components/FerroStatePane";
import { SettingsModal } from "../components/SettingsModal";
import { TurnCard } from "../components/TurnCard";
import { VaultGatePane } from "../components/VaultGatePane";
import { WireMailPane } from "../components/WireMailPane";
import {
  formatTimestamp,
  getActiveTimeZone,
  normalizeTimeZone,
  setActiveTimeZone,
} from "../lib/datetime";
import { DEFAULT_HUB_SETTINGS, type HubSettings } from "../types/api";

describe("TimeZone option & timestamp formatting across FerroDeck+", () => {
  beforeEach(() => {
    window.localStorage.clear();
    setActiveTimeZone("UTC");
  });

  afterEach(() => {
    window.localStorage.clear();
    setActiveTimeZone("UTC");
  });

  it("normalizes timezones and formats UTC, IANA zones, local, and non-date strings cleanly", () => {
    expect(normalizeTimeZone("utc")).toBe("UTC");
    expect(normalizeTimeZone("local")).toBe("local");
    expect(normalizeTimeZone("America/New_York")).toBe("America/New_York");
    expect(normalizeTimeZone("../etc/passwd")).toBe("UTC");
    expect(normalizeTimeZone("/America/New_York")).toBe("UTC");
    expect(normalizeTimeZone("America/New_York/")).toBe("UTC");

    // Default UTC preserves clean ISO-8601 Z strings untouched
    expect(formatTimestamp("2026-10-27T14:30:00Z", "UTC")).toBe("2026-10-27T14:30:00Z");

    // Default UTC formats epoch ms as YYYY-MM-DD HH:MM:SS UTC
    const epochMs = Date.parse("2026-10-27T14:30:00Z");
    expect(formatTimestamp(epochMs, "UTC")).toBe("2026-10-27 14:30:00 UTC");
    expect(formatVersionTime(epochMs, "UTC")).toBe("2026-10-27 14:30:00 UTC");

    // IANA timezones convert both ISO strings and epoch ms
    // 2026-10-27T14:30:00Z is EDT (UTC-4) -> 10:30:00 and PDT (UTC-7) -> 07:30:00
    expect(formatTimestamp("2026-10-27T14:30:00Z", "America/New_York")).toMatch(
      /^2026-10-27 10:30:00/,
    );
    expect(formatTimestamp("2026-10-27T14:30:00Z", "America/Los_Angeles")).toMatch(
      /^2026-10-27 07:30:00/,
    );
    expect(formatVersionTime(epochMs, "America/New_York")).toMatch(
      /^2026-10-27 10:30:00/,
    );

    // Non-date strings pass through untouched
    expect(formatTimestamp("Never", "America/New_York")).toBe("Never");
    expect(formatTimestamp("now", "America/New_York")).toBe("now");
    expect(formatTimestamp(null, "America/New_York")).toBe("");
  });

  it("saves Time Zone in SettingsModal via PUT /api/v1/settings and updates active timezone", async () => {
    let savedPayload: Partial<HubSettings> | null = null;
    const initialSettings: HubSettings = {
      ...DEFAULT_HUB_SETTINGS,
      timezone: "UTC",
    };

    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      const json = (body: unknown) =>
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });

      if (url.endsWith("/api/v1/settings") && method === "GET") {
        return json(initialSettings);
      }
      if (url.endsWith("/api/v1/settings") && method === "PUT") {
        savedPayload = JSON.parse(String(init?.body)) as Partial<HubSettings>;
        return json(savedPayload);
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const onSettingsUpdated = vi.fn();

    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        onSettingsUpdated={onSettingsUpdated}
        apiClient={apiClient}
      />,
    );

    const tzSelect = (await screen.findByTestId(
      "settings-timezone-select",
    )) as HTMLSelectElement;
    await waitFor(() => {
      expect(tzSelect.value).toBe("UTC");
    });

    fireEvent.change(tzSelect, { target: { value: "America/New_York" } });
    fireEvent.click(screen.getByTestId("save-settings-btn"));

    expect(await screen.findByTestId("settings-saved-banner")).toHaveTextContent(
      /Settings saved/i,
    );
    expect(savedPayload).toMatchObject({
      timezone: "America/New_York",
    });
    expect(getActiveTimeZone()).toBe("America/New_York");
    expect(onSettingsUpdated).toHaveBeenCalledWith(
      expect.objectContaining({ timezone: "America/New_York" }),
    );
  });

  it("updates displayed timestamps in TurnCard, WireMailPane, FerroStatePane, VaultGatePane, and ArtifactPreviewPane when active timezone changes", async () => {
    const isoUtc = "2026-10-27T14:30:00Z";
    const epochMs = Date.parse(isoUtc);

    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const json = (body: unknown) =>
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      if (url.includes("/api/v1/artifacts/list")) {
        return json({
          artifacts: [
            {
              path: "/tmp/spec.md",
              name: "spec.md",
              kind: "markdown",
              size_bytes: 12,
              Archived: false,
            },
          ],
        });
      }
      if (url.includes("/api/v1/artifacts/preview")) {
        return json({
          path: "/tmp/spec.md",
          name: "spec.md",
          kind: "markdown",
          mime_type: "text/markdown",
          size_bytes: 12,
          content: "# Spec",
        });
      }
      if (url.includes("/api/v1/artifacts/versions")) {
        return json({
          path: "/tmp/spec.md",
          versions: [
            {
              version: 1,
              sha256: "abcdef1234567890",
              size_bytes: 12,
              recorded_at_ms: epochMs,
              is_current: true,
            },
          ],
        });
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <div>
        <TurnCard
          turn={{
            index: 1,
            role: "assistant",
            content: "Hello operator",
            tool_calls: [],
            created_at: isoUtc,
          }}
        />
        <WireMailPane
          selectedAgentName="felix"
          messages={[
            {
              message: {
                id: "m-1",
                thread_id: "t-1",
                sender_name: "ferris",
                to: "felix",
                subject: "Timezone check",
                body: "Verify display",
                urgency: "normal",
                kind: "direct",
                durable: true,
                created_at: isoUtc,
              },
              receipt: {
                message_id: "m-1",
                recipient_id: "r-felix",
                recipient_name: "felix",
                status: "acked",
                attempts: 1,
                updated_at: isoUtc,
                acked_at: isoUtc,
              },
            },
          ]}
        />
        <FerroStatePane
          entries={[
            {
              id: "st-1",
              scope: "workspace",
              concern: "memory",
              slug: "tz-note",
              updated_at: isoUtc,
              by: "felix",
              sensitive: false,
              ever_sensitive: false,
              head_revision: "1",
              payload: {
                memory: {
                  category: "note",
                  basis: "measured",
                  lifecycle: "active",
                  summary: "Timezone note",
                  body: "Body",
                  pinned: false,
                },
              },
            },
          ]}
        />
        <VaultGatePane
          secrets={[
            {
              name: "OPENAI_API_KEY",
              description: "Test key",
              allowed_hosts: ["api.openai.com"],
              allowed_methods: ["POST"],
              allowed_agents: ["*"],
              allow_private_network: false,
              created_at: isoUtc,
              updated_at: isoUtc,
              expires_at: isoUtc,
            },
          ]}
        />
        <ArtifactPreviewPane
          apiClient={apiClient}
          activePath="/tmp/spec.md"
        />
      </div>,
    );

    // Open ArtifactPreviewPane version history
    const historyBtn = await screen.findByTestId("artifact-history-toggle");
    fireEvent.click(historyBtn);

    // Initially in UTC
    expect(screen.getAllByText(isoUtc).length).toBeGreaterThanOrEqual(4);
    expect(await screen.findByText("2026-10-27 14:30:00 UTC")).toBeInTheDocument();

    // Switch active timezone to America/New_York (EDT = UTC-4 -> 2026-10-27 10:30:00)
    act(() => {
      setActiveTimeZone("America/New_York");
    });

    await waitFor(() => {
      const matches = screen.getAllByText(/^2026-10-27 10:30:00/);
      expect(matches.length).toBeGreaterThanOrEqual(5);
    });

    // Switch active timezone to America/Los_Angeles (PDT = UTC-7 -> 2026-10-27 07:30:00)
    act(() => {
      setActiveTimeZone("America/Los_Angeles");
    });

    await waitFor(() => {
      const matches = screen.getAllByText(/^2026-10-27 07:30:00/);
      expect(matches.length).toBeGreaterThanOrEqual(5);
    });
  });

  it("renders 3-tab SettingsModal (Agents, System, Auth) and supports agent re-casting with lead replacement confirmation", async () => {
    const json = (body: unknown, status = 200) =>
      new Response(JSON.stringify(body), {
        status,
        headers: { "Content-Type": "application/json" },
      });
    const recastRequests: Array<{ url: string; body: Record<string, unknown> }> = [];
    const sessionClearRequests: Array<{ url: string; body: Record<string, unknown> }> = [];
    const bulkActionRequests: Array<{ url: string; body: Record<string, unknown> }> = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = (init?.method ?? "GET").toUpperCase();
      if (url.endsWith("/api/v1/settings") && method === "GET") {
        return json({
          theme: "dark",
          timezone: "UTC",
          poll_interval_secs: 5,
          enable_ferrogate: true,
          enable_ferrostate: true,
          enable_ferrostep: false,
          notifications: {
            notify_urgent_wire: true,
            notify_gate_approvals: true,
            notify_session_errors: true,
          },
          installed_products: [],
        });
      }
      if (url.endsWith("/api/v1/auth/operators")) {
        return json({ operators: [], mfa_require_step_up_for_gate: true });
      }
      if (url.endsWith("/api/v1/cloud/adapters")) {
        return json({ adapters: [] });
      }
      if (url.endsWith("/api/v1/sessions/bulk-action") && method === "POST") {
        const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        bulkActionRequests.push({ url, body });
        return json({ action: body.action ?? "reforge", total: 2, succeeded: 2, results: [] });
      }
      if (url.endsWith("/api/v1/sessions/felix/clear") && method === "POST") {
        const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        sessionClearRequests.push({ url, body });
        return json({ status: "ok", mode: body.mode ?? "reforge" });
      }
      if (url.endsWith("/api/v1/agents/felix/recast") && method === "POST") {
        const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        recastRequests.push({ url, body });
        if (!body.replace_lead) {
          return json(
            {
              error: "Project FerroLoop already has lead Ferris; pass replace_lead=true to confirm",
            },
            409,
          );
        }
        return json({
          status: "recast",
          agent: {
            name_key: "felix",
            display_name: "Felix",
            project: String(body.project ?? "FerroLoop"),
            role: String(body.role ?? "lead"),
            adapter: "antigravity",
          },
          retires_lead: "ferris",
        });
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const onSelectCockpitTab = vi.fn();
    const onClose = vi.fn();

    render(
      <SettingsModal
        isOpen={true}
        onClose={onClose}
        apiClient={apiClient}
        agents={[
          {
            id: "ag-ferris",
            name_key: "ferris",
            display_name: "Ferris",
            project: "FerroLoop",
            role: "lead",
            status: "idle",
          },
          {
            id: "ag-felix",
            name_key: "felix",
            display_name: "Felix",
            project: "FerroLoop",
            role: "ui-developer",
            status: "idle",
          },
        ]}
        onSelectCockpitTab={onSelectCockpitTab}
      />,
    );

    // 3-tab bar and "<N> Agents" heading
    expect(screen.getByTestId("settings-tab-bar")).toBeInTheDocument();
    expect(screen.getByTestId("settings-tab-agents")).toHaveTextContent("2 Agents");
    expect(screen.getByTestId("settings-agents-heading")).toHaveTextContent("2 Agents");
    expect(screen.getByTestId("settings-tab-system")).toHaveTextContent("System");
    expect(screen.getByTestId("settings-tab-auth")).toHaveTextContent("Auth");

    // Re-cast felix -> lead via Re-Cast Modal triggers 409 confirmation first, then succeeds with replace_lead: true
    fireEvent.click(screen.getByTestId("recast-agent-btn-felix"));
    expect(screen.getByTestId("recast-modal")).toBeInTheDocument();
    const roleSelect = screen.getByTestId("recast-role-select-felix") as HTMLSelectElement;
    fireEvent.change(roleSelect, { target: { value: "lead" } });
    fireEvent.click(screen.getByTestId("recast-submit-btn-felix"));

    const confirmBtn = await screen.findByTestId("recast-confirm-lead-btn-felix");
    expect(confirmBtn).toBeInTheDocument();
    fireEvent.click(confirmBtn);

    await waitFor(() => {
      expect(recastRequests.length).toBe(2);
      expect(recastRequests[0].body.replace_lead).toBe(false);
      expect(recastRequests[0].body.create_area).toBe(false);
      expect(recastRequests[1].body.replace_lead).toBe(true);
      expect(recastRequests[1].body.create_area).toBe(false);
    });

    // Re-cast with "+ Create custom role..." auto-prefixes "custom:"
    fireEvent.click(screen.getByTestId("recast-agent-btn-felix"));
    fireEvent.change(screen.getByTestId("recast-role-select-felix"), {
      target: { value: "__create_custom__" },
    });
    const customInput = screen.getByTestId("recast-custom-role-input-felix");
    fireEvent.change(customInput, { target: { value: "reviewer" } });
    fireEvent.click(screen.getByTestId("recast-cancel-btn-felix"));
    expect(screen.queryByTestId("recast-modal")).toBeNull();

    // Switch to System tab and launch Vault & Gate console
    fireEvent.click(screen.getByTestId("settings-tab-system"));
    fireEvent.click(screen.getByTestId("settings-open-gate-btn"));
    expect(onSelectCockpitTab).toHaveBeenCalledWith("gate");
    expect(onClose).toHaveBeenCalled();

    // Multi-select checkboxes and Select All / Clear Selection update bulk action labels
    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    expect(screen.getByTestId("bulk-compact-btn")).toHaveTextContent("Compact All");
    fireEvent.click(screen.getByTestId("agent-select-checkbox-felix"));
    expect(screen.getByTestId("settings-selected-agents-count")).toHaveTextContent("1 selected");
    expect(screen.getByTestId("bulk-compact-btn")).toHaveTextContent("Compact Selected (1)");
    expect(screen.getByTestId("bulk-clear-btn")).toHaveTextContent("Clear Selected (1)");
    expect(screen.getByTestId("bulk-reforge-btn")).toHaveTextContent("Reforge Selected (1)");

    // Confirming Reforge Selected (1) executes ONLY for selected agent (felix) and does NOT call /api/v1/sessions/bulk-action
    fireEvent.click(screen.getByTestId("bulk-reforge-btn"));
    expect(screen.getByTestId("bulk-confirm-btn")).toHaveTextContent(
      "Confirm Reforge Selected (1)",
    );
    fireEvent.click(screen.getByTestId("bulk-confirm-btn"));
    await waitFor(() => {
      expect(sessionClearRequests.length).toBe(1);
      expect(sessionClearRequests[0].body.mode).toBe("reforge");
      expect(bulkActionRequests.length).toBe(0);
    });

    // Select All (2) -> Clear Selection
    fireEvent.click(screen.getByTestId("settings-agents-select-all-btn"));
    expect(screen.getByTestId("settings-selected-agents-count")).toHaveTextContent("2 selected");
    expect(screen.getByTestId("bulk-compact-btn")).toHaveTextContent("Compact Selected (2)");
    fireEvent.click(screen.getByTestId("settings-agents-select-all-btn"));
    expect(screen.getByTestId("bulk-compact-btn")).toHaveTextContent("Compact All");

    // Project filter renders scoped Project Summary Card with Pin Project button
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), {
      target: { value: "FerroLoop" },
    });
    expect(screen.getByTestId("settings-project-summary-card")).toBeInTheDocument();
    expect(screen.getByTestId("settings-project-summary-name")).toHaveTextContent("FerroLoop");
    expect(screen.getByTestId("settings-project-summary-lead")).toHaveTextContent("Lead: Ferris");
    expect(screen.getByTestId("session-cwd-badge")).toHaveTextContent(
      "~/Projects/Artificial-Humanity/FerroLoop",
    );
    expect(screen.getByTestId("project-pin-toggle-btn")).toHaveAttribute(
      "aria-label",
      "Pin Project",
    );
    expect(screen.getByTestId("bulk-action-prepare-btn")).toBeInTheDocument();
    expect(screen.getByTestId("bulk-action-teardown-btn")).toBeInTheDocument();
  });

  it("omits System and Auth tabs when SettingsModal is opened from a Project cog (initialProject non-null) and matches project case-insensitively with scoped agent counts", () => {
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        initialProject="ferroloop"
        agents={[
          {
            id: "ag-ferris",
            name_key: "ferris",
            display_name: "Ferris",
            project: "FerroLoop",
            role: "lead",
            status: "idle",
          },
          {
            id: "ag-felix",
            name_key: "felix",
            display_name: "Felix",
            project: "FerroLoop",
            role: "ui-developer",
            status: "idle",
          },
          {
            id: "ag-jarvis",
            name_key: "jarvis",
            display_name: "JARVIS",
            project: "AI-Lab-AMD",
            role: "lab_manager",
            status: "idle",
          },
        ]}
      />,
    );

    // Tab and heading display scoped filtered count (2 Agents) instead of total fleet count (3 Agents)
    expect(screen.getByTestId("settings-tab-agents")).toHaveTextContent("2 Agents");
    expect(screen.getByTestId("settings-agents-heading")).toHaveTextContent("2 Agents");
    // Case-insensitive initialProject="ferroloop" selects "FerroLoop" in dropdown
    expect(
      (screen.getByTestId("settings-project-filter-select") as HTMLSelectElement).value,
    ).toBe("FerroLoop");
    // Top button says "+ New Agent" and redundant project spin-up button is removed
    expect(screen.getByTestId("settings-spin-up-btn")).toHaveTextContent("+ New Agent");
    expect(screen.queryByTestId("settings-project-spin-up-btn")).toBeNull();
    expect(screen.queryByTestId("settings-tab-system")).toBeNull();
    expect(screen.queryByTestId("settings-tab-auth")).toBeNull();
    expect(screen.queryByTestId("settings-panel-system")).toBeNull();
    expect(screen.queryByTestId("settings-panel-auth")).toBeNull();
    expect(screen.getByTestId("settings-project-summary-card")).toBeInTheDocument();

    // Toggling project pin renders pin-icon-slash when pinned
    const pinBtn = screen.getByTestId("project-pin-toggle-btn");
    expect(pinBtn.querySelector('[data-testid="pin-icon-slash"]')).toBeNull();
    fireEvent.click(pinBtn);
    expect(pinBtn).toHaveAttribute("aria-label", "Unpin Project");
    expect(pinBtn.querySelector('[data-testid="pin-icon-slash"]')).not.toBeNull();
    fireEvent.click(pinBtn);
  });

  it("supports Screenshot & Attachment Retention schedule and on-demand prune / purge actions in System tab", async () => {
    const pruneCalls: Array<Record<string, unknown>> = [];
    let savedPatch: Record<string, unknown> | null = null;
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = (init?.method ?? "GET").toUpperCase();
      if (url.endsWith("/api/v1/settings") && method === "PUT") {
        savedPatch = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        return new Response(
          JSON.stringify({
            theme_preference: "dark",
            timezone: "UTC",
            attachment_retention_days: savedPatch.attachment_retention_days ?? 14,
            subsystems: {
              ferrogate_enabled: true,
              ferrostate_enabled: true,
              ferrostep_enabled: true,
            },
            notifications: {
              notify_on_pr_review: true,
              notify_on_session_error: true,
              notify_on_wire_urgent: true,
              sound_enabled: false,
            },
            hub_connection: {
              auto_reconnect_ws: true,
              poll_interval_secs: 5,
            },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.endsWith("/api/v1/attachments/prune") && method === "POST") {
        const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        pruneCalls.push(body);
        return new Response(
          JSON.stringify({
            deleted_count: body.older_than_days === 0 ? 8 : 3,
            deleted_bytes: body.older_than_days === 0 ? 64000 : 24000,
            kept_count: body.older_than_days === 0 ? 0 : 5,
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response(JSON.stringify({}), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(<SettingsModal isOpen={true} onClose={vi.fn()} initialTab="system" apiClient={apiClient} />);

    const retentionSelect = screen.getByTestId(
      "settings-attachment-retention-select",
    ) as HTMLSelectElement;
    expect(retentionSelect.value).toBe("7");

    fireEvent.change(retentionSelect, { target: { value: "14" } });
    fireEvent.click(screen.getByTestId("save-settings-btn"));

    await waitFor(() => {
      expect(savedPatch?.attachment_retention_days).toBe(14);
    });

    fireEvent.click(screen.getByTestId("prune-expired-attachments-btn"));
    await waitFor(() => {
      expect(pruneCalls).toEqual([{}]);
      expect(screen.getByTestId("attachment-prune-status")).toHaveTextContent(
        /Cleaned up 3 expired screenshot\/attachment files/i,
      );
    });

    fireEvent.click(screen.getByTestId("purge-all-attachments-btn"));
    await waitFor(() => {
      expect(pruneCalls).toEqual([{}, { older_than_days: 0 }]);
      expect(screen.getByTestId("attachment-prune-status")).toHaveTextContent(
        /Purged 8 screenshot\/attachment files/i,
      );
    });
  });

  it("supports AgentSettingsModal Re-Cast modal parity, custom role auto-prefix, and granular 409 create_area confirmation", async () => {
    const recastRequests: Array<{ body: Record<string, unknown> }> = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = (init?.method ?? "GET").toUpperCase();
      if (url.endsWith("/api/v1/agents/felix/recast") && method === "POST") {
        const body = JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown>;
        recastRequests.push({ body });
        if (!body.create_area) {
          return new Response(
            JSON.stringify({
              error: "Missing coverage area NewProject; pass create_area=true to confirm",
              missing_areas: ["NewProject"],
            }),
            { status: 409, headers: { "Content-Type": "application/json" } },
          );
        }
        return new Response(
          JSON.stringify({
            agent: {
              id: "ag-felix",
              name_key: "felix",
              display_name: "Felix",
              project: String(body.project ?? "FerroLoop"),
              role: String(body.role ?? "custom:reviewer"),
              status: "idle",
            },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response(JSON.stringify({}), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <AgentSettingsModal
        isOpen={true}
        onClose={vi.fn()}
        agentName="felix"
        agentDisplayName="Felix"
        agentRole="custom:ui-developer"
        agentProject="FerroLoop"
        adapter="antigravity"
        sessionId="sess-felix"
        selectedModel="gemini-4-argon"
        selectedThinkingLevel="high"
        onChangeModelAndThinking={vi.fn()}
        contextEstimate={{ usedTokens: 12000, maxTokens: 200000, percent: 6 }}
        canCompact={false}
        compacting={false}
        onCompactContext={vi.fn()}
        canClear={true}
        clearing={false}
        onOpenClearModal={vi.fn()}
        canDecommission={true}
        decommissioning={false}
        onOpenDecommissionModal={vi.fn()}
        apiClient={apiClient}
      />,
    );

    expect(screen.getByTestId("agent-pin-toggle-btn")).toHaveAttribute(
      "aria-label",
      "Pin Agent",
    );
    fireEvent.click(screen.getByTestId("recast-agent-btn-felix"));
    expect(screen.getByTestId("recast-modal")).toBeInTheDocument();

    fireEvent.change(screen.getByTestId("recast-role-select-felix"), {
      target: { value: "__create_custom__" },
    });
    fireEvent.change(screen.getByTestId("recast-custom-role-input-felix"), {
      target: { value: "reviewer" },
    });
    fireEvent.click(screen.getByTestId("recast-submit-btn-felix"));

    const confirmBtn = await screen.findByTestId("recast-confirm-lead-btn-felix");
    fireEvent.click(confirmBtn);

    await waitFor(() => {
      expect(recastRequests.length).toBe(2);
      expect(recastRequests[0].body.role).toBe("custom:reviewer");
      expect(recastRequests[0].body.replace_lead).toBe(false);
      expect(recastRequests[0].body.create_area).toBe(false);
      expect(recastRequests[1].body.role).toBe("custom:reviewer");
      expect(recastRequests[1].body.replace_lead).toBe(false);
      expect(recastRequests[1].body.create_area).toBe(true);
    });
  });
});

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { ArtifactPreviewPane } from "../components/ArtifactPreviewPane";
import { HELP_TOPICS, HelpDrawer } from "../components/HelpDrawer";
import { SettingsModal } from "../components/SettingsModal";
import { DEFAULT_HUB_SETTINGS, type HubSettings } from "../types/api";

describe("Wave A UI — Records 10, 4 & 29", () => {
  describe("Record 10 — Contextual Help (?) Drawer & View-Targeted Documentation", () => {
    it("renders all 9 help topics, switches active topic on pill click, and filters via help-search-input", () => {
      const onClose = vi.fn();
      const onSelectTopic = vi.fn();

      render(
        <HelpDrawer
          isOpen={true}
          activeTopic="chat"
          onSelectTopic={onSelectTopic}
          onClose={onClose}
        />,
      );

      const drawer = screen.getByTestId("contextual-help-drawer");
      expect(drawer).toHaveAttribute(
        "aria-label",
        "FerroDeck Documentation & Help",
      );

      expect(HELP_TOPICS.map((t) => t.id)).toEqual([
        "chat",
        "lifecycle",
        "mail",
        "artifacts",
        "git",
        "board",
        "terminal",
        "hub",
        "gate",
        "state",
      ]);

      for (const topic of HELP_TOPICS) {
        expect(screen.getByTestId(`help-topic-${topic.id}`)).toBeInTheDocument();
      }

      const activeContent = screen.getByTestId("help-active-topic-content");
      expect(activeContent).toHaveAttribute("data-topic", "chat");
      expect(activeContent).toHaveTextContent(
        "Agent Chat, Prompt Queue, Slash Commands (/) & Model Selector",
      );

      // Click 'gate' topic pill
      fireEvent.click(screen.getByTestId("help-topic-gate"));
      expect(onSelectTopic).toHaveBeenCalledWith("gate");
      expect(screen.getByTestId("help-active-topic-content")).toHaveAttribute(
        "data-topic",
        "gate",
      );
      expect(screen.getByTestId("help-active-topic-content")).toHaveTextContent(
        "FerroGate Zero-Knowledge Vault, Proxy & Tiered MCP Registry",
      );

      // Filter via search input
      const searchInput = screen.getByTestId("help-search-input");
      fireEvent.change(searchInput, { target: { value: "Reforge" } });
      expect(screen.getByTestId("help-section-lifecycle")).toHaveTextContent(
        "Session Clear vs. Reforge & Agent Spin-Up / Decommission",
      );
      expect(screen.queryByTestId("help-section-terminal")).toBeNull();
    });

    it("opens HelpDrawer to the current activeTab from ContextHeader context-help-btn and to 'lifecycle' from Clear/Reforge open-full-help-btn", async () => {
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
        if (url.includes("/api/v1/sessions") && !url.includes("/transcript")) {
          return json({
            sessions: [
              {
                id: "sess-ferris-01",
                source: "route",
                adapter: "claude",
                persona: "ferris",
                status: "active",
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

      // Open right split panel and switch to Git & PRs tab, then click context-help-btn
      const tabBar = await screen.findByTestId("cockpit-tab-bar");
      fireEvent.click(screen.getByTestId("split-view-toggle"));
      fireEvent.click(screen.getByTestId("right-panel-tab-git"));
      fireEvent.click(screen.getByTestId("context-help-btn"));

      const drawer = await screen.findByTestId("contextual-help-drawer");
      expect(
        within(drawer).getByTestId("help-active-topic-content"),
      ).toHaveAttribute("data-topic", "git");
      fireEvent.click(within(drawer).getByTestId("close-help-drawer-btn"));
      expect(screen.queryByTestId("contextual-help-drawer")).toBeNull();

      // Switch back to Chat, open AgentSettingsModal -> Clear/Reforge modal -> '?' popover -> open-full-help-btn
      fireEvent.click(within(tabBar).getByRole("tab", { name: /^Chat$/i }));
      fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
      fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
      fireEvent.click(screen.getByTestId("clear-modal-help-btn"));

      const popover = screen.getByTestId("clear-modal-help-popover");
      expect(popover).toBeInTheDocument();
      fireEvent.click(within(popover).getByTestId("open-full-help-btn"));

      const lifecycleDrawer = await screen.findByTestId("contextual-help-drawer");
      expect(
        within(lifecycleDrawer).getByTestId("help-active-topic-content"),
      ).toHaveAttribute("data-topic", "lifecycle");

      unmount();
    });
  });

  describe("Record 4 — Unified Settings Panel", () => {
    it("loads settings via GET /api/v1/settings, updates controls, saves via PUT /api/v1/settings, and shows settings-saved-banner while preserving bulk actions", async () => {
      let savedPayload: Partial<HubSettings> | null = null;
      const initialSettings: HubSettings = {
        ...DEFAULT_HUB_SETTINGS,
        theme_preference: "dark",
        hub_connection: {
          auto_reconnect_ws: true,
          poll_interval_secs: 5,
        },
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

      const pollInput = (await screen.findByTestId(
        "input-poll-interval",
      )) as HTMLInputElement;
      await waitFor(() => {
        expect(pollInput.value).toBe("5");
      });

      // Change theme, subsystem toggle, notification toggle, and poll interval
      fireEvent.change(screen.getByTestId("settings-theme-select"), {
        target: { value: "light" },
      });
      fireEvent.click(screen.getByTestId("toggle-ferrostep"));
      fireEvent.click(screen.getByTestId("toggle-notify-sound"));
      fireEvent.change(screen.getByTestId("input-poll-interval"), {
        target: { value: "10" },
      });

      // Verify bulk operation buttons are still present
      expect(screen.getByTestId("bulk-compact-btn")).toBeInTheDocument();
      expect(screen.getByTestId("bulk-clear-btn")).toBeInTheDocument();
      expect(screen.getByTestId("bulk-reforge-btn")).toBeInTheDocument();

      fireEvent.click(screen.getByTestId("save-settings-btn"));

      expect(await screen.findByTestId("settings-saved-banner")).toHaveTextContent(
        /Settings saved/i,
      );
      expect(savedPayload).toMatchObject({
        theme_preference: "light",
        subsystems: {
          ferrogate_enabled: true,
          ferrostate_enabled: true,
          ferrostep_enabled: false,
        },
        notifications: {
          notify_on_pr_review: true,
          notify_on_session_error: true,
          notify_on_wire_urgent: true,
          sound_enabled: true,
        },
        hub_connection: {
          auto_reconnect_ws: true,
          poll_interval_secs: 10,
        },
      });
      expect(onSettingsUpdated).toHaveBeenCalledWith(
        expect.objectContaining({ theme_preference: "light" }),
      );
    });
  });

  describe("Record 29 — Artifact Pruning & Archived Badge", () => {
    it("renders artifact-archived-badge for archived session artifacts and prunes dead paths via prune-artifacts-btn", async () => {
      const archivedPath =
        "/home/lmcfarlin/.cache/ferroloop/archive/spec-archived.md";
      let pruneCalls = 0;

      const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? "GET";
        const json = (body: unknown) =>
          new Response(JSON.stringify(body), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });

        if (url.startsWith("/api/v1/artifacts/list")) {
          return json({
            artifacts: [
              {
                path: archivedPath,
                name: "spec-archived.md",
                ext: "md",
                kind: "markdown",
                size_bytes: 128,
                status: "archived",
                original_path: "/tmp/removed-worktree/docs/spec.md",
              },
            ],
          });
        }
        if (url.startsWith("/api/v1/artifacts/prune") && method === "POST") {
          pruneCalls += 1;
          const body = JSON.parse(String(init?.body));
          expect(body).toEqual({ session_id: "sess-ferris-01" });
          return json({
            pruned_count: 3,
            archived_count: 1,
          });
        }
        if (url.startsWith("/api/v1/artifacts/preview")) {
          return json({
            path: archivedPath,
            name: "spec-archived.md",
            ext: "md",
            mime_type: "text/plain; charset=utf-8",
            size_bytes: 128,
            kind: "markdown",
            content: "# Preserved Spec",
            raw_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(archivedPath)}&mode=raw`,
            download_url: `/api/v1/artifacts/preview?path=${encodeURIComponent(archivedPath)}&download=1`,
          });
        }
        return json({});
      });

      const apiClient = new HubApiClient({
        fetchImpl: fetchImpl as unknown as typeof fetch,
      });

      render(
        <ArtifactPreviewPane
          apiClient={apiClient}
          sessionId="sess-ferris-01"
        />,
      );

      const archivedBadge = await screen.findByTestId("artifact-archived-badge");
      expect(archivedBadge).toHaveTextContent("Archived");

      const pruneBtn = screen.getByTestId("prune-artifacts-btn");
      fireEvent.click(pruneBtn);

      const status = await screen.findByTestId("prune-artifacts-status");
      expect(status).toHaveTextContent("Pruned 3 dead paths");
      expect(pruneCalls).toBe(1);
    });
  });
});

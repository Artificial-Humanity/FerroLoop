import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App, {
  FONT_SCALE_STORAGE_KEY,
  THEME_STORAGE_KEY,
} from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { ArtifactPreviewPane } from "../components/ArtifactPreviewPane";
import { ChatPane } from "../components/ChatPane";
import { ContextHeader } from "../components/ContextHeader";
import { FerroStatePane } from "../components/FerroStatePane";
import { FerroWatchPane } from "../components/FerroWatchPane";
import { HubDrawer } from "../components/HubDrawer";
import { RecordsBoardPane } from "../components/RecordsBoardPane";
import { SettingsModal } from "../components/SettingsModal";
import { TerminalPane } from "../components/TerminalPane";
import { VaultGatePane } from "../components/VaultGatePane";
import { WireMailPane } from "../components/WireMailPane";

function makeStubApiClient(): HubApiClient {
  const fetchImpl = vi.fn(async () =>
    new Response(JSON.stringify({}), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
  return new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
}

function makeStubWsClient(): HubWsClient {
  return {
    setToken: vi.fn(),
    connect: vi.fn(),
    disconnect: vi.fn(),
    onStatusChange: () => () => {},
    subscribe: () => () => {},
    send: vi.fn(),
    sendFrame: vi.fn(),
  } as unknown as HubWsClient;
}

describe("SettingsModal Accessibility tab & UI Polish Pass", () => {
  const originalMatchMedia = window.matchMedia;

  beforeEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
    document.documentElement.style.removeProperty("--fd-font-scale");
  });

  afterEach(() => {
    window.matchMedia = originalMatchMedia;
  });

  it("supports Dark, Light, and System theme modes with live matchMedia OS preference updates", () => {
    let matchesLight = false;
    const listeners = new Set<(event: MediaQueryListEvent) => void>();

    window.matchMedia = vi.fn().mockImplementation((query: string) => ({
      matches: query.includes("light") ? matchesLight : !matchesLight,
      media: query,
      onchange: null,
      addEventListener: (_type: string, cb: (event: MediaQueryListEvent) => void) => {
        listeners.add(cb);
      },
      removeEventListener: (_type: string, cb: (event: MediaQueryListEvent) => void) => {
        listeners.delete(cb);
      },
      addListener: (cb: (event: MediaQueryListEvent) => void) => {
        listeners.add(cb);
      },
      removeListener: (cb: (event: MediaQueryListEvent) => void) => {
        listeners.delete(cb);
      },
      dispatchEvent: () => true,
    }));

    const apiClient = makeStubApiClient();
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        initialTab="accessibility"
        apiClient={apiClient}
      />,
    );

    expect(screen.getByTestId("settings-tab-accessibility")).toHaveAttribute(
      "aria-selected",
      "true",
    );

    const darkBtn = screen.getByTestId("theme-mode-dark");
    const lightBtn = screen.getByTestId("theme-mode-light");
    const systemBtn = screen.getByTestId("theme-mode-system");

    fireEvent.click(lightBtn);
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(lightBtn).toHaveAttribute("aria-pressed", "true");

    fireEvent.click(darkBtn);
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(darkBtn).toHaveAttribute("aria-pressed", "true");

    fireEvent.click(systemBtn);
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("system");
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(systemBtn).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("theme-system-status")).toHaveTextContent(
      /Follows OS preference via prefers-color-scheme.*dark/i,
    );

    // Simulate OS switching to light theme
    act(() => {
      matchesLight = true;
      for (const listener of listeners) {
        listener({ matches: true } as MediaQueryListEvent);
      }
    });
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(screen.getByTestId("theme-system-status")).toHaveTextContent(
      /Follows OS preference via prefers-color-scheme.*light/i,
    );
  });

  it("scales font size between 90% and 125% (presets + slider + reset) and scales/restores inline font sizes in .fd-cockpit", async () => {
    const apiClient = makeStubApiClient();
    const wsClient = makeStubWsClient();

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsClient} />);

    const leftTitle = screen.getByTestId("left-panel-active-title");
    expect(leftTitle.style.fontSize).toBe("12px");

    fireEvent.click(screen.getByTestId("settings-modal-btn"));
    fireEvent.click(screen.getByTestId("settings-tab-accessibility"));

    // Verify presets are 90%, 100%, 110%, 120%, 125%
    for (const pct of [90, 100, 110, 120, 125]) {
      expect(screen.getByTestId(`font-scale-preset-${pct}`)).toBeInTheDocument();
    }

    const slider = screen.getByTestId("font-scale-slider") as HTMLInputElement;
    expect(slider.min).toBe("90");
    expect(slider.max).toBe("125");
    expect(slider.step).toBe("5");

    // Click 125% preset
    fireEvent.click(screen.getByTestId("font-scale-preset-125"));
    expect(window.localStorage.getItem(FONT_SCALE_STORAGE_KEY)).toBe("125");
    expect(document.documentElement.style.getPropertyValue("--fd-font-scale")).toBe(
      "1.25",
    );
    expect(leftTitle.getAttribute("data-fd-base-font-px")).toBe("12");
    expect(leftTitle.style.fontSize).toBe("15px");

    // Change slider to 90%
    fireEvent.change(slider, { target: { value: "90" } });
    expect(window.localStorage.getItem(FONT_SCALE_STORAGE_KEY)).toBe("90");
    expect(document.documentElement.style.getPropertyValue("--fd-font-scale")).toBe(
      "0.9",
    );
    expect(leftTitle.style.fontSize).toBe("10.8px");

    // Click Reset (100%) -> restores exact '12px' and removes data-fd-base-font-px
    fireEvent.click(screen.getByTestId("font-scale-reset"));
    expect(window.localStorage.getItem(FONT_SCALE_STORAGE_KEY)).toBe("100");
    expect(document.documentElement.style.getPropertyValue("--fd-font-scale")).toBe(
      "1",
    );
    expect(leftTitle.style.fontSize).toBe("12px");
    expect(leftTitle.getAttribute("data-fd-base-font-px")).toBeNull();

    unmount();
  });

  it("enforces 326px minimum panel width, desktop sidebar close button, and stacked/single-column panel layouts", () => {
    const apiClient = makeStubApiClient();
    const wsClient = makeStubWsClient();

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsClient} />);

    // 1. 326px minimum width & left header close button closes desktop sidebar
    const sidebar = screen.getByTestId("roster-sidebar");
    expect(sidebar.style.width).toBe("326px");
    const leftResizeHandle = within(sidebar).getByTestId("left-panel-resize-handle");
    fireEvent.mouseDown(leftResizeHandle, { clientX: 326 });
    fireEvent.mouseMove(window, { clientX: 200 });
    fireEvent.mouseUp(window);
    expect(sidebar.style.width).toBe("326px");

    const closeBtn = within(sidebar).getByLabelText("Close Roster");
    fireEvent.click(closeBtn);
    expect(sidebar).toHaveAttribute("data-desktop-collapsed", "true");

    unmount();

    // 2. TerminalPane: 8px container padding/gap and stacked bottom command form
    const { unmount: unmountTerm } = render(
      <TerminalPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        sessionId="shell"
        wsClient={wsClient}
      />,
    );
    const xtermContainer = screen.getByTestId("xterm-container");
    expect(xtermContainer.style.padding).toBe("6px 10px 6px 6px");
    const scrollParent = xtermContainer.parentElement as HTMLElement;
    expect(scrollParent.style.padding).toBe("8px");
    expect(scrollParent.style.gap).toBe("8px");
    const cmdInput = screen.getByLabelText("Shell Command") as HTMLInputElement;
    expect(cmdInput.style.width).toBe("100%");
    const cmdForm = cmdInput.closest("form") as HTMLFormElement;
    expect(cmdForm.style.flexDirection).toBe("column");
    unmountTerm();

    // 3. HubDrawer: Workflow Definition occupies full row (1 1 100%)
    const { unmount: unmountHub } = render(
      <HubDrawer workflows={[]} workflowRuns={[]} />,
    );
    const slugInput = screen.getByTestId("workflow-slug-input");
    const slugLabel = slugInput.closest("label") as HTMLLabelElement;
    expect(slugLabel.style.flex).toBe("1 1 100%");
    expect(slugLabel.style.width).toBe("100%");
    unmountHub();

    // 4. VaultGatePane: single full-width column (minmax(0, 1fr))
    const { unmount: unmountVault } = render(
      <VaultGatePane
        secrets={[]}
        connectors={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );
    const vaultForm = screen.getByTestId("vault-secret-form");
    const vaultGrid = vaultForm.querySelector("div") as HTMLElement;
    expect(vaultGrid.style.gridTemplateColumns).toBe("minmax(0, 1fr)");
    unmountVault();

    // 5. FerroStatePane: stacked search form, single-column upsert form and code search form
    const { unmount: unmountState } = render(
      <FerroStatePane
        entries={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );
    const stateSearchForm = screen.getByTestId("state-search-form");
    const searchQueryLabel = stateSearchForm.querySelector("label") as HTMLLabelElement;
    expect(searchQueryLabel.style.flex).toBe("1 1 100%");
    const upsertForm = screen.getByTestId("state-upsert-form");
    const upsertGrid = upsertForm.querySelector("div") as HTMLElement;
    expect(upsertGrid.style.gridTemplateColumns).toBe("minmax(0, 1fr)");
    fireEvent.click(screen.getByTestId("subview-code-btn"));
    const codeSearchForm = screen.getByTestId("code-search-form");
    expect(codeSearchForm.style.gridTemplateColumns).toBe("minmax(0, 1fr)");
    unmountState();

    // 6. FerroWatchPane: KPI grid and non-overflowing KPI cards
    const { unmount: unmountWatch } = render(
      <FerroWatchPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        agents={[]}
        sessions={[]}
        apiClient={apiClient}
      />,
    );
    expect(screen.getByTestId("watch-kpi-grid")).toHaveClass("fd-watch-kpi-grid");
    expect(screen.getByTestId("watch-kpi-spend").style.minWidth).toBe("0");
    expect(screen.getByTestId("watch-kpi-attempts").style.minWidth).toBe("0");
    unmountWatch();

    // 7. WireMailPane: mail-status badge precedes sender -> recipient route group
    const { unmount: unmountMail } = render(
      <WireMailPane
        selectedAgentName="felix"
        messages={[
          {
            message: {
              id: "msg-1",
              thread_id: "thread-1",
              sender_name: "ferris",
              to: "felix",
              subject: "Test",
              body: "Hello",
              kind: "mail",
              urgency: "normal",
              durable: true,
              created_at: "2026-11-02T17:00:00Z",
            },
            receipt: {
              message_id: "msg-1",
              recipient_id: "felix",
              recipient_name: "felix",
              status: "acked",
              attempts: 1,
              updated_at: "2026-11-02T17:01:00Z",
              acked_at: "2026-11-02T17:01:00Z",
            },
          },
        ]}
        onRefresh={vi.fn()}
        onAckMessage={vi.fn()}
        onSendMessage={vi.fn()}
      />,
    );
    const statusBadge = screen.getByTestId("mail-status-msg-1");
    const routeGroup = screen.getByTestId("mail-route-msg-1");
    expect(
      statusBadge.compareDocumentPosition(routeGroup) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    unmountMail();

    // 8. ArtifactPreviewPane: redundant artifact-close-panel-btn removed
    const { unmount: unmountArtifact } = render(
      <ArtifactPreviewPane
        apiClient={apiClient}
        activePath="/tmp/sample.md"
        recentPaths={["/tmp/sample.md"]}
        onSelectPath={vi.fn()}
        onClose={vi.fn()}
      />,
    );
    expect(screen.queryByTestId("artifact-close-panel-btn")).toBeNull();
    unmountArtifact();

    // 9. RecordsBoardPane: board-search-input flex 1 1 220px and grouped board-actions-group
    const { unmount: unmountBoard } = render(
      <RecordsBoardPane selectedProject="FerroLoop" apiClient={apiClient} />,
    );
    const boardSearchInput = screen.getByTestId("board-search-input");
    expect(boardSearchInput.style.flex).toBe("1 1 220px");
    const boardActionsGroup = screen.getByTestId("board-actions-group");
    expect(within(boardActionsGroup).getByTestId("board-new-record-btn")).toBeInTheDocument();
    expect(within(boardActionsGroup).getByTestId("board-refresh-btn")).toBeInTheDocument();
    unmountBoard();

    // 10. ContextHeader: pinned toggle buttons & wrapping tier-agent-nav
    const { unmount: unmountHeader } = render(
      <ContextHeader
        selectedAgent={{
          id: "agent-felix",
          name_key: "felix",
          display_name: "Felix",
          role: "ui-developer",
          project: "FerroLoop",
          status: "active",
        }}
        selectedSessionId={null}
        sessions={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
        activeTab="chat"
        onSelectTab={vi.fn()}
        onSelectSession={vi.fn()}
        onOpenPairingModal={vi.fn()}
        onOpenMobileRoster={vi.fn()}
        onToggleSplitView={vi.fn()}
      />,
    );
    expect(screen.getByTestId("cockpit-tab-bar").style.flexWrap).toBe("nowrap");
    expect(screen.getByTestId("tier-agent-nav").style.flexWrap).toBe("wrap");
    expect(screen.getByTestId("tier-agent-nav").style.minWidth).toBe("0");
    unmountHeader();

    // 11. ChatPane: wrapping toolbar + encapsulated-work-block timestamp overflow fix
    const { unmount: unmountChat } = render(
      <ChatPane
        agentName="felix"
        agentDisplayName="Felix"
        turns={[
          {
            index: 1,
            role: "user",
            content: "Run checks",
            tool_calls: [],
            created_at: "2026-11-02T17:50:00Z",
          },
          {
            index: 2,
            role: "assistant",
            content: "",
            thinking: "Checking files",
            tool_calls: [{ name: "run_command", arguments: "{}" }],
            created_at: "2026-11-02T17:50:05Z",
          },
          {
            index: 3,
            role: "tool",
            content: "ok",
            tool_calls: [],
            created_at: "2026-11-02T17:50:06Z",
          },
          {
            index: 4,
            role: "assistant",
            content: "",
            thinking: "Verifying build",
            tool_calls: [{ name: "read_file", arguments: "{}" }],
            created_at: "2026-11-02T17:50:10Z",
          },
        ]}
        onSendPrompt={vi.fn()}
      />,
    );
    const chatPane = screen.getByTestId("chat-pane");
    const chatToolbar = chatPane.querySelector(".fd-chat-toolbar") as HTMLElement | null;
    expect(chatToolbar?.style.flexWrap).toBe("wrap");
    const filterInput = chatPane.querySelector(
      ".fd-transcript-filter-input",
    ) as HTMLElement | null;
    expect(filterInput?.style.flex).toBe("1 1 180px");
    const workBlock = screen.getByTestId("encapsulated-work-block");
    expect(workBlock.style.overflow).toBe("hidden");
    const workMeta = screen.getByTestId("encapsulated-work-meta");
    expect(workMeta.style.flexWrap).toBe("wrap");
    expect(workMeta.style.minWidth).toBe("0");
    expect(workMeta.style.maxWidth).toBe("100%");
    unmountChat();
  });

  it("persists user-modified left and right panel widths to localStorage and restores on mount while preserving 360px center chat width", () => {
    const originalInnerWidth = window.innerWidth;
    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      writable: true,
      value: 1200,
    });

    try {
      const apiClient = makeStubApiClient();
      const wsClient = makeStubWsClient();

      const { unmount } = render(<App apiClient={apiClient} wsClient={wsClient} />);

      const sidebar = screen.getByTestId("roster-sidebar");
      expect(sidebar.style.width).toBe("326px");

      // Resize left roster panel to 410px
      const leftHandle = screen.getByTestId("left-panel-resize-handle");
      fireEvent.mouseDown(leftHandle, { clientX: 326 });
      fireEvent.mouseMove(window, { clientX: 410 });
      fireEvent.mouseUp(window);

      expect(sidebar.style.width).toBe("410px");
      expect(window.localStorage.getItem("fd_left_roster_width")).toBe("410");

      // Open split view and resize right panel
      fireEvent.click(screen.getByTestId("split-view-toggle"));
      const rightPane = screen.getByTestId("right-split-panel");
      expect(rightPane.style.width).toBe("460px");

      // Attempt to drag right panel very wide (e.g. 600px); with innerWidth=1200 and left=410,
      // max right width preserving 360px center chat is 1200 - 360 - 410 = 430px
      const rightHandle = screen.getByTestId("right-panel-resize-handle");
      fireEvent.mouseDown(rightHandle, { clientX: 740 });
      fireEvent.mouseMove(window, { clientX: 500 });
      fireEvent.mouseUp(window);

      expect(rightPane.style.width).toBe("430px");
      expect(window.localStorage.getItem("fd_right_panel_width")).toBe("430");

      unmount();

      // Re-mount App and verify persisted widths are restored from localStorage
      const { unmount: unmount2 } = render(<App apiClient={apiClient} wsClient={wsClient} />);
      expect(screen.getByTestId("roster-sidebar").style.width).toBe("410px");
      fireEvent.click(screen.getByTestId("split-view-toggle"));
      expect(screen.getByTestId("right-split-panel").style.width).toBe("430px");
      unmount2();
    } finally {
      Object.defineProperty(window, "innerWidth", {
        configurable: true,
        writable: true,
        value: originalInnerWidth,
      });
    }
  });

  it("opens System and Project Change Artifacts Directory modals (#207), submits POST requests with migrate checkbox, and strips read-only fields on PUT /api/v1/settings", async () => {
    const calls: Array<{ url: string; method: string; body?: unknown }> = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = (init?.method || "GET").toUpperCase();
      const parsedBody =
        typeof init?.body === "string" ? JSON.parse(init.body) : undefined;
      calls.push({ url, method, body: parsedBody });

      if (url.endsWith("/api/v1/settings") && method === "GET") {
        return new Response(
          JSON.stringify({
            theme_preference: "dark",
            timezone: "UTC",
            attachment_retention_days: 7,
            home_dir: "/home/lmcfarlin/.local/share/fl",
            artifacts_dir: null,
            default_artifacts_dir: "/home/lmcfarlin/.local/share/fl/artifacts",
            effective_artifacts_dir: "/home/lmcfarlin/.local/share/fl/artifacts",
            project_artifacts_dirs: {
              ferroloop: "/mnt/nvme/ferroloop-artifacts",
            },
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
              poll_interval_secs: 3,
            },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }

      if (url.endsWith("/api/v1/projects") && method === "GET") {
        return new Response(
          JSON.stringify({
            projects: [
              {
                id: "ferroloop",
                artifacts_dir: "/mnt/nvme/ferroloop-artifacts",
                default_artifacts_dir:
                  "/home/lmcfarlin/.local/share/fl/artifacts/ferroloop",
                effective_artifacts_dir: "/mnt/nvme/ferroloop-artifacts",
              },
              {
                id: "sonora",
                artifacts_dir: null,
                default_artifacts_dir:
                  "/home/lmcfarlin/.local/share/fl/artifacts/sonora",
                effective_artifacts_dir:
                  "/home/lmcfarlin/.local/share/fl/artifacts/sonora",
              },
            ],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }

      if (url.endsWith("/api/v1/settings/artifacts-dir") && method === "POST") {
        const body = parsedBody as { new_path: string | null; migrate: boolean };
        const effective =
          body.new_path || "/home/lmcfarlin/.local/share/fl/artifacts";
        return new Response(
          JSON.stringify({
            old_path: "/home/lmcfarlin/.local/share/fl/artifacts",
            new_path: effective,
            migrated_files: body.migrate ? 14 : 0,
            migrated_bytes: body.migrate ? 20480 : 0,
            artifacts_dir: body.new_path,
            default_artifacts_dir: "/home/lmcfarlin/.local/share/fl/artifacts",
            effective_artifacts_dir: effective,
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }

      if (
        url.endsWith("/api/v1/projects/sonora/artifacts-dir") &&
        method === "POST"
      ) {
        const body = parsedBody as { new_path: string | null; migrate: boolean };
        const effective =
          body.new_path || "/home/lmcfarlin/.local/share/fl/artifacts/sonora";
        return new Response(
          JSON.stringify({
            project: "sonora",
            old_path: "/home/lmcfarlin/.local/share/fl/artifacts/sonora",
            new_path: effective,
            migrated_files: body.migrate ? 5 : 0,
            migrated_bytes: body.migrate ? 8192 : 0,
            artifacts_dir: body.new_path,
            default_artifacts_dir:
              "/home/lmcfarlin/.local/share/fl/artifacts/sonora",
            effective_artifacts_dir: effective,
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }

      if (url.endsWith("/api/v1/settings") && method === "PUT") {
        return new Response(JSON.stringify(parsedBody ?? {}), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
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
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        initialTab="system"
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
      />,
    );

    // Wait for GET /api/v1/settings and GET /api/v1/projects to resolve
    const sysDirVal = await screen.findByTestId(
      "settings-system-artifacts-dir-value",
    );
    await act(async () => {
      await Promise.resolve();
    });
    expect(sysDirVal).toHaveTextContent(
      "/home/lmcfarlin/.local/share/fl/artifacts",
    );

    // Open System Change Artifacts Directory modal
    fireEvent.click(
      screen.getByTestId("settings-system-change-artifacts-dir-btn"),
    );
    const modal = screen.getByTestId("change-artifacts-dir-modal");
    expect(modal).toBeInTheDocument();
    expect(
      within(modal).getByTestId("change-artifacts-dir-modal-title"),
    ).toHaveTextContent("Change System Artifacts Directory");
    expect(
      (
        within(modal).getByTestId(
          "change-artifacts-dir-old-path",
        ) as HTMLInputElement
      ).value,
    ).toBe("/home/lmcfarlin/.local/share/fl/artifacts");

    const newPathInput = within(modal).getByTestId(
      "change-artifacts-dir-new-path",
    ) as HTMLInputElement;
    const migrateCheckbox = within(modal).getByTestId(
      "change-artifacts-dir-migrate-checkbox",
    ) as HTMLInputElement;
    expect(migrateCheckbox.checked).toBe(true);

    fireEvent.change(newPathInput, {
      target: { value: "/mnt/fast-ssd/fl-artifacts" },
    });
    fireEvent.click(within(modal).getByTestId("change-artifacts-dir-confirm-btn"));

    const sysStatus = await screen.findByTestId("system-artifacts-dir-status");
    expect(sysStatus).toHaveTextContent(
      /Updated artifacts directory to \/mnt\/fast-ssd\/fl-artifacts \(migrated 14 files, 20480 bytes\)/i,
    );
    expect(
      screen.getByTestId("settings-system-artifacts-dir-value"),
    ).toHaveTextContent("/mnt/fast-ssd/fl-artifacts");
    expect(
      screen.getByTestId("settings-system-artifacts-dir-override-badge"),
    ).toHaveTextContent("Override");

    const sysPostCall = calls.find(
      (c) => c.url.endsWith("/api/v1/settings/artifacts-dir") && c.method === "POST",
    );
    expect(sysPostCall?.body).toEqual({
      new_path: "/mnt/fast-ssd/fl-artifacts",
      migrate: true,
    });

    // Save System Settings and verify read-only artifacts_dir fields are stripped from PUT /api/v1/settings
    fireEvent.click(screen.getByTestId("save-settings-btn"));
    await screen.findByTestId("settings-saved-banner");
    const putCall = calls.find(
      (c) => c.url.endsWith("/api/v1/settings") && c.method === "PUT",
    );
    expect(putCall?.body).toBeDefined();
    expect(putCall?.body).not.toHaveProperty("home_dir");
    expect(putCall?.body).not.toHaveProperty("artifacts_dir");
    expect(putCall?.body).not.toHaveProperty("default_artifacts_dir");
    expect(putCall?.body).not.toHaveProperty("effective_artifacts_dir");
    expect(putCall?.body).not.toHaveProperty("project_artifacts_dirs");

    // Switch to Agents & Projects tab and select Sonora (derived from the updated system artifacts root)
    fireEvent.click(screen.getByTestId("settings-tab-agents"));
    fireEvent.change(screen.getByTestId("settings-project-filter-select"), {
      target: { value: "Sonora" },
    });

    expect(
      screen.getByTestId("settings-project-artifacts-dir-value"),
    ).toHaveTextContent("/mnt/fast-ssd/fl-artifacts/sonora");

    fireEvent.click(
      screen.getByTestId("settings-project-change-artifacts-dir-btn"),
    );
    const projModal = screen.getByTestId("change-artifacts-dir-modal");
    expect(
      within(projModal).getByTestId("change-artifacts-dir-modal-title"),
    ).toHaveTextContent("Change Sonora Artifacts Directory");
    expect(
      (
        within(projModal).getByTestId(
          "change-artifacts-dir-old-path",
        ) as HTMLInputElement
      ).value,
    ).toBe("/mnt/fast-ssd/fl-artifacts/sonora");

    fireEvent.change(
      within(projModal).getByTestId("change-artifacts-dir-new-path"),
      { target: { value: "/mnt/nvme/sonora-artifacts" } },
    );
    fireEvent.click(
      within(projModal).getByTestId("change-artifacts-dir-migrate-checkbox"),
    );
    fireEvent.click(
      within(projModal).getByTestId("change-artifacts-dir-confirm-btn"),
    );

    const projStatus = await screen.findByTestId("project-artifacts-dir-status");
    expect(projStatus).toHaveTextContent(
      /Updated Sonora artifacts directory to \/mnt\/nvme\/sonora-artifacts/i,
    );
    expect(
      screen.getByTestId("settings-project-artifacts-dir-value"),
    ).toHaveTextContent("/mnt/nvme/sonora-artifacts");

    const projPostCall = calls.find(
      (c) =>
        c.url.endsWith("/api/v1/projects/sonora/artifacts-dir") &&
        c.method === "POST",
    );
    expect(projPostCall?.body).toEqual({
      new_path: "/mnt/nvme/sonora-artifacts",
      migrate: false,
    });
  });

  it("renders dedicated Products tab with 7 product configuration cards, canonical icons, locked FerroLoop core engine toggle, and per-product config slots", async () => {
    let savedBody: Record<string, unknown> | null = null;
    const fetchImpl = vi.fn(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? "GET";
        if (url.endsWith("/api/v1/settings") && method === "PUT") {
          savedBody = JSON.parse(String(init?.body));
          return new Response(JSON.stringify(savedBody), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        return new Response(JSON.stringify({}), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      },
    );

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
      />,
    );

    const productsTabBtn = screen.getByTestId("settings-tab-products");
    expect(productsTabBtn).toHaveTextContent("Products");
    fireEvent.click(productsTabBtn);
    expect(productsTabBtn).toHaveAttribute("aria-selected", "true");

    const panel = screen.getByTestId("settings-panel-products");
    expect(panel).toHaveStyle({ display: "flex" });

    for (const slug of [
      "ferroloop",
      "ferrodeck",
      "ferrowire",
      "ferrostate",
      "ferrogate",
      "ferrotrack",
      "ferrowatch",
    ]) {
      const card = within(panel).getByTestId(`product-card-${slug}`);
      expect(card).toBeInTheDocument();
      expect(card).toHaveStyle({ minWidth: "0", overflow: "hidden" });
      expect(
        within(card).getByTestId(`product-config-${slug}`),
      ).toBeInTheDocument();
      const img = card.querySelector("img");
      expect(img?.getAttribute("src")).toBe(`/brand/${slug}-icon.jpg`);
      const headerRow = img?.parentElement?.parentElement;
      expect(headerRow).toHaveStyle({ flexWrap: "wrap" });
      const toggleLabel = card.querySelector("label");
      expect(toggleLabel).toHaveStyle({ flexWrap: "wrap", maxWidth: "100%" });
    }

    // FerroLoop core engine toggle is required / locked on
    expect(screen.getByTestId("product-status-ferroloop")).toHaveTextContent(
      "Required",
    );
    const ferroLoopToggle = screen.getByTestId(
      "toggle-ferroloop",
    ) as HTMLInputElement;
    expect(ferroLoopToggle.checked).toBe(true);
    expect(ferroLoopToggle.disabled).toBe(true);

    // Retired FerroStep product name does not appear anywhere
    expect(panel.textContent).not.toMatch(/FerroStep/);

    // Toggle FerroGate and FerroWire and save from Products tab
    fireEvent.click(screen.getByTestId("toggle-ferrogate"));
    fireEvent.click(screen.getByTestId("toggle-ferrowire"));
    fireEvent.click(screen.getByTestId("save-products-settings-btn"));

    expect(
      await screen.findByTestId("settings-saved-banner"),
    ).toHaveTextContent(/Settings saved/i);
    expect(savedBody).toMatchObject({
      subsystems: {
        ferrogate_enabled: false,
        ferrostate_enabled: true,
        ferrostep_enabled: true,
        ferrowire_enabled: false,
      },
    });
  });

  it("pins the modal header and tab bar while switching sub-tabs across Agents, System, and Products", async () => {
    const apiClient = makeStubApiClient();
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        apiClient={apiClient}
      />,
    );

    const modal = screen.getByTestId("settings-modal");
    const tabBar = screen.getByTestId("settings-tab-bar");
    const scrollBody = screen.getByTestId("settings-scroll-body");

    expect(modal).toHaveStyle({ overflow: "hidden" });
    expect(tabBar).toHaveStyle({ flexShrink: "0" });
    expect(scrollBody).toHaveStyle({ overflowY: "auto" });

    // 1. Agents sub-tabs
    expect(screen.getByTestId("agents-subtab-bar")).toBeInTheDocument();
    const rosterSubTab = screen.getByTestId("agents-subtab-roster");
    const rolesSubTab = screen.getByTestId("agents-subtab-roles");
    const runtimesSubTab = screen.getByTestId("agents-subtab-runtimes");

    const rosterPanel = screen.getByTestId("agents-subpanel-roster");
    const rolesPanel = screen.getByTestId("agents-subpanel-roles");
    const runtimesPanel = screen.getByTestId("agents-subpanel-runtimes");

    expect(rosterSubTab).toHaveAttribute("aria-selected", "true");
    expect(rolesSubTab).toHaveAttribute("aria-selected", "false");
    expect(runtimesSubTab).toHaveAttribute("aria-selected", "false");
    expect(rosterPanel).toHaveStyle({ display: "flex" });
    expect(rolesPanel).toHaveStyle({ display: "none" });
    expect(runtimesPanel).toHaveStyle({ display: "none" });

    fireEvent.click(rolesSubTab);
    expect(rosterSubTab).toHaveAttribute("aria-selected", "false");
    expect(rolesSubTab).toHaveAttribute("aria-selected", "true");
    expect(rosterPanel).toHaveStyle({ display: "none" });
    expect(rolesPanel).toHaveStyle({ display: "flex" });
    expect(runtimesPanel).toHaveStyle({ display: "none" });

    fireEvent.click(runtimesSubTab);
    expect(rolesSubTab).toHaveAttribute("aria-selected", "false");
    expect(runtimesSubTab).toHaveAttribute("aria-selected", "true");
    expect(rosterPanel).toHaveStyle({ display: "none" });
    expect(rolesPanel).toHaveStyle({ display: "none" });
    expect(runtimesPanel).toHaveStyle({ display: "flex" });

    // 2. System sub-tabs
    fireEvent.click(screen.getByTestId("settings-tab-system"));
    expect(screen.getByTestId("system-subtab-bar")).toBeInTheDocument();

    const sysGeneralTab = screen.getByTestId("system-subtab-general");
    const sysStorageTab = screen.getByTestId("system-subtab-storage");
    const sysNotificationsTab = screen.getByTestId(
      "system-subtab-notifications",
    );

    const sysGeneralPanel = screen.getByTestId("system-subpanel-general");
    const sysStoragePanel = screen.getByTestId("system-subpanel-storage");
    const sysNotificationsPanel = screen.getByTestId(
      "system-subpanel-notifications",
    );

    expect(sysGeneralTab).toHaveAttribute("aria-selected", "true");
    expect(sysGeneralPanel).toHaveStyle({ display: "flex" });
    expect(sysStoragePanel).toHaveStyle({ display: "none" });
    expect(sysNotificationsPanel).toHaveStyle({ display: "none" });

    fireEvent.click(sysStorageTab);
    expect(sysStorageTab).toHaveAttribute("aria-selected", "true");
    expect(sysGeneralPanel).toHaveStyle({ display: "none" });
    expect(sysStoragePanel).toHaveStyle({ display: "flex" });
    expect(sysNotificationsPanel).toHaveStyle({ display: "none" });

    fireEvent.click(sysNotificationsTab);
    expect(sysNotificationsTab).toHaveAttribute("aria-selected", "true");
    expect(sysGeneralPanel).toHaveStyle({ display: "none" });
    expect(sysStoragePanel).toHaveStyle({ display: "none" });
    expect(sysNotificationsPanel).toHaveStyle({ display: "flex" });

    // 3. Products sub-tabs
    fireEvent.click(screen.getByTestId("settings-tab-products"));
    expect(screen.getByTestId("products-subtab-bar")).toBeInTheDocument();

    const prodCatalogTab = screen.getByTestId("products-subtab-catalog");
    const prodInstallerTab = screen.getByTestId("products-subtab-installer");

    const prodCatalogPanel = screen.getByTestId("products-subpanel-catalog");
    const prodInstallerPanel = screen.getByTestId("products-subpanel-installer");

    expect(prodCatalogTab).toHaveAttribute("aria-selected", "true");
    expect(prodCatalogPanel).toHaveStyle({ display: "flex" });
    expect(prodInstallerPanel).toHaveStyle({ display: "none" });

    fireEvent.click(prodInstallerTab);
    expect(prodCatalogTab).toHaveAttribute("aria-selected", "false");
    expect(prodInstallerTab).toHaveAttribute("aria-selected", "true");
    expect(prodCatalogPanel).toHaveStyle({ display: "none" });
    expect(prodInstallerPanel).toHaveStyle({ display: "flex" });
  });

  it("renders 4 focused sub-tabs in the Project-Level Settings Modal when initialProject is set", async () => {
    const apiClient = makeStubApiClient();
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        apiClient={apiClient}
        initialProject="FerroLoop"
      />,
    );

    expect(
      screen.getByTestId("project-settings-subtab-bar"),
    ).toBeInTheDocument();

    const overviewTab = screen.getByTestId("project-tab-overview");
    const agentsTab = screen.getByTestId("project-tab-agents");
    const rolesTab = screen.getByTestId("project-tab-roles");
    const runtimesTab = screen.getByTestId("project-tab-runtimes");

    const overviewPanel = screen.getByTestId("project-subpanel-overview");
    const agentsPanel = screen.getByTestId("project-subpanel-agents");
    const rolesPanel = screen.getByTestId("project-subpanel-roles");
    const runtimesPanel = screen.getByTestId("project-subpanel-runtimes");

    expect(overviewTab).toHaveAttribute("aria-selected", "true");
    expect(overviewPanel).toHaveStyle({ display: "flex" });
    expect(agentsPanel).toHaveStyle({ display: "none" });
    expect(rolesPanel).toHaveStyle({ display: "none" });
    expect(runtimesPanel).toHaveStyle({ display: "none" });

    fireEvent.click(agentsTab);
    expect(overviewTab).toHaveAttribute("aria-selected", "false");
    expect(agentsTab).toHaveAttribute("aria-selected", "true");
    expect(overviewPanel).toHaveStyle({ display: "none" });
    expect(agentsPanel).toHaveStyle({ display: "flex" });
    expect(rolesPanel).toHaveStyle({ display: "none" });
    expect(runtimesPanel).toHaveStyle({ display: "none" });

    fireEvent.click(rolesTab);
    expect(agentsTab).toHaveAttribute("aria-selected", "false");
    expect(rolesTab).toHaveAttribute("aria-selected", "true");
    expect(overviewPanel).toHaveStyle({ display: "none" });
    expect(agentsPanel).toHaveStyle({ display: "none" });
    expect(rolesPanel).toHaveStyle({ display: "flex" });
    expect(runtimesPanel).toHaveStyle({ display: "none" });

    fireEvent.click(runtimesTab);
    expect(rolesTab).toHaveAttribute("aria-selected", "false");
    expect(runtimesTab).toHaveAttribute("aria-selected", "true");
    expect(overviewPanel).toHaveStyle({ display: "none" });
    expect(agentsPanel).toHaveStyle({ display: "none" });
    expect(rolesPanel).toHaveStyle({ display: "none" });
    expect(runtimesPanel).toHaveStyle({ display: "flex" });
  });
});

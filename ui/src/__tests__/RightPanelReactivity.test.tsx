import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { App } from "../App";
import { ArtifactPreviewPane } from "../components/ArtifactPreviewPane";
import { FerroStatePane } from "../components/FerroStatePane";
import { GitPrPane } from "../components/GitPrPane";
import { HubDrawer } from "../components/HubDrawer";
import {
  BOARD_BUCKET_STORAGE_KEY,
  RecordsBoardPane,
} from "../components/RecordsBoardPane";
import { TerminalPane } from "../components/TerminalPane";
import { VaultGatePane } from "../components/VaultGatePane";
import { WireMailPane } from "../components/WireMailPane";
import { DEFAULT_WORKSPACE_AGENTS, type WireInboxItem } from "../types/api";

const SAMPLE_WIRE_MESSAGES: WireInboxItem[] = [
  {
    message: {
      id: "msg-latest",
      thread_id: "thr-1",
      sender_name: "jarvis",
      to: "ferris",
      kind: "mail",
      urgency: "urgent",
      durable: true,
      subject: "Most recent message subject",
      body: "Preloaded body of the most recent message.",
      created_at: "2026-11-02T14:30:00Z",
    },
    receipt: {
      message_id: "msg-latest",
      recipient_id: "urn:fl:agent:ferris",
      recipient_name: "ferris",
      status: "delivered",
      attempts: 1,
      updated_at: "2026-11-02T14:30:01Z",
      acked_at: null,
    },
  },
  {
    message: {
      id: "msg-older",
      thread_id: "thr-2",
      sender_name: "felix",
      to: "ferris",
      kind: "mail",
      urgency: "normal",
      durable: true,
      subject: "Older message subject",
      body: "Lazy-loaded body of the older message.",
      created_at: "2026-11-02T14:00:00Z",
    },
    receipt: {
      message_id: "msg-older",
      recipient_id: "urn:fl:agent:ferris",
      recipient_name: "ferris",
      status: "delivered",
      attempts: 1,
      updated_at: "2026-11-02T14:00:01Z",
      acked_at: null,
    },
  },
];

describe("Left & Right Panel Toggle Synchronization and Right-Panel Minimum-Width Reactivity", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("synchronizes left-panel toggle active state with right-panel split-view toggle (active when open, normal when closed)", async () => {
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/v1/auth/session")) {
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
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
    const wsStub = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: () => () => {},
    } as unknown as HubWsClient;

    const { unmount } = render(<App apiClient={apiClient} wsClient={wsStub} />);

    const leftToggle = await screen.findByTestId("mobile-sidebar-toggle-btn");
    const rightToggle = await screen.findByTestId("split-view-toggle");
    const sidebar = await screen.findByTestId("roster-sidebar");

    // On desktop initial render: left panel is OPEN, right panel is CLOSED.
    expect(sidebar.className).not.toContain("is-desktop-collapsed");
    expect(leftToggle.className).toContain("fd-btn-active");
    expect(leftToggle).toHaveAttribute("aria-expanded", "true");
    expect(leftToggle).toHaveAttribute("aria-pressed", "true");

    expect(rightToggle.className).not.toContain("fd-btn-active");
    expect(rightToggle).toHaveAttribute("aria-pressed", "false");

    // Clicking left toggle closes the left panel on desktop -> button becomes inactive (normal).
    fireEvent.click(leftToggle);
    expect(sidebar.className).toContain("is-desktop-collapsed");
    expect(leftToggle.className).not.toContain("fd-btn-active");
    expect(leftToggle).toHaveAttribute("aria-expanded", "false");
    expect(leftToggle).toHaveAttribute("aria-pressed", "false");

    // Clicking right toggle opens the right panel -> right button becomes active.
    fireEvent.click(rightToggle);
    expect(rightToggle.className).toContain("fd-btn-active");
    expect(rightToggle).toHaveAttribute("aria-pressed", "true");

    unmount();
  });

  it("WireMailPane: stacks compose form above inbox when narrow, collapses all messages by default, preloads only the most recent message body, and lazy-loads older bodies on open", () => {
    render(
      <WireMailPane
        messages={SAMPLE_WIRE_MESSAGES}
        selectedAgentName="ferris"
        agents={DEFAULT_WORKSPACE_AGENTS}
      />,
    );

    const layout = screen.getByTestId("wire-mail-layout");
    expect(layout).toHaveAttribute("data-layout", "stacked");

    // Verify Compose form appears before Inbox column in DOM order when stacked
    const composeForm = screen.getByTestId("wire-compose-form");
    const inboxColumn = screen.getByTestId("wire-inbox-column");
    expect(
      composeForm.compareDocumentPosition(inboxColumn) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();

    // Both messages start collapsed
    const latestCard = screen.getByTestId("mail-item-msg-latest");
    const olderCard = screen.getByTestId("mail-item-msg-older");
    expect(latestCard).toHaveAttribute("data-expanded", "false");
    expect(olderCard).toHaveAttribute("data-expanded", "false");

    // Most recent message body is preloaded in DOM (hidden until expanded)
    const latestBody = screen.getByTestId("mail-body-msg-latest");
    expect(latestBody).toHaveAttribute("data-preloaded", "true");
    expect(latestBody).toHaveStyle({ display: "none" });

    // Older message body is NOT mounted before user expands it
    expect(screen.queryByTestId("mail-body-msg-older")).toBeNull();

    // Expanding latest message reveals its preloaded body
    fireEvent.click(screen.getByTestId("mail-toggle-msg-latest"));
    expect(latestCard).toHaveAttribute("data-expanded", "true");
    expect(latestBody).toHaveStyle({ display: "block" });

    // Expanding older message lazy-loads and reveals its body
    fireEvent.click(screen.getByTestId("mail-toggle-msg-older"));
    expect(olderCard).toHaveAttribute("data-expanded", "true");
    const olderBody = screen.getByTestId("mail-body-msg-older");
    expect(olderBody).toHaveAttribute("data-preloaded", "false");
    expect(olderBody).toHaveStyle({ display: "block" });
    expect(olderBody).toHaveTextContent("Lazy-loaded body of the older message.");
  });

  it("ArtifactPreviewPane: opens Clean Up modal with scope, retention age, file kind, and version filters and invokes pruneArtifacts", async () => {
    const pruneCalls: Array<Record<string, unknown>> = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const json = (body: unknown, status = 200) =>
        new Response(JSON.stringify(body), {
          status,
          headers: { "Content-Type": "application/json" },
        });
      if (url.endsWith("/api/v1/artifacts/prune")) {
        const parsed = init?.body ? JSON.parse(String(init.body)) : {};
        pruneCalls.push(parsed);
        return json({
          ok: true,
          dry_run: Boolean(parsed.dry_run),
          removed_entries: 3,
          removed_versions: 5,
          bytes_freed: 16384,
          failed_deletes: 0,
          pruned_count: 3,
          archived_count: 1,
          active_count: 1,
          storage: {
            root_dir: "/tmp/artifacts",
            managed_files: 2,
            managed_bytes: 3072,
            version_files: 4,
            version_bytes: 8192,
            registry_entries: 2,
            truncated: false,
          },
        });
      }
      if (url.startsWith("/api/v1/artifacts/list")) {
        return json({
          artifacts: [
            {
              path: "/tmp/dead-worktree/plan.md",
              name: "plan.md",
              ext: "md",
              kind: "markdown",
              size_bytes: 1024,
              status: "archived",
            },
            {
              path: "/tmp/live/diagram.png",
              name: "diagram.png",
              ext: "png",
              kind: "image",
              size_bytes: 2048,
              status: "live",
            },
          ],
          pruned_count: 0,
          storage: {
            root_dir: "/tmp/artifacts",
            managed_files: 5,
            managed_bytes: 19456,
            version_files: 9,
            version_bytes: 24576,
            registry_entries: 5,
            truncated: false,
          },
        });
      }
      return json({
        path: "/tmp/dead-worktree/plan.md",
        name: "plan.md",
        ext: "md",
        mime_type: "text/plain",
        size_bytes: 1024,
        kind: "markdown",
        content: "# Plan",
        raw_url: "/api/v1/artifacts/preview?path=%2Ftmp%2Fdead-worktree%2Fplan.md&mode=raw",
        download_url:
          "/api/v1/artifacts/preview?path=%2Ftmp%2Fdead-worktree%2Fplan.md&download=1",
      });
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <ArtifactPreviewPane
        apiClient={apiClient}
        sessionId="sess-ferris"
        recentPaths={["/tmp/dead-worktree/plan.md", "/tmp/live/diagram.png"]}
      />,
    );

    const cleanupBtn = await screen.findByTestId("artifact-cleanup-btn");
    fireEvent.click(cleanupBtn);

    const modal = await screen.findByTestId("artifact-cleanup-modal");
    expect(modal).toBeInTheDocument();
    await waitFor(() => {
      expect(
        screen.getByTestId("artifact-cleanup-storage-summary"),
      ).toHaveTextContent(/Managed: 5 files/i);
    });

    fireEvent.change(screen.getByTestId("artifact-cleanup-scope"), {
      target: { value: "all" },
    });
    fireEvent.change(screen.getByTestId("artifact-cleanup-age"), {
      target: { value: "7d" },
    });
    fireEvent.change(screen.getByTestId("artifact-cleanup-kind"), {
      target: { value: "docs" },
    });
    fireEvent.change(screen.getByTestId("artifact-cleanup-versions-keep"), {
      target: { value: "3" },
    });

    // Run dry-run preview first
    fireEvent.click(screen.getByTestId("artifact-cleanup-dryrun-btn"));
    await waitFor(() => {
      expect(
        screen.getByTestId("artifact-cleanup-dryrun-summary"),
      ).toHaveTextContent(/3 entries, 5 versions/i);
    });
    expect(pruneCalls[0]).toEqual({
      dry_run: true,
      status: "all",
      max_age_days: 7,
      kinds: ["markdown", "pdf"],
      prune_versions_keep: 3,
    });

    // Confirm cleanup
    fireEvent.click(screen.getByTestId("artifact-cleanup-confirm-btn"));
    await waitFor(() => {
      expect(screen.queryByTestId("artifact-cleanup-modal")).toBeNull();
    });
    expect(pruneCalls[1]).toEqual({
      dry_run: false,
      status: "all",
      max_age_days: 7,
      kinds: ["markdown", "pdf"],
      prune_versions_keep: 3,
    });
    expect(screen.getByTestId("prune-artifacts-status")).toHaveTextContent(
      /Cleaned up 3 artifacts, 5 versions/i,
    );
  });

  it("GitPrPane & DiffViewer: stacks worktree list above DiffViewer at narrow width and keeps empty diff state bounded", () => {
    render(
      <GitPrPane
        worktrees={[
          {
            id: "main",
            path: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
            head: "3c65d77",
            branch: "main",
            dirty_files: [],
            is_clean: true,
          },
        ]}
        selectedWorktreeId="main"
        worktreeDiff={{
          id: "main",
          path: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
          branch: "main",
          head: "3c65d77",
          files: [],
          diff: "",
        }}
      />,
    );

    const layout = screen.getByTestId("git-worktrees-layout");
    expect(layout).toHaveAttribute("data-layout", "stacked");

    const emptyDiff = screen.getByTestId("diff-empty-state");
    expect(emptyDiff).toHaveTextContent(
      "Working tree has no uncommitted diff against HEAD.",
    );
    expect(emptyDiff).toHaveStyle({
      maxWidth: "100%",
      boxSizing: "border-box",
    });
  });

  it("RecordsBoardPane: renders unlabeled bucket dropdown between completion bar and swimlanes, defaults to 'todo', and persists selection in localStorage", async () => {
    const fetchImpl = vi.fn(async () =>
      new Response(
        JSON.stringify({
          project: "ferroloop",
          available_projects: ["ferroloop"],
          records: [
            {
              id: "rec-1",
              display_id: "101",
              project: "ferroloop",
              title: "Todo work item",
              state: "todo",
              area: "ui",
            },
            {
              id: "rec-2",
              display_id: "102",
              project: "ferroloop",
              title: "Doing work item",
              state: "doing",
              area: "ui",
            },
            {
              id: "rec-3",
              display_id: "103",
              project: "ferroloop",
              title: "Done work item",
              state: "done",
              area: "hub",
            },
          ],
          summary: {
            todo_count: 1,
            doing_count: 1,
            done_count: 1,
            total_count: 3,
            completion_pct: 33,
            open_findings_count: 0,
            attempts_count: 2,
            total_cost_usd: 1.25,
          },
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    const { unmount } = render(
      <RecordsBoardPane selectedProject="ferroloop" apiClient={apiClient} />,
    );

    await screen.findByTestId("record-card-101");

    const completionBar = screen.getByTestId("board-completion-bar");
    const bucketSelect = screen.getByTestId(
      "board-bucket-select",
    ) as HTMLSelectElement;
    const swimlanesGrid = screen.getByTestId("board-swimlanes-grid");

    // Positioned between completion bar and swimlanes grid
    expect(
      completionBar.compareDocumentPosition(bucketSelect) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(
      bucketSelect.compareDocumentPosition(swimlanesGrid) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();

    // Defaults to "todo"
    expect(bucketSelect.value).toBe("todo");
    expect(screen.getByTestId("swimlane-todo")).toHaveAttribute(
      "data-bucket-visible",
      "true",
    );
    expect(screen.getByTestId("swimlane-doing")).toHaveAttribute(
      "data-bucket-visible",
      "false",
    );
    expect(screen.getByTestId("swimlane-done")).toHaveAttribute(
      "data-bucket-visible",
      "false",
    );

    // Selecting "doing" updates visibility and persists to localStorage
    fireEvent.change(bucketSelect, { target: { value: "doing" } });
    expect(bucketSelect.value).toBe("doing");
    expect(window.localStorage.getItem(BOARD_BUCKET_STORAGE_KEY)).toBe("doing");
    expect(screen.getByTestId("swimlane-todo")).toHaveAttribute(
      "data-bucket-visible",
      "false",
    );
    expect(screen.getByTestId("swimlane-doing")).toHaveAttribute(
      "data-bucket-visible",
      "true",
    );

    unmount();

    // Re-mounting restores "doing" from localStorage
    render(
      <RecordsBoardPane selectedProject="ferroloop" apiClient={apiClient} />,
    );
    const restoredSelect = (await screen.findByTestId(
      "board-bucket-select",
    )) as HTMLSelectElement;
    expect(restoredSelect.value).toBe("doing");
  });

  it("Left-Panel Consoles (FerroState, Vault & Connectors, Workflows, Terminal): enforces minimum-width reactivity, two-line Hub subtabs, and clean Terminal reconnect without Fit button", () => {
    // 1. FerroStatePane: Scope Filter & Concern Filter selects stay bounded (width: 100%, minWidth: 0)
    const { unmount: unmountState } = render(
      <FerroStatePane
        entries={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );
    const stateSearchForm = screen.getByTestId("state-search-form");
    const scopeSelect = stateSearchForm.querySelectorAll("select")[0] as HTMLSelectElement;
    expect(scopeSelect.style.width).toBe("100%");
    expect(scopeSelect.style.maxWidth).toBe("100%");
    unmountState();

    // 2. VaultGatePane: + Add / Rotate Secret form uses bounded grid and box-sizing: border-box inputs
    const { unmount: unmountVault } = render(
      <VaultGatePane
        secrets={[]}
        connectors={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );
    const vaultPane = screen.getByTestId("vault-gate-pane");
    expect(vaultPane.style.overflow).toBe("hidden");
    const vaultBody = screen.getByTestId("vault-gate-body");
    expect(vaultBody).toHaveClass("fd-scroll");
    expect(vaultBody.style.overflowY).toBe("auto");
    const vaultForm = screen.getByTestId("vault-secret-form");
    expect(vaultForm.style.maxWidth).toBe("100%");
    expect(vaultForm.style.boxSizing).toBe("border-box");
    unmountVault();

    // 3. HubDrawer: Compact 2x2 subtab grid (subtab-workflows, subtab-routines, subtab-arch, subtab-stats) and bounded workflow-slug-input
    const { unmount: unmountHub } = render(
      <HubDrawer workflows={[]} workflowRuns={[]} />,
    );
    const hubDrawer = screen.getByTestId("hub-drawer");
    expect(hubDrawer.style.overflow).toBe("hidden");
    const hubDrawerBody = screen.getByTestId("hub-drawer-body");
    expect(hubDrawerBody).toHaveClass("fd-scroll");
    expect(hubDrawerBody.style.overflowY).toBe("auto");
    const subtabGrid = screen.getByTestId("hub-subtab-grid");
    expect(subtabGrid.style.display).toBe("grid");
    expect(subtabGrid.style.gridTemplateColumns).toBe("repeat(2, minmax(0, 1fr))");
    const archBtn = screen.getByTestId("subtab-arch");
    const statsBtn = screen.getByTestId("subtab-stats");
    expect(archBtn).toHaveTextContent("Architecture");
    expect(statsBtn).toHaveTextContent("Telemetry");
    const slugInput = screen.getByTestId("workflow-slug-input");
    expect(slugInput.style.width).toBe("100%");
    expect(slugInput.style.boxSizing).toBe("border-box");
    unmountHub();

    // 4. TerminalPane: No manual Fit button, and Reconnect / New Shell clears previous output log
    const onSendTerminalFrame = vi.fn();
    render(
      <TerminalPane
        authority={{ tier: "owner", login: "lmcfarlin" }}
        sessionId="term-react-01"
        cwd="/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop"
        terminalFrames={[
          {
            channel: "terminal",
            event: "output",
            data: {
              session_id: "term-react-01",
              stream: "stdout",
              data: "lmcfarlin@ai-lab-0:~/FerroLoop$ ",
            },
          },
        ]}
        onSendTerminalFrame={onSendTerminalFrame}
      />,
    );

    expect(screen.queryByRole("button", { name: /^Fit \(/i })).toBeNull();
    const outputLog = screen.getByTestId("terminal-output-log");
    expect(outputLog).toHaveTextContent("lmcfarlin@ai-lab-0:~/FerroLoop$");

    fireEvent.click(screen.getByTestId("terminal-reconnect-btn"));
    expect(outputLog).toHaveTextContent("Ready for interactive PTY output...");
  });

  it("Phone modal close buttons & right-panel split backdrop: renders mobile-split-backdrop when right split panel opens and provides top-right close buttons in Clear & Decommission modals", async () => {
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/api/v1/auth/session")) {
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
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
    const wsClient = {
      setToken: vi.fn(),
      connect: vi.fn(),
      disconnect: vi.fn(),
      onStatusChange: () => () => {},
      subscribe: () => () => {},
    } as unknown as HubWsClient;

    render(<App apiClient={apiClient} wsClient={wsClient} />);

    const splitToggleBtn = await screen.findByTestId("split-view-toggle");
    expect(screen.queryByTestId("mobile-split-backdrop")).toBeNull();

    // Opening right split panel renders mobile-split-backdrop; clicking backdrop closes split view
    fireEvent.click(splitToggleBtn);
    const splitBackdrop = screen.getByTestId("mobile-split-backdrop");
    expect(splitBackdrop).toBeInTheDocument();
    fireEvent.click(splitBackdrop);
    expect(screen.queryByTestId("mobile-split-backdrop")).toBeNull();

    // Open AgentSettingsModal -> Clear / Reforge modal -> close via top-right ✕ button
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    fireEvent.click(screen.getByTestId("open-clear-reforge-modal-btn"));
    expect(screen.getByTestId("clear-session-modal")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("clear-modal-close-btn"));
    expect(screen.queryByTestId("clear-session-modal")).toBeNull();

    // Open AgentSettingsModal -> Decommission modal -> close via top-right ✕ button
    fireEvent.click(screen.getByTestId("agent-settings-btn-ferris"));
    fireEvent.click(screen.getByTestId("open-decommission-modal-btn"));
    expect(screen.getByTestId("decommission-modal")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("decommission-modal-close-btn"));
    expect(screen.queryByTestId("decommission-modal")).toBeNull();
  });
});

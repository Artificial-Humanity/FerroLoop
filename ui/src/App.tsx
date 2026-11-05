import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import "./styles/tokens.css";
import { HubApiClient, HubApiError } from "./api/client";
import { HubWsClient } from "./api/ws";
import { ArtifactPreviewPane } from "./components/ArtifactPreviewPane";
import { ChatPane } from "./components/ChatPane";
import { ContextHeader, RightPanelHeader } from "./components/ContextHeader";
import { GitPrPane } from "./components/GitPrPane";
import {
  HelpDrawer,
  OPEN_HELP_EVENT,
  type HelpTopicId,
} from "./components/HelpDrawer";
import { HubDrawer } from "./components/HubDrawer";
import { NodesModal } from "./components/NodesModal";
import { PairingModal } from "./components/PairingModal";
import { PHONE_MAX_VIEWPORT_PX, useDeviceLayout } from "./hooks/useDeviceLayout";
import { useProjects } from "./hooks/useProjects";
import { PhoneShell } from "./components/phone/PhoneShell";
import { mapToPhoneView, type PhoneView } from "./components/phone/phoneNavigation";
import { RecordsBoardPane } from "./components/RecordsBoardPane";
import { OPEN_ARTIFACT_EVENT, normalizeArtifactTarget } from "./components/RichMarkdown";
import {
  RosterSidebar,
  classifyTurnUnreadSeverity,
  classifyWireUnreadSeverity,
  hasTrailingIncompleteWork,
  mergeUnreadSeverity,
  type LeftPanelTab,
  type UnreadSeverity,
} from "./components/RosterSidebar";
import {
  SAFE_AREA_MARGINS_EVENT,
  SettingsModal,
  executeScopedBulkSessionAction,
  persistShowSafeAreaMargins,
  readInitialShowSafeAreaMargins,
  type AgentsSubTabId,
  type ProjectSettingsSubTabId,
  type SettingsOriginMemento,
  type SettingsTabId,
} from "./components/SettingsModal";
import { SpinUpAgentModal } from "./components/SpinUpAgentModal";
import { TerminalPane } from "./components/TerminalPane";
import { VaultGatePane } from "./components/VaultGatePane";
import { FerroStatePane } from "./components/FerroStatePane";
import { FerroWatchPane } from "./components/FerroWatchPane";
import { TriageQueuePane } from "./components/TriageQueuePane";
import { WireMailPane } from "./components/WireMailPane";
import {
  setActiveTimeZone,
  useActiveTimeZone,
} from "./lib/datetime";
import {
  FONT_SCALE_MAX_PCT,
  FONT_SCALE_MIN_PCT,
  FONT_SCALE_PRESETS,
  FONT_SCALE_STEP_PCT,
  FONT_SCALE_STORAGE_KEY,
  THEME_STORAGE_KEY,
  applyFontScalePct,
  applyFontScaleToElementTree,
  applyThemeMode,
  clampFontScalePct,
  getInitialFontScalePct,
  getInitialThemeMode,
  persistFontScalePct,
  persistThemeMode,
  resolveEffectiveTheme,
  resolveSystemTheme,
  type ThemeMode,
} from "./lib/accessibility";
import {
  DEFAULT_WORKSPACE_AGENTS,
  type AgentLivenessItem,
  type AgentRecord,
  type AuthSessionResponse,
  type ArchitectureResponse,
  type BulkSessionAction,
  type CockpitTab,
  type DecommissionMode,
  type DiscoveredSessionSummary,
  type EnrolledNode,
  type HubAuthority,
  type HubStatsResponse,
  type PrMergeRequest,
  type PromptRequest,
  type PrReviewRequest,
  type PullRequestItem,
  type SessionContextUsage,
  type SessionQuotaUsage,
  type SpinUpSessionRequest,
  type QueuedOperatorPrompt,
  type TranscriptTurn,
  type WireInboxItem,
  type WireSendRequest,
  type WorkflowDef,
  type WorkflowRun,
  type WorktreeDiff,
  type WorktreeSummary,
} from "./types/api";

export interface AppProps {
  apiClient?: HubApiClient;
  wsClient?: HubWsClient;
  initialSelectedAgentName?: string | null;
}

export const PANEL_MIN_WIDTH_PX = 360;
export const TWO_PANEL_MIN_VIEWPORT_PX = PHONE_MAX_VIEWPORT_PX;
export const THREE_PANEL_MIN_VIEWPORT_PX = 1080;

export type ViewportPanelMode = "single" | "two-panel" | "three-panel";

export type TabletMaster = "docked" | "overlay";
/** Docked tablet master (roster / console) width limits, in px. */
export const TABLET_MASTER_MIN_WIDTH_PX = 320;
export const TABLET_MASTER_MAX_WIDTH_PX = 360;

export function resolveTabletDockedWidth(viewportWidth: number): number {
  return Math.min(
    TABLET_MASTER_MAX_WIDTH_PX,
    Math.max(TABLET_MASTER_MIN_WIDTH_PX, Math.round(viewportWidth * 0.38)),
  );
}

export function resolveViewportPanelMode(width: number): ViewportPanelMode {
  if (width < TWO_PANEL_MIN_VIEWPORT_PX) {
    return "single";
  }
  if (width < THREE_PANEL_MIN_VIEWPORT_PX) {
    return "two-panel";
  }
  return "three-panel";
}

function readStoredPanelWidth(
  key: string,
  fallback: number,
  min: number,
  max: number,
): number {
  if (typeof window === "undefined") return fallback;
  try {
    const raw = window.localStorage.getItem(key);
    if (!raw) return fallback;
    const parsed = Number(raw);
    if (!Number.isFinite(parsed) || parsed <= 0) return fallback;
    return Math.round(Math.max(min, Math.min(max, parsed)));
  } catch {
    return fallback;
  }
}

function persistPanelWidth(key: string, width: number): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(key, String(Math.round(width)));
  } catch {
    // Ignore storage write errors
  }
}

export {
  FONT_SCALE_MAX_PCT,
  FONT_SCALE_MIN_PCT,
  FONT_SCALE_PRESETS,
  FONT_SCALE_STEP_PCT,
  FONT_SCALE_STORAGE_KEY,
  THEME_STORAGE_KEY,
  applyFontScalePct,
  applyFontScaleToElementTree,
  applyThemeMode,
  clampFontScalePct,
  getInitialFontScalePct,
  getInitialThemeMode,
  persistFontScalePct,
  persistThemeMode,
  resolveEffectiveTheme,
  resolveSystemTheme,
  type ThemeMode,
};

export interface OptimisticPrompt {
  content: string;
  sentAt: number;
  agentKey?: string;
  sessionId?: string;
}

export const OPTIMISTIC_PROMPT_TTL_MS = 60_000;

function isPromptOnDisk(turns: TranscriptTurn[], content: string): boolean {
  const trimmed = content.trim();
  if (!trimmed) return true;
  return turns.slice(-10).some((turn) => {
    if ((turn.role || "").toLowerCase() !== "user" || !turn.content) {
      return false;
    }
    return (
      turn.content.trim() === trimmed || turn.content.includes(trimmed)
    );
  });
}

export function pruneOptimisticPrompts(
  prompts: OptimisticPrompt[],
  turns: TranscriptTurn[],
  nowMs: number = Date.now(),
  currentAgentKey?: string | null,
  currentSessionId?: string | null,
): OptimisticPrompt[] {
  const activeKey = (currentAgentKey ?? "").trim().toLowerCase();
  return prompts.filter((p) => {
    if (!p || !p.content?.trim()) return false;
    if (nowMs - p.sentAt > OPTIMISTIC_PROMPT_TTL_MS) return false;
    const promptKey = (p.agentKey ?? "").trim().toLowerCase();
    const matchesActive = (!activeKey || !promptKey || promptKey === activeKey) &&
      (currentSessionId === undefined || (p.sessionId ?? null) === currentSessionId);
    if (matchesActive && isPromptOnDisk(turns, p.content)) {
      return false;
    }
    return true;
  });
}

/**
 * Keep just-sent user turns visible in chronological order until the vendor CLI
 * flushes them to the on-disk transcript. Without this, an immediate transcript
 * refresh replaces the optimistic turns with a stale disk snapshot and the
 * outbound prompts vanish while in FerroWire transit or queue buffer.
 */
export function preserveOptimisticTurn(
  turns: TranscriptTurn[],
  optimistic: OptimisticPrompt | OptimisticPrompt[] | null | undefined,
  nowMs: number = Date.now(),
  currentAgentKey?: string | null,
  currentSessionId?: string | null,
): TranscriptTurn[] {
  const rawList: OptimisticPrompt[] = Array.isArray(optimistic)
    ? optimistic
    : optimistic
      ? [optimistic]
      : [];
  if (rawList.length === 0) {
    return turns;
  }
  const activeKey = (currentAgentKey ?? "").trim().toLowerCase();
  const pending: OptimisticPrompt[] = [];
  for (const item of rawList) {
    if (!item || !item.content?.trim()) continue;
    if (nowMs - item.sentAt > OPTIMISTIC_PROMPT_TTL_MS) continue;
    const itemKey = (item.agentKey ?? "").trim().toLowerCase();
    if (currentSessionId !== undefined && (item.sessionId ?? null) !== currentSessionId) continue;
    if (activeKey && itemKey && itemKey !== activeKey) {
      continue;
    }
    if (isPromptOnDisk(turns, item.content)) {
      continue;
    }
    pending.push(item);
  }
  if (pending.length === 0) {
    return turns;
  }
  pending.sort((a, b) => a.sentAt - b.sentAt);
  const out = [...turns];
  for (const item of pending) {
    if (isPromptOnDisk(out, item.content)) {
      continue;
    }
    const last = out[out.length - 1];
    out.push({
      index: last ? last.index + 1 : out.length,
      role: "user",
      content: item.content,
      thinking: null,
      tool_calls: [],
      created_at: new Date(item.sentAt).toISOString(),
    });
  }
  return out.length === turns.length ? turns : out;
}

/** Live transcript poll cadence: tighter while the agent works. */
export function transcriptPollIntervalMs(isSessionBusy: boolean): number {
  return isSessionBusy ? 2_000 : 3_500;
}

export const LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY =
  "ferrodeck.last_seen_assistant_turn";

/**
 * Only prose assistant output addressed to the operator counts toward a
 * roster unread badge: no wire turns, no tool output, no thinking-only.
 */
export function isUserDirectedAssistantTurn(turn: TranscriptTurn): boolean {
  if ((turn.role || "").toLowerCase() !== "assistant") {
    return false;
  }
  const content = turn.content || "";
  if (content.trim().length === 0) {
    return false;
  }
  return !content.trimStart().startsWith("Created At:");
}

export function maxUserDirectedTurnIndex(turns: TranscriptTurn[]): number {
  let max = -1;
  for (const turn of turns) {
    if (isUserDirectedAssistantTurn(turn) && turn.index > max) {
      max = turn.index;
    }
  }
  return max;
}

/**
 * Unseen user-directed assistant turns past `lastSeenIndex`. A missing
 * baseline counts as zero: first observation establishes the baseline so
 * history never badges on first load.
 */
export function countUnseenAssistantTurns(
  turns: TranscriptTurn[],
  lastSeenIndex?: number,
): number {
  if (lastSeenIndex === undefined) {
    return 0;
  }
  return turns.filter(
    (turn) => isUserDirectedAssistantTurn(turn) && turn.index > lastSeenIndex,
  ).length;
}

export function computeUnreadByAgent(
  transcriptsByAgent: Record<string, TranscriptTurn[]>,
  lastSeenAssistantTurnIndex: Record<string, number>,
): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const [agentKey, turns] of Object.entries(transcriptsByAgent)) {
    const count = countUnseenAssistantTurns(
      turns,
      lastSeenAssistantTurnIndex[agentKey],
    );
    if (count > 0) {
      counts[agentKey] = count;
    }
  }
  return counts;
}

export function computeUnreadSeverityByAgent(
  transcriptsByAgent: Record<string, TranscriptTurn[]>,
  lastSeenAssistantTurnIndex: Record<string, number>,
  wireMessages?: WireInboxItem[],
): Record<string, UnreadSeverity> {
  const severities: Record<string, UnreadSeverity> = {};
  for (const [agentKey, turns] of Object.entries(transcriptsByAgent)) {
    const lastSeen = lastSeenAssistantTurnIndex[agentKey];
    if (lastSeen === undefined) continue;
    const unseen = turns.filter(
      (turn) => isUserDirectedAssistantTurn(turn) && turn.index > lastSeen,
    );
    if (unseen.length > 0) {
      let sev: UnreadSeverity = "normal";
      for (const turn of unseen) {
        sev = mergeUnreadSeverity(sev, classifyTurnUnreadSeverity(turn));
        if (sev === "important") break;
      }
      severities[agentKey.toLowerCase()] = sev;
    }
  }
  if (Array.isArray(wireMessages)) {
    for (const item of wireMessages) {
      const receiptStatus = (item.receipt?.status || "").toLowerCase();
      const allAcked =
        receiptStatus === "acked" ||
        Boolean(item.receipt?.acked_at) ||
        (Array.isArray(item.receipts) &&
          item.receipts.length > 0 &&
          item.receipts.every(
            (r) => (r.status || "").toLowerCase() === "acked" || Boolean(r.acked_at),
          ));
      if (allAcked) continue;
      const recipient = (item.message?.to || "").trim().toLowerCase();
      if (!recipient || recipient === "all") continue;
      const msgSev = classifyWireUnreadSeverity(item);
      severities[recipient] = mergeUnreadSeverity(
        severities[recipient] ?? "normal",
        msgSev,
      );
    }
  }
  return severities;
}

function readInitialLastSeenAssistantTurn(): Record<string, number> {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      const raw = window.localStorage.getItem(
        LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY,
      );
      if (raw) {
        const parsed = JSON.parse(raw) as unknown;
        if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
          const out: Record<string, number> = {};
          for (const [key, value] of Object.entries(
            parsed as Record<string, unknown>,
          )) {
            if (typeof value === "number" && Number.isFinite(value)) {
              out[key.toLowerCase()] = value;
            }
          }
          return out;
        }
      }
    }
  } catch {
    // Ignore storage access errors in private/sandboxed contexts.
  }
  return {};
}

export const App: React.FC<AppProps> = ({
  apiClient: propApiClient,
  wsClient: propWsClient,
  initialSelectedAgentName,
}) => {
  const apiClient = useMemo(() => propApiClient ?? new HubApiClient(), [propApiClient]);
  // One project list for the switcher, the roster, Spin-Up and the settings dropdowns.
  const {
    projects,
    fromHub: projectsFromHub,
    refresh: refreshProjects,
  } = useProjects(apiClient);
  const wsClientRef = useRef<HubWsClient | null>(null);
  if (!wsClientRef.current) {
    wsClientRef.current =
      propWsClient ??
      new HubWsClient({
        token: apiClient.getToken(),
        autoReconnect: true,
      });
  }
  const wsClient = propWsClient ?? wsClientRef.current;

  // Auth & Pairing State
  const [authority, setAuthority] = useState<HubAuthority | null>(null);
  const [pairingOpen, setPairingOpen] = useState<boolean>(false);
  const [pairingError, setPairingError] = useState<string | null>(null);
  const [nodesModalOpen, setNodesModalOpen] = useState<boolean>(false);
  const [settingsModalOpen, setSettingsModalOpen] = useState<boolean>(false);
  const [settingsInitialProject, setSettingsInitialProject] = useState<string | null>(null);
  const [settingsInitialProjectFilter, setSettingsInitialProjectFilter] =
    useState<string | null>(null);
  const [settingsInitialTab, setSettingsInitialTab] =
    useState<SettingsTabId | undefined>(undefined);
  const [settingsInitialProjectSubTab, setSettingsInitialProjectSubTab] =
    useState<ProjectSettingsSubTabId | undefined>(undefined);
  const [settingsInitialAgentsSubTab, setSettingsInitialAgentsSubTab] =
    useState<AgentsSubTabId | undefined>(undefined);
  const [settingsOriginMemento, setSettingsOriginMemento] =
    useState<SettingsOriginMemento | null>(null);
  const [helpDrawerOpen, setHelpDrawerOpen] = useState<boolean>(false);
  const [helpTopic, setHelpTopic] = useState<HelpTopicId>("chat");
  const [spinUpModalOpen, setSpinUpModalOpen] = useState<boolean>(false);
  const [spinUpInitialProject, setSpinUpInitialProject] = useState<string | null>(null);
  const [agentSettingsOpen, setAgentSettingsOpen] = useState<boolean>(false);
  const [nodes, setNodes] = useState<EnrolledNode[]>([]);
  const [wsConnected, setWsConnected] = useState<boolean>(false);

  // Navigation & Layout State
  const [activeTab, setActiveTab] = useState<CockpitTab>("chat");
  const [secondaryTab, setSecondaryTab] = useState<Exclude<CockpitTab, "chat">>("git");
  const [splitView, setSplitView] = useState<boolean>(false);
  const [leftPanelTab, setLeftPanelTab] = useState<LeftPanelTab>("roster");
  const [leftRosterWidth, setLeftRosterWidth] = useState<number>(() =>
    readStoredPanelWidth("fd_left_roster_width", PANEL_MIN_WIDTH_PX, PANEL_MIN_WIDTH_PX, 720),
  );
  const [leftConsoleWidth, setLeftConsoleWidth] = useState<number>(() =>
    readStoredPanelWidth("fd_left_console_width", 440, PANEL_MIN_WIDTH_PX, 720),
  );
  const [rightPanelWidth, setRightPanelWidth] = useState<number>(() =>
    readStoredPanelWidth("fd_right_panel_width", 460, PANEL_MIN_WIDTH_PX, 800),
  );
  const panelResizeCleanupRef = useRef<(() => void) | null>(null);
  const [mobileRosterOpen, setMobileRosterOpen] = useState<boolean>(false);
  const [desktopSidebarCollapsed, setDesktopSidebarCollapsed] = useState<boolean>(false);
  const { layout: deviceLayout, orientation: deviceOrientation } = useDeviceLayout();
  const deviceLayoutRef = useRef(deviceLayout);
  deviceLayoutRef.current = deviceLayout;
  // Tablet master-detail: the roster is docked in landscape and an overlay drawer in portrait.
  const tabletMaster: TabletMaster | undefined =
    deviceLayout === "tablet"
      ? deviceOrientation === "portrait"
        ? "overlay"
        : "docked"
      : undefined;
  const tabletMasterRef = useRef(tabletMaster);
  tabletMasterRef.current = tabletMaster;
  // Phone navigation state. Lives apart from the desktop arrangement (splitView, leftPanelTab,
  // mobileRosterOpen, desktopSidebarCollapsed) so rotating or resizing restores the desktop view.
  const [phoneView, setPhoneView] = useState<PhoneView>("chat");
  const navigatePhone = useCallback((tab: CockpitTab | LeftPanelTab) => {
    setPhoneView(mapToPhoneView(tab));
  }, []);
  const [viewportWidth, setViewportWidth] = useState<number>(() =>
    typeof window !== "undefined" ? window.innerWidth : 1200,
  );
  const [themeMode, setThemeMode] = useState<ThemeMode>(getInitialThemeMode);
  const [fontScalePct, setFontScalePct] = useState<number>(getInitialFontScalePct);
  const [showSafeAreaMargins, setShowSafeAreaMargins] = useState<boolean>(
    readInitialShowSafeAreaMargins,
  );
  const safeAreaProbeRef = useRef<HTMLDivElement | null>(null);
  const [safeAreaMetrics, setSafeAreaMetrics] = useState(() => ({
    top: 0,
    right: 0,
    bottom: 0,
    left: 0,
    width: typeof window !== "undefined" ? window.innerWidth : 1200,
    height: typeof window !== "undefined" ? window.innerHeight : 800,
    visualHeight:
      typeof window !== "undefined"
        ? Math.round(window.visualViewport?.height ?? window.innerHeight)
        : 800,
    dpr:
      typeof window !== "undefined"
        ? Number((window.devicePixelRatio || 1).toFixed(2))
        : 1,
  }));
  useActiveTimeZone();
  const [activeArtifactPath, setActiveArtifactPath] = useState<string | null>(null);
  const [recentArtifacts, setRecentArtifacts] = useState<string[]>([]);

  useEffect(() => {
    return () => {
      panelResizeCleanupRef.current?.();
    };
  }, []);

  const lastResizeWidthRef = useRef<number>(
    typeof window !== "undefined" ? window.innerWidth : 1200,
  );
  useEffect(() => {
    if (typeof window === "undefined") return;
    const handleResize = () => {
      const vw = window.innerWidth;
      setViewportWidth(vw);
      // Height-only resizes (soft keyboard, collapsing address bar) must not rearrange panels.
      const widthChanged = vw !== lastResizeWidthRef.current;
      lastResizeWidthRef.current = vw;
      if (!widthChanged) {
        return;
      }
      // The phone shell keeps its own navigation state and never edits the desktop arrangement,
      // so a phone-width resize leaves split view and the left panel exactly as they were.
      if (vw < TWO_PANEL_MIN_VIEWPORT_PX) {
        return;
      }
      if (vw < THREE_PANEL_MIN_VIEWPORT_PX) {
        setSplitView((currentSplit) => {
          if (currentSplit) {
            setDesktopSidebarCollapsed(true);
            setMobileRosterOpen(false);
          }
          return currentSplit;
        });
      }
    };
    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const handleSafeMarginsEvent = (event: Event) => {
      const custom = event as CustomEvent<{ enabled?: boolean }>;
      if (typeof custom.detail?.enabled === "boolean") {
        setShowSafeAreaMargins(custom.detail.enabled);
      } else {
        setShowSafeAreaMargins(readInitialShowSafeAreaMargins());
      }
    };
    window.addEventListener(SAFE_AREA_MARGINS_EVENT, handleSafeMarginsEvent);
    return () =>
      window.removeEventListener(
        SAFE_AREA_MARGINS_EVENT,
        handleSafeMarginsEvent,
      );
  }, []);

  useEffect(() => {
    if (!showSafeAreaMargins || typeof window === "undefined") return;
    const measureSafeArea = () => {
      const probe = safeAreaProbeRef.current;
      let top = 0;
      let right = 0;
      let bottom = 0;
      let left = 0;
      if (probe && typeof window.getComputedStyle === "function") {
        const computed = window.getComputedStyle(probe);
        top = Math.round( parseFloat(computed.paddingTop || "0") || 0 );
        right = Math.round( parseFloat(computed.paddingRight || "0") || 0 );
        bottom = Math.round( parseFloat(computed.paddingBottom || "0") || 0 );
        left = Math.round( parseFloat(computed.paddingLeft || "0") || 0 );
      }
      setSafeAreaMetrics({
        top,
        right,
        bottom,
        left,
        width: window.innerWidth,
        height: window.innerHeight,
        visualHeight: Math.round(
          window.visualViewport?.height ?? window.innerHeight,
        ),
        dpr: Number((window.devicePixelRatio || 1).toFixed(2)),
      });
    };
    measureSafeArea();
    window.addEventListener("resize", measureSafeArea);
    const vv = window.visualViewport;
    vv?.addEventListener("resize", measureSafeArea);
    vv?.addEventListener("scroll", measureSafeArea);
    return () => {
      window.removeEventListener("resize", measureSafeArea);
      vv?.removeEventListener("resize", measureSafeArea);
      vv?.removeEventListener("scroll", measureSafeArea);
    };
  }, [showSafeAreaMargins]);

  const effectiveLeftWidth = Math.max(
    PANEL_MIN_WIDTH_PX,
    leftPanelTab === "roster" ? leftRosterWidth : leftConsoleWidth,
  );

  const applyLeftResizeDelta = useCallback(
    (startWidth: number, delta: number, isRoster: boolean) => {
      const vw = typeof window !== "undefined" ? window.innerWidth : 1200;
      const mode = resolveViewportPanelMode(vw);
      const maxAllowed =
        mode === "three-panel"
          ? Math.max(
              PANEL_MIN_WIDTH_PX,
              Math.min(
                720,
                vw - PANEL_MIN_WIDTH_PX - (splitView ? rightPanelWidth : 0),
              ),
            )
          : mode === "two-panel"
            ? Math.max(PANEL_MIN_WIDTH_PX, Math.min(720, vw - PANEL_MIN_WIDTH_PX))
            : 720;
      const nextWidth = Math.round(
        Math.max(PANEL_MIN_WIDTH_PX, Math.min(maxAllowed, startWidth + delta)),
      );
      if (isRoster) {
        setLeftRosterWidth(nextWidth);
        persistPanelWidth("fd_left_roster_width", nextWidth);
      } else {
        setLeftConsoleWidth(nextWidth);
        persistPanelWidth("fd_left_console_width", nextWidth);
      }
    },
    [splitView, rightPanelWidth],
  );

  const handleLeftResizeStart = useCallback(
    (e: React.MouseEvent<HTMLDivElement>) => {
      e.preventDefault();
      panelResizeCleanupRef.current?.();
      const startX = e.clientX;
      const isRoster = leftPanelTab === "roster";
      const startWidth = isRoster ? leftRosterWidth : leftConsoleWidth;
      const handleMouseMove = (moveEvent: MouseEvent) => {
        applyLeftResizeDelta(startWidth, moveEvent.clientX - startX, isRoster);
      };
      const handleMouseUp = () => {
        window.removeEventListener("mousemove", handleMouseMove);
        window.removeEventListener("mouseup", handleMouseUp);
        panelResizeCleanupRef.current = null;
      };
      window.addEventListener("mousemove", handleMouseMove);
      window.addEventListener("mouseup", handleMouseUp);
      panelResizeCleanupRef.current = handleMouseUp;
    },
    [leftPanelTab, leftRosterWidth, leftConsoleWidth, applyLeftResizeDelta],
  );

  const handleLeftTouchResizeStart = useCallback(
    (e: React.TouchEvent<HTMLDivElement>) => {
      if (!e.touches || e.touches.length === 0) return;
      e.preventDefault();
      panelResizeCleanupRef.current?.();
      const startX = e.touches[0].clientX;
      const isRoster = leftPanelTab === "roster";
      const startWidth = isRoster ? leftRosterWidth : leftConsoleWidth;
      const handleTouchMove = (moveEvent: TouchEvent) => {
        if (!moveEvent.touches || moveEvent.touches.length === 0) return;
        if (moveEvent.cancelable) {
          moveEvent.preventDefault();
        }
        applyLeftResizeDelta(
          startWidth,
          moveEvent.touches[0].clientX - startX,
          isRoster,
        );
      };
      const handleTouchEnd = () => {
        window.removeEventListener("touchmove", handleTouchMove);
        window.removeEventListener("touchend", handleTouchEnd);
        window.removeEventListener("touchcancel", handleTouchEnd);
        panelResizeCleanupRef.current = null;
      };
      window.addEventListener("touchmove", handleTouchMove, { passive: false });
      window.addEventListener("touchend", handleTouchEnd);
      window.addEventListener("touchcancel", handleTouchEnd);
      panelResizeCleanupRef.current = handleTouchEnd;
    },
    [leftPanelTab, leftRosterWidth, leftConsoleWidth, applyLeftResizeDelta],
  );

  const applyRightResizeDelta = useCallback(
    (startWidth: number, delta: number) => {
      const vw = typeof window !== "undefined" ? window.innerWidth : 1200;
      const mode = resolveViewportPanelMode(vw);
      const maxAllowed =
        mode === "three-panel"
          ? Math.max(
              PANEL_MIN_WIDTH_PX,
              Math.min(
                800,
                vw -
                  PANEL_MIN_WIDTH_PX -
                  (desktopSidebarCollapsed ? 0 : effectiveLeftWidth),
              ),
            )
          : mode === "two-panel"
            ? Math.max(PANEL_MIN_WIDTH_PX, Math.min(800, vw - PANEL_MIN_WIDTH_PX))
            : 800;
      const nextWidth = Math.round(
        Math.max(PANEL_MIN_WIDTH_PX, Math.min(maxAllowed, startWidth + delta)),
      );
      setRightPanelWidth(nextWidth);
      persistPanelWidth("fd_right_panel_width", nextWidth);
    },
    [desktopSidebarCollapsed, effectiveLeftWidth],
  );

  const handleRightResizeStart = useCallback(
    (e: React.MouseEvent<HTMLDivElement>) => {
      e.preventDefault();
      panelResizeCleanupRef.current?.();
      const startX = e.clientX;
      const startWidth = rightPanelWidth;
      const handleMouseMove = (moveEvent: MouseEvent) => {
        applyRightResizeDelta(startWidth, startX - moveEvent.clientX);
      };
      const handleMouseUp = () => {
        window.removeEventListener("mousemove", handleMouseMove);
        window.removeEventListener("mouseup", handleMouseUp);
        panelResizeCleanupRef.current = null;
      };
      window.addEventListener("mousemove", handleMouseMove);
      window.addEventListener("mouseup", handleMouseUp);
      panelResizeCleanupRef.current = handleMouseUp;
    },
    [rightPanelWidth, applyRightResizeDelta],
  );

  const handleRightTouchResizeStart = useCallback(
    (e: React.TouchEvent<HTMLDivElement>) => {
      if (!e.touches || e.touches.length === 0) return;
      e.preventDefault();
      panelResizeCleanupRef.current?.();
      const startX = e.touches[0].clientX;
      const startWidth = rightPanelWidth;
      const handleTouchMove = (moveEvent: TouchEvent) => {
        if (!moveEvent.touches || moveEvent.touches.length === 0) return;
        if (moveEvent.cancelable) {
          moveEvent.preventDefault();
        }
        applyRightResizeDelta(startWidth, startX - moveEvent.touches[0].clientX);
      };
      const handleTouchEnd = () => {
        window.removeEventListener("touchmove", handleTouchMove);
        window.removeEventListener("touchend", handleTouchEnd);
        window.removeEventListener("touchcancel", handleTouchEnd);
        panelResizeCleanupRef.current = null;
      };
      window.addEventListener("touchmove", handleTouchMove, { passive: false });
      window.addEventListener("touchend", handleTouchEnd);
      window.addEventListener("touchcancel", handleTouchEnd);
      panelResizeCleanupRef.current = handleTouchEnd;
    },
    [rightPanelWidth, applyRightResizeDelta],
  );

  const handleOpenArtifact = useCallback((rawPathOrUrl: string) => {
    const trimmed = (rawPathOrUrl || "").trim();
    if (!trimmed) return;
    const normalized = trimmed.startsWith("mermaid://")
      ? trimmed
      : normalizeArtifactTarget(trimmed);
    if (!normalized) return;
    setActiveArtifactPath(normalized);
    setRecentArtifacts((prev) => {
      const filtered = prev.filter((item) => item !== normalized);
      return [normalized, ...filtered].slice(0, 20);
    });
    if (deviceLayoutRef.current === "phone") {
      navigatePhone("artifacts");
      return;
    }
    setSplitView(true);
    if (
      typeof window !== "undefined" &&
      window.innerWidth < THREE_PANEL_MIN_VIEWPORT_PX
    ) {
      setDesktopSidebarCollapsed(true);
    }
    setMobileRosterOpen(false);
    setSecondaryTab("artifacts");
    setActiveTab("artifacts");
  }, [navigatePhone]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const handler = (event: Event) => {
      const custom = event as CustomEvent<{ path?: string }>;
      if (custom.detail?.path) {
        handleOpenArtifact(custom.detail.path);
      }
    };
    window.addEventListener(OPEN_ARTIFACT_EVENT, handler);
    return () => window.removeEventListener(OPEN_ARTIFACT_EVENT, handler);
  }, [handleOpenArtifact]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const handler = (event: Event) => {
      const custom = event as CustomEvent<{ topic?: HelpTopicId }>;
      if (custom.detail?.topic) {
        setHelpTopic(custom.detail.topic);
      }
      setHelpDrawerOpen(true);
    };
    window.addEventListener(OPEN_HELP_EVENT, handler);
    return () => window.removeEventListener(OPEN_HELP_EVENT, handler);
  }, []);

  useEffect(() => {
    applyThemeMode(themeMode);
    if (
      themeMode !== "system" ||
      typeof window === "undefined" ||
      typeof window.matchMedia !== "function"
    ) {
      return;
    }
    const mql = window.matchMedia("(prefers-color-scheme: light)");
    const handleChange = (event?: MediaQueryListEvent) => {
      if (typeof document !== "undefined") {
        const isLight =
          typeof event?.matches === "boolean" ? event.matches : mql.matches;
        document.documentElement.setAttribute(
          "data-theme",
          isLight ? "light" : "dark",
        );
      }
    };
    if (typeof mql.addEventListener === "function") {
      mql.addEventListener("change", handleChange);
      return () => mql.removeEventListener("change", handleChange);
    }
    if (typeof mql.addListener === "function") {
      mql.addListener(handleChange);
      return () => mql.removeListener(handleChange);
    }
  }, [themeMode]);

  useEffect(() => {
    applyFontScalePct(fontScalePct);
    if (
      fontScalePct === 100 ||
      typeof document === "undefined" ||
      typeof MutationObserver === "undefined"
    ) {
      return;
    }
    const cockpit = document.querySelector(".fd-cockpit");
    if (!cockpit) {
      return;
    }
    let applying = false;
    const observer = new MutationObserver(() => {
      if (applying) return;
      applying = true;
      try {
        applyFontScaleToElementTree(cockpit, fontScalePct);
      } finally {
        applying = false;
      }
    });
    observer.observe(cockpit, { childList: true, subtree: true });
    return () => observer.disconnect();
  }, [fontScalePct]);

  const handleToggleTheme = useCallback(() => {
    const effective = resolveEffectiveTheme(themeMode);
    const next: ThemeMode = effective === "dark" ? "light" : "dark";
    persistThemeMode(next);
    setThemeMode(next);
  }, [themeMode]);

  const handleChangeThemeMode = useCallback((next: ThemeMode) => {
    persistThemeMode(next);
    applyThemeMode(next);
    setThemeMode(next);
  }, []);

  const handleChangeFontScalePct = useCallback((nextPct: number) => {
    const clamped = clampFontScalePct(nextPct);
    persistFontScalePct(clamped);
    applyFontScalePct(clamped);
    setFontScalePct(clamped);
  }, []);

  // Data State
  const [agents, setAgents] = useState<AgentRecord[]>(DEFAULT_WORKSPACE_AGENTS);
  const [routes, setRoutes] = useState<DiscoveredSessionSummary[]>([]);
  const [liveness, setLiveness] = useState<AgentLivenessItem[]>([]);
  const [sessions, setSessions] = useState<DiscoveredSessionSummary[]>([]);
  const [selectedAgentName, setSelectedAgentName] = useState<string>(
    initialSelectedAgentName === undefined ? "ferris" : (initialSelectedAgentName ?? ""),
  );
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);
  const selectedAgentNameRef = useRef<string>(selectedAgentName);
  selectedAgentNameRef.current = selectedAgentName;
  const selectedSessionIdRef = useRef<string | null>(selectedSessionId);
  selectedSessionIdRef.current = selectedSessionId;
  const [transcriptTurns, setTranscriptTurns] = useState<TranscriptTurn[]>([]);
  const [sessionQueue, setSessionQueue] = useState<QueuedOperatorPrompt[]>([]);
  const [isSessionBusy, setIsSessionBusy] = useState<boolean>(false);
  const [unconfirmedBySession, setUnconfirmedBySession] = useState<Record<string, boolean>>({});
  const [sessionContextUsage, setSessionContextUsage] =
    useState<SessionContextUsage | null>(null);
  const [sessionQuotaUsage, setSessionQuotaUsage] =
    useState<SessionQuotaUsage | null>(null);
  const optimisticPromptRef = useRef<OptimisticPrompt[]>([]);
  const activeSessionIdRef = useRef<string | null>(null);
  const refreshInFlightRef = useRef<boolean>(false);
  const tornDownTargetsRef = useRef<Set<string>>(new Set());
  const loggedOutRef = useRef<boolean>(false);
  const pairInFlightRef = useRef<Promise<AuthSessionResponse> | null>(null);
  const [lastSeenAssistantTurnIndex, setLastSeenAssistantTurnIndex] =
    useState<Record<string, number>>(readInitialLastSeenAssistantTurn);
  const [backgroundTranscripts, setBackgroundTranscripts] = useState<
    Record<string, TranscriptTurn[]>
  >({});
  const backgroundTranscriptsRef = useRef<Record<string, TranscriptTurn[]>>({});
  backgroundTranscriptsRef.current = backgroundTranscripts;
  const backgroundStampRef = useRef<Record<string, string>>({});
  const [busyByAgent, setBusyByAgent] = useState<Record<string, boolean>>({});
  const [contextUsageByAgent, setContextUsageByAgent] = useState<
    Record<string, SessionContextUsage | null>
  >({});

  const recordAgentBusy = useCallback(
    (agentKey: string | undefined | null, isBusy: boolean) => {
      const key = (agentKey ?? "").trim().toLowerCase();
      if (!key) {
        return;
      }
      setBusyByAgent((prev) =>
        prev[key] === isBusy ? prev : { ...prev, [key]: isBusy },
      );
    },
    [],
  );

  const recordAgentContextUsage = useCallback(
    (
      agentKey: string | undefined | null,
      usage: SessionContextUsage | null | undefined,
    ) => {
      const key = (agentKey ?? "").trim().toLowerCase();
      if (!key || usage === undefined) {
        return;
      }
      setContextUsageByAgent((prev) => {
        const existing = prev[key];
        if (
          existing === usage ||
          (existing &&
            usage &&
            existing.used_tokens === usage.used_tokens &&
            existing.max_tokens === usage.max_tokens)
        ) {
          return prev;
        }
        return { ...prev, [key]: usage };
      });
    },
    [],
  );

  const recordOptimisticPrompt = useCallback(
    (content: string, agentKey?: string | null, sentAt: number = Date.now(), sessionId?: string | null) => {
      const trimmed = (content || "").trim();
      if (!trimmed) return;
      const normalizedKey = (agentKey ?? "").trim().toLowerCase();
      const current = optimisticPromptRef.current.filter(
        (p) => sentAt - p.sentAt <= OPTIMISTIC_PROMPT_TTL_MS,
      );
      const duplicate = current.some(
        (p) =>
          p.content.trim() === trimmed &&
          (p.agentKey ?? "").trim().toLowerCase() === normalizedKey &&
          (p.sessionId ?? "") === (sessionId ?? "") &&
          Math.abs(sentAt - p.sentAt) < 3_000,
      );
      if (!duplicate) {
        current.push({
          content,
          sentAt,
          agentKey: normalizedKey || undefined,
          sessionId: sessionId || undefined,
        });
      }
      optimisticPromptRef.current = current;
    },
    [],
  );

  useEffect(() => {
    try {
      if (typeof window !== "undefined" && window.localStorage) {
        window.localStorage.setItem(
          LAST_SEEN_ASSISTANT_TURN_STORAGE_KEY,
          JSON.stringify(lastSeenAssistantTurnIndex),
        );
      }
    } catch {
      // Ignore storage errors.
    }
  }, [lastSeenAssistantTurnIndex]);

  /** Cache a transcript under its owning agent for roster badge counts. */
  const recordTranscript = useCallback(
    (agentKey: string | undefined | null, turns: TranscriptTurn[]) => {
      const key = (agentKey ?? "").trim().toLowerCase();
      if (!key) {
        return;
      }
      backgroundTranscriptsRef.current = {
        ...backgroundTranscriptsRef.current,
        [key]: turns,
      };
      setBackgroundTranscripts((prev) =>
        prev[key] === turns ? prev : { ...prev, [key]: turns },
      );
    },
    [],
  );

  /** Apply a disk transcript without wiping just-sent optimistic turns. */
  const applyTranscriptTurns = useCallback((turns: TranscriptTurn[]) => {
    const nowMs = Date.now();
    const activeKey = selectedAgentNameRef.current;
    const merged = preserveOptimisticTurn(
      turns,
      optimisticPromptRef.current,
      nowMs,
      activeKey,
      activeSessionIdRef.current,
    );
    optimisticPromptRef.current = pruneOptimisticPrompts(
      optimisticPromptRef.current,
      turns,
      nowMs,
      activeKey,
      activeSessionIdRef.current,
    );
    setTranscriptTurns(merged);
  }, []);

  const [worktrees, setWorktrees] = useState<WorktreeSummary[]>([]);
  const [selectedWorktreeId, setSelectedWorktreeId] = useState<string | null>(null);
  const [worktreeDiff, setWorktreeDiff] = useState<WorktreeDiff | null>(null);
  const [prs, setPrs] = useState<PullRequestItem[]>([]);

  const [wireMessages, setWireMessages] = useState<WireInboxItem[]>([]);
  const [workflows, setWorkflows] = useState<WorkflowDef[]>([]);
  const [workflowRuns, setWorkflowRuns] = useState<WorkflowRun[]>([]);
  const [architecture, setArchitecture] = useState<ArchitectureResponse | null>(null);
  const [stats, setStats] = useState<HubStatsResponse | null>(null);

  const [loading, setLoading] = useState<boolean>(false);

  // ChatPane always stays on the primary canvas; system consoles open in the Left Panel;
  // agent/project secondary tabs ("mail" | "artifacts" | "git" | "board") open in the Right Panel.
  const handleSelectTab = (tab: CockpitTab) => {
    if (deviceLayoutRef.current === "phone") {
      navigatePhone(tab);
      void refreshSessionsAndTranscriptRef.current();
      return;
    }
    if (tab === "terminal" || tab === "hub" || tab === "gate" || tab === "state") {
      setLeftPanelTab(tab);
      setDesktopSidebarCollapsed(false);
      setMobileRosterOpen(true);
      if (
        typeof window !== "undefined" &&
        window.innerWidth < THREE_PANEL_MIN_VIEWPORT_PX
      ) {
        setSplitView(false);
      }
      setActiveTab(tab);
      void refreshSessionsAndTranscriptRef.current();
      return;
    }
    if (tab === "chat") {
      setSplitView(false);
      setActiveTab("chat");
      void refreshSessionsAndTranscriptRef.current();
      return;
    }
    if (splitView && secondaryTab === tab) {
      setSplitView(false);
      setActiveTab(
        leftPanelTab === "roster"
          ? "chat"
          : leftPanelTab === "watch"
            ? "state"
            : leftPanelTab === "triage"
              ? "hub"
              : leftPanelTab,
      );
    } else {
      setSecondaryTab(tab);
      setActiveTab(tab);
      setSplitView(true);
      if (
        typeof window !== "undefined" &&
        window.innerWidth < THREE_PANEL_MIN_VIEWPORT_PX
      ) {
        setDesktopSidebarCollapsed(true);
      }
      setMobileRosterOpen(false);
    }
    void refreshSessionsAndTranscriptRef.current();
  };

  const handleSelectLeftPanelTab = useCallback(
    (tab: LeftPanelTab) => {
      if (deviceLayoutRef.current === "phone") {
        navigatePhone(tab);
        return;
      }
      setLeftPanelTab(tab);
      if (tab === "roster") {
        setActiveTab((prev) =>
          prev === "terminal" || prev === "hub" || prev === "gate" || prev === "state"
            ? splitView
              ? secondaryTab
              : "chat"
            : prev,
        );
      } else if (tab === "watch") {
        setActiveTab("state");
      } else if (tab === "triage") {
        setActiveTab("hub");
      } else {
        setActiveTab(tab);
      }
    },
    [splitView, secondaryTab, navigatePhone],
  );

  const refreshWireAndRoster = useCallback(async () => {
    try {
      const [agentsRes, livenessRes, wireRes] = await Promise.all([
        apiClient.getAgents().catch(() => null),
        apiClient.getAgentsLiveness().catch(() => null),
        apiClient.getWireMessages().catch(() => null),
      ]);
      if (agentsRes) {
        if (agentsRes.agents && agentsRes.agents.length > 0) {
          setAgents(agentsRes.agents);
        }
        if (agentsRes.routes) {
          setRoutes(agentsRes.routes);
        }
      }
      if (livenessRes?.liveness) {
        setLiveness(livenessRes.liveness);
      }
      if (wireRes?.messages) {
        setWireMessages(wireRes.messages);
      }
    } catch {
      // Ignore background refresh errors
    }
  }, [apiClient]);

  // Refresh background agents' transcripts only when their session stamp
  // moved, so roster badges track new assistant output without refetching
  // every window on every poll.
  const scanBackgroundTranscripts = useCallback(
    async (list: DiscoveredSessionSummary[]) => {
      // Read through the ref so this callback stays referentially stable;
      // see the WebSocket effect, which depends on it.
      const selectedKey = selectedAgentNameRef.current.trim().toLowerCase();
      const scannedPersonas = new Set<string>();
      for (const session of list) {
        const persona = (session.persona ?? "").trim().toLowerCase();
        if (!persona || persona === selectedKey || scannedPersonas.has(persona)) {
          continue;
        }
        scannedPersonas.add(persona);
        const stamp = session.updated_at ?? "";
        if (backgroundStampRef.current[persona] === stamp) {
          continue;
        }
        const bgRes = await apiClient
          .getSessionTranscript(session.id)
          .catch(() => null);
        if (loggedOutRef.current) {
          return;
        }
        if (bgRes) {
          if (bgRes.turns) {
            backgroundStampRef.current[persona] = stamp;
            recordTranscript(persona, bgRes.turns);
          }
          if (bgRes.context_usage !== undefined) {
            recordAgentContextUsage(persona, bgRes.context_usage);
          }
          const busy =
            typeof bgRes.is_busy === "boolean"
              ? bgRes.is_busy
              : hasTrailingIncompleteWork(bgRes.turns ?? []);
          recordAgentBusy(persona, busy);
        }
      }
    },
    [apiClient, recordTranscript, recordAgentBusy, recordAgentContextUsage],
  );

  // A refresh requested while one is in flight is coalesced into a single
  // trailing run instead of being dropped (a dropped post-send or post-switch
  // refresh leaves the transcript stale until the next poll tick).
  const refreshQueuedRef = useRef<{ override: string | null | undefined } | null>(null);
  const refreshSessionsAndTranscriptRef = useRef<
    (sessionOverride?: string | null) => Promise<void>
  >(async () => {});

  const refreshSessionsAndTranscript = useCallback(
    async (sessionOverride?: string | null) => {
      if (loggedOutRef.current) {
        return;
      }
      if (refreshInFlightRef.current) {
        refreshQueuedRef.current = { override: sessionOverride };
        return;
      }
      refreshInFlightRef.current = true;
      try {
        const requestedAgentName = selectedAgentNameRef.current;
        const requestedAgentKey = requestedAgentName.trim().toLowerCase();
        const requestedSessionId = selectedSessionIdRef.current;
        const requestedActiveSessionId = activeSessionIdRef.current;
        const sessRes = await apiClient.getSessions().catch(() => null);
        if (loggedOutRef.current) {
          return;
        }
        const rawList = sessRes?.sessions ?? [];
        const list = rawList.filter(
          (s) =>
            !tornDownTargetsRef.current.has(s.id.toLowerCase()) &&
            !tornDownTargetsRef.current.has((s.persona ?? "").toLowerCase()),
        );
        if (sessRes) {
          setSessions(list);
        }
        void scanBackgroundTranscripts(list);
        const rawTargetId =
          sessionOverride === null
            ? null
            : (sessionOverride ?? requestedSessionId ?? requestedActiveSessionId ??
              list.find((session) => (session.persona ?? "").toLowerCase() === requestedAgentKey)?.id ??
              requestedAgentName);
        const targetId =
          rawTargetId &&
          !tornDownTargetsRef.current.has(rawTargetId.trim().toLowerCase()) &&
          !tornDownTargetsRef.current.has(requestedAgentKey)
            ? rawTargetId
            : null;
        if (targetId) {
          const trRes = await apiClient.getSessionTranscript(targetId).catch(() => null);
          if (loggedOutRef.current) {
            return;
          }
          if (trRes) {
            // A persona can have multiple sessions. Never publish or cache a
            // response after the concrete session changed while awaiting it.
            const activeNow = activeSessionIdRef.current;
            const stillSameTarget =
              selectedAgentNameRef.current.trim().toLowerCase() === requestedAgentKey &&
              selectedSessionIdRef.current === requestedSessionId &&
              (requestedActiveSessionId
                ? activeNow === requestedActiveSessionId && targetId === requestedActiveSessionId
                : activeNow === null || activeNow === targetId);
            if (!stillSameTarget) return;
            const targetPersona =
              list.find((session) => session.id === targetId)?.persona ??
              (list.some(
                (session) =>
                  (session.persona ?? "").toLowerCase() ===
                  targetId.toLowerCase(),
              )
                ? targetId
                : requestedAgentKey);
            if (trRes.turns) {
              recordTranscript(targetPersona, trRes.turns);
            }
            if (trRes.context_usage !== undefined) {
              recordAgentContextUsage(targetPersona, trRes.context_usage);
            }
            recordAgentBusy(
              targetPersona,
              typeof trRes.is_busy === "boolean"
                ? trRes.is_busy
                : hasTrailingIncompleteWork(trRes.turns ?? []),
            );
            if (targetPersona.trim().toLowerCase() === selectedAgentNameRef.current.trim().toLowerCase()) {
              if (trRes.turns) {
                applyTranscriptTurns(trRes.turns);
              }
              setUnconfirmedBySession((prev) => ({ ...prev, [targetId]: Boolean(trRes.in_flight_unconfirmed) }));
              setSessionContextUsage(trRes.context_usage ?? null);
              setSessionQuotaUsage(trRes.quota_usage ?? null);
              if (typeof trRes.is_busy === "boolean") {
                setIsSessionBusy(trRes.is_busy);
              }
              if (Array.isArray(trRes.queue)) {
                setSessionQueue(trRes.queue);
              }
            }
          }
        }
      } catch {
        // Ignore
      } finally {
        refreshInFlightRef.current = false;
        const queued = refreshQueuedRef.current;
        if (queued) {
          refreshQueuedRef.current = null;
          void refreshSessionsAndTranscriptRef.current(queued.override);
        }
      }
    },
    // Selection is read through refs so the identity is stable across agent
    // and session switches (the WebSocket effect depends on this callback).
    [
      apiClient,
      applyTranscriptTurns,
      scanBackgroundTranscripts,
      recordTranscript,
      recordAgentBusy,
      recordAgentContextUsage,
    ],
  );
  refreshSessionsAndTranscriptRef.current = refreshSessionsAndTranscript;

  const refreshWorkflows = useCallback(async () => {
    try {
      const [wfRes, runsRes, statsRes] = await Promise.all([
        apiClient.getWorkflows().catch(() => null),
        apiClient.getWorkflowRuns().catch(() => null),
        apiClient.getStats().catch(() => null),
      ]);
      if (wfRes?.workflows) {
        setWorkflows(wfRes.workflows);
      }
      if (runsRes?.runs) {
        setWorkflowRuns(runsRes.runs);
      }
      if (statsRes) {
        setStats(statsRes);
      }
    } catch {
      // Ignore
    }
  }, [apiClient]);

  const refreshNodes = useCallback(async () => {
    try {
      const list = await apiClient.fetchNodes();
      setNodes(list);
    } catch {
      // Ignore node refresh errors
    }
  }, [apiClient]);

  const loadAllInitialState = useCallback(async () => {
    loggedOutRef.current = false;
    setLoading(true);
    try {
      const [
        agentsRes,
        livenessRes,
        sessionsRes,
        worktreesRes,
        prsRes,
        wireRes,
        workflowsRes,
        runsRes,
        archRes,
        statsRes,
        nodesRes,
        settingsRes,
      ] = await Promise.all([
        apiClient.getAgents().catch(() => null),
        apiClient.getAgentsLiveness().catch(() => null),
        apiClient.getSessions().catch(() => null),
        apiClient.getWorktrees().catch(() => null),
        apiClient.getPrs().catch(() => null),
        apiClient.getWireMessages().catch(() => null),
        apiClient.getWorkflows().catch(() => null),
        apiClient.getWorkflowRuns().catch(() => null),
        apiClient.getArchitecture().catch(() => null),
        apiClient.getStats().catch(() => null),
        apiClient.fetchNodes().catch(() => null),
        apiClient.getSettings().catch(() => null),
      ]);

      if (settingsRes?.timezone) {
        setActiveTimeZone(settingsRes.timezone);
      }

      if (nodesRes) {
        setNodes(nodesRes);
      }

      if (agentsRes) {
        if (agentsRes.agents && agentsRes.agents.length > 0) {
          setAgents(agentsRes.agents);
        }
        if (agentsRes.routes) {
          setRoutes(agentsRes.routes);
        }
      }
      if (livenessRes?.liveness) {
        setLiveness(livenessRes.liveness);
      }
      if (sessionsRes?.sessions) {
        setSessions(sessionsRes.sessions);
        void scanBackgroundTranscripts(sessionsRes.sessions);
      }
      if (worktreesRes?.worktrees) {
        setWorktrees(worktreesRes.worktrees);
        const firstWt = worktreesRes.worktrees[0]?.id ?? null;
        setSelectedWorktreeId((prev) => prev ?? firstWt);
      }
      if (prsRes?.prs) {
        setPrs(prsRes.prs);
      }
      if (wireRes?.messages) {
        setWireMessages(wireRes.messages);
      }
      if (workflowsRes?.workflows) {
        setWorkflows(workflowsRes.workflows);
      }
      if (runsRes?.runs) {
        setWorkflowRuns(runsRes.runs);
      }
      if (archRes) {
        setArchitecture(archRes);
      }
      if (statsRes) {
        setStats(statsRes);
      }

      const initialAgentName = selectedAgentNameRef.current;
      const initialKey = initialAgentName.trim().toLowerCase();
      const initialSessionId = selectedSessionIdRef.current;
      const matchedSessionId =
        initialSessionId ??
        sessionsRes?.sessions?.find((s) => (s.persona ?? "").toLowerCase() === initialKey)?.id ??
        agentsRes?.routes?.find((r) => (r.persona ?? "").toLowerCase() === initialKey)?.id ??
        initialAgentName;
      if (
        matchedSessionId &&
        !tornDownTargetsRef.current.has(matchedSessionId.trim().toLowerCase()) &&
        !tornDownTargetsRef.current.has(initialKey)
      ) {
        const trRes = await apiClient.getSessionTranscript(matchedSessionId).catch(() => null);
        if (
          trRes &&
          !loggedOutRef.current &&
          selectedAgentNameRef.current.trim().toLowerCase() === initialKey &&
          selectedSessionIdRef.current === initialSessionId &&
          (activeSessionIdRef.current === null || activeSessionIdRef.current === matchedSessionId) &&
          !tornDownTargetsRef.current.has(matchedSessionId.trim().toLowerCase()) &&
          !tornDownTargetsRef.current.has(initialKey)
        ) {
          recordTranscript(initialAgentName, trRes.turns ?? []);
          if (trRes.context_usage !== undefined) {
            recordAgentContextUsage(initialAgentName, trRes.context_usage);
          }
          recordAgentBusy(
            initialAgentName,
            typeof trRes.is_busy === "boolean"
              ? trRes.is_busy
              : hasTrailingIncompleteWork(trRes.turns ?? []),
          );
          applyTranscriptTurns(trRes.turns ?? []);
          setSessionQueue(trRes.queue ?? []);
          setUnconfirmedBySession((prev) => ({ ...prev, [matchedSessionId]: Boolean(trRes.in_flight_unconfirmed) }));
          setIsSessionBusy(Boolean(trRes.is_busy));
          setSessionContextUsage(trRes.context_usage ?? null);
          setSessionQuotaUsage(trRes.quota_usage ?? null);
        }
      }
    } finally {
      setLoading(false);
    }
  }, [
    apiClient,
    applyTranscriptTurns,
    scanBackgroundTranscripts,
    recordTranscript,
    recordAgentBusy,
    recordAgentContextUsage,
  ]);

  // Startup URL pairing check + Auth session check + initial load
  useEffect(() => {
    let cancelled = false;

    const bootstrap = async () => {
      if (typeof window !== "undefined" && window.location) {
        const params = new URLSearchParams(window.location.search);
        const pairCode = params.get("pair") || params.get("code");
        if (pairCode && pairCode.trim()) {
          // Strip the single-use credential from the address bar before any
          // await: a second bootstrap run (React StrictMode remount) would
          // otherwise re-submit the same code and surface a spurious
          // "rejected" error, and the code would linger in history meanwhile.
          params.delete("pair");
          params.delete("code");
          const nextQuery = params.toString();
          window.history.replaceState(
            {},
            "",
            `${window.location.pathname}${nextQuery ? `?${nextQuery}` : ""}${
              window.location.hash || ""
            }`,
          );
          pairInFlightRef.current = apiClient.pair({ code: pairCode.trim() });
        }
      }

      // The in-flight pairing lives in a ref so a remounted bootstrap run
      // (which no longer sees the code in the URL) still waits for it rather
      // than racing getAuthSession ahead of the session cookie.
      const pendingPair = pairInFlightRef.current;
      if (pendingPair) {
        try {
          const pairRes = await pendingPair;
          if (!cancelled) {
            if (pairRes.authority) {
              setAuthority(pairRes.authority);
            }
            wsClient.setToken(apiClient.getToken());
            wsClient.connect();
          }
        } catch (err) {
          if (!cancelled) {
            setPairingError(err instanceof Error ? err.message : "Pairing code rejected");
            setPairingOpen(true);
          }
        } finally {
          if (pairInFlightRef.current === pendingPair) {
            pairInFlightRef.current = null;
          }
        }
      }

      try {
        const authRes = await apiClient.getAuthSession();
        if (!cancelled) {
          if (authRes.authenticated && authRes.authority) {
            setAuthority(authRes.authority);
          } else {
            setPairingOpen(true);
          }
        }
      } catch (err) {
        if (!cancelled) {
          if (err instanceof HubApiError && err.status === 401) {
            setPairingOpen(true);
          }
        }
      }

      if (!cancelled) {
        await loadAllInitialState();
      }
    };

    void bootstrap();

    return () => {
      cancelled = true;
    };
  }, [apiClient, wsClient, loadAllInitialState]);

  // Connect WebSocket and subscribe to wire, session, and workflow channels
  useEffect(() => {
    wsClient.setToken(apiClient.getToken());
    const unsubStatus = wsClient.onStatusChange((connected) => {
      setWsConnected(connected);
    });
    const unsubWire = wsClient.subscribe("wire", () => {
      void refreshWireAndRoster();
    });
    const unsubSession = wsClient.subscribe("session", (frame) => {
      if (
        frame?.event === "prompt_sent" &&
        frame.data &&
        typeof frame.data === "object"
      ) {
        const data = frame.data as Record<string, unknown>;
        const rawContent = (
          typeof data.effective_body === "string" && data.effective_body.trim()
            ? data.effective_body
            : typeof data.prompt === "string"
              ? data.prompt
              : ""
        ).trim();
        const rawPersona =
          typeof data.persona === "string"
            ? data.persona.trim().toLowerCase()
            : "";
        const rawSessionId =
          typeof data.session_id === "string" ? data.session_id.trim() : "";
        const activeKey = selectedAgentNameRef.current.trim().toLowerCase();
        const activeSessionId = activeSessionIdRef.current;
        // A persona is reusable; a session-scoped event must match the active session exactly.
        // Legacy persona-only events cannot safely be assigned when a session is active.
        const matchesActive = rawSessionId
          ? rawSessionId === activeSessionId && (!rawPersona || rawPersona === activeKey)
          : !activeSessionId && Boolean(rawPersona) && rawPersona === activeKey;
        if (matchesActive && rawContent && data.delivered !== false) {
          recordOptimisticPrompt(rawContent, activeKey, Date.now(), rawSessionId || null);
          setTranscriptTurns((prev) =>
            preserveOptimisticTurn(prev, optimisticPromptRef.current, Date.now(), activeKey, activeSessionId),
          );
        }
        if (matchesActive) {
          if (Array.isArray(data.queue)) setSessionQueue(data.queue as QueuedOperatorPrompt[]);
          if (typeof data.is_busy === "boolean") setIsSessionBusy(data.is_busy);
        }
      }
      void refreshSessionsAndTranscript();
    });
    const unsubWorkflow = wsClient.subscribe("workflow", () => {
      void refreshWorkflows();
    });

    try {
      wsClient.connect();
    } catch {
      // Ignore WebSocket connection errors in offline/test environments
    }

    return () => {
      unsubStatus();
      unsubWire();
      unsubSession();
      unsubWorkflow();
      wsClient.disconnect();
    };
  }, [wsClient, apiClient, refreshWireAndRoster, refreshSessionsAndTranscript, refreshWorkflows]);

  // Load worktree diff when selectedWorktreeId changes
  useEffect(() => {
    if (!selectedWorktreeId) {
      setWorktreeDiff(null);
      return;
    }
    let cancelled = false;
    apiClient
      .getWorktreeDiff(selectedWorktreeId)
      .then((diff) => {
        if (!cancelled) {
          setWorktreeDiff(diff);
        }
      })
      .catch(() => {
        // Ignore
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, selectedWorktreeId]);

  // Compute active agent and session
  const selectedAgent = useMemo(() => {
    const key = selectedAgentName.trim().toLowerCase();
    if (!key) {
      return null;
    }
    return (
      agents.find((a) => a.name_key.toLowerCase() === key) ??
      DEFAULT_WORKSPACE_AGENTS.find((a) => a.name_key.toLowerCase() === key) ??
      agents[0] ??
      null
    );
  }, [agents, selectedAgentName]);

  const [spunUpByAgent, setSpunUpByAgent] = useState<
    Record<
      string,
      { adapter: string; model: string | null; sessionId: string | null }
    >
  >({});

  const activeSession = useMemo(() => {
    const key = (selectedAgent?.name_key ?? "").toLowerCase();
    if (selectedSessionId) {
      const explicit = sessions.find((s) => s.id === selectedSessionId);
      if (
        explicit &&
        (!key || !explicit.persona || explicit.persona.toLowerCase() === key)
      ) {
        return explicit;
      }
    }
    if (!key) {
      return null;
    }
    return (
      sessions.find((s) => (s.persona ?? "").toLowerCase() === key) ??
      routes.find((r) => (r.persona ?? "").toLowerCase() === key) ??
      null
    );
  }, [sessions, routes, selectedSessionId, selectedAgent]);

  activeSessionIdRef.current = activeSession?.id ?? null;

  const activeAgentKey = (selectedAgent?.name_key ?? "").trim().toLowerCase();
  const activeRoute = useMemo(
    () =>
      activeAgentKey
        ? (routes.find((r) => (r.persona ?? "").toLowerCase() === activeAgentKey) ??
          null)
        : null,
    [routes, activeAgentKey],
  );
  const activeLiveness = useMemo(
    () =>
      activeAgentKey
        ? (liveness.find((l) => l.agent.toLowerCase() === activeAgentKey) ??
          null)
        : null,
    [liveness, activeAgentKey],
  );
  const activeSpunUp = activeAgentKey ? spunUpByAgent[activeAgentKey] : undefined;
  const activeAdapter = useMemo(() => {
    const candidates = [
      activeSession?.adapter,
      activeRoute?.adapter,
      activeLiveness?.adapter,
      activeSpunUp?.adapter,
    ];
    for (const candidate of candidates) {
      const trimmed = (candidate ?? "").trim();
      if (trimmed && trimmed.toLowerCase() !== "unknown") {
        return trimmed;
      }
    }
    return (
      activeSession?.adapter ??
      activeRoute?.adapter ??
      activeSpunUp?.adapter ??
      null
    );
  }, [activeSession, activeRoute, activeLiveness, activeSpunUp]);
  const activeModel = useMemo(() => {
    const candidates = [
      activeSession?.model,
      activeRoute?.model,
      activeSpunUp?.model,
    ];
    for (const candidate of candidates) {
      const trimmed = (candidate ?? "").trim();
      if (trimmed) {
        return trimmed;
      }
    }
    return null;
  }, [activeSession, activeRoute, activeSpunUp]);

  const prevLoadedAgentKeyRef = useRef<string>("");

  // Load transcript when activeSession or selectedAgentName changes
  useEffect(() => {
    const nextAgentKey = (selectedAgent?.name_key ?? "").trim().toLowerCase();
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (
      loggedOutRef.current ||
      !targetId ||
      tornDownTargetsRef.current.has(targetId.trim().toLowerCase()) ||
      (nextAgentKey && tornDownTargetsRef.current.has(nextAgentKey))
    ) {
      prevLoadedAgentKeyRef.current = nextAgentKey;
      setTranscriptTurns([]);
      setSessionQueue([]);
      setIsSessionBusy(false);
      setSessionContextUsage(null);
      setSessionQuotaUsage(null);
      return;
    }
    let cancelled = false;
    if (prevLoadedAgentKeyRef.current !== nextAgentKey) {
      prevLoadedAgentKeyRef.current = nextAgentKey;
      applyTranscriptTurns(
        nextAgentKey ? (backgroundTranscriptsRef.current[nextAgentKey] ?? []) : [],
      );
      setSessionQueue([]);
      setIsSessionBusy(false);
      setSessionContextUsage(null);
      setSessionQuotaUsage(null);
    } else if (selectedSessionIdRef.current !== null) {
      setTranscriptTurns([]);
      setSessionQueue([]);
      setIsSessionBusy(false);
      setSessionContextUsage(null);
      setSessionQuotaUsage(null);
    }
    apiClient
      .getSessionTranscript(targetId)
      .then((res) => {
        if (!cancelled && !loggedOutRef.current) {
          applyTranscriptTurns(res.turns ?? []);
          recordTranscript(selectedAgent?.name_key, res.turns ?? []);
          recordAgentBusy(
            selectedAgent?.name_key,
            typeof res.is_busy === "boolean"
              ? res.is_busy
              : hasTrailingIncompleteWork(res.turns ?? []),
          );
          setSessionQueue(res.queue ?? []);
          setUnconfirmedBySession((prev) => ({ ...prev, [targetId]: Boolean(res.in_flight_unconfirmed) }));
          setIsSessionBusy(Boolean(res.is_busy));
          setSessionContextUsage(res.context_usage ?? null);
          if (res.context_usage !== undefined) {
            recordAgentContextUsage(selectedAgent?.name_key, res.context_usage);
          }
          setSessionQuotaUsage(res.quota_usage ?? null);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setTranscriptTurns([]);
          setSessionQueue([]);
          setIsSessionBusy(false);
          setSessionContextUsage(null);
          setSessionQuotaUsage(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, activeSession?.id, selectedAgent?.name_key, authority?.tier, applyTranscriptTurns, recordTranscript, recordAgentContextUsage]);

  // The hub emits no WebSocket event for raw transcript.jsonl appends, so
  // poll the active transcript while the chat is on screen.
  useEffect(() => {
    if (!authority || (activeTab !== "chat" && !splitView)) {
      return;
    }
    const intervalId = setInterval(() => {
      if (typeof document !== "undefined" && document.hidden) {
        return;
      }
      void refreshSessionsAndTranscript();
    }, transcriptPollIntervalMs(isSessionBusy));
    return () => clearInterval(intervalId);
  }, [authority, activeTab, splitView, isSessionBusy, refreshSessionsAndTranscript]);

  // Re-fetch /api/v1/sessions and active transcript when returning focus or visibility
  // to FerroDeck (e.g. after changing model in a vendor CLI or the Claude app).
  useEffect(() => {
    if (typeof window === "undefined") {
      return;
    }
    const handleWindowFocus = () => {
      void refreshSessionsAndTranscript();
    };
    const handleVisibilityChange = () => {
      if (typeof document !== "undefined" && !document.hidden) {
        void refreshSessionsAndTranscript();
      }
    };
    window.addEventListener("focus", handleWindowFocus);
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", handleVisibilityChange);
    }
    return () => {
      window.removeEventListener("focus", handleWindowFocus);
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", handleVisibilityChange);
      }
    };
  }, [refreshSessionsAndTranscript]);

  // Transcripts keyed by owning agent (background scans + recorded active
  // loads). Keying by owner avoids crediting one agent with another's turns
  // during the transient after a roster switch.
  const transcriptsByAgent = backgroundTranscripts;

  // Establish first-observation baselines, and mark the viewed agent read.
  useEffect(() => {
    const selectedKey = (
      selectedAgent?.name_key ?? selectedAgentName
    )
      .trim()
      .toLowerCase();
    // Phone navigation never writes desktop state, so the phone view alone says what is on screen.
    // (Wire Mail without an agent falls back to Artifacts, never Chat, so phoneView is exact.)
    const viewingChat =
      deviceLayout === "phone" ? phoneView === "chat" : activeTab === "chat" || splitView;
    setLastSeenAssistantTurnIndex((prev) => {
      let next: Record<string, number> | null = null;
      for (const [agentKey, turns] of Object.entries(transcriptsByAgent)) {
        const maxIndex = maxUserDirectedTurnIndex(turns);
        if (maxIndex < 0) {
          continue;
        }
        const isViewed = viewingChat && agentKey === selectedKey;
        if (prev[agentKey] === undefined || (isViewed && maxIndex > prev[agentKey])) {
          if (!next) {
            next = { ...prev };
          }
          next[agentKey] = maxIndex;
        }
      }
      return next ?? prev;
    });
  }, [transcriptsByAgent, selectedAgent, selectedAgentName, activeTab, splitView, deviceLayout, phoneView]);

  // Compute unread user-directed assistant counts per agent
  const unreadByAgent = useMemo<Record<string, number>>(
    () => computeUnreadByAgent(transcriptsByAgent, lastSeenAssistantTurnIndex),
    [transcriptsByAgent, lastSeenAssistantTurnIndex],
  );

  const unreadSeverityByAgent = useMemo<Record<string, UnreadSeverity>>(
    () =>
      computeUnreadSeverityByAgent(
        transcriptsByAgent,
        lastSeenAssistantTurnIndex,
        wireMessages,
      ),
    [transcriptsByAgent, lastSeenAssistantTurnIndex, wireMessages],
  );

  const handlePairWithCode = async (code: string) => {
    setPairingError(null);
    const res = await apiClient.pair({ code });
    if (res.authority) {
      setAuthority(res.authority);
    }
    wsClient.setToken(apiClient.getToken());
    wsClient.connect();
    setPairingOpen(false);
    await loadAllInitialState();
  };

  const handlePairWithToken = async (token: string) => {
    setPairingError(null);
    const res = await apiClient.pair({ token });
    if (res.authority) {
      setAuthority(res.authority);
    }
    wsClient.setToken(apiClient.getToken());
    wsClient.connect();
    setPairingOpen(false);
    await loadAllInitialState();
  };

  const handleLogout = async () => {
    loggedOutRef.current = true;
    await apiClient.logout().catch(() => {});
    setAuthority(null);
    wsClient.setToken(null);
    wsClient.disconnect();
    // Drop everything fetched under the old identity so the next principal
    // to pair on this browser never sees (or polls against) it.
    optimisticPromptRef.current = [];
    backgroundTranscriptsRef.current = {};
    backgroundStampRef.current = {};
    setBackgroundTranscripts({});
    setBusyByAgent({});
    setContextUsageByAgent({});
    setTranscriptTurns([]);
    setSessionQueue([]);
    setIsSessionBusy(false);
    setUnconfirmedBySession({});
    setSessionContextUsage(null);
    setSessionQuotaUsage(null);
    setSessions([]);
    setRoutes([]);
    setLiveness([]);
    setWireMessages([]);
    setWorktrees([]);
    setWorktreeDiff(null);
    setPrs([]);
    setWorkflows([]);
    setWorkflowRuns([]);
    setArchitecture(null);
    setStats(null);
    setNodes([]);
    setAgentSettingsOpen(false);
    setPairingOpen(true);
  };

  const handleSendPrompt = async (targetId: string, payload: PromptRequest) => {
    const submittedAgent = selectedAgentNameRef.current;
    const submittedSession = selectedSessionIdRef.current;
    const submittedActiveSession = activeSessionIdRef.current;
    const res = await apiClient.sendSessionPrompt(targetId, payload);
    if (
      (!res.queued && res.status !== "queued" && res.delivered === false) ||
      selectedAgentNameRef.current !== submittedAgent ||
      selectedSessionIdRef.current !== submittedSession ||
      activeSessionIdRef.current !== submittedActiveSession ||
      (submittedActiveSession !== null && submittedActiveSession !== targetId)
    ) {
      return res;
    }
    if (typeof res.in_flight_unconfirmed === "boolean") {
      const unconfirmed = res.in_flight_unconfirmed;
      setUnconfirmedBySession((prev) => ({ ...prev, [targetId]: unconfirmed }));
    }
    if (Array.isArray(res.queue)) {
      setSessionQueue(res.queue);
    }
    if (typeof res.is_busy === "boolean") {
      setIsSessionBusy(res.is_busy);
    }
    if (!res.queued && res.status !== "queued") {
      const content = res.effective_body || payload.prompt;
      const agentKey = (selectedAgent?.name_key ?? selectedAgentName)
        .trim()
        .toLowerCase();
      recordOptimisticPrompt(content, agentKey, Date.now(), submittedActiveSession);
      setTranscriptTurns((prev) =>
        preserveOptimisticTurn(
          prev,
          optimisticPromptRef.current,
          Date.now(),
          selectedAgentNameRef.current,
          submittedActiveSession,
        ),
      );
    }
    void refreshSessionsAndTranscript(targetId);
    return res;
  };

  const handleSendQueuedNow = async (promptId: string) => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) return;
    const submittedSession = activeSessionIdRef.current;
    const submittedAgent = selectedAgentNameRef.current;
    const res = await apiClient.sendQueuedPromptNow(targetId, promptId);
    if (activeSessionIdRef.current !== submittedSession || selectedAgentNameRef.current !== submittedAgent) {
      if (res.delivered === false) throw new Error(res.delivery_error || "Queued message delivery could not be confirmed");
      return;
    }
    if (Array.isArray(res.queue)) {
      setSessionQueue(res.queue);
    }
    if (typeof res.is_busy === "boolean") {
      setIsSessionBusy(res.is_busy);
    }
    if (res.delivered === false) {
      throw new Error(res.delivery_error || "Queued message delivery could not be confirmed");
    }
    const content = res.effective_body || res.prompt;
    if (content) {
      const agentKey = (selectedAgent?.name_key ?? selectedAgentName)
        .trim()
        .toLowerCase();
      recordOptimisticPrompt(content, agentKey, Date.now(), activeSessionIdRef.current);
      setTranscriptTurns((prev) =>
        preserveOptimisticTurn(
          prev,
          optimisticPromptRef.current,
          Date.now(),
          selectedAgentNameRef.current,
          activeSessionIdRef.current,
        ),
      );
    }
    void refreshSessionsAndTranscript(targetId);
  };

  const handleCancelQueued = async (promptId: string) => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) return;
    const res = await apiClient.cancelQueuedPrompt(targetId, promptId);
    if (Array.isArray(res.queue)) {
      setSessionQueue(res.queue);
    }
    if (typeof res.is_busy === "boolean") {
      setIsSessionBusy(res.is_busy);
    }
  };

  const handleUpdateQueued = async (promptId: string, prompt: string) => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) return;
    const res = await apiClient.updateQueuedPrompt(targetId, promptId, prompt);
    if (Array.isArray(res.queue)) {
      setSessionQueue(res.queue);
    }
    if (typeof res.is_busy === "boolean") {
      setIsSessionBusy(res.is_busy);
    }
  };

  const handleReviewPr = async (number: number, payload: PrReviewRequest) => {
    await apiClient.reviewPullRequest(number, payload);
    const prsRes = await apiClient.getPrs().catch(() => null);
    if (prsRes?.prs) {
      setPrs(prsRes.prs);
    }
  };

  const handleMergePr = async (number: number, payload: PrMergeRequest) => {
    await apiClient.mergePullRequest(number, payload);
    const prsRes = await apiClient.getPrs().catch(() => null);
    if (prsRes?.prs) {
      setPrs(prsRes.prs);
    }
  };

  const handleAckWireMessage = async (messageId: string, byAgent?: string) => {
    await apiClient.ackWireMessage({ id: messageId, by: byAgent });
    await refreshWireAndRoster();
  };

  const handleSendWireMessage = async (payload: WireSendRequest) => {
    await apiClient.sendWireMessage(payload);
    await refreshWireAndRoster();
  };

  const handleSpinUpSession = async (req: SpinUpSessionRequest) => {
    const res = await apiClient.spinUpSession(req);
    const personaKey = (res.persona || req.persona || "").trim().toLowerCase();
    if (personaKey) {
      setSpunUpByAgent((prev) => ({
        ...prev,
        [personaKey]: {
          adapter: res.vendor || req.vendor,
          model: res.model ?? req.model ?? null,
          sessionId: res.session_id || null,
        },
      }));
    }
    if (req.persona) {
      tornDownTargetsRef.current.delete(req.persona.trim().toLowerCase());
    }
    if (res.persona) {
      tornDownTargetsRef.current.delete(res.persona.trim().toLowerCase());
      selectedAgentNameRef.current = res.persona;
      setSelectedAgentName(res.persona);
    }
    if (res.session_id) {
      tornDownTargetsRef.current.delete(res.session_id.trim().toLowerCase());
      selectedSessionIdRef.current = res.session_id;
      setSelectedSessionId(res.session_id);
    }
    await Promise.all([
      refreshWireAndRoster(),
      refreshSessionsAndTranscript(res.session_id),
    ]);
    return res;
  };

  const handleDecommissionSession = async (mode: DecommissionMode) => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) {
      throw new Error("No active session selected to decommission.");
    }
    if (mode === "teardown") {
      tornDownTargetsRef.current.add(targetId.trim().toLowerCase());
      if (activeSession?.id) {
        tornDownTargetsRef.current.add(activeSession.id.trim().toLowerCase());
      }
      if (selectedAgent?.name_key) {
        tornDownTargetsRef.current.add(
          selectedAgent.name_key.trim().toLowerCase(),
        );
      }
    }
    const res = await apiClient.decommissionSession(targetId, mode);
    if (mode === "teardown") {
      const agentKey = (selectedAgent?.name_key ?? "").trim().toLowerCase();
      if (agentKey) {
        delete backgroundTranscriptsRef.current[agentKey];
        setBackgroundTranscripts((prev) => {
          const next = { ...prev };
          delete next[agentKey];
          return next;
        });
        setBusyByAgent((prev) => {
          const next = { ...prev };
          delete next[agentKey];
          return next;
        });
        setContextUsageByAgent((prev) => {
          const next = { ...prev };
          delete next[agentKey];
          return next;
        });
      }
      setSelectedSessionId(null);
      selectedSessionIdRef.current = null;
      setTranscriptTurns([]);
      setSessionQueue([]);
      setIsSessionBusy(false);
      setSessionContextUsage(null);
      setSessionQuotaUsage(null);
    }
    await Promise.all([
      refreshWireAndRoster(),
      refreshSessionsAndTranscript(mode === "teardown" ? null : targetId),
    ]);
    return res;
  };

  const handleInterruptSession = async () => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) {
      throw new Error("No active session selected to interrupt.");
    }
    const res = await apiClient.interruptSession(targetId);
    setIsSessionBusy(false);
    recordAgentBusy(selectedAgent?.name_key, false);
    await refreshSessionsAndTranscript(targetId);
    return res;
  };

  const handleSetSessionModel = async (
    model: string,
    thinkingLevel?: string | null,
  ) => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) {
      throw new Error("No active session selected to update model.");
    }
    const res = await apiClient.setSessionModel(targetId, model, thinkingLevel);
    const matchesTarget = (sess: DiscoveredSessionSummary) =>
      sess.id === targetId ||
      (sess.persona ?? "").toLowerCase() === targetId.toLowerCase();
    const updatedModel = res.model || model;
    const updatedThinking =
      res.thinking_level !== undefined ? res.thinking_level : thinkingLevel;
    setSessions((prev) =>
      prev.map((sess) =>
        matchesTarget(sess)
          ? {
              ...sess,
              model: updatedModel,
              ...(updatedThinking !== undefined
                ? { thinking_level: updatedThinking }
                : {}),
            }
          : sess,
      ),
    );
    setRoutes((prev) =>
      prev.map((sess) =>
        matchesTarget(sess)
          ? {
              ...sess,
              model: updatedModel,
              ...(updatedThinking !== undefined
                ? { thinking_level: updatedThinking }
                : {}),
            }
          : sess,
      ),
    );
    await refreshSessionsAndTranscript(targetId);
    return res;
  };

  const handleBulkSessionAction = async (
    action: BulkSessionAction,
    selectedAgentKeys?: string[],
  ) => {
    const res = await executeScopedBulkSessionAction(
      apiClient,
      action,
      selectedAgentKeys,
      agents.map((a) => a.name_key.toLowerCase()),
    );
    await Promise.all([
      refreshWireAndRoster(),
      refreshSessionsAndTranscript(),
    ]);
    return res;
  };

  // Chat and the roster have their own elements (chatPaneElement, renderRosterSidebar); every
  // caller passes one of the remaining tool panes.
  const renderSecondaryPane = (paneTab: Exclude<CockpitTab | LeftPanelTab, "chat" | "roster">) => {
    switch (paneTab) {
      case "triage":
        return (
          <TriageQueuePane
            liveness={liveness}
            sessions={mergedRoutes}
            workflowRuns={workflowRuns}
            prs={prs}
            wireMessages={wireMessages}
            apiClient={apiClient}
            loading={loading}
            onRefresh={() => void loadAllInitialState()}
            onSelectAgent={(nameKey) => {
              const nextKey = nameKey.trim().toLowerCase();
              if (nextKey !== selectedAgentNameRef.current.trim().toLowerCase()) {
                selectedAgentNameRef.current = nameKey;
                selectedSessionIdRef.current = null;
                setTranscriptTurns(
                  preserveOptimisticTurn(
                    backgroundTranscriptsRef.current[nextKey] ?? [],
                    optimisticPromptRef.current,
                    Date.now(),
                    nextKey,
                    null,
                  ),
                );
                setSessionQueue([]);
                setIsSessionBusy(false);
                setSessionContextUsage(contextUsageByAgent[nextKey] ?? null);
                setSessionQuotaUsage(null);
              }
              setSelectedAgentName(nameKey);
              setSelectedSessionId(null);
              if (deviceLayoutRef.current === "phone") {
                setPhoneView("chat");
              }
              void refreshSessionsAndTranscript();
            }}
            onSelectSession={(sessionId) => {
              if (sessionId !== selectedSessionIdRef.current) {
                selectedSessionIdRef.current = sessionId;
                activeSessionIdRef.current = sessionId;
                setTranscriptTurns([]);
                setSessionQueue([]);
                setIsSessionBusy(false);
                setSessionContextUsage(null);
                setSessionQuotaUsage(null);
              }
              setSelectedSessionId(sessionId);
              if (deviceLayoutRef.current === "phone") {
                setPhoneView("chat");
              }
              void refreshSessionsAndTranscript(sessionId);
            }}
            onSelectTab={(tab) => handleSelectTab(tab)}
            onSelectLeftPanelTab={(tab) => handleSelectLeftPanelTab(tab)}
            onAckWireMessage={(id) => handleAckWireMessage(id)}
          />
        );
      case "watch":
        return (
          <FerroWatchPane
            authority={authority}
            agents={agents}
            sessions={mergedRoutes}
            apiClient={apiClient}
            onAfterRemodel={() => {
              void refreshWireAndRoster();
              void refreshSessionsAndTranscript();
            }}
          />
        );
      case "artifacts":
        return (
          <ArtifactPreviewPane
            apiClient={apiClient}
            activePath={activeArtifactPath}
            recentPaths={recentArtifacts}
            sessionId={activeSession?.id ?? selectedAgent?.name_key ?? null}
            turns={transcriptTurns}
            onSelectPath={(path) => {
              setActiveArtifactPath(path);
              setRecentArtifacts((prev) => {
                const filtered = prev.filter((item) => item !== path);
                return [path, ...filtered].slice(0, 20);
              });
            }}
            onClose={
              deviceLayout === "phone"
                ? () => navigatePhone("chat")
                : splitView
                  ? () => {
                      setSplitView(false);
                      setActiveTab("chat");
                    }
                  : undefined
            }
          />
        );
      case "git":
        return (
          <GitPrPane
            selectedProject={selectedAgent?.project ?? null}
            worktrees={worktrees}
            selectedWorktreeId={selectedWorktreeId}
            worktreeDiff={worktreeDiff}
            prs={prs}
            onSelectWorktree={setSelectedWorktreeId}
            onRefreshWorktrees={() => void loadAllInitialState()}
            onReviewPr={handleReviewPr}
            onMergePr={handleMergePr}
            sessionId={activeSession?.id ?? activeSpunUp?.sessionId ?? null}
            agentName={selectedAgent?.name_key ?? null}
            onSubmitDiffReview={async (batch) => {
              const targetId =
                activeSession?.id ??
                activeSpunUp?.sessionId ??
                selectedAgent?.name_key ??
                "";
              if (!targetId) {
                throw new Error("No active agent session selected to receive the review.");
              }
              return handleSendPrompt(targetId, { prompt: batch.formattedPrompt });
            }}
            loading={loading}
            apiClient={apiClient}
          />
        );
      case "board":
        return (
          <RecordsBoardPane
            projects={projects}
            selectedProject={selectedAgent?.project ?? null}
            apiClient={apiClient}
          />
        );
      case "mail":
        return (
          <WireMailPane
            messages={wireMessages}
            selectedAgentName={selectedAgent?.name_key ?? null}
            agents={agents}
            loading={loading}
            onRefresh={() => void refreshWireAndRoster()}
            onAckMessage={handleAckWireMessage}
            onSendMessage={handleSendWireMessage}
            apiClient={apiClient}
          />
        );
      case "terminal":
        return (
          <TerminalPane
            authority={authority}
            sessionId={activeSession?.id ?? selectedAgent?.name_key ?? "shell"}
            cwd={activeSession?.cwd ?? null}
            wsClient={wsClient}
            onOpenPairing={() => setPairingOpen(true)}
          />
        );
      case "hub":
        return (
          <HubDrawer
            workflows={workflows}
            workflowRuns={workflowRuns}
            architecture={architecture}
            stats={stats}
            loading={loading}
            onRefresh={() => void loadAllInitialState()}
            onStartWorkflowRun={async (payload) => {
              await apiClient.startWorkflowRun(payload);
              await refreshWorkflows();
            }}
            onApproveWorkflowStep={async (runId, payload) => {
              await apiClient.approveWorkflowRun(runId, payload);
              await refreshWorkflows();
            }}
            onRejectWorkflowStep={async (runId, payload) => {
              await apiClient.rejectWorkflowRun(runId, payload);
              await refreshWorkflows();
            }}
            apiClient={apiClient}
          />
        );
      case "gate":
        return <VaultGatePane authority={authority} apiClient={apiClient} />;
      case "state":
        return <FerroStatePane authority={authority} apiClient={apiClient} />;
    }
  };

  const isRightSplitTab = (
    tab: CockpitTab,
  ): tab is "mail" | "artifacts" | "git" | "board" =>
    tab === "mail" || tab === "artifacts" || tab === "git" || tab === "board";
  const rawRightSplitTab: Exclude<CockpitTab, "chat"> = isRightSplitTab(activeTab)
    ? activeTab
    : isRightSplitTab(secondaryTab)
      ? secondaryTab
      : "git";
  const rightSplitTab: Exclude<CockpitTab, "chat"> =
    !selectedAgent && rawRightSplitTab === "mail" ? "artifacts" : rawRightSplitTab;

  useEffect(() => {
    if (!selectedAgent) {
      if (secondaryTab === "mail") {
        setSecondaryTab("artifacts");
      }
      if (activeTab === "mail") {
        setActiveTab("artifacts");
      }
    }
  }, [selectedAgent, secondaryTab, activeTab]);

  const onlineNodesCount = useMemo(
    () => nodes.filter((n) => n.status === "online").length,
    [nodes],
  );

  const mergedRoutes = useMemo<DiscoveredSessionSummary[]>(() => {
    if (sessions.length === 0) return routes;
    if (routes.length === 0) return sessions;
    const matchedSessionIds = new Set<string>();
    const overlaid = routes.map((route) => {
      const routePersona = (route.persona ?? "").trim().toLowerCase();
      const fresh = sessions.find(
        (s) =>
          s.id === route.id ||
          (routePersona && (s.persona ?? "").trim().toLowerCase() === routePersona),
      );
      if (fresh) {
        matchedSessionIds.add(fresh.id);
        return fresh;
      }
      return route;
    });
    for (const s of sessions) {
      if (!matchedSessionIds.has(s.id)) {
        overlaid.push(s);
      }
    }
    return overlaid;
  }, [routes, sessions]);

  const handleOpenAgentSettingsFor = useCallback(
    (nameKey?: string) => {
      let switchedAgent = false;
      if (nameKey) {
        const nextKey = nameKey.trim().toLowerCase();
        if (nextKey !== selectedAgentNameRef.current.trim().toLowerCase()) {
          switchedAgent = true;
          selectedAgentNameRef.current = nameKey;
          selectedSessionIdRef.current = null;
          setTranscriptTurns(
            preserveOptimisticTurn(
              backgroundTranscriptsRef.current[nextKey] ?? [],
              optimisticPromptRef.current,
              Date.now(),
              nextKey,
              null,
            ),
          );
          setSessionQueue([]);
          setIsSessionBusy(false);
          setSessionContextUsage(contextUsageByAgent[nextKey] ?? null);
          setSessionQuotaUsage(null);
        }
        setSelectedAgentName(nameKey);
        setSelectedSessionId(null);
      }
      if (deviceLayoutRef.current === "phone") {
        // Agent settings render inside ChatPane, so show Chat without touching desktop state.
        setPhoneView("chat");
        setAgentSettingsOpen(true);
        if (switchedAgent) {
          void refreshSessionsAndTranscript();
        }
        return;
      }
      if (!splitView && activeTab !== "chat") {
        setActiveTab("chat");
      }
      setMobileRosterOpen(false);
      setAgentSettingsOpen(true);
      if (switchedAgent) {
        void refreshSessionsAndTranscript();
      }
    },
    [splitView, activeTab, refreshSessionsAndTranscript, contextUsageByAgent],
  );

  const effectiveBusyByAgent = useMemo<Record<string, boolean>>(() => {
    const selectedKey = (selectedAgent?.name_key ?? selectedAgentName)
      .trim()
      .toLowerCase();
    if (!selectedKey || !isSessionBusy) {
      return busyByAgent;
    }
    if (busyByAgent[selectedKey] === true) {
      return busyByAgent;
    }
    return { ...busyByAgent, [selectedKey]: true };
  }, [busyByAgent, isSessionBusy, selectedAgent?.name_key, selectedAgentName]);

  const effectiveContextUsageByAgent = useMemo<
    Record<string, SessionContextUsage | null>
  >(() => {
    const selectedKey = (selectedAgent?.name_key ?? selectedAgentName)
      .trim()
      .toLowerCase();
    if (!selectedKey || !sessionContextUsage) {
      return contextUsageByAgent;
    }
    if (contextUsageByAgent[selectedKey] === sessionContextUsage) {
      return contextUsageByAgent;
    }
    return { ...contextUsageByAgent, [selectedKey]: sessionContextUsage };
  }, [
    contextUsageByAgent,
    sessionContextUsage,
    selectedAgent?.name_key,
    selectedAgentName,
  ]);

  const handleReturnToOriginSettings = useCallback(() => {
    if (!settingsOriginMemento) {
      setAgentSettingsOpen(false);
      return;
    }
    const memento = settingsOriginMemento;
    setAgentSettingsOpen(false);
    setSettingsOriginMemento(null);
    setSettingsInitialProject(memento.initialProject);
    setSettingsInitialProjectFilter(memento.projectFilter);
    setSettingsInitialTab(memento.activeTab);
    setSettingsInitialProjectSubTab(memento.projectSubTab);
    setSettingsInitialAgentsSubTab(memento.agentsSubTab);
    setSettingsModalOpen(true);
  }, [settingsOriginMemento]);

  const returnToSettingsLabel = useMemo(() => {
    if (!settingsOriginMemento) return null;
    if (settingsOriginMemento.initialProject) {
      return `← Back to Project Settings (${settingsOriginMemento.initialProject})`;
    }
    return "← Back to Settings & Fleet Operations";
  }, [settingsOriginMemento]);

  const handleCloseAgentSettingsWithOrigin = useCallback(() => {
    if (settingsOriginMemento) {
      handleReturnToOriginSettings();
    } else {
      setAgentSettingsOpen(false);
    }
  }, [settingsOriginMemento, handleReturnToOriginSettings]);

  const handleToggleSplitView = useCallback(() => {
    setSplitView((prev) => {
      const next = !prev;
      const targetTab =
        !selectedAgent && secondaryTab === "mail" ? "artifacts" : secondaryTab;
      setActiveTab(
        next
          ? targetTab
          : leftPanelTab === "roster"
            ? "chat"
            : leftPanelTab === "watch"
              ? "state"
              : leftPanelTab === "triage"
                ? "hub"
                : leftPanelTab,
      );
      if (next) {
        if (
          typeof window !== "undefined" &&
          window.innerWidth < THREE_PANEL_MIN_VIEWPORT_PX
        ) {
          setDesktopSidebarCollapsed(true);
        }
        setMobileRosterOpen(false);
      }
      return next;
    });
  }, [secondaryTab, selectedAgent, leftPanelTab]);

  const handleToggleLeftSidebar = useCallback(() => {
    if (tabletMasterRef.current === "overlay") {
      // Portrait tablet: the roster is a drawer over the workspace. Opening it never changes
      // the docked arrangement (desktopSidebarCollapsed) or the right dock.
      setMobileRosterOpen((prev) => !prev);
      return;
    }
    const vw = typeof window !== "undefined" ? window.innerWidth : 1200;
    if (vw < THREE_PANEL_MIN_VIEWPORT_PX && splitView) {
      setSplitView(false);
      setMobileRosterOpen(true);
      setDesktopSidebarCollapsed(false);
      setActiveTab(
        leftPanelTab === "roster"
          ? "chat"
          : leftPanelTab === "watch"
            ? "state"
            : leftPanelTab === "triage"
              ? "hub"
              : leftPanelTab,
      );
      return;
    }
    setMobileRosterOpen((prev) => {
      const next = !prev;
      if (next && vw < THREE_PANEL_MIN_VIEWPORT_PX) {
        setSplitView(false);
        setActiveTab(
          leftPanelTab === "roster"
            ? "chat"
            : leftPanelTab === "watch"
              ? "state"
              : leftPanelTab === "triage"
                ? "hub"
                : leftPanelTab,
        );
      }
      return next;
    });
    setDesktopSidebarCollapsed((prev) => !prev);
  }, [leftPanelTab, splitView]);

  const currentInnerWidth =
    typeof window !== "undefined" ? window.innerWidth : viewportWidth;
  const viewportPanelMode = resolveViewportPanelMode(currentInnerWidth);
  const isMobileViewport = deviceLayout === "phone";
  const isTabletOverlay = tabletMaster === "overlay";
  const isTabletDocked = tabletMaster === "docked";
  const tabletDockedWidth = resolveTabletDockedWidth(currentInnerWidth);
  const leftSidebarOpen = isTabletOverlay ? mobileRosterOpen : !desktopSidebarCollapsed;

  // Rotating between tablet portrait and landscape keeps the selected agent and transcript;
  // only the roster arrangement follows the mode.
  const prevTabletMasterRef = useRef<TabletMaster | undefined>(tabletMaster);
  useEffect(() => {
    const prev = prevTabletMasterRef.current;
    prevTabletMasterRef.current = tabletMaster;
    if (prev === tabletMaster) return;
    if (tabletMaster === "overlay") {
      setMobileRosterOpen(false);
    } else if (tabletMaster === "docked" && prev === "overlay") {
      setMobileRosterOpen(false);
      setDesktopSidebarCollapsed(splitView);
    }
  }, [tabletMaster, splitView]);

  const isPhoneLayout = deviceLayout === "phone";

  const handleOpenSettingsFromHeader = () => {
    setSettingsOriginMemento(null);
    setSettingsInitialProject(null);
    setSettingsInitialProjectFilter(null);
    setSettingsInitialTab(undefined);
    setSettingsInitialProjectSubTab(undefined);
    setSettingsInitialAgentsSubTab(undefined);
    setSettingsModalOpen(true);
  };

  const handleSelectSessionFromHeader = (sessionId: string) => {
    if (sessionId !== selectedSessionIdRef.current) {
      selectedSessionIdRef.current = sessionId;
      activeSessionIdRef.current = sessionId;
      setTranscriptTurns([]);
      setSessionQueue([]);
      setIsSessionBusy(false);
      setSessionContextUsage(null);
      setSessionQuotaUsage(null);
    }
    setSelectedSessionId(sessionId);
    void refreshSessionsAndTranscript(sessionId);
  };

  // Wire Mail needs an agent; without one the phone falls back to Artifacts, as the desktop does.
  const effectivePhoneView: PhoneView =
    !selectedAgent && phoneView === "mail" ? "artifacts" : phoneView;

  const phoneHelpTopic: HelpTopicId =
    effectivePhoneView === "agents" || effectivePhoneView === "watch"
      ? "chat"
      : effectivePhoneView === "events"
        ? "hub"
        : effectivePhoneView;

  const chatPaneElement = (
    <ChatPane
      projects={projects}
      agentName={selectedAgent?.name_key ?? null}
      agentDisplayName={selectedAgent?.display_name ?? null}
      agentRole={selectedAgent?.role ?? null}
      agentProject={selectedAgent?.project ?? null}
      adapter={activeAdapter}
      model={activeModel}
      thinkingLevel={activeSession?.thinking_level ?? null}
      availableModels={
        activeSession?.available_models ??
        activeRoute?.available_models ??
        null
      }
      executionMode={
        activeSession?.execution_mode ?? activeRoute?.execution_mode
      }
      sessionId={activeSession?.id ?? activeSpunUp?.sessionId ?? null}
      turns={transcriptTurns}
      contextUsage={sessionContextUsage}
      quotaUsage={sessionQuotaUsage ?? activeSession?.quota_usage ?? null}
      isBusy={isSessionBusy}
      inFlightUnconfirmed={unconfirmedBySession[activeSession?.id ?? selectedAgent?.name_key ?? ""] ?? false}
      queue={sessionQueue}
      loading={loading}
      wsConnected={wsConnected}
      agentSettingsOpen={agentSettingsOpen}
      onOpenAgentSettings={() => {
        setSettingsOriginMemento(null);
        setAgentSettingsOpen(true);
      }}
      onCloseAgentSettings={handleCloseAgentSettingsWithOrigin}
      returnToSettingsLabel={returnToSettingsLabel}
      onReturnToSettings={handleReturnToOriginSettings}
      splitView={splitView}
      onToggleSplitView={handleToggleSplitView}
      onOpenMobileRoster={
        isPhoneLayout ? () => navigatePhone("roster") : handleToggleLeftSidebar
      }
      mobileSidebarOpen={
        isPhoneLayout ? effectivePhoneView === "agents" : leftSidebarOpen
      }
      onRefresh={() => {
        void refreshSessionsAndTranscript();
        void refreshWireAndRoster();
      }}
      onSendPrompt={handleSendPrompt}
      onSendQueuedNow={handleSendQueuedNow}
      onCancelQueued={handleCancelQueued}
      onUpdateQueued={handleUpdateQueued}
      onDecommissionSession={handleDecommissionSession}
      onInterruptSession={handleInterruptSession}
      onSetSessionModel={handleSetSessionModel}
      onOpenArtifact={handleOpenArtifact}
      onOpenHelpTopic={(topic) => {
        setHelpTopic(topic);
        setHelpDrawerOpen(true);
      }}
      apiClient={apiClient}
    />
  );

  const renderRosterSidebar = (embedded: boolean) => (
    <RosterSidebar
      projects={projects}
      showEmptyProjects={projectsFromHub}
      agents={agents}
      routes={mergedRoutes}
      sessions={sessions}
      liveness={liveness}
      unreadByAgent={unreadByAgent}
      unreadSeverityByAgent={unreadSeverityByAgent}
      busyByAgent={effectiveBusyByAgent}
      contextUsageByAgent={effectiveContextUsageByAgent}
      transcriptsByAgent={transcriptsByAgent}
      selectedAgentName={selectedAgent?.name_key ?? null}
      onSelectAgent={(nameKey) => {
        const nextKey = nameKey.trim().toLowerCase();
        if (nextKey !== selectedAgentNameRef.current.trim().toLowerCase()) {
          selectedAgentNameRef.current = nameKey;
          selectedSessionIdRef.current = null;
          setTranscriptTurns(
            preserveOptimisticTurn(
              backgroundTranscriptsRef.current[nextKey] ?? [],
              optimisticPromptRef.current,
              Date.now(),
              nextKey,
              null,
            ),
          );
          setSessionQueue([]);
          setIsSessionBusy(false);
          setSessionContextUsage(contextUsageByAgent[nextKey] ?? null);
          setSessionQuotaUsage(null);
        }
        setSelectedAgentName(nameKey);
        setSelectedSessionId(null);
        if (deviceLayoutRef.current === "phone") {
          setPhoneView("chat");
        } else {
          setMobileRosterOpen(false);
        }
        void refreshSessionsAndTranscript();
      }}
      onOpenAgentSettings={(nameKey) => {
        setSettingsOriginMemento(null);
        handleOpenAgentSettingsFor(nameKey);
      }}
      onOpenProjectSettings={(project) => {
        setSettingsOriginMemento(null);
        setSettingsInitialProject(project);
        setSettingsInitialProjectFilter(null);
        setSettingsInitialTab(undefined);
        setSettingsInitialProjectSubTab(undefined);
        setSettingsInitialAgentsSubTab(undefined);
        setSettingsModalOpen(true);
      }}
      onOpenSettingsModal={() => {
        setSettingsOriginMemento(null);
        setSettingsInitialProject(null);
        setSettingsInitialProjectFilter(null);
        setSettingsInitialTab(undefined);
        setSettingsInitialProjectSubTab(undefined);
        setSettingsInitialAgentsSubTab(undefined);
        setSettingsModalOpen(true);
      }}
      onOpenHelpModal={() => {
        setHelpTopic(isPhoneLayout ? phoneHelpTopic : activeTab);
        setHelpDrawerOpen(true);
      }}
      themeMode={themeMode}
      onToggleTheme={handleToggleTheme}
      onOpenSpinUpModal={() => setSpinUpModalOpen(true)}
      isOpen={embedded ? false : mobileRosterOpen}
      isCollapsedDesktop={embedded || isTabletOverlay ? false : desktopSidebarCollapsed}
      onClose={
        embedded
          ? undefined
          : () => {
              setMobileRosterOpen(false);
              if (!isTabletOverlay) {
                setDesktopSidebarCollapsed(true);
              }
            }
      }
      leftPanelTab={leftPanelTab}
      onSelectLeftPanelTab={handleSelectLeftPanelTab}
      consoleContent={
        leftPanelTab !== "roster" ? renderSecondaryPane(leftPanelTab) : undefined
      }
      width={
        isMobileViewport || isTabletOverlay
          ? undefined
          : isTabletDocked
            ? tabletDockedWidth
            : effectiveLeftWidth
      }
      minWidth={isTabletDocked ? TABLET_MASTER_MIN_WIDTH_PX : undefined}
      onResizeStart={isTabletDocked || isTabletOverlay ? undefined : handleLeftResizeStart}
      onTouchResizeStart={
        isTabletDocked || isTabletOverlay ? undefined : handleLeftTouchResizeStart
      }
      embedded={embedded}
    />
  );

  const renderPhoneView = (): React.ReactNode => {
    switch (effectivePhoneView) {
      case "chat":
        return chatPaneElement;
      case "agents":
        return renderRosterSidebar(true);
      case "events":
        return renderSecondaryPane("triage");
      default:
        return renderSecondaryPane(effectivePhoneView);
    }
  };

  return (
    <div
      className="fd-cockpit"
      data-testid="ferrodeck-cockpit"
      data-viewport-panel-mode={viewportPanelMode}
      data-device-layout={deviceLayout}
      data-tablet-master={tabletMaster}
    >
      {isPhoneLayout ? (
        <PhoneShell
          view={effectivePhoneView}
          hasAgent={Boolean(selectedAgent)}
          onNavigate={setPhoneView}
          onBack={() => navigatePhone("chat")}
          selectedAgent={selectedAgent}
          sessions={mergedRoutes}
          selectedSessionId={activeSession?.id ?? null}
          onSelectSession={handleSelectSessionFromHeader}
          wsConnected={wsConnected}
          agentsUnread={Object.values(unreadByAgent).reduce((sum, n) => sum + n, 0)}
          actions={{
            themeMode,
            onToggleTheme: handleToggleTheme,
            onOpenSettings: handleOpenSettingsFromHeader,
            onOpenHelp: () => {
              setHelpTopic(phoneHelpTopic);
              setHelpDrawerOpen(true);
            },
            onOpenNodes: () => setNodesModalOpen(true),
            onOpenSpinUp: () => setSpinUpModalOpen(true),
            onOpenPairing: () => setPairingOpen(true),
            onLogout: handleLogout,
          }}
        >
          {renderPhoneView()}
        </PhoneShell>
      ) : (
        <>
          <ContextHeader
            selectedAgent={selectedAgent}
            sessions={mergedRoutes}
            selectedSessionId={activeSession?.id ?? null}
            onSelectSession={handleSelectSessionFromHeader}
            activeTab={activeTab}
            onSelectTab={handleSelectTab}
            splitView={splitView}
            onToggleSplitView={handleToggleSplitView}
            themeMode={themeMode}
            onToggleTheme={handleToggleTheme}
            authority={authority}
            wsConnected={wsConnected}
            onlineNodesCount={onlineNodesCount}
            onOpenNodesModal={() => setNodesModalOpen(true)}
            onOpenSettingsModal={handleOpenSettingsFromHeader}
            onOpenHelpModal={(tab) => {
              setHelpTopic(tab ?? activeTab);
              setHelpDrawerOpen(true);
            }}
            onOpenAgentSettings={() => {
              setSettingsOriginMemento(null);
              handleOpenAgentSettingsFor();
            }}
            onOpenSpinUpModal={() => setSpinUpModalOpen(true)}
            onOpenPairingModal={() => setPairingOpen(true)}
            onLogout={handleLogout}
            onOpenMobileRoster={handleToggleLeftSidebar}
            mobileSidebarOpen={leftSidebarOpen}
            leftPanelWidth={
              leftSidebarOpen && !isMobileViewport && !isTabletOverlay
                ? isTabletDocked
                  ? tabletDockedWidth
                  : effectiveLeftWidth
                : undefined
            }
          />

          <div className="fd-cockpit-body" data-testid="cockpit-body">
            {renderRosterSidebar(false)}

            <div className="fd-main">
              <main
                className={`fd-workspace${splitView ? " is-split" : ""}`}
                data-testid="cockpit-workspace"
              >
              {chatPaneElement}
              {splitView && (
                <>
                  <div
                    className="fd-right-split-backdrop"
                    data-testid="mobile-split-backdrop"
                    aria-hidden="true"
                    onClick={handleToggleSplitView}
                  />
                  <div
                    className="fd-pane fd-right-split-pane"
                    data-testid="right-split-panel"
                  style={{
                    width: `${Math.max(PANEL_MIN_WIDTH_PX, rightPanelWidth)}px`,
                    minWidth: `${Math.max(PANEL_MIN_WIDTH_PX, rightPanelWidth)}px`,
                    flex: `0 0 ${Math.max(PANEL_MIN_WIDTH_PX, rightPanelWidth)}px`,
                    position: "relative",
                  }}
                >
                  <div
                    data-testid="right-panel-resize-handle"
                    role="separator"
                    aria-orientation="vertical"
                    aria-label="Resize right panel"
                    title="Drag left or right to resize right panel"
                    onMouseDown={handleRightResizeStart}
                    onTouchStart={handleRightTouchResizeStart}
                    style={{
                      position: "absolute",
                      top: 0,
                      left: "-6px",
                      bottom: 0,
                      width: "12px",
                      cursor: "col-resize",
                      zIndex: 25,
                      userSelect: "none",
                      touchAction: "none",
                    }}
                  />
                  <RightPanelHeader
                    selectedAgent={selectedAgent}
                    rightSplitTab={rightSplitTab}
                    onSelectTab={(tab) => {
                      setSecondaryTab(tab);
                      setActiveTab(tab);
                      setSplitView(true);
                      if (
                        typeof window !== "undefined" &&
                        window.innerWidth < THREE_PANEL_MIN_VIEWPORT_PX
                      ) {
                        setDesktopSidebarCollapsed(true);
                        setMobileRosterOpen(false);
                      }
                      void refreshSessionsAndTranscriptRef.current();
                    }}
                    onClose={handleToggleSplitView}
                  />
                  {renderSecondaryPane(rightSplitTab)}
                  </div>
                </>
              )}
              </main>
            </div>
          </div>
        </>
      )}

      <PairingModal
        isOpen={pairingOpen}
        onClose={() => setPairingOpen(false)}
        onPairWithCode={handlePairWithCode}
        onPairWithToken={handlePairWithToken}
        onAuthenticated={(auth) => {
          if (auth) {
            setAuthority(auth);
          }
          wsClient.setToken(apiClient.getToken());
          wsClient.connect();
          setPairingOpen(false);
          void loadAllInitialState();
        }}
        apiClient={apiClient}
        error={pairingError}
      />

      <SpinUpAgentModal
        isOpen={spinUpModalOpen}
        onClose={() => {
          setSpinUpModalOpen(false);
          setSpinUpInitialProject(null);
        }}
        initialProject={spinUpInitialProject}
        projects={projects}
        nodes={nodes}
        onSpinUp={handleSpinUpSession}
        apiClient={apiClient}
      />

      <SettingsModal
        isOpen={settingsModalOpen}
        onClose={() => {
          setSettingsModalOpen(false);
          setSettingsInitialProject(null);
          setSettingsInitialProjectFilter(null);
          setSettingsInitialTab(undefined);
          setSettingsInitialProjectSubTab(undefined);
          setSettingsInitialAgentsSubTab(undefined);
        }}
        initialProject={settingsInitialProject}
        initialProjectFilter={settingsInitialProjectFilter}
        initialTab={settingsInitialTab}
        initialProjectSubTab={settingsInitialProjectSubTab}
        initialAgentsSubTab={settingsInitialAgentsSubTab}
        agents={agents}
        authority={authority}
        wsConnected={wsConnected}
        onlineNodesCount={onlineNodesCount}
        activeCockpitTab={activeTab}
        onSelectCockpitTab={handleSelectTab}
        onOpenNodesModal={() => setNodesModalOpen(true)}
        onOpenSpinUpModal={(proj) => {
          setSpinUpInitialProject(proj ?? null);
          setSpinUpModalOpen(true);
        }}
        onOpenPairingModal={() => setPairingOpen(true)}
        onOpenAgentSettings={(nameKey, originMemento) => {
          setSettingsModalOpen(false);
          if (originMemento) {
            setSettingsOriginMemento(originMemento);
          }
          handleOpenAgentSettingsFor(nameKey);
        }}
        onLogout={handleLogout}
        onAgentsUpdated={() => void refreshWireAndRoster()}
        projects={projects}
        projectsFromHub={projectsFromHub}
        refreshProjects={refreshProjects}
        onProjectsChanged={() => void refreshProjects()}
        onDecommissionAgent={async (targetId, mode) => {
          const res = await apiClient.decommissionSession(targetId, mode);
          await Promise.all([
            refreshWireAndRoster(),
            refreshSessionsAndTranscript(),
          ]);
          return res;
        }}
        onBulkSessionAction={handleBulkSessionAction}
        themeMode={themeMode}
        onChangeThemeMode={handleChangeThemeMode}
        fontScalePct={fontScalePct}
        onChangeFontScalePct={handleChangeFontScalePct}
        showSafeAreaMargins={showSafeAreaMargins}
        onChangeShowSafeAreaMargins={(next) => {
          persistShowSafeAreaMargins(next);
          setShowSafeAreaMargins(next);
        }}
        onSettingsUpdated={(updated) => {
          if (
            updated.theme_preference === "dark" ||
            updated.theme_preference === "light" ||
            updated.theme_preference === "system"
          ) {
            persistThemeMode(updated.theme_preference);
            setThemeMode(updated.theme_preference);
          }
          if (updated.timezone) {
            setActiveTimeZone(updated.timezone);
          }
        }}
        apiClient={apiClient}
      />

      <NodesModal
        isOpen={nodesModalOpen}
        onClose={() => setNodesModalOpen(false)}
        nodes={nodes}
        onRefreshNodes={refreshNodes}
        onCreateEnrollToken={(label, ttlSecs) =>
          apiClient.createNodeEnrollToken(label, ttlSecs)
        }
        onRevokeNode={async (nodeId) => {
          await apiClient.revokeNode(nodeId);
          setNodes((prev) => prev.filter((n) => n.node_id !== nodeId));
          await refreshNodes();
        }}
        apiClient={apiClient}
      />

      <HelpDrawer
        isOpen={helpDrawerOpen}
        activeTopic={helpTopic}
        onSelectTopic={setHelpTopic}
        onClose={() => setHelpDrawerOpen(false)}
      />

      {showSafeAreaMargins && (
        <div
          data-testid="ios-safe-area-overlay"
          style={{
            position: "fixed",
            top: 0,
            right: 0,
            bottom: 0,
            left: 0,
            pointerEvents: "none",
            zIndex: 9999,
            border: "2px dashed rgba(56, 189, 248, 0.85)",
            boxSizing: "border-box",
          }}
        >
          <div
            ref={safeAreaProbeRef}
            data-testid="ios-safe-area-probe"
            aria-hidden="true"
            style={{
              position: "fixed",
              top: 0,
              left: 0,
              width: 0,
              height: 0,
              paddingTop: "env(safe-area-inset-top, 0px)",
              paddingRight: "env(safe-area-inset-right, 0px)",
              paddingBottom: "env(safe-area-inset-bottom, 0px)",
              paddingLeft: "env(safe-area-inset-left, 0px)",
              pointerEvents: "none",
              visibility: "hidden",
              zIndex: -1,
            }}
          />
          <div
            data-testid="ios-safe-area-band-top"
            style={{
              position: "absolute",
              top: 0,
              left: 0,
              right: 0,
              height: `${Math.max(0, safeAreaMetrics.top)}px`,
              backgroundColor: "rgba(245, 158, 11, 0.22)",
              borderBottom: "2px dashed #F59E0B",
              boxSizing: "border-box",
            }}
          />
          <div
            data-testid="ios-safe-area-band-bottom"
            style={{
              position: "absolute",
              bottom: 0,
              left: 0,
              right: 0,
              height: `${Math.max(0, safeAreaMetrics.bottom)}px`,
              backgroundColor: "rgba(245, 158, 11, 0.22)",
              borderTop: "2px dashed #F59E0B",
              boxSizing: "border-box",
            }}
          />
          <div
            data-testid="ios-safe-area-band-left"
            style={{
              position: "absolute",
              top: 0,
              bottom: 0,
              left: 0,
              width: `${Math.max(0, safeAreaMetrics.left)}px`,
              backgroundColor: "rgba(56, 189, 248, 0.18)",
              borderRight:
                safeAreaMetrics.left > 0 ? "2px dashed #38BDF8" : "none",
              boxSizing: "border-box",
            }}
          />
          <div
            data-testid="ios-safe-area-band-right"
            style={{
              position: "absolute",
              top: 0,
              bottom: 0,
              right: 0,
              width: `${Math.max(0, safeAreaMetrics.right)}px`,
              backgroundColor: "rgba(56, 189, 248, 0.18)",
              borderLeft:
                safeAreaMetrics.right > 0 ? "2px dashed #38BDF8" : "none",
              boxSizing: "border-box",
            }}
          />
          <div
            data-testid="ios-safe-area-hud"
            className="fd-mono"
            style={{
              position: "fixed",
              top: `${Math.max(4, safeAreaMetrics.top + 4)}px`,
              left: "50%",
              transform: "translateX(-50%)",
              maxWidth: "calc(100vw - 12px)",
              backgroundColor: "rgba(10, 13, 18, 0.92)",
              border: "1px solid #F59E0B",
              borderRadius: "999px",
              padding: "2px 8px",
              fontSize: "10px",
              lineHeight: 1.35,
              color: "#FBBF24",
              display: "inline-flex",
              alignItems: "center",
              gap: "6px",
              whiteSpace: "nowrap",
              overflow: "hidden",
              textOverflow: "ellipsis",
              pointerEvents: "auto",
              zIndex: 10000,
              boxShadow: "0 4px 12px rgba(0, 0, 0, 0.55)",
            }}
          >
            <span style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
              Safe Margins: T:{safeAreaMetrics.top}px B:{safeAreaMetrics.bottom}px L:{safeAreaMetrics.left}px R:{safeAreaMetrics.right}px · Viewport: {safeAreaMetrics.width}×{safeAreaMetrics.height}px (dpr:{safeAreaMetrics.dpr})
            </span>
            <button
              type="button"
              data-testid="ios-safe-area-close-btn"
              aria-label="Hide iOS safe-area margins overlay"
              title="Hide iOS safe-area margins overlay"
              onClick={() => {
                persistShowSafeAreaMargins(false);
                setShowSafeAreaMargins(false);
              }}
              style={{
                border: "none",
                background: "rgba(245, 158, 11, 0.2)",
                color: "#FBBF24",
                borderRadius: "999px",
                padding: "0 5px",
                fontSize: "10px",
                fontWeight: 700,
                cursor: "pointer",
                lineHeight: 1.4,
                flexShrink: 0,
              }}
            >
              [×]
            </button>
          </div>
        </div>
      )}
    </div>
  );
};

export default App;

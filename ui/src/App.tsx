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
  SettingsModal,
  executeScopedBulkSessionAction,
} from "./components/SettingsModal";
import { SpinUpAgentModal } from "./components/SpinUpAgentModal";
import { TerminalPane } from "./components/TerminalPane";
import { VaultGatePane } from "./components/VaultGatePane";
import { FerroStatePane } from "./components/FerroStatePane";
import { FerroWatchPane } from "./components/FerroWatchPane";
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
): OptimisticPrompt[] {
  const activeKey = (currentAgentKey ?? "").trim().toLowerCase();
  return prompts.filter((p) => {
    if (!p || !p.content?.trim()) return false;
    if (nowMs - p.sentAt > OPTIMISTIC_PROMPT_TTL_MS) return false;
    const promptKey = (p.agentKey ?? "").trim().toLowerCase();
    const matchesActive = !activeKey || !promptKey || promptKey === activeKey;
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
    readStoredPanelWidth("fd_left_roster_width", 326, 326, 720),
  );
  const [leftConsoleWidth, setLeftConsoleWidth] = useState<number>(() =>
    readStoredPanelWidth("fd_left_console_width", 440, 326, 720),
  );
  const [rightPanelWidth, setRightPanelWidth] = useState<number>(() =>
    readStoredPanelWidth("fd_right_panel_width", 460, 326, 800),
  );
  const panelResizeCleanupRef = useRef<(() => void) | null>(null);
  const [mobileRosterOpen, setMobileRosterOpen] = useState<boolean>(false);
  const [desktopSidebarCollapsed, setDesktopSidebarCollapsed] = useState<boolean>(false);
  const [themeMode, setThemeMode] = useState<ThemeMode>(getInitialThemeMode);
  const [fontScalePct, setFontScalePct] = useState<number>(getInitialFontScalePct);
  useActiveTimeZone();
  const [activeArtifactPath, setActiveArtifactPath] = useState<string | null>(null);
  const [recentArtifacts, setRecentArtifacts] = useState<string[]>([]);

  useEffect(() => {
    return () => {
      panelResizeCleanupRef.current?.();
    };
  }, []);

  const effectiveLeftWidth = Math.max(
    326,
    leftPanelTab === "roster" ? leftRosterWidth : leftConsoleWidth,
  );

  const handleLeftResizeStart = useCallback(
    (e: React.MouseEvent<HTMLDivElement>) => {
      e.preventDefault();
      panelResizeCleanupRef.current?.();
      const startX = e.clientX;
      const isRoster = leftPanelTab === "roster";
      const startWidth = isRoster ? leftRosterWidth : leftConsoleWidth;
      const handleMouseMove = (moveEvent: MouseEvent) => {
        const delta = moveEvent.clientX - startX;
        const maxAllowed =
          typeof window !== "undefined" && window.innerWidth > 1024
            ? Math.max(
                326,
                Math.min(720, window.innerWidth - 360 - (splitView ? rightPanelWidth : 0)),
              )
            : 720;
        const nextWidth = Math.round(Math.max(326, Math.min(maxAllowed, startWidth + delta)));
        if (isRoster) {
          setLeftRosterWidth(nextWidth);
          persistPanelWidth("fd_left_roster_width", nextWidth);
        } else {
          setLeftConsoleWidth(nextWidth);
          persistPanelWidth("fd_left_console_width", nextWidth);
        }
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
    [leftPanelTab, leftRosterWidth, leftConsoleWidth, splitView, rightPanelWidth],
  );

  const handleRightResizeStart = useCallback(
    (e: React.MouseEvent<HTMLDivElement>) => {
      e.preventDefault();
      panelResizeCleanupRef.current?.();
      const startX = e.clientX;
      const startWidth = rightPanelWidth;
      const handleMouseMove = (moveEvent: MouseEvent) => {
        const delta = startX - moveEvent.clientX;
        const maxAllowed =
          typeof window !== "undefined" && window.innerWidth > 1024
            ? Math.max(
                326,
                Math.min(
                  800,
                  window.innerWidth - 360 - (desktopSidebarCollapsed ? 0 : effectiveLeftWidth),
                ),
              )
            : 800;
        const nextWidth = Math.round(Math.max(326, Math.min(maxAllowed, startWidth + delta)));
        setRightPanelWidth(nextWidth);
        persistPanelWidth("fd_right_panel_width", nextWidth);
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
    [rightPanelWidth, desktopSidebarCollapsed, effectiveLeftWidth],
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
    setSplitView(true);
    setSecondaryTab("artifacts");
    setActiveTab("artifacts");
  }, []);

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
  const [sessionContextUsage, setSessionContextUsage] =
    useState<SessionContextUsage | null>(null);
  const [sessionQuotaUsage, setSessionQuotaUsage] =
    useState<SessionQuotaUsage | null>(null);
  const optimisticPromptRef = useRef<OptimisticPrompt[]>([]);
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
    (content: string, agentKey?: string | null, sentAt: number = Date.now()) => {
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
          Math.abs(sentAt - p.sentAt) < 3_000,
      );
      if (!duplicate) {
        current.push({
          content,
          sentAt,
          agentKey: normalizedKey || undefined,
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
    );
    optimisticPromptRef.current = pruneOptimisticPrompts(
      optimisticPromptRef.current,
      turns,
      nowMs,
      activeKey,
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
    if (tab === "terminal" || tab === "hub" || tab === "gate" || tab === "state") {
      setLeftPanelTab(tab);
      setDesktopSidebarCollapsed(false);
      setMobileRosterOpen(true);
      if (typeof window !== "undefined" && window.innerWidth <= 768) {
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
            : leftPanelTab,
      );
    } else {
      setSecondaryTab(tab);
      setActiveTab(tab);
      setSplitView(true);
      setMobileRosterOpen(false);
    }
    void refreshSessionsAndTranscriptRef.current();
  };

  const handleSelectLeftPanelTab = useCallback(
    (tab: LeftPanelTab) => {
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
      } else {
        setActiveTab(tab);
      }
    },
    [splitView, secondaryTab],
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
            : (sessionOverride ?? requestedSessionId ?? requestedAgentName);
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
            const stillSameTarget =
              selectedAgentNameRef.current.trim().toLowerCase() === requestedAgentKey &&
              selectedSessionIdRef.current === requestedSessionId &&
              targetPersona.trim().toLowerCase() === selectedAgentNameRef.current.trim().toLowerCase();
            if (stillSameTarget) {
              if (trRes.turns) {
                applyTranscriptTurns(trRes.turns);
              }
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
          if (
            selectedAgentNameRef.current.trim().toLowerCase() === initialKey &&
            selectedSessionIdRef.current === initialSessionId
          ) {
            applyTranscriptTurns(trRes.turns ?? []);
            setSessionQueue(trRes.queue ?? []);
            setIsSessionBusy(Boolean(trRes.is_busy));
            setSessionContextUsage(trRes.context_usage ?? null);
            setSessionQuotaUsage(trRes.quota_usage ?? null);
          }
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
        const targetAgentKey =
          rawPersona ||
          (rawSessionId && rawSessionId === selectedSessionIdRef.current
            ? activeKey
            : "");
        if (rawContent) {
          recordOptimisticPrompt(rawContent, targetAgentKey || activeKey);
          if (!targetAgentKey || targetAgentKey === activeKey) {
            setTranscriptTurns((prev) =>
              preserveOptimisticTurn(
                prev,
                optimisticPromptRef.current,
                Date.now(),
                activeKey,
              ),
            );
          }
        }
        if (!targetAgentKey || targetAgentKey === activeKey) {
          if (Array.isArray(data.queue)) {
            setSessionQueue(data.queue as QueuedOperatorPrompt[]);
          }
          if (typeof data.is_busy === "boolean") {
            setIsSessionBusy(data.is_busy);
          }
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

  const activeSession = useMemo(() => {
    if (selectedSessionId) {
      const explicit = sessions.find((s) => s.id === selectedSessionId);
      if (explicit) return explicit;
    }
    const key = (selectedAgent?.name_key ?? "").toLowerCase();
    if (!key) {
      return null;
    }
    return (
      sessions.find((s) => (s.persona ?? "").toLowerCase() === key) ??
      routes.find((r) => (r.persona ?? "").toLowerCase() === key) ??
      null
    );
  }, [sessions, routes, selectedSessionId, selectedAgent]);

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
    const viewingChat = activeTab === "chat" || splitView;
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
  }, [transcriptsByAgent, selectedAgent, selectedAgentName, activeTab, splitView]);

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
    const res = await apiClient.sendSessionPrompt(targetId, payload);
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
      recordOptimisticPrompt(content, agentKey);
      setTranscriptTurns((prev) =>
        preserveOptimisticTurn(
          prev,
          optimisticPromptRef.current,
          Date.now(),
          selectedAgentNameRef.current,
        ),
      );
    }
    void refreshSessionsAndTranscript(targetId);
    return res;
  };

  const handleSendQueuedNow = async (promptId: string) => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) return;
    const res = await apiClient.sendQueuedPromptNow(targetId, promptId);
    if (Array.isArray(res.queue)) {
      setSessionQueue(res.queue);
    }
    if (typeof res.is_busy === "boolean") {
      setIsSessionBusy(res.is_busy);
    }
    const content = res.effective_body || res.prompt;
    if (content) {
      const agentKey = (selectedAgent?.name_key ?? selectedAgentName)
        .trim()
        .toLowerCase();
      recordOptimisticPrompt(content, agentKey);
      setTranscriptTurns((prev) =>
        preserveOptimisticTurn(
          prev,
          optimisticPromptRef.current,
          Date.now(),
          selectedAgentNameRef.current,
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

  const renderSecondaryPane = (paneTab: CockpitTab | LeftPanelTab) => {
    switch (paneTab) {
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
              splitView
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
            worktrees={worktrees}
            selectedWorktreeId={selectedWorktreeId}
            worktreeDiff={worktreeDiff}
            prs={prs}
            onSelectWorktree={setSelectedWorktreeId}
            onRefreshWorktrees={() => void loadAllInitialState()}
            onReviewPr={handleReviewPr}
            onMergePr={handleMergePr}
            loading={loading}
            apiClient={apiClient}
          />
        );
      case "board":
        return (
          <RecordsBoardPane
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
      case "chat":
      default:
        return (
          <ChatPane
            agentName={selectedAgent?.name_key ?? null}
            agentDisplayName={selectedAgent?.display_name ?? null}
            agentRole={selectedAgent?.role ?? null}
            agentProject={selectedAgent?.project ?? null}
            adapter={activeSession?.adapter ?? null}
            model={activeSession?.model ?? null}
            thinkingLevel={activeSession?.thinking_level ?? null}
            availableModels={activeSession?.available_models ?? null}
            executionMode={activeSession?.execution_mode}
            sessionId={activeSession?.id ?? null}
            turns={transcriptTurns}
            contextUsage={sessionContextUsage}
            quotaUsage={sessionQuotaUsage ?? activeSession?.quota_usage ?? null}
            isBusy={isSessionBusy}
            queue={sessionQueue}
            loading={loading}
            wsConnected={wsConnected}
            agentSettingsOpen={agentSettingsOpen}
            onOpenAgentSettings={() => setAgentSettingsOpen(true)}
            onCloseAgentSettings={() => setAgentSettingsOpen(false)}
            onRefresh={() => void refreshSessionsAndTranscript()}
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
              : leftPanelTab,
      );
      if (next) {
        setMobileRosterOpen(false);
      }
      return next;
    });
  }, [secondaryTab, selectedAgent, leftPanelTab]);

  const handleToggleLeftSidebar = useCallback(() => {
    setMobileRosterOpen((prev) => {
      const next = !prev;
      if (next && typeof window !== "undefined" && window.innerWidth <= 768) {
        setSplitView(false);
        setActiveTab(
          leftPanelTab === "roster"
            ? "chat"
            : leftPanelTab === "watch"
              ? "state"
              : leftPanelTab,
        );
      }
      return next;
    });
    setDesktopSidebarCollapsed((prev) => !prev);
  }, [leftPanelTab]);

  const isMobileViewport =
    typeof window !== "undefined" && window.innerWidth <= 768;
  const leftSidebarOpen = isMobileViewport
    ? mobileRosterOpen
    : !desktopSidebarCollapsed;

  return (
    <div className="fd-cockpit" data-testid="ferrodeck-cockpit">
      <RosterSidebar
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
              ),
            );
            setSessionQueue([]);
            setIsSessionBusy(false);
            setSessionContextUsage(contextUsageByAgent[nextKey] ?? null);
            setSessionQuotaUsage(null);
          }
          setSelectedAgentName(nameKey);
          setSelectedSessionId(null);
          setMobileRosterOpen(false);
          void refreshSessionsAndTranscript();
        }}
        onOpenAgentSettings={(nameKey) => handleOpenAgentSettingsFor(nameKey)}
        onOpenProjectSettings={(project) => {
          setSettingsInitialProject(project);
          setSettingsModalOpen(true);
        }}
        onOpenSettingsModal={() => {
          setSettingsInitialProject(null);
          setSettingsModalOpen(true);
        }}
        onOpenHelpModal={() => {
          setHelpTopic(activeTab);
          setHelpDrawerOpen(true);
        }}
        themeMode={themeMode}
        onToggleTheme={handleToggleTheme}
        onOpenSpinUpModal={() => setSpinUpModalOpen(true)}
        isOpen={mobileRosterOpen}
        isCollapsedDesktop={desktopSidebarCollapsed}
        onClose={() => {
          setMobileRosterOpen(false);
          setDesktopSidebarCollapsed(true);
        }}
        leftPanelTab={leftPanelTab}
        onSelectLeftPanelTab={handleSelectLeftPanelTab}
        consoleContent={
          leftPanelTab !== "roster" ? renderSecondaryPane(leftPanelTab) : undefined
        }
        width={effectiveLeftWidth}
        onResizeStart={handleLeftResizeStart}
      />

      <div className="fd-main">
        <ContextHeader
          selectedAgent={selectedAgent}
          sessions={mergedRoutes}
          selectedSessionId={activeSession?.id ?? null}
          onSelectSession={(sessionId) => {
            if (sessionId !== selectedSessionIdRef.current) {
              selectedSessionIdRef.current = sessionId;
              setTranscriptTurns([]);
              setSessionQueue([]);
              setIsSessionBusy(false);
              setSessionContextUsage(null);
              setSessionQuotaUsage(null);
            }
            setSelectedSessionId(sessionId);
            void refreshSessionsAndTranscript(sessionId);
          }}
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
          onOpenSettingsModal={() => {
            setSettingsInitialProject(null);
            setSettingsModalOpen(true);
          }}
          onOpenHelpModal={(tab) => {
            setHelpTopic(tab ?? activeTab);
            setHelpDrawerOpen(true);
          }}
          onOpenAgentSettings={() => handleOpenAgentSettingsFor()}
          onOpenSpinUpModal={() => setSpinUpModalOpen(true)}
          onOpenPairingModal={() => setPairingOpen(true)}
          onLogout={handleLogout}
          onOpenMobileRoster={handleToggleLeftSidebar}
          mobileSidebarOpen={leftSidebarOpen}
        />

        <main
          className={`fd-workspace${splitView ? " is-split" : ""}`}
          data-testid="cockpit-workspace"
        >
          <ChatPane
            agentName={selectedAgent?.name_key ?? null}
            agentDisplayName={selectedAgent?.display_name ?? null}
            agentRole={selectedAgent?.role ?? null}
            agentProject={selectedAgent?.project ?? null}
            adapter={activeSession?.adapter ?? null}
            model={activeSession?.model ?? null}
            thinkingLevel={activeSession?.thinking_level ?? null}
            availableModels={activeSession?.available_models ?? null}
            executionMode={activeSession?.execution_mode}
            sessionId={activeSession?.id ?? null}
            turns={transcriptTurns}
            contextUsage={sessionContextUsage}
            quotaUsage={sessionQuotaUsage ?? activeSession?.quota_usage ?? null}
            isBusy={isSessionBusy}
            queue={sessionQueue}
            loading={loading}
            wsConnected={wsConnected}
            agentSettingsOpen={agentSettingsOpen}
            onOpenAgentSettings={() => setAgentSettingsOpen(true)}
            onCloseAgentSettings={() => setAgentSettingsOpen(false)}
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
                width: `${rightPanelWidth}px`,
                minWidth: `${rightPanelWidth}px`,
                flex: `0 0 ${rightPanelWidth}px`,
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
                style={{
                  position: "absolute",
                  top: 0,
                  left: "-3px",
                  bottom: 0,
                  width: "6px",
                  cursor: "col-resize",
                  zIndex: 25,
                  userSelect: "none",
                }}
              />
              <RightPanelHeader
                selectedAgent={selectedAgent}
                rightSplitTab={rightSplitTab}
                onSelectTab={(tab) => {
                  setSecondaryTab(tab);
                  setActiveTab(tab);
                  setSplitView(true);
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

      <SpinUpAgentModal
        isOpen={spinUpModalOpen}
        onClose={() => {
          setSpinUpModalOpen(false);
          setSpinUpInitialProject(null);
        }}
        initialProject={spinUpInitialProject}
        nodes={nodes}
        onSpinUp={handleSpinUpSession}
        apiClient={apiClient}
      />

      <SettingsModal
        isOpen={settingsModalOpen}
        onClose={() => {
          setSettingsModalOpen(false);
          setSettingsInitialProject(null);
        }}
        initialProject={settingsInitialProject}
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
        onOpenAgentSettings={(nameKey) => {
          setSettingsModalOpen(false);
          setSettingsInitialProject(null);
          handleOpenAgentSettingsFor(nameKey);
        }}
        onLogout={handleLogout}
        onAgentsUpdated={() => void refreshWireAndRoster()}
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

      <HelpDrawer
        isOpen={helpDrawerOpen}
        activeTopic={helpTopic}
        onSelectTopic={setHelpTopic}
        onClose={() => setHelpDrawerOpen(false)}
      />
    </div>
  );
};

export default App;

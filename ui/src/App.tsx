import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import "./styles/tokens.css";
import { HubApiClient, HubApiError } from "./api/client";
import { HubWsClient } from "./api/ws";
import { ChatPane } from "./components/ChatPane";
import { COCKPIT_TABS, ContextHeader } from "./components/ContextHeader";
import { GitPrPane } from "./components/GitPrPane";
import { HubDrawer } from "./components/HubDrawer";
import { PairingModal } from "./components/PairingModal";
import { RosterSidebar } from "./components/RosterSidebar";
import { TerminalPane } from "./components/TerminalPane";
import { VaultGatePane } from "./components/VaultGatePane";
import { FerroStatePane } from "./components/FerroStatePane";
import { WireMailPane } from "./components/WireMailPane";
import {
  DEFAULT_WORKSPACE_AGENTS,
  type AgentLivenessItem,
  type AgentRecord,
  type ArchitectureResponse,
  type CockpitTab,
  type DiscoveredSessionSummary,
  type HubAuthority,
  type HubStatsResponse,
  type PrMergeRequest,
  type PromptRequest,
  type PrReviewRequest,
  type PullRequestItem,
  type SessionContextUsage,
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
}

export interface OptimisticPrompt {
  content: string;
  sentAt: number;
  agentKey?: string;
}

export const OPTIMISTIC_PROMPT_TTL_MS = 5_000;

/**
 * Keep a just-sent user turn visible until the vendor CLI flushes it to the
 * on-disk transcript. Without this, an immediate transcript refresh replaces
 * the optimistic turn with a stale disk snapshot and the prompt vanishes.
 */
export function preserveOptimisticTurn(
  turns: TranscriptTurn[],
  optimistic: OptimisticPrompt | null,
  nowMs: number = Date.now(),
  currentAgentKey?: string | null,
): TranscriptTurn[] {
  if (!optimistic || !optimistic.content.trim()) {
    return turns;
  }
  if (
    currentAgentKey != null &&
    optimistic.agentKey != null &&
    optimistic.agentKey.trim().toLowerCase() !==
      currentAgentKey.trim().toLowerCase()
  ) {
    return turns;
  }
  if (nowMs - optimistic.sentAt > OPTIMISTIC_PROMPT_TTL_MS) {
    return turns;
  }
  const onDisk = turns.slice(-5).some((turn) => {
    if ((turn.role || "").toLowerCase() !== "user" || !turn.content) {
      return false;
    }
    return turn.content.trim() === optimistic.content.trim();
  });
  if (onDisk) {
    return turns;
  }
  const last = turns[turns.length - 1];
  return [
    ...turns,
    {
      index: last ? last.index + 1 : turns.length,
      role: "user",
      content: optimistic.content,
      thinking: null,
      tool_calls: [],
      created_at: new Date(optimistic.sentAt).toISOString(),
    },
  ];
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

export const App: React.FC<AppProps> = ({ apiClient: propApiClient, wsClient: propWsClient }) => {
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
  const [wsConnected, setWsConnected] = useState<boolean>(false);

  // Navigation & Layout State
  const [activeTab, setActiveTab] = useState<CockpitTab>("chat");
  const [secondaryTab, setSecondaryTab] = useState<Exclude<CockpitTab, "chat">>("git");
  const [splitView, setSplitView] = useState<boolean>(false);
  const [mobileRosterOpen, setMobileRosterOpen] = useState<boolean>(false);

  // Data State
  const [agents, setAgents] = useState<AgentRecord[]>(DEFAULT_WORKSPACE_AGENTS);
  const [routes, setRoutes] = useState<DiscoveredSessionSummary[]>([]);
  const [liveness, setLiveness] = useState<AgentLivenessItem[]>([]);
  const [sessions, setSessions] = useState<DiscoveredSessionSummary[]>([]);
  const [selectedAgentName, setSelectedAgentName] = useState<string>("ferris");
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
  const optimisticPromptRef = useRef<OptimisticPrompt | null>(null);
  const refreshInFlightRef = useRef<boolean>(false);
  const [lastSeenAssistantTurnIndex, setLastSeenAssistantTurnIndex] =
    useState<Record<string, number>>(readInitialLastSeenAssistantTurn);
  const [backgroundTranscripts, setBackgroundTranscripts] = useState<
    Record<string, TranscriptTurn[]>
  >({});
  const backgroundStampRef = useRef<Record<string, string>>({});

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
      setBackgroundTranscripts((prev) =>
        prev[key] === turns ? prev : { ...prev, [key]: turns },
      );
    },
    [],
  );

  /** Apply a disk transcript without wiping a just-sent optimistic turn. */
  const applyTranscriptTurns = useCallback((turns: TranscriptTurn[]) => {
    const merged = preserveOptimisticTurn(
      turns,
      optimisticPromptRef.current,
      Date.now(),
      selectedAgentNameRef.current,
    );
    if (merged === turns && optimisticPromptRef.current) {
      // Found on disk, aged out, or agent switched; guard has served its purpose.
      optimisticPromptRef.current = null;
    }
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

  // Update secondaryTab whenever user selects a non-chat tab
  const handleSelectTab = (tab: CockpitTab) => {
    setActiveTab(tab);
    if (tab !== "chat") {
      setSecondaryTab(tab);
    }
  };

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
      const selectedKey = selectedAgentName.trim().toLowerCase();
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
        if (bgRes?.turns) {
          backgroundStampRef.current[persona] = stamp;
          recordTranscript(persona, bgRes.turns);
        }
      }
    },
    [apiClient, selectedAgentName, recordTranscript],
  );

  const refreshSessionsAndTranscript = useCallback(
    async (sessionOverride?: string | null) => {
      if (refreshInFlightRef.current) {
        return;
      }
      refreshInFlightRef.current = true;
      try {
        const sessRes = await apiClient.getSessions().catch(() => null);
        const list = sessRes?.sessions ?? [];
        if (sessRes) {
          setSessions(list);
        }
        await scanBackgroundTranscripts(list);
        const targetId = sessionOverride ?? selectedSessionId ?? selectedAgentName;
        if (targetId) {
          const trRes = await apiClient.getSessionTranscript(targetId).catch(() => null);
          if (trRes) {
            if (trRes.turns) {
              applyTranscriptTurns(trRes.turns);
              const targetPersona =
                list.find((session) => session.id === targetId)?.persona ??
                (list.some(
                  (session) =>
                    (session.persona ?? "").toLowerCase() ===
                    targetId.toLowerCase(),
                )
                  ? targetId
                  : selectedAgentName);
              recordTranscript(targetPersona, trRes.turns);
            }
            setSessionContextUsage(trRes.context_usage ?? null);
            if (typeof trRes.is_busy === "boolean") {
              setIsSessionBusy(trRes.is_busy);
            }
            if (Array.isArray(trRes.queue)) {
              setSessionQueue(trRes.queue);
            }
          }
        }
      } catch {
        // Ignore
      } finally {
        refreshInFlightRef.current = false;
      }
    },
    [apiClient, selectedSessionId, selectedAgentName, applyTranscriptTurns, scanBackgroundTranscripts, recordTranscript],
  );

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

  const loadAllInitialState = useCallback(async () => {
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
      const matchedSessionId =
        selectedSessionIdRef.current ??
        sessionsRes?.sessions?.find((s) => (s.persona ?? "").toLowerCase() === initialKey)?.id ??
        agentsRes?.routes?.find((r) => (r.persona ?? "").toLowerCase() === initialKey)?.id ??
        initialAgentName;
      if (matchedSessionId) {
        const trRes = await apiClient.getSessionTranscript(matchedSessionId).catch(() => null);
        if (trRes) {
          applyTranscriptTurns(trRes.turns ?? []);
          recordTranscript(initialAgentName, trRes.turns ?? []);
          setSessionQueue(trRes.queue ?? []);
          setIsSessionBusy(Boolean(trRes.is_busy));
          setSessionContextUsage(trRes.context_usage ?? null);
        }
      }
    } finally {
      setLoading(false);
    }
  }, [apiClient, applyTranscriptTurns, scanBackgroundTranscripts, recordTranscript]);

  // Startup URL pairing check + Auth session check + initial load
  useEffect(() => {
    let cancelled = false;

    const bootstrap = async () => {
      if (typeof window !== "undefined" && window.location) {
        const params = new URLSearchParams(window.location.search);
        const pairCode = params.get("pair") || params.get("code");
        if (pairCode && pairCode.trim()) {
          try {
            const pairRes = await apiClient.pair({ code: pairCode.trim() });
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
            params.delete("pair");
            params.delete("code");
            const nextQuery = params.toString();
            const cleanUrl = `${window.location.pathname}${nextQuery ? `?${nextQuery}` : ""}${
              window.location.hash || ""
            }`;
            window.history.replaceState({}, "", cleanUrl);
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
    const unsubSession = wsClient.subscribe("session", () => {
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

  // Load transcript when activeSession or selectedAgentName changes
  useEffect(() => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) {
      setTranscriptTurns([]);
      setSessionQueue([]);
      setIsSessionBusy(false);
      return;
    }
    let cancelled = false;
    apiClient
      .getSessionTranscript(targetId)
      .then((res) => {
        if (!cancelled) {
          applyTranscriptTurns(res.turns ?? []);
          recordTranscript(selectedAgent?.name_key, res.turns ?? []);
          setSessionQueue(res.queue ?? []);
          setIsSessionBusy(Boolean(res.is_busy));
          setSessionContextUsage(res.context_usage ?? null);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setTranscriptTurns([]);
          setSessionQueue([]);
          setIsSessionBusy(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, activeSession?.id, selectedAgent?.name_key, authority?.tier, applyTranscriptTurns, recordTranscript]);

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
    await apiClient.logout().catch(() => {});
    setAuthority(null);
    wsClient.setToken(null);
    wsClient.disconnect();
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
      optimisticPromptRef.current = {
        content,
        sentAt: Date.now(),
        agentKey: selectedAgentName.trim().toLowerCase(),
      };
      setTranscriptTurns((prev) => [
        ...prev,
        {
          index: prev.length,
          role: "user",
          content,
          thinking: null,
          tool_calls: [],
          created_at: new Date().toISOString(),
        },
      ]);
    }
    await refreshSessionsAndTranscript(targetId);
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
      setTranscriptTurns((prev) => [
        ...prev,
        {
          index: prev.length,
          role: "user",
          content,
          thinking: null,
          tool_calls: [],
          created_at: new Date().toISOString(),
        },
      ]);
    }
    await refreshSessionsAndTranscript(targetId);
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

  const renderSecondaryPane = (paneTab: CockpitTab) => {
    switch (paneTab) {
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
            cwd={activeSession?.cwd ?? "/home/lmcfarlin/Projects/Artificial-Humanity"}
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
            adapter={activeSession?.adapter ?? null}
            sessionId={activeSession?.id ?? null}
            turns={transcriptTurns}
            contextUsage={sessionContextUsage}
            isBusy={isSessionBusy}
            queue={sessionQueue}
            loading={loading}
            wsConnected={wsConnected}
            onRefresh={() => void refreshSessionsAndTranscript()}
            onSendPrompt={handleSendPrompt}
            onSendQueuedNow={handleSendQueuedNow}
            onCancelQueued={handleCancelQueued}
            apiClient={apiClient}
          />
        );
    }
  };

  const rightSplitTab: Exclude<CockpitTab, "chat"> =
    activeTab === "chat" ? secondaryTab : activeTab;

  return (
    <div className="fd-cockpit" data-testid="ferrodeck-cockpit">
      <RosterSidebar
        agents={agents}
        routes={routes}
        liveness={liveness}
        unreadByAgent={unreadByAgent}
        selectedAgentName={selectedAgent?.name_key ?? null}
        onSelectAgent={(nameKey) => {
          setSelectedAgentName(nameKey);
          setSelectedSessionId(null);
        }}
        isOpen={mobileRosterOpen}
        onClose={() => setMobileRosterOpen(false)}
      />

      <div className="fd-main">
        <ContextHeader
          selectedAgent={selectedAgent}
          sessions={sessions.length > 0 ? sessions : routes}
          selectedSessionId={activeSession?.id ?? null}
          onSelectSession={setSelectedSessionId}
          activeTab={activeTab}
          onSelectTab={handleSelectTab}
          splitView={splitView}
          onToggleSplitView={() => setSplitView((prev) => !prev)}
          authority={authority}
          wsConnected={wsConnected}
          onOpenPairingModal={() => setPairingOpen(true)}
          onLogout={handleLogout}
          onOpenMobileRoster={() => setMobileRosterOpen((prev) => !prev)}
        />

        <main
          className={`fd-workspace${splitView ? " is-split" : ""}`}
          data-testid="cockpit-workspace"
        >
          {splitView ? (
            <>
              <ChatPane
                agentName={selectedAgent?.name_key ?? null}
                agentDisplayName={selectedAgent?.display_name ?? null}
                adapter={activeSession?.adapter ?? null}
                sessionId={activeSession?.id ?? null}
                turns={transcriptTurns}
                contextUsage={sessionContextUsage}
                isBusy={isSessionBusy}
                queue={sessionQueue}
                loading={loading}
                wsConnected={wsConnected}
                onRefresh={() => void refreshSessionsAndTranscript()}
                onSendPrompt={handleSendPrompt}
                onSendQueuedNow={handleSendQueuedNow}
                onCancelQueued={handleCancelQueued}
                apiClient={apiClient}
              />
              {renderSecondaryPane(rightSplitTab)}
            </>
          ) : (
            renderSecondaryPane(activeTab)
          )}
        </main>

        {/* Mobile Bottom Navigation Bar */}
        <nav
          className="fd-mobile-nav"
          aria-label="Mobile Cockpit Navigation"
          data-testid="mobile-bottom-nav"
        >
          {COCKPIT_TABS.map((tab) => {
            const isActive = activeTab === tab.id;
            return (
              <button
                key={tab.id}
                type="button"
                className={`fd-btn${isActive ? " fd-btn-active" : ""}`}
                onClick={() => handleSelectTab(tab.id)}
              >
                {tab.label}
              </button>
            );
          })}
        </nav>
      </div>

      <PairingModal
        isOpen={pairingOpen}
        onClose={() => setPairingOpen(false)}
        onPairWithCode={handlePairWithCode}
        onPairWithToken={handlePairWithToken}
        error={pairingError}
      />
    </div>
  );
};

export default App;

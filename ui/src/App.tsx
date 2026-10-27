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
  const [transcriptTurns, setTranscriptTurns] = useState<TranscriptTurn[]>([]);

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

  const refreshSessionsAndTranscript = useCallback(
    async (sessionOverride?: string | null) => {
      try {
        const sessRes = await apiClient.getSessions().catch(() => null);
        const list = sessRes?.sessions ?? [];
        if (sessRes) {
          setSessions(list);
        }
        const targetId = sessionOverride ?? selectedSessionId ?? selectedAgentName;
        if (targetId) {
          const trRes = await apiClient.getSessionTranscript(targetId).catch(() => null);
          if (trRes?.turns) {
            setTranscriptTurns(trRes.turns);
          }
        }
      } catch {
        // Ignore
      }
    },
    [apiClient, selectedSessionId, selectedAgentName],
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
    } finally {
      setLoading(false);
    }
  }, [apiClient]);

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
    return (
      sessions.find((s) => (s.persona ?? "").toLowerCase() === key) ??
      routes.find((r) => (r.persona ?? "").toLowerCase() === key) ??
      sessions[0] ??
      null
    );
  }, [sessions, routes, selectedSessionId, selectedAgent]);

  // Load transcript when activeSession or selectedAgentName changes
  useEffect(() => {
    const targetId = activeSession?.id || selectedAgent?.name_key;
    if (!targetId) {
      setTranscriptTurns([]);
      return;
    }
    let cancelled = false;
    apiClient
      .getSessionTranscript(targetId)
      .then((res) => {
        if (!cancelled) {
          setTranscriptTurns(res.turns ?? []);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setTranscriptTurns([]);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [apiClient, activeSession?.id, selectedAgent?.name_key]);

  // Compute unread mail counts per agent
  const unreadByAgent = useMemo<Record<string, number>>(() => {
    const counts: Record<string, number> = {};
    for (const item of wireMessages) {
      const receipt =
        item.receipt ??
        (item.receipts && item.receipts.length > 0 ? item.receipts[0] : undefined);
      const allReceiptsAcked =
        Boolean(item.receipts && item.receipts.length > 0) &&
        item.receipts!.every((r) => Boolean(r.acked_at || r.status === "acked"));
      const isAcked = Boolean(
        receipt?.acked_at || receipt?.status === "acked" || allReceiptsAcked,
      );
      if (!isAcked) {
        const toKey = (item.message.to ?? "").trim().toLowerCase();
        if (toKey && toKey !== "all" && toKey !== "agents.all") {
          counts[toKey] = (counts[toKey] ?? 0) + 1;
        }
      }
    }
    return counts;
  }, [wireMessages]);

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
    await apiClient.sendSessionPrompt(targetId, payload);
    await refreshSessionsAndTranscript(targetId);
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
      case "chat":
      default:
        return (
          <ChatPane
            agentName={selectedAgent?.name_key ?? null}
            agentDisplayName={selectedAgent?.display_name ?? null}
            adapter={activeSession?.adapter ?? null}
            sessionId={activeSession?.id ?? null}
            turns={transcriptTurns}
            loading={loading}
            wsConnected={wsConnected}
            onRefresh={() => void refreshSessionsAndTranscript()}
            onSendPrompt={handleSendPrompt}
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
                loading={loading}
                wsConnected={wsConnected}
                onRefresh={() => void refreshSessionsAndTranscript()}
                onSendPrompt={handleSendPrompt}
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

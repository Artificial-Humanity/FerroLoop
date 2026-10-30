import React, { useMemo, useState } from "react";
import {
  DEFAULT_WORKSPACE_AGENTS,
  resolveLivenessState,
  type AgentLivenessItem,
  type AgentRecord,
  type DiscoveredSessionSummary,
} from "../types/api";

export interface RosterSidebarProps {
  agents: AgentRecord[];
  routes: DiscoveredSessionSummary[];
  liveness: AgentLivenessItem[];
  unreadByAgent: Record<string, number>;
  selectedAgentName: string | null;
  onSelectAgent: (nameKey: string) => void;
  onOpenAgentSettings?: (nameKey: string) => void;
  onOpenSpinUpModal?: () => void;
  isOpen?: boolean;
  onClose?: () => void;
}

const CANONICAL_PROJECTS = [
  "AI-Lab-AMD",
  "FerroLoop",
  "Kaggle-Gemma4",
  "Lucida",
  "Prosodia",
  "Sonora",
  "Website",
];

interface EnrichedAgent {
  record: AgentRecord;
  route: DiscoveredSessionSummary | null;
  livenessItem: AgentLivenessItem | null;
  livenessState: "alive" | "stale" | "dead";
  adapter: string | null;
  unreadCount: number;
}

export function formatRoleLabel(role: string): string {
  const clean = role.trim();
  if (clean === "lab_manager") {
    return "lab_manager";
  }
  return clean || "member";
}

export function getAdapterBadgeClass(adapter: string | null | undefined): string {
  if (!adapter) {
    return "fd-badge";
  }
  const lower = adapter.toLowerCase();
  if (lower.includes("muse")) return "fd-badge fd-adapter-muse";
  if (lower.includes("antigravity") || lower === "agy") return "fd-badge fd-adapter-antigravity";
  if (lower.includes("codex")) return "fd-badge fd-adapter-codex";
  if (lower.includes("claude")) return "fd-badge fd-adapter-claude";
  if (lower.includes("grok")) return "fd-badge fd-adapter-grok";
  return "fd-badge";
}

export const RosterSidebar: React.FC<RosterSidebarProps> = ({
  agents,
  routes,
  liveness,
  unreadByAgent,
  selectedAgentName,
  onSelectAgent,
  onOpenAgentSettings,
  onOpenSpinUpModal,
  isOpen = false,
  onClose,
}) => {
  const [searchQuery, setSearchQuery] = useState("");

  const enrichedAgents = useMemo<EnrichedAgent[]>(() => {
    const baseAgents = agents.length > 0 ? agents : DEFAULT_WORKSPACE_AGENTS;

    const routeMap = new Map<string, DiscoveredSessionSummary>();
    for (const r of routes) {
      const personaKey = (r.persona ?? "").trim().toLowerCase();
      if (personaKey && !routeMap.has(personaKey)) {
        routeMap.set(personaKey, r);
      }
    }

    const livenessMap = new Map<string, AgentLivenessItem>();
    for (const item of liveness) {
      const key = item.agent.trim().toLowerCase();
      if (key) {
        livenessMap.set(key, item);
      }
    }

    const seenKeys = new Set<string>();
    const merged: AgentRecord[] = [];
    for (const ag of baseAgents) {
      const key = ag.name_key.trim().toLowerCase();
      if (!seenKeys.has(key)) {
        seenKeys.add(key);
        merged.push(ag);
      }
    }

    // Also include any agents reported in liveness or routes that weren't in baseAgents
    for (const [key, item] of livenessMap.entries()) {
      if (!seenKeys.has(key)) {
        seenKeys.add(key);
        merged.push({
          id: `urn:fl:agent:${key}`,
          name_key: key,
          display_name: item.agent,
          project: item.project || "Workspace",
          role: item.role || "assistant",
          status: "active",
        });
      }
    }

    return merged.map((record) => {
      const key = record.name_key.trim().toLowerCase();
      const route = routeMap.get(key) ?? null;
      const livenessItem = livenessMap.get(key) ?? null;
      const hasRoute = Boolean(route || livenessItem?.has_route);
      const livenessState = resolveLivenessState(livenessItem?.verdict, hasRoute);
      const adapter = route?.adapter ?? livenessItem?.adapter ?? null;
      const unreadCount = unreadByAgent[key] ?? unreadByAgent[record.name_key] ?? 0;

      return {
        record,
        route,
        livenessItem,
        livenessState,
        adapter,
        unreadCount,
      };
    });
  }, [agents, routes, liveness, unreadByAgent]);

  const groupedByProject = useMemo(() => {
    const query = searchQuery.trim().toLowerCase();
    const filtered = enrichedAgents.filter((item) => {
      if (!query) return true;
      const { record, adapter } = item;
      return (
        record.name_key.toLowerCase().includes(query) ||
        record.display_name.toLowerCase().includes(query) ||
        record.project.toLowerCase().includes(query) ||
        record.role.toLowerCase().includes(query) ||
        (adapter && adapter.toLowerCase().includes(query))
      );
    });

    const groups = new Map<string, EnrichedAgent[]>();
    for (const proj of CANONICAL_PROJECTS) {
      groups.set(proj, []);
    }

    for (const item of filtered) {
      const proj = item.record.project || "Workspace";
      const list = groups.get(proj) ?? [];
      list.push(item);
      groups.set(proj, list);
    }

    return Array.from(groups.entries()).filter(([, list]) => list.length > 0);
  }, [enrichedAgents, searchQuery]);

  const selectedKey = (selectedAgentName ?? "").trim().toLowerCase();

  return (
    <>
      {isOpen && (
        <div
          className="fd-sidebar-backdrop"
          data-testid="mobile-sidebar-backdrop"
          onClick={onClose}
          aria-hidden="true"
        />
      )}
      <aside
        className={`fd-sidebar${isOpen ? " is-open" : ""}`}
        aria-label="Agent Roster"
        data-testid="roster-sidebar"
        data-mobile-open={isOpen ? "true" : "false"}
      >
      <div
        style={{
          padding: "12px 14px",
          borderBottom: "1px solid var(--border-subtle)",
          display: "flex",
          flexDirection: "column",
          gap: "10px",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
            <span
              style={{
                width: "10px",
                height: "10px",
                borderRadius: "2px",
                backgroundColor: "var(--accent-primary)",
                display: "inline-block",
              }}
            />
            <span
              style={{
                fontWeight: 700,
                fontSize: "13px",
                letterSpacing: "0.06em",
                textTransform: "uppercase",
              }}
            >
              FerroDeck
            </span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: "6px" }}>
            <span className="fd-badge" title="Registered Workspace Agents">
              {enrichedAgents.length} agents
            </span>
            {onOpenSpinUpModal && (
              <button
                type="button"
                className="fd-btn fd-btn-primary"
                data-testid="spin-up-agent-btn"
                onClick={onOpenSpinUpModal}
                style={{ padding: "3px 8px", fontSize: "11px" }}
              >
                + Spin Up
              </button>
            )}
            {onClose && (
              <button
                type="button"
                className="fd-btn"
                onClick={onClose}
                aria-label="Close Roster"
                style={{ padding: "2px 6px" }}
              >
                ✕
              </button>
            )}
          </div>
        </div>

        <input
          type="search"
          className="fd-input"
          placeholder="Search agents, projects, adapters..."
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          style={{ width: "100%" }}
        />
      </div>

      <div className="fd-scroll" style={{ padding: "10px 8px" }}>
        {groupedByProject.length === 0 ? (
          <div
            style={{
              padding: "20px 12px",
              color: "var(--text-muted)",
              textAlign: "center",
              fontSize: "12px",
            }}
          >
            No matching agents found.
          </div>
        ) : (
          groupedByProject.map(([project, items]) => (
            <div key={project} style={{ marginBottom: "14px" }}>
              <div
                style={{
                  padding: "4px 8px",
                  fontSize: "11px",
                  fontWeight: 700,
                  textTransform: "uppercase",
                  letterSpacing: "0.05em",
                  color: "var(--text-muted)",
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                }}
              >
                <span>{project}</span>
                <span style={{ fontSize: "10px", color: "var(--text-dim)" }}>{items.length}</span>
              </div>

              <div style={{ display: "flex", flexDirection: "column", gap: "3px" }}>
                {items.map(({ record, livenessState, adapter, unreadCount }) => {
                  const key = record.name_key.toLowerCase();
                  const isSelected = key === selectedKey;
                  const dotClass =
                    livenessState === "alive"
                      ? "fd-dot fd-dot-alive"
                      : livenessState === "stale"
                        ? "fd-dot fd-dot-stale"
                        : "fd-dot fd-dot-dead";

                  return (
                    <div
                      key={record.id || record.name_key}
                      role="button"
                      tabIndex={0}
                      data-testid={`agent-row-${key}`}
                      onClick={() => {
                        onSelectAgent(record.name_key);
                        onClose?.();
                      }}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          onSelectAgent(record.name_key);
                          onClose?.();
                        }
                      }}
                      style={{
                        width: "100%",
                        textAlign: "left",
                        padding: "8px 10px",
                        borderRadius: "var(--radius-md)",
                        border: isSelected
                          ? "1px solid var(--border-accent)"
                          : "1px solid transparent",
                        backgroundColor: isSelected ? "var(--accent-subtle)" : "transparent",
                        color: "var(--text-primary)",
                        cursor: "pointer",
                        display: "flex",
                        flexDirection: "column",
                        gap: "5px",
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: "6px",
                        }}
                      >
                        <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                          <span
                            data-testid={`liveness-dot-${key}`}
                            className={dotClass}
                            title={`Liveness: ${livenessState}`}
                          />
                          <span style={{ fontWeight: 600, fontSize: "13px" }}>
                            {record.display_name}
                          </span>
                        </div>

                        <div style={{ display: "flex", alignItems: "center", gap: "4px" }}>
                          {unreadCount > 0 && (
                            <span
                              data-testid={`unread-badge-${key}`}
                              className="fd-badge"
                              style={{
                                backgroundColor: "var(--accent-primary)",
                                color: "#0A0D12",
                                borderColor: "var(--accent-primary)",
                                fontWeight: 700,
                              }}
                            >
                              {unreadCount}
                            </span>
                          )}
                          <button
                            type="button"
                            className="fd-btn"
                            data-testid={`agent-settings-btn-${key}`}
                            title={`Settings for ${record.display_name}`}
                            aria-label={`Settings for ${record.display_name}`}
                            onClick={(e) => {
                              e.stopPropagation();
                              onSelectAgent(record.name_key);
                              onOpenAgentSettings?.(record.name_key);
                              onClose?.();
                            }}
                            style={{
                              padding: "1px 5px",
                              fontSize: "11px",
                              minHeight: "20px",
                              lineHeight: 1,
                              color: "var(--text-muted)",
                            }}
                          >
                            ⚙
                          </button>
                        </div>
                      </div>

                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: "6px",
                          paddingLeft: "16px",
                          flexWrap: "wrap",
                        }}
                      >
                        <span className="fd-badge" data-testid={`role-badge-${key}`}>
                          {formatRoleLabel(record.role)}
                        </span>
                        {adapter && (
                          <span
                            data-testid={`adapter-pill-${key}`}
                            className={getAdapterBadgeClass(adapter)}
                          >
                            {adapter}
                          </span>
                        )}
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          ))
        )}
      </div>
      </aside>
    </>
  );
};

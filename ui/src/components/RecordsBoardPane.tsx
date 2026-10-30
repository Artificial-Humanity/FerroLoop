import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { HubApiClient } from "../api/client";
import type {
  ProjectRecordItem,
  RecordState,
  RecordsBoardSummary,
} from "../types/api";

export interface RecordsBoardPaneProps {
  selectedProject?: string | null;
  apiClient?: HubApiClient;
}

const DEFAULT_PROJECTS = [
  "ferroloop",
  "sonora",
  "prosodia",
  "lucida",
  "kaggle-gemma4",
  "ai-lab-amd",
  "website",
];

const SWIMLANES: Array<{
  state: RecordState;
  label: string;
  accentColor: string;
}> = [
  { state: "todo", label: "Todo", accentColor: "var(--text-muted, #94a3b8)" },
  { state: "doing", label: "Doing", accentColor: "var(--accent, #38bdf8)" },
  { state: "done", label: "Done", accentColor: "var(--status-alive, #22c55e)" },
];

function computeSummaryFromRecords(
  records: ProjectRecordItem[],
  base?: Partial<RecordsBoardSummary> | null,
): RecordsBoardSummary {
  const todo_count = records.filter((r) => r.state === "todo").length;
  const doing_count = records.filter((r) => r.state === "doing").length;
  const done_count = records.filter((r) => r.state === "done").length;
  const total_count = records.length;
  const completion_pct =
    total_count > 0 ? Math.round((done_count / total_count) * 100) : 0;
  const open_findings_count = records.reduce(
    (sum, r) => sum + (r.findings_open ?? 0),
    0,
  );
  return {
    todo_count,
    doing_count,
    done_count,
    total_count,
    completion_pct,
    open_findings_count,
    attempts_count: base?.attempts_count ?? 0,
    total_cost_usd: base?.total_cost_usd ?? 0,
  };
}

export const RecordsBoardPane: React.FC<RecordsBoardPaneProps> = ({
  selectedProject,
  apiClient: providedClient,
}) => {
  const apiClient = useMemo(
    () => providedClient ?? new HubApiClient(),
    [providedClient],
  );

  const [project, setProject] = useState<string>(() =>
    selectedProject && selectedProject.trim().length > 0
      ? selectedProject.trim().toLowerCase()
      : "ferroloop",
  );
  const [availableProjects, setAvailableProjects] =
    useState<string[]>(DEFAULT_PROJECTS);
  const [records, setRecords] = useState<ProjectRecordItem[]>([]);
  const [summary, setSummary] = useState<RecordsBoardSummary | null>(null);
  const [searchQuery, setSearchQuery] = useState<string>("");
  const [loading, setLoading] = useState<boolean>(false);
  const [error, setError] = useState<string | null>(null);

  // Create Record Form state
  const [showCreateForm, setShowCreateForm] = useState<boolean>(false);
  const [newTitle, setNewTitle] = useState<string>("");
  const [newArea, setNewArea] = useState<string>("");
  const [newState, setNewState] = useState<RecordState>("todo");
  const [creating, setCreating] = useState<boolean>(false);

  // Drag-and-drop state
  const [draggedRecordId, setDraggedRecordId] = useState<string | null>(null);
  const [dragOverLane, setDragOverLane] = useState<RecordState | null>(null);
  const [movingId, setMovingId] = useState<string | null>(null);

  const prevSelectedProjectRef = useRef<string | null | undefined>(
    selectedProject,
  );

  useEffect(() => {
    if (selectedProject !== prevSelectedProjectRef.current) {
      prevSelectedProjectRef.current = selectedProject;
      if (selectedProject && selectedProject.trim().length > 0) {
        setProject(selectedProject.trim().toLowerCase());
      }
    }
  }, [selectedProject]);

  const loadRecords = useCallback(
    async (targetProject: string) => {
      setLoading(true);
      setError(null);
      try {
        const res = await apiClient.listRecords(targetProject);
        const loadedRecords = Array.isArray(res?.records) ? res.records : [];
        setRecords(loadedRecords);
        if (
          Array.isArray(res?.available_projects) &&
          res.available_projects.length > 0
        ) {
          setAvailableProjects(res.available_projects);
        }
        if (res?.summary && typeof res.summary === "object") {
          setSummary({
            ...computeSummaryFromRecords(loadedRecords, res.summary),
            ...res.summary,
          });
        } else {
          setSummary(computeSummaryFromRecords(loadedRecords, null));
        }
      } catch (err) {
        setError(
          err instanceof Error ? err.message : "Failed to load project records",
        );
      } finally {
        setLoading(false);
      }
    },
    [apiClient],
  );

  useEffect(() => {
    void loadRecords(project);
  }, [loadRecords, project]);

  const projectOptions = useMemo(() => {
    const seen = new Set<string>();
    const result: string[] = [];
    for (const p of [project, ...availableProjects, ...DEFAULT_PROJECTS]) {
      const normalized = p.trim();
      if (!normalized) continue;
      const key = normalized.toLowerCase();
      if (!seen.has(key)) {
        seen.add(key);
        result.push(normalized);
      }
    }
    return result;
  }, [project, availableProjects]);

  const effectiveSummary = useMemo<RecordsBoardSummary>(() => {
    if (summary) {
      return summary;
    }
    return computeSummaryFromRecords(records, null);
  }, [summary, records]);

  const filteredRecords = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) {
      return records;
    }
    return records.filter(
      (r) =>
        r.title.toLowerCase().includes(q) ||
        r.display_id.toLowerCase().includes(q) ||
        r.id.toLowerCase().includes(q) ||
        (r.area ?? "").toLowerCase().includes(q),
    );
  }, [records, searchQuery]);

  const handleCreateRecord = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmedTitle = newTitle.trim();
    if (!trimmedTitle) {
      return;
    }
    setCreating(true);
    setError(null);
    try {
      const trimmedArea = newArea.trim();
      const res = await apiClient.createRecord({
        project,
        title: trimmedTitle,
        ...(trimmedArea ? { area: trimmedArea } : {}),
        state: newState,
      });
      if (res?.record) {
        setRecords((prev) => {
          const next = [
            ...prev.filter(
              (r) =>
                r.id !== res.record.id &&
                r.display_id !== res.record.display_id,
            ),
            res.record,
          ];
          setSummary((prevSum) => computeSummaryFromRecords(next, prevSum));
          return next;
        });
      } else {
        await loadRecords(project);
      }
      setNewTitle("");
      setNewArea("");
      setNewState("todo");
      setShowCreateForm(false);
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to create project record",
      );
    } finally {
      setCreating(false);
    }
  };

  const handleMoveRecord = async (
    record: ProjectRecordItem,
    toState: RecordState,
  ) => {
    if (record.state === toState) {
      return;
    }
    const recordIdentifier = record.id || record.display_id;
    setMovingId(record.display_id);
    setError(null);
    try {
      const res = await apiClient.moveRecord(
        recordIdentifier,
        toState,
        project,
      );
      const updatedRecord: ProjectRecordItem = res?.record ?? {
        ...record,
        state: toState,
      };
      setRecords((prev) => {
        const next = prev.map((item) =>
          item.id === record.id || item.display_id === record.display_id
            ? updatedRecord
            : item,
        );
        setSummary((prevSum) => computeSummaryFromRecords(next, prevSum));
        return next;
      });
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : `Failed to move record #${record.display_id} to ${toState}`,
      );
    } finally {
      setMovingId(null);
      setDraggedRecordId(null);
      setDragOverLane(null);
    }
  };

  const handleDropOnLane = (
    e: React.DragEvent<HTMLDivElement>,
    laneState: RecordState,
  ) => {
    e.preventDefault();
    setDragOverLane(null);
    let droppedId = "";
    try {
      droppedId = e.dataTransfer?.getData("text/plain") ?? "";
    } catch {
      // Ignore dataTransfer read issues in synthetic test environments
    }
    const effectiveId = droppedId || draggedRecordId;
    if (!effectiveId) {
      return;
    }
    const matched = records.find(
      (r) => r.display_id === effectiveId || r.id === effectiveId,
    );
    if (matched && matched.state !== laneState) {
      void handleMoveRecord(matched, laneState);
    }
  };

  const todoPct =
    effectiveSummary.total_count > 0
      ? (effectiveSummary.todo_count / effectiveSummary.total_count) * 100
      : 0;
  const doingPct =
    effectiveSummary.total_count > 0
      ? (effectiveSummary.doing_count / effectiveSummary.total_count) * 100
      : 0;
  const donePct =
    effectiveSummary.total_count > 0
      ? (effectiveSummary.done_count / effectiveSummary.total_count) * 100
      : 0;

  return (
    <section
      className="fd-pane"
      data-testid="records-board-pane"
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100%",
        overflow: "hidden",
      }}
    >
      {/* Header & Filter Controls */}
      <div
        className="fd-pane-header"
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "10px",
          flexWrap: "wrap",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <strong style={{ fontSize: "14px" }}>Project Records &amp; Kanban Board</strong>
          <select
            aria-label="Select Project"
            data-testid="board-project-select"
            className="fd-select fd-mono"
            style={{ fontSize: "12px", padding: "4px 8px" }}
            value={project}
            onChange={(e) => setProject(e.target.value)}
          >
            {projectOptions.map((proj) => (
              <option key={proj} value={proj}>
                {proj}
              </option>
            ))}
          </select>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: "8px", flexWrap: "wrap" }}>
          <input
            type="search"
            aria-label="Filter records"
            data-testid="board-search-input"
            className="fd-input"
            placeholder="Filter by #id, title, or area..."
            style={{ fontSize: "12px", padding: "5px 10px", minWidth: "210px" }}
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
          />
          <button
            type="button"
            data-testid="board-new-record-btn"
            className={`fd-btn${showCreateForm ? " fd-btn-active" : " fd-btn-primary"}`}
            onClick={() => setShowCreateForm((prev) => !prev)}
          >
            + New Record
          </button>
          <button
            type="button"
            data-testid="board-refresh-btn"
            className="fd-btn"
            onClick={() => void loadRecords(project)}
            disabled={loading}
          >
            {loading ? "Refreshing..." : "Refresh"}
          </button>
        </div>
      </div>

      {/* Velocity & Completion Metrics Strip */}
      <div
        data-testid="board-metrics-strip"
        style={{
          padding: "10px 14px",
          borderBottom: "1px solid var(--border-subtle, #1e293b)",
          background: "var(--bg-elevated, rgba(15, 23, 42, 0.55))",
          display: "flex",
          flexDirection: "column",
          gap: "8px",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: "12px",
            flexWrap: "wrap",
            fontSize: "12px",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: "12px", flexWrap: "wrap" }}>
            <span className="fd-badge">
              Todo: <strong>{effectiveSummary.todo_count}</strong>
            </span>
            <span className="fd-badge">
              Doing: <strong>{effectiveSummary.doing_count}</strong>
            </span>
            <span className="fd-badge">
              Done: <strong>{effectiveSummary.done_count}</strong>
            </span>
            <span className="fd-badge">
              Total: <strong>{effectiveSummary.total_count}</strong>
            </span>
            <span className="fd-badge">
              Completion: <strong>{Math.round(effectiveSummary.completion_pct)}%</strong>
            </span>
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: "12px", flexWrap: "wrap" }}>
            <span className="fd-badge">
              Open Findings: <strong>{effectiveSummary.open_findings_count}</strong>
            </span>
            <span className="fd-badge">
              Attempts: <strong>{effectiveSummary.attempts_count}</strong>
            </span>
            <span className="fd-badge fd-mono">
              Cost: <strong>${(effectiveSummary.total_cost_usd ?? 0).toFixed(2)}</strong>
            </span>
          </div>
        </div>

        {/* Visual Stacked Progress Bar */}
        <div
          data-testid="board-completion-bar"
          role="progressbar"
          aria-valuenow={Math.round(effectiveSummary.completion_pct)}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-label={`Project completion ${Math.round(effectiveSummary.completion_pct)}%`}
          style={{
            height: "6px",
            borderRadius: "999px",
            background: "rgba(148, 163, 184, 0.16)",
            overflow: "hidden",
            display: "flex",
            width: "100%",
          }}
        >
          <div
            style={{
              width: `${donePct}%`,
              background: "var(--status-alive, #22c55e)",
              transition: "width 0.2s ease",
            }}
            title={`Done: ${effectiveSummary.done_count}`}
          />
          <div
            style={{
              width: `${doingPct}%`,
              background: "var(--accent, #38bdf8)",
              transition: "width 0.2s ease",
            }}
            title={`Doing: ${effectiveSummary.doing_count}`}
          />
          <div
            style={{
              width: `${todoPct}%`,
              background: "rgba(148, 163, 184, 0.45)",
              transition: "width 0.2s ease",
            }}
            title={`Todo: ${effectiveSummary.todo_count}`}
          />
        </div>
      </div>

      {/* Create Record Form */}
      {showCreateForm && (
        <form
          data-testid="create-record-form"
          onSubmit={handleCreateRecord}
          style={{
            padding: "12px 14px",
            borderBottom: "1px solid var(--border-subtle, #1e293b)",
            background: "rgba(30, 41, 59, 0.45)",
            display: "flex",
            alignItems: "center",
            gap: "8px",
            flexWrap: "wrap",
          }}
        >
          <input
            type="text"
            aria-label="New Record Title"
            data-testid="new-record-title-input"
            className="fd-input"
            placeholder="Record title (required)..."
            style={{ flex: "1 1 240px", fontSize: "12px", padding: "6px 10px" }}
            value={newTitle}
            onChange={(e) => setNewTitle(e.target.value)}
            required
          />
          <input
            type="text"
            aria-label="New Record Area"
            data-testid="new-record-area-input"
            className="fd-input"
            placeholder="Area (optional, e.g. ui, hub)..."
            style={{ width: "170px", fontSize: "12px", padding: "6px 10px" }}
            value={newArea}
            onChange={(e) => setNewArea(e.target.value)}
          />
          <select
            aria-label="Initial State"
            data-testid="new-record-state-select"
            className="fd-select"
            style={{ fontSize: "12px", padding: "6px 10px" }}
            value={newState}
            onChange={(e) => setNewState(e.target.value as RecordState)}
          >
            <option value="todo">todo</option>
            <option value="doing">doing</option>
            <option value="done">done</option>
          </select>
          <button
            type="submit"
            data-testid="submit-new-record-btn"
            className="fd-btn fd-btn-primary"
            disabled={creating || !newTitle.trim()}
          >
            {creating ? "Creating..." : "Create Record"}
          </button>
        </form>
      )}

      {error && (
        <div
          role="alert"
          data-testid="records-board-error"
          style={{
            margin: "8px 14px 0",
            padding: "8px 12px",
            borderRadius: "6px",
            background: "rgba(239, 68, 68, 0.14)",
            border: "1px solid rgba(239, 68, 68, 0.35)",
            color: "#fca5a5",
            fontSize: "12px",
          }}
        >
          {error}
        </div>
      )}

      {/* 3-Column Kanban Swimlanes */}
      <div
        style={{
          flex: 1,
          display: "grid",
          gridTemplateColumns: "repeat(3, minmax(240px, 1fr))",
          gap: "12px",
          padding: "12px 14px",
          overflow: "auto",
          alignItems: "stretch",
        }}
      >
        {SWIMLANES.map((lane) => {
          const laneRecords = filteredRecords.filter(
            (r) => r.state === lane.state,
          );
          const isDragOver = dragOverLane === lane.state;

          return (
            <div
              key={lane.state}
              data-testid={`swimlane-${lane.state}`}
              onDragOver={(e) => {
                e.preventDefault();
                if (dragOverLane !== lane.state) {
                  setDragOverLane(lane.state);
                }
              }}
              onDragLeave={() => {
                if (dragOverLane === lane.state) {
                  setDragOverLane(null);
                }
              }}
              onDrop={(e) => handleDropOnLane(e, lane.state)}
              style={{
                display: "flex",
                flexDirection: "column",
                borderRadius: "8px",
                border: isDragOver
                  ? "1px dashed var(--accent, #38bdf8)"
                  : "1px solid var(--border-subtle, #1e293b)",
                background: isDragOver
                  ? "rgba(56, 189, 248, 0.06)"
                  : "var(--bg-surface, rgba(15, 23, 42, 0.4))",
                overflow: "hidden",
                minHeight: "220px",
              }}
            >
              {/* Swimlane Column Header */}
              <div
                style={{
                  padding: "10px 12px",
                  borderBottom: "1px solid var(--border-subtle, #1e293b)",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: "8px",
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                  <span
                    style={{
                      width: "8px",
                      height: "8px",
                      borderRadius: "50%",
                      background: lane.accentColor,
                      display: "inline-block",
                    }}
                  />
                  <strong style={{ fontSize: "13px" }}>{lane.label}</strong>
                </div>
                <span className="fd-badge fd-mono" style={{ fontSize: "11px" }}>
                  {laneRecords.length}
                </span>
              </div>

              {/* Swimlane Cards */}
              <div
                style={{
                  flex: 1,
                  padding: "10px",
                  display: "flex",
                  flexDirection: "column",
                  gap: "8px",
                  overflowY: "auto",
                }}
              >
                {laneRecords.length === 0 ? (
                  <div
                    style={{
                      padding: "18px 10px",
                      textAlign: "center",
                      fontSize: "12px",
                      color: "var(--text-muted)",
                    }}
                  >
                    No {lane.label.toLowerCase()} records
                  </div>
                ) : (
                  laneRecords.map((record) => {
                    const otherStates = (
                      ["todo", "doing", "done"] as RecordState[]
                    ).filter((s) => s !== record.state);
                    const handleLabel = record.display_id.startsWith("#")
                      ? record.display_id
                      : `#${record.display_id}`;

                    return (
                      <div
                        key={record.id || record.display_id}
                        data-testid={`record-card-${record.display_id}`}
                        draggable={true}
                        onDragStart={(e) => {
                          setDraggedRecordId(record.display_id);
                          try {
                            e.dataTransfer?.setData(
                              "text/plain",
                              record.display_id,
                            );
                          } catch {
                            // Ignore in synthetic test environments
                          }
                        }}
                        onDragEnd={() => {
                          setDraggedRecordId(null);
                          setDragOverLane(null);
                        }}
                        style={{
                          padding: "10px 12px",
                          borderRadius: "6px",
                          border: "1px solid var(--border-subtle, #1e293b)",
                          background: "var(--bg-elevated, rgba(30, 41, 59, 0.65))",
                          display: "flex",
                          flexDirection: "column",
                          gap: "8px",
                          cursor: "grab",
                        }}
                      >
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            gap: "6px",
                            flexWrap: "wrap",
                          }}
                        >
                          <span
                            className="fd-badge fd-mono"
                            style={{ fontWeight: 700, fontSize: "11px" }}
                          >
                            {handleLabel}
                          </span>

                          <div
                            style={{
                              display: "flex",
                              alignItems: "center",
                              gap: "4px",
                              flexWrap: "wrap",
                            }}
                          >
                            {record.area && (
                              <span
                                className="fd-badge"
                                data-testid={`record-area-${record.display_id}`}
                                style={{ fontSize: "10px" }}
                              >
                                {record.area}
                              </span>
                            )}
                            {record.tier && (
                              <span
                                className="fd-badge fd-mono"
                                style={{ fontSize: "10px" }}
                              >
                                {record.tier}
                              </span>
                            )}
                            {(record.findings_open ?? 0) > 0 && (
                              <span
                                className="fd-badge"
                                data-testid={`record-findings-${record.display_id}`}
                                style={{
                                  fontSize: "10px",
                                  background: "rgba(245, 158, 11, 0.16)",
                                  border: "1px solid rgba(245, 158, 11, 0.4)",
                                  color: "#fcd34d",
                                }}
                              >
                                {record.findings_open} open finding
                                {record.findings_open === 1 ? "" : "s"}
                              </span>
                            )}
                          </div>
                        </div>

                        <div
                          style={{
                            fontSize: "13px",
                            fontWeight: 500,
                            lineHeight: 1.4,
                            wordBreak: "break-word",
                          }}
                        >
                          {record.title}
                        </div>

                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "flex-end",
                            gap: "6px",
                            flexWrap: "wrap",
                          }}
                        >
                          {otherStates.map((targetState) => (
                            <button
                              key={targetState}
                              type="button"
                              data-testid={`move-record-${record.display_id}-${targetState}`}
                              className="fd-btn"
                              style={{ fontSize: "11px", padding: "3px 8px" }}
                              disabled={movingId === record.display_id}
                              onClick={() =>
                                void handleMoveRecord(record, targetState)
                              }
                            >
                              → {targetState}
                            </button>
                          ))}
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            </div>
          );
        })}
      </div>
    </section>
  );
};

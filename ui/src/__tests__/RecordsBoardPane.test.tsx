import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { HubApiClient } from "../api/client";
import type { HubWsClient } from "../api/ws";
import { RecordsBoardPane } from "../components/RecordsBoardPane";
import { setActiveCurrency } from "../lib/currency";
import type {
  ProjectRecordItem,
  RecordState,
  RecordsListResponse,
} from "../types/api";

function makeInitialRecords(): ProjectRecordItem[] {
  return [
    {
      id: "urn:fl:record:01a1-rec-1",
      handle: 1,
      display_id: "1",
      project: "ferroloop",
      title: "Project Records & Kanban Board UI",
      state: "todo",
      area: "ui",
      tier: "local",
      findings_open: 2,
      findings_total: 3,
    },
    {
      id: "urn:fl:record:01a1-rec-2",
      handle: 2,
      display_id: "2",
      project: "ferroloop",
      title: "Contextual Help Drawer & View Documentation",
      state: "doing",
      area: "cockpit",
      tier: "local",
      findings_open: 0,
      findings_total: 1,
    },
    {
      id: "urn:fl:record:01a1-rec-3",
      handle: 3,
      display_id: "3",
      project: "ferroloop",
      title: "3-Tier Navigation Hierarchy",
      state: "done",
      area: "header",
      tier: "local",
      findings_open: 0,
      findings_total: 0,
    },
  ];
}

function makeBoardResponse(
  records: ProjectRecordItem[] = makeInitialRecords(),
  project = "ferroloop",
): RecordsListResponse {
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
    project,
    available_projects: ["ferroloop", "sonora", "prosodia"],
    records,
    summary: {
      todo_count,
      doing_count,
      done_count,
      total_count,
      completion_pct,
      open_findings_count,
      attempts_count: 14,
      total_cost_usd: 1.85,
    },
  };
}

describe("Wave B UI — Record 1: RecordsBoardPane Kanban Swimlanes & Velocity Strip", () => {
  it("loads project records into Todo, Doing, and Done swimlanes and renders the velocity metrics strip", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const listSpy = vi
      .spyOn(client, "listRecords")
      .mockResolvedValue(makeBoardResponse());

    render(<RecordsBoardPane selectedProject="FerroLoop" apiClient={client} />);

    await waitFor(() => {
      expect(listSpy).toHaveBeenCalledWith("ferroloop");
    });

    const boardPane = screen.getByTestId("records-board-pane");
    expect(boardPane).toBeInTheDocument();
    expect(boardPane.style.overflowY).toBe("auto");
    expect(boardPane.style.overflowX).toBe("hidden");
    expect(screen.getByTestId("board-project-select")).toHaveValue("ferroloop");

    const projectActionsRow = screen.getByTestId("board-project-actions-row");
    expect(
      within(projectActionsRow).getByTestId("board-project-select"),
    ).toBeInTheDocument();
    const actionsGroup = within(projectActionsRow).getByTestId(
      "board-actions-group",
    );
    const newRecordBtn = within(actionsGroup).getByTestId("board-new-record-btn");
    const refreshBtn = within(actionsGroup).getByTestId("board-refresh-btn");
    expect(newRecordBtn).toHaveAttribute("aria-label", "+ New Record");
    expect(newRecordBtn.querySelector("svg")).not.toBeNull();
    expect(refreshBtn).toHaveAttribute("aria-label", "Refresh");
    expect(refreshBtn.querySelector("svg")).not.toBeNull();

    const swimlanesGrid = screen.getByTestId("board-swimlanes-grid");
    expect(swimlanesGrid.style.overflow).toBe("");
    const todoSwimlane = screen.getByTestId("swimlane-todo");
    expect(todoSwimlane.style.overflow).toBe("");
    expect(todoSwimlane.style.minHeight).toBe("");
    const todoCardsList = screen.getByTestId("swimlane-cards-todo");
    expect(todoCardsList.style.overflowY).toBe("");

    const metricsStrip = screen.getByTestId("board-metrics-strip");
    expect(metricsStrip).toHaveTextContent(/Todo:\s*1/i);
    expect(metricsStrip).toHaveTextContent(/Doing:\s*1/i);
    expect(metricsStrip).toHaveTextContent(/Done:\s*1/i);
    expect(metricsStrip).toHaveTextContent(/Total:\s*3/i);
    expect(metricsStrip).toHaveTextContent(/Completion:\s*33%/i);
    expect(metricsStrip).toHaveTextContent(/Open Findings:\s*2/i);
    expect(metricsStrip).toHaveTextContent(/Attempts:\s*14/i);
    expect(metricsStrip).toHaveTextContent(/Cost:\s*\$1\.85/i);

    const countsGroup = screen.getByTestId("board-metrics-counts-group");
    expect(countsGroup).toHaveTextContent(/Todo:\s*1/i);
    expect(countsGroup).toHaveTextContent(/Doing:\s*1/i);
    expect(countsGroup).toHaveTextContent(/Done:\s*1/i);
    expect(countsGroup).toHaveTextContent(/Total:\s*3/i);
    expect(countsGroup.style.display).toBe("grid");
    expect(countsGroup.style.gridTemplateColumns).toBe(
      "repeat(2, minmax(0, 1fr))",
    );
    expect(countsGroup.style.gridColumn).toBe("1 / -1");

    const completionFindingsGroup = screen.getByTestId(
      "board-metrics-completion-findings-group",
    );
    expect(completionFindingsGroup).toHaveTextContent(/Completion:\s*33%/i);
    expect(completionFindingsGroup).toHaveTextContent(/Open Findings:\s*2/i);
    expect(completionFindingsGroup.style.display).toBe("grid");
    expect(completionFindingsGroup.style.gridTemplateColumns).toBe(
      "repeat(4, minmax(0, 1fr))",
    );
    expect(completionFindingsGroup.style.gridColumn).toBe("1 / -1");
    Array.from(completionFindingsGroup.children).forEach((badge) => {
      expect((badge as HTMLElement).style.gridColumn).toBe("span 2");
    });

    const attemptsCostGroup = screen.getByTestId(
      "board-metrics-attempts-cost-group",
    );
    expect(attemptsCostGroup).toHaveTextContent(/Attempts:\s*14/i);
    expect(attemptsCostGroup).toHaveTextContent(/Cost:\s*\$1\.85/i);
    expect(attemptsCostGroup.style.display).toBe("grid");
    expect(attemptsCostGroup.style.gridTemplateColumns).toBe(
      "repeat(4, minmax(0, 1fr))",
    );
    expect(attemptsCostGroup.style.gridColumn).toBe("1 / -1");
    Array.from(attemptsCostGroup.children).forEach((badge) => {
      expect((badge as HTMLElement).style.gridColumn).toBe("span 2");
    });

    act(() => {
      setActiveCurrency("EUR");
    });
    expect(attemptsCostGroup).toHaveTextContent(/Cost:\s*€1\.70/i);
    act(() => {
      setActiveCurrency("USD");
    });
    expect(screen.getByTestId("board-completion-bar")).toHaveAttribute(
      "aria-valuenow",
      "33",
    );

    const todoLane = screen.getByTestId("swimlane-todo");
    const doingLane = screen.getByTestId("swimlane-doing");
    const doneLane = screen.getByTestId("swimlane-done");

    const card1 = within(todoLane).getByTestId("record-card-1");
    expect(card1).toHaveTextContent("#1");
    expect(card1).toHaveTextContent("Project Records & Kanban Board UI");
    expect(card1).toHaveTextContent("ui");
    expect(card1).toHaveTextContent("2 open findings");

    const card2 = within(doingLane).getByTestId("record-card-2");
    expect(card2).toHaveTextContent("#2");
    expect(card2).toHaveTextContent(
      "Contextual Help Drawer & View Documentation",
    );

    const card3 = within(doneLane).getByTestId("record-card-3");
    expect(card3).toHaveTextContent("#3");
    expect(card3).toHaveTextContent("3-Tier Navigation Hierarchy");
  });

  it("filters cards across swimlanes when typing into board-search-input and reloads when changing project", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const listSpy = vi
      .spyOn(client, "listRecords")
      .mockImplementation(async (proj?: string) =>
        makeBoardResponse(makeInitialRecords(), proj ?? "ferroloop"),
      );

    render(<RecordsBoardPane apiClient={client} />);

    await waitFor(() => {
      expect(screen.getByTestId("record-card-1")).toBeInTheDocument();
    });

    const searchInput = screen.getByTestId("board-search-input");
    fireEvent.change(searchInput, { target: { value: "Contextual Help" } });

    expect(screen.queryByTestId("record-card-1")).not.toBeInTheDocument();
    expect(screen.getByTestId("record-card-2")).toBeInTheDocument();
    expect(screen.queryByTestId("record-card-3")).not.toBeInTheDocument();

    fireEvent.change(searchInput, { target: { value: "" } });
    expect(screen.getByTestId("record-card-1")).toBeInTheDocument();
    expect(screen.getByTestId("record-card-3")).toBeInTheDocument();

    fireEvent.change(screen.getByTestId("board-project-select"), {
      target: { value: "sonora" },
    });

    await waitFor(() => {
      expect(listSpy).toHaveBeenCalledWith("sonora");
    });
  });

  it("opens create-record-form via board-new-record-btn and creates a new record", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    vi.spyOn(client, "listRecords").mockResolvedValue(makeBoardResponse());

    const createdRecord: ProjectRecordItem = {
      id: "urn:fl:record:01a1-rec-4",
      handle: 4,
      display_id: "4",
      project: "ferroloop",
      title: "Artifact Dead-Path Pruning",
      state: "doing",
      area: "artifacts",
      tier: "local",
      findings_open: 0,
      findings_total: 0,
    };
    const createSpy = vi
      .spyOn(client, "createRecord")
      .mockResolvedValue({ record: createdRecord });

    render(<RecordsBoardPane apiClient={client} />);

    await waitFor(() => {
      expect(screen.getByTestId("record-card-1")).toBeInTheDocument();
    });

    expect(screen.queryByTestId("create-record-form")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("board-new-record-btn"));

    const form = screen.getByTestId("create-record-form");
    expect(form).toBeInTheDocument();

    fireEvent.change(screen.getByTestId("new-record-title-input"), {
      target: { value: "Artifact Dead-Path Pruning" },
    });
    fireEvent.change(screen.getByTestId("new-record-area-input"), {
      target: { value: "artifacts" },
    });
    fireEvent.change(screen.getByTestId("new-record-state-select"), {
      target: { value: "doing" },
    });

    fireEvent.click(screen.getByTestId("submit-new-record-btn"));

    await waitFor(() => {
      expect(createSpy).toHaveBeenCalledWith({
        project: "ferroloop",
        title: "Artifact Dead-Path Pruning",
        area: "artifacts",
        state: "doing",
      });
    });

    await waitFor(() => {
      expect(
        within(screen.getByTestId("swimlane-doing")).getByTestId(
          "record-card-4",
        ),
      ).toBeInTheDocument();
    });
    expect(screen.queryByTestId("create-record-form")).not.toBeInTheDocument();
    expect(screen.getByTestId("board-metrics-strip")).toHaveTextContent(
      /Total:\s*4/i,
    );
  });

  it("moves a record across swimlanes via quick-move buttons and HTML5 drag-and-drop", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const records = makeInitialRecords();
    vi.spyOn(client, "listRecords").mockResolvedValue(
      makeBoardResponse(records),
    );

    const moveSpy = vi
      .spyOn(client, "moveRecord")
      .mockImplementation(
        async (id: string, to: RecordState, project?: string) => {
          const found =
            records.find((r) => r.id === id || r.display_id === id) ??
            records[0];
          return {
            ok: true,
            outcome: "ungated",
            record: {
              ...found,
              project: project ?? found.project,
              state: to,
            },
          };
        },
      );

    render(<RecordsBoardPane apiClient={client} />);

    await waitFor(() => {
      expect(
        within(screen.getByTestId("swimlane-todo")).getByTestId("record-card-1"),
      ).toBeInTheDocument();
    });

    // 1. Quick-move record #1 from Todo -> Doing
    const quickMoveDoingBtn = screen.getByTestId("move-record-1-doing");
    fireEvent.click(quickMoveDoingBtn);

    await waitFor(() => {
      expect(moveSpy).toHaveBeenCalledWith(
        "urn:fl:record:01a1-rec-1",
        "doing",
        "ferroloop",
      );
    });

    await waitFor(() => {
      expect(
        within(screen.getByTestId("swimlane-doing")).getByTestId(
          "record-card-1",
        ),
      ).toBeInTheDocument();
    });

    // 2. HTML5 drag-and-drop record #1 from Doing -> Done
    const card1InDoing = within(screen.getByTestId("swimlane-doing")).getByTestId(
      "record-card-1",
    );
    const doneLane = screen.getByTestId("swimlane-done");

    const dataTransfer = {
      data: {} as Record<string, string>,
      setData(type: string, val: string) {
        this.data[type] = val;
      },
      getData(type: string) {
        return this.data[type] ?? "";
      },
    };

    fireEvent.dragStart(card1InDoing, { dataTransfer });
    fireEvent.dragOver(doneLane, { dataTransfer });
    fireEvent.drop(doneLane, { dataTransfer });

    await waitFor(() => {
      expect(moveSpy).toHaveBeenCalledWith(
        "urn:fl:record:01a1-rec-1",
        "done",
        "ferroloop",
      );
    });

    await waitFor(() => {
      expect(
        within(screen.getByTestId("swimlane-done")).getByTestId("record-card-1"),
      ).toBeInTheDocument();
    });

    // Completion should now be 2 of 3 Done = 67%
    expect(screen.getByTestId("board-metrics-strip")).toHaveTextContent(
      /Completion:\s*67%/i,
    );
  });

  it("integrates Records & Board into the Project-tier header tab bar in App and opens targeted contextual help", async () => {
    const mockFetch = vi.fn(async (input: RequestInfo | URL) => {
      const url = typeof input === "string" ? input : input.toString();
      if (url.includes("/api/v1/auth/session")) {
        return new Response(
          JSON.stringify({
            authenticated: true,
            authority: { tier: "owner", login: "lmcfarlin" },
          }),
          { status: 200 },
        );
      }
      if (url.includes("/api/v1/records")) {
        return new Response(JSON.stringify(makeBoardResponse()), {
          status: 200,
        });
      }
      return new Response(JSON.stringify({}), { status: 200 });
    });

    const client = new HubApiClient({
      baseUrl: "http://localhost:9000",
      fetchImpl: mockFetch as unknown as typeof fetch,
    });
    const fakeWs = {
      connect: vi.fn(),
      disconnect: vi.fn(),
      setToken: vi.fn(),
      onStatusChange: vi.fn(() => () => {}),
      subscribe: vi.fn(() => () => {}),
      send: vi.fn(),
    } as unknown as HubWsClient;

    render(<App apiClient={client} wsClient={fakeWs} />);

    await screen.findByTestId("tier-agent-nav");
    fireEvent.click(screen.getByTestId("split-view-toggle"));
    const rightHeader = screen.getByTestId("right-panel-header");
    const boardTabBtn = within(rightHeader).getByTestId("right-panel-tab-board");
    fireEvent.click(boardTabBtn);

    expect(
      await screen.findByTestId("records-board-pane"),
    ).toBeInTheDocument();
    expect(screen.getByTestId("active-context-tier-badge")).toHaveTextContent(
      "Scope: Project · FerroLoop",
    );

    // Click '?' contextual help button while on the 'board' tab
    fireEvent.click(screen.getByTestId("context-help-btn"));
    expect(screen.getByTestId("contextual-help-drawer")).toBeInTheDocument();
    expect(screen.getByTestId("help-active-topic-content")).toHaveAttribute(
      "data-topic",
      "board",
    );
  });
});

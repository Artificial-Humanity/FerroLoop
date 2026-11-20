import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { DiffPane, DiffViewer, formatDiffReviewPrompt } from "../components/DiffViewer";
import { GitPrPane } from "../components/GitPrPane";
import type { PullRequestItem, WorktreeDiff, WorktreeSummary } from "../types/api";

const SAMPLE_DIFF = [
  "diff --git a/crates/hub/src/api.rs b/crates/hub/src/api.rs",
  "index 1111111..2222222 100644",
  "--- a/crates/hub/src/api.rs",
  "+++ b/crates/hub/src/api.rs",
  "@@ -10,4 +10,5 @@ pub fn build_router() -> Router {",
  "     let app = Router::new()",
  "-        .route(\"/health\", get(old_health))",
  "+        .route(\"/api/v1/health\", get(health_check))",
  "+        .route(\"/api/v1/worktrees\", get(list_worktrees))",
  "         .with_state(state);",
  "diff --git a/ui/src/App.tsx b/ui/src/App.tsx",
  "index 3333333..4444444 100644",
  "--- a/ui/src/App.tsx",
  "+++ b/ui/src/App.tsx",
  "@@ -1,3 +1,4 @@",
  " import React from \"react\";",
  "+import { GitPrPane } from \"./components/GitPrPane\";",
  " export function App() {",
].join("\n");

const SAMPLE_WORKTREES: WorktreeSummary[] = [
  {
    id: "FerroLoop",
    path: "/home/lmcfarlin/Projects/Artificial-Humanity/FerroLoop",
    head: "776973e9a1b2c3d4",
    branch: "main",
    dirty_files: [],
    is_clean: true,
  },
  {
    id: "fl-ferrodeck-ui",
    path: "/home/lmcfarlin/worktrees/fl-ferrodeck-ui",
    head: "cfc91128f0e1d2c3",
    branch: "wesley/ferrodeck-ui",
    dirty_files: ["M crates/hub/src/api.rs", "A ui/src/App.tsx"],
    is_clean: false,
  },
];

const SAMPLE_WORKTREE_DIFF: WorktreeDiff = {
  id: "fl-ferrodeck-ui",
  path: "/home/lmcfarlin/worktrees/fl-ferrodeck-ui",
  branch: "wesley/ferrodeck-ui",
  head: "cfc91128f0e1d2c3",
  files: [
    { status: "M", path: "crates/hub/src/api.rs" },
    { status: "A", path: "ui/src/App.tsx" },
  ],
  diff: SAMPLE_DIFF,
};

const SAMPLE_PRS: PullRequestItem[] = [
  {
    number: 42,
    title: "feat(hub): ship FerroDeck UI worktree diff & PR governance",
    author: "wesley",
    project: "FerroLoop",
    branch: "wesley/ferrodeck-ui",
    head_sha: "cfc91128f0e1d2c3",
    status: "open",
    required_tier: "lead",
    reviews: [
      {
        id: 1,
        author_login: "ferroloop[bot]",
        author_id: 341521384,
        state: "APPROVED",
        body: "LGTM — verified against live hub routes.",
      },
    ],
  },
  {
    number: 108,
    title: "feat(site): update hero typography",
    author: "wesley",
    project: "ArtificialHumanity.io",
    branch: "wesley/hero-type",
    head_sha: "9988776655443322",
    status: "open",
  },
];

describe("DiffViewer", () => {
  it("parses unified git diff hunks and toggles between Unified and Split modes", () => {
    render(<DiffViewer diff={SAMPLE_DIFF} />);

    expect(screen.getByText("crates/hub/src/api.rs")).toBeInTheDocument();
    expect(screen.getByText("ui/src/App.tsx")).toBeInTheDocument();
    expect(
      screen.getByText(/\+\s+\.route\("\/api\/v1\/health", get\(health_check\)\)/),
    ).toBeInTheDocument();
    expect(screen.getByText(/-\s+\.route\("\/health", get\(old_health\)\)/)).toBeInTheDocument();

    const splitBtn = screen.getByRole("button", { name: /Side-by-Side/i });
    const unifiedBtn = screen.getByRole("button", { name: /Unified/i });

    expect(unifiedBtn).toHaveAttribute("aria-pressed", "true");
    expect(splitBtn).toHaveAttribute("aria-pressed", "false");

    fireEvent.click(splitBtn);
    expect(splitBtn).toHaveAttribute("aria-pressed", "true");
    expect(unifiedBtn).toHaveAttribute("aria-pressed", "false");
    expect(screen.getAllByTestId("diff-split-table").length).toBe(2);
  });
});

describe("GitPrPane", () => {
  it("keeps the icon controls and phone select in sync when switching views", () => {
    render(<GitPrPane worktrees={SAMPLE_WORKTREES} prs={SAMPLE_PRS} />);
    const worktrees = screen.getByRole("button", { name: "Worktrees & Diffs (2)" });
    const prs = screen.getByRole("button", { name: "Pull Requests & Governance (2)" });
    const select = screen.getByTestId("git-subview-select");
    expect(worktrees).toHaveAttribute("aria-pressed", "true");
    expect(prs).toHaveAttribute("aria-pressed", "false");
    fireEvent.click(prs);
    expect(prs).toHaveAttribute("aria-pressed", "true");
    expect(select).toHaveValue("prs");
    expect(screen.getByTestId("git-pane-title")).toHaveTextContent("Pull Requests & Governance");
    fireEvent.change(select, { target: { value: "worktrees" } });
    expect(worktrees).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("git-pane-title")).toHaveTextContent("Worktrees & Diffs");
  });
  it("renders WorktreeSummary list, selects a worktree, and filters diff by modified file", () => {
    const onSelectWorktree = vi.fn();
    const onRefreshWorktrees = vi.fn();

    render(
      <GitPrPane
        worktrees={SAMPLE_WORKTREES}
        selectedWorktreeId="fl-ferrodeck-ui"
        worktreeDiff={SAMPLE_WORKTREE_DIFF}
        prs={SAMPLE_PRS}
        onSelectWorktree={onSelectWorktree}
        onRefreshWorktrees={onRefreshWorktrees}
        onReviewPr={vi.fn()}
        onMergePr={vi.fn()}
      />,
    );

    // Both worktrees appear with status badges
    expect(screen.getByText("FerroLoop")).toBeInTheDocument();
    expect(screen.getByText("Clean")).toBeInTheDocument();
    expect(screen.getByText("fl-ferrodeck-ui")).toBeInTheDocument();
    expect(screen.getByText("2 modified")).toBeInTheDocument();

    // Clicking FerroLoop triggers onSelectWorktree
    fireEvent.click(screen.getByTestId("worktree-item-FerroLoop"));
    expect(onSelectWorktree).toHaveBeenCalledWith("FerroLoop");

    // Modified files list shows M and A badges
    const fileFilterBtn = screen.getByTestId("modified-file-ui/src/App.tsx");
    expect(fileFilterBtn).toBeInTheDocument();

    // Clicking a file filters DiffViewer to that file
    fireEvent.click(fileFilterBtn);
    expect(screen.queryByTestId("diff-file-crates/hub/src/api.rs")).toBeNull();
    expect(screen.getByTestId("diff-file-ui/src/App.tsx")).toBeInTheDocument();

    // Clicking All files resets filter
    const allFilesBtn = screen.getByRole("button", { name: /All files/i });
    fireEvent.click(allFilesBtn);
    expect(screen.getByTestId("diff-file-crates/hub/src/api.rs")).toBeInTheDocument();
    expect(screen.getByTestId("diff-file-ui/src/App.tsx")).toBeInTheDocument();
  });

  it("switches to Pull Requests & Governance, renders PullRequestItem rows, and submits review and merge actions", async () => {
    const onReviewPr = vi.fn(async () => {});
    const onMergePr = vi.fn(async () => {});

    render(
      <GitPrPane
        worktrees={SAMPLE_WORKTREES}
        selectedWorktreeId="fl-ferrodeck-ui"
        worktreeDiff={SAMPLE_WORKTREE_DIFF}
        prs={SAMPLE_PRS}
        onSelectWorktree={vi.fn()}
        onRefreshWorktrees={vi.fn()}
        onReviewPr={onReviewPr}
        onMergePr={onMergePr}
      />,
    );

    // Switch sub-navigation to Pull Requests & Governance via dropdown selector
    expect(screen.getByTestId("git-pane-title")).toHaveTextContent(
      "Worktrees & Diffs",
    );
    const subviewSelect = screen.getByTestId("git-subview-select");
    fireEvent.change(subviewSelect, { target: { value: "prs" } });
    expect(screen.getByTestId("git-pane-title")).toHaveTextContent(
      "Pull Requests & Governance",
    );

    expect(
      screen.getByText(/feat\(hub\): ship FerroDeck UI worktree diff & PR governance/i),
    ).toBeInTheDocument();
    expect(screen.getByText("#42")).toBeInTheDocument();
    expect(screen.getByText("lead")).toBeInTheDocument();
    // ArtificialHumanity.io infers owner reviewer tier
    expect(screen.getByText("owner")).toBeInTheDocument();

    // Quick action buttons on PR #42
    const approveBtn = screen.getByTestId("pr-approve-42");
    fireEvent.click(approveBtn);

    await waitFor(() => {
      expect(onReviewPr).toHaveBeenCalledWith(
        42,
        expect.objectContaining({
          verdict: "approve",
          approve: true,
          project: "FerroLoop",
          head_sha: "cfc91128f0e1d2c3",
        }),
      );
    });

    const mergeBtn = screen.getByTestId("pr-merge-42");
    fireEvent.click(mergeBtn);

    await waitFor(() => {
      expect(onMergePr).toHaveBeenCalledWith(
        42,
        expect.objectContaining({
          method: "squash",
          project: "FerroLoop",
          head_sha: "cfc91128f0e1d2c3",
        }),
      );
    });

    // Direct PR Governance Action Form
    const prNumInput = screen.getByLabelText(/PR Number/i) as HTMLInputElement;
    const projectInput = screen.getByLabelText(/Project Slug/i) as HTMLInputElement;
    const verdictSelect = screen.getByLabelText(/Review Verdict/i) as HTMLSelectElement;
    const bodyInput = screen.getByLabelText(/Review Body/i) as HTMLTextAreaElement;

    expect(projectInput.value).toBe("ferroloop");

    fireEvent.change(prNumInput, { target: { value: "77" } });
    fireEvent.change(projectInput, { target: { value: "sonora" } });
    fireEvent.change(verdictSelect, { target: { value: "changes_requested" } });
    fireEvent.change(bodyInput, { target: { value: "Please add unit tests for vocoder." } });

    const submitReviewBtn = screen.getByRole("button", { name: /Submit Review/i });
    fireEvent.click(submitReviewBtn);

    await waitFor(() => {
      expect(onReviewPr).toHaveBeenCalledWith(
        77,
        expect.objectContaining({
          verdict: "request_changes",
          request_changes: true,
          project: "sonora",
          body: "Please add unit tests for vocoder.",
        }),
      );
    });

    const mergeFormBtn = screen.getByRole("button", { name: /Merge Approved PR/i });
    fireEvent.click(mergeFormBtn);

    await waitFor(() => {
      expect(onMergePr).toHaveBeenCalledWith(
        77,
        expect.objectContaining({
          project: "sonora",
        }),
      );
    });
  });
});

describe("empty-state muting", () => {
  it("disables Unified and Side-by-Side when the diff has no files", () => {
    render(<DiffViewer diff="" />);

    expect(screen.getByRole("button", { name: /^Unified$/i })).toBeDisabled();
    expect(screen.getByRole("button", { name: /^Side-by-Side$/i })).toBeDisabled();
    expect(screen.getByTestId("diff-empty-state")).toBeInTheDocument();
  });

  it("greys and disables the PR governance form when no PRs are tracked", () => {
    render(
      <GitPrPane
        worktrees={SAMPLE_WORKTREES}
        selectedWorktreeId="fl-ferrodeck-ui"
        worktreeDiff={SAMPLE_WORKTREE_DIFF}
        prs={[]}
        onSelectWorktree={vi.fn()}
        onRefreshWorktrees={vi.fn()}
        onReviewPr={vi.fn()}
        onMergePr={vi.fn()}
      />,
    );

    const subviewSelect = screen.getByTestId(
      "git-subview-select",
    ) as HTMLSelectElement;
    expect(subviewSelect.value).toBe("worktrees");
    fireEvent.change(subviewSelect, { target: { value: "prs" } });

    const form = screen.getByTestId("pr-governance-form");
    expect(form).toHaveAttribute("data-prs-empty", "true");
    expect(form.style.opacity).toBe("0.45");
    expect(screen.getByLabelText(/PR Number/i)).toBeDisabled();
    expect(screen.getByRole("button", { name: /Submit Review/i })).toBeDisabled();
  });

  it("renders git-worktrees-project-badge under Worktrees & Diffs and hides it in Pull Requests & Governance", () => {
    render(
      <GitPrPane
        selectedProject="FerroLoop"
        worktrees={SAMPLE_WORKTREES}
        selectedWorktreeId="fl-ferrodeck-ui"
        worktreeDiff={SAMPLE_WORKTREE_DIFF}
        prs={[]}
      />,
    );

    expect(screen.getByTestId("git-pane-title")).toHaveTextContent(
      "Worktrees & Diffs",
    );
    const projectBadge = screen.getByTestId("git-worktrees-project-badge");
    expect(projectBadge).toHaveTextContent("FerroLoop");

    const subviewSelect = screen.getByTestId("git-subview-select");
    fireEvent.change(subviewSelect, { target: { value: "prs" } });
    expect(screen.queryByTestId("git-worktrees-project-badge")).toBeNull();
    expect(screen.getByTestId("git-pane-title")).toHaveTextContent(
      "Pull Requests & Governance",
    );

    fireEvent.change(subviewSelect, { target: { value: "worktrees" } });
    expect(screen.getByTestId("git-worktrees-project-badge")).toHaveTextContent(
      "FerroLoop",
    );
  });

  it("supports inline diff line commenting, pending review draft list (count badge, edit/delete, summary), and structured batch dispatch via sendSessionPrompt (immediate and busy-queued)", async () => {
    const promptRequests: Array<{ sessionId: string; prompt: string }> = [];
    let shouldQueue = false;

    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = (init?.method ?? "GET").toUpperCase();
      if (url.includes("/api/v1/sessions/") && url.endsWith("/prompt") && method === "POST") {
        const match = /\/api\/v1\/sessions\/([^/]+)\/prompt$/.exec(url);
        const body = JSON.parse(String(init?.body ?? "{}")) as { prompt?: string };
        promptRequests.push({
          sessionId: match?.[1] ?? "",
          prompt: body.prompt ?? "",
        });
        return new Response(
          JSON.stringify({
            session_id: match?.[1] ?? "sess-felix",
            status: shouldQueue ? "queued" : "dispatched",
            queued: shouldQueue,
            is_busy: shouldQueue,
            queue: [],
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

    render(
      <DiffPane
        diff={SAMPLE_DIFF}
        sessionId="sess-felix"
        agentName="felix"
        apiClient={apiClient}
      />,
    );

    // Click an added diff line in crates/hub/src/api.rs (new line 11: + .route("/api/v1/health", get(health_check)))
    fireEvent.click(
      screen.getByTestId("diff-line-comment-btn-crates/hub/src/api.rs-new-11"),
    );
    expect(screen.getByTestId("diff-inline-comment-editor")).toBeInTheDocument();

    const commentInput = screen.getByTestId(
      "diff-inline-comment-input",
    ) as HTMLTextAreaElement;
    fireEvent.change(commentInput, {
      target: { value: "Verify rate limiting on /api/v1/health endpoint." },
    });
    fireEvent.click(screen.getByTestId("diff-inline-comment-save-btn"));

    // Pending count badge and draft panel appear
    expect(screen.getByTestId("diff-review-count-badge")).toHaveTextContent(
      "1 comment",
    );
    expect(screen.getByTestId("diff-review-draft-panel")).toBeInTheDocument();
    expect(
      screen.getByTestId("diff-inline-comment-card-diff-comment-1"),
    ).toHaveTextContent("Verify rate limiting on /api/v1/health endpoint.");

    // Add a second comment on ui/src/App.tsx line 2
    fireEvent.click(
      screen.getByTestId("diff-line-row-ui/src/App.tsx-0-1"),
    );
    fireEvent.change(screen.getByTestId("diff-inline-comment-input"), {
      target: { value: "Keep imports sorted alphabetically." },
    });
    fireEvent.click(screen.getByTestId("diff-inline-comment-save-btn"));

    expect(screen.getByTestId("diff-review-count-badge")).toHaveTextContent(
      "2 comments",
    );
    expect(screen.getByTestId("diff-draft-panel-count")).toHaveTextContent(
      "2 pending comments",
    );

    // Edit the first comment from the draft list
    fireEvent.click(screen.getByTestId("diff-draft-edit-btn-diff-comment-1"));
    expect(
      (screen.getByTestId("diff-inline-comment-input") as HTMLTextAreaElement)
        .value,
    ).toBe("Verify rate limiting on /api/v1/health endpoint.");
    fireEvent.change(screen.getByTestId("diff-inline-comment-input"), {
      target: { value: "Ensure /api/v1/health returns 200 without auth." },
    });
    fireEvent.click(screen.getByTestId("diff-inline-comment-save-btn"));
    expect(
      screen.getByTestId("diff-draft-item-diff-comment-1"),
    ).toHaveTextContent("Ensure /api/v1/health returns 200 without auth.");

    // Delete the second comment
    fireEvent.click(screen.getByTestId("diff-draft-delete-btn-diff-comment-2"));
    expect(screen.getByTestId("diff-review-count-badge")).toHaveTextContent(
      "1 comment",
    );

    // Add overall review summary and submit review to agent
    fireEvent.change(screen.getByTestId("diff-review-summary-input"), {
      target: { value: "Please address the inline finding below before merging." },
    });
    fireEvent.click(screen.getByTestId("diff-review-submit-btn"));

    await waitFor(() => {
      expect(promptRequests.length).toBe(1);
    });
    expect(promptRequests[0].sessionId).toBe("sess-felix");
    expect(promptRequests[0].prompt).toContain("## Inline Diff Review");
    expect(promptRequests[0].prompt).toContain(
      "Please address the inline finding below before merging.",
    );
    expect(promptRequests[0].prompt).toContain("### `crates/hub/src/api.rs`");
    expect(promptRequests[0].prompt).toContain(
      "**Line 11 (+L11)** (`.route(\"/api/v1/health\", get(health_check))`): Ensure /api/v1/health returns 200 without auth.",
    );
    expect(screen.getByTestId("diff-review-status-banner")).toHaveTextContent(
      /Dispatched review batch \(1 comment\) to agent session/i,
    );
    expect(screen.queryByTestId("diff-review-count-badge")).toBeNull();

    // Verify busy-session queueing path in Side-by-Side mode
    shouldQueue = true;
    fireEvent.click(screen.getByRole("button", { name: /Side-by-Side/i }));
    fireEvent.click(
      screen.getByTestId("diff-split-row-crates/hub/src/api.rs-0-1"),
    );
    fireEvent.change(screen.getByTestId("diff-inline-comment-input"), {
      target: { value: "Remove legacy /health route handler once migrated." },
    });
    fireEvent.click(screen.getByTestId("diff-inline-comment-save-btn"));
    fireEvent.click(screen.getByTestId("diff-review-submit-btn"));

    await waitFor(() => {
      expect(promptRequests.length).toBe(2);
    });
    expect(screen.getByTestId("diff-review-status-banner")).toHaveTextContent(
      /Queued review batch \(1 comment\) for busy session/i,
    );

    // Also unit-test formatDiffReviewPrompt directly
    const formatted = formatDiffReviewPrompt(
      [
        {
          id: "c1",
          filePath: "src/lib.rs",
          hunkHeader: "@@ -1,3 +1,4 @@",
          oldLine: 4,
          newLine: null,
          side: "old",
          lineContent: "let x = 1;",
          comment: "Unused binding.",
        },
      ],
      "Quick nit.",
    );
    expect(formatted).toContain("Quick nit.");
    expect(formatted).toContain("- **Line 4 (-L4)** (`let x = 1;`): Unused binding.");
  });
});

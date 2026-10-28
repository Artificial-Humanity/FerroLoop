import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { DiffViewer } from "../components/DiffViewer";
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
        reviewer_login: "ferris",
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

    // Switch sub-navigation to Pull Requests & Governance
    const prTabBtn = screen.getByRole("button", { name: /Pull Requests & Governance/i });
    fireEvent.click(prTabBtn);

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

    const prTab = screen.getByRole("button", {
      name: /Pull Requests & Governance \(0\)/i,
    });
    expect(prTab.style.opacity).toBe("0.65");
    fireEvent.click(prTab);

    const form = screen.getByTestId("pr-governance-form");
    expect(form).toHaveAttribute("data-prs-empty", "true");
    expect(form.style.opacity).toBe("0.45");
    expect(screen.getByLabelText(/PR Number/i)).toBeDisabled();
    expect(screen.getByRole("button", { name: /Submit Review/i })).toBeDisabled();
  });
});

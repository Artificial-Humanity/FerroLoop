// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ACP_CONFORMANCE_OPERATIONS,
  DEFAULT_ACP_CONFORMANCE_MATRIX,
  FerroWatchPane,
} from "../components/FerroWatchPane";
import {
  ACP_CONFORMANCE_OPERATIONS as ReExportedAcpOps,
  DEFAULT_ACP_CONFORMANCE_MATRIX as ReExportedAcpMatrix,
  DEFAULT_VERSION_PROVENANCE,
  SettingsModal,
} from "../components/SettingsModal";
import {
  DEFAULT_ROUTED_CI_PR_EVENTS,
  TriageQueuePane,
} from "../components/TriageQueuePane";
import { DEFAULT_ROUTED_CI_PR_EVENTS as ReExportedCiPrEvents } from "../components/WireMailPane";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("Wave 5 Item 1 (#205 Phase A1): <ui_dir>/build-info.json & Live /api/v1/version Parity", () => {
  it("renders Code <-> UI Commit Parity badge and UI build-info.json metadata in SettingsModal", () => {
    render(
      <SettingsModal
        isOpen={true}
        onClose={() => {}}
        initialTab="products"
      />,
    );

    const parityBadge = screen.getByTestId("version-code-ui-parity-badge");
    expect(parityBadge.textContent).toContain("code_ui_agree=true");

    const uiBuildInfo = screen.getByTestId("receipt-ui-build-info");
    expect(uiBuildInfo.textContent).toContain("commit=b0888c6bc1ed");
    expect(uiBuildInfo.textContent).toContain("v=0.1.0");
    expect(uiBuildInfo.textContent).toContain("dirty=false");
  });

  it("fetches GET /api/v1/version when clicking Verify /api/v1/version Parity and reflects drift", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue({
      ok: true,
      json: async () => ({
        ...DEFAULT_VERSION_PROVENANCE,
        commit: "7d180aa84300bb8e066e10c31ebf54a242f60784",
        commit_short: "7d180aa84300",
        ui: {
          commit: "6fdabd8000000000000000000000000000000000",
          build_time_unix: 1775235000,
          version: "0.1.0",
          dirty: true,
        },
        code_ui_agree: false,
      }),
    } as Response);

    render(
      <SettingsModal
        isOpen={true}
        onClose={() => {}}
        initialTab="products"
      />,
    );

    fireEvent.click(screen.getByTestId("receipt-verify-version-btn"));

    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith("/api/v1/version");
      expect(
        screen.getByTestId("version-code-ui-parity-badge").textContent,
      ).toContain("Drift Detected (code_ui_agree=false)");
    });

    expect(screen.getByTestId("receipt-ui-build-info").textContent).toContain(
      "commit=6fdabd800000",
    );
    expect(screen.getByTestId("receipt-ui-build-info").textContent).toContain(
      "dirty=true",
    );
  });
});

describe("Wave 5 Item 2 (Record #88): Automated CI Check Failure & PR Review Feedback Routing UI", () => {
  it("renders routed CI check failure and PR review feedback cards and dispatches/acknowledges them", () => {
    expect(ReExportedCiPrEvents).toEqual(DEFAULT_ROUTED_CI_PR_EVENTS);

    render(<TriageQueuePane />);

    expect(screen.getByTestId("triage-routed-ci-pr-section")).toBeTruthy();
    expect(
      screen.getByTestId("routed-ci-pr-card-ci-fail-pr-238"),
    ).toBeTruthy();
    expect(
      screen.getByTestId("routed-ci-pr-kind-ci-fail-pr-238").textContent,
    ).toBe("ci-check-failure");
    expect(
      screen.getByTestId("routed-ci-pr-branch-ci-fail-pr-238").textContent,
    ).toContain("PR #238");

    expect(
      screen.getByTestId("routed-ci-pr-card-pr-rev-pr-35"),
    ).toBeTruthy();
    expect(
      screen.getByTestId("routed-ci-pr-kind-pr-rev-pr-35").textContent,
    ).toBe("pr-review-feedback");

    fireEvent.click(
      screen.getByTestId("routed-ci-pr-dispatch-btn-ci-fail-pr-238"),
    );
    expect(
      screen.getByTestId("routed-ci-pr-status-ci-fail-pr-238").textContent,
    ).toBe("dispatched");
    expect(
      screen.getByTestId("triage-routed-action-status").textContent,
    ).toContain("Dispatched ci-check-failure fix task for PR #238");

    fireEvent.click(screen.getByTestId("routed-ci-pr-ack-btn-pr-rev-pr-35"));
    expect(
      screen.getByTestId("routed-ci-pr-status-pr-rev-pr-35").textContent,
    ).toBe("acknowledged");
    expect(
      screen.getByTestId("triage-routed-action-status").textContent,
    ).toContain("Acknowledged pr-review-feedback event for PR #35.");
  });
});

describe("Wave 5 Item 3 (Record #93): ACP Vendor Transport Conformance Matrix UI", () => {
  it("renders all 6 vendor transport rows and 6 ACP operations in FerroWatchPane", () => {
    expect(ReExportedAcpOps).toEqual(ACP_CONFORMANCE_OPERATIONS);
    expect(ReExportedAcpMatrix).toEqual(DEFAULT_ACP_CONFORMANCE_MATRIX);
    expect(ACP_CONFORMANCE_OPERATIONS).toEqual([
      "approvals",
      "interrupt",
      "reset",
      "compact",
      "thread_identity",
      "usage",
    ]);

    render(<FerroWatchPane />);

    expect(screen.getByTestId("acp-conformance-matrix-card")).toBeTruthy();
    for (const adapter of ["claude", "agy", "codex", "muse", "grok", "acp"]) {
      expect(
        screen.getByTestId(`acp-conformance-row-${adapter}`),
      ).toBeTruthy();
      for (const op of ACP_CONFORMANCE_OPERATIONS) {
        expect(
          screen.getByTestId(`acp-conformance-cell-${adapter}-${op}`),
        ).toBeTruthy();
      }
    }
  });
});

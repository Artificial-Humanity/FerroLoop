// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { ArtifactPreviewPane } from "../components/ArtifactPreviewPane";
import {
  CONSOLE_LAYOUT_MODE_STORAGE_KEY as ReExportedLayoutKey,
  DEFAULT_DEDICATED_ARTIFACT_LAYOUT as ReExportedArtifactLayout,
  SYSTEM_CONSOLE_LAYOUT_OPTIONS as ReExportedLayoutOptions,
} from "../components/HubDrawer";
import {
  CONSOLE_LAYOUT_MODE_STORAGE_KEY,
  DEFAULT_DEDICATED_ARTIFACT_LAYOUT,
  SYSTEM_CONSOLE_LAYOUT_OPTIONS,
  SettingsModal,
} from "../components/SettingsModal";

afterEach(() => {
  cleanup();
  window.localStorage.removeItem(CONSOLE_LAYOUT_MODE_STORAGE_KEY);
  document.documentElement.removeAttribute("data-console-layout");
  vi.restoreAllMocks();
});

describe("Wave 12 Item 1 (Record #186 & Record #77): Dedicated FerroDeck Artifact Directory Layout & Retention Lifecycle UI", () => {
  it("renders canonical artifact paths, scattered vendor CLI origins, per-kind/per-project storage breakdowns, retention controls, and dry-run/apply consolidation actions", () => {
    expect(ReExportedArtifactLayout).toEqual(DEFAULT_DEDICATED_ARTIFACT_LAYOUT);
    expect(DEFAULT_DEDICATED_ARTIFACT_LAYOUT.session_layout_pattern).toBe(
      "[home]/artifacts/[project-name]/[session-id]/",
    );

    render(
      <SettingsModal isOpen={true} onClose={() => {}} initialTab="system" />,
    );

    expect(screen.getByTestId("dedicated-artifact-layout-card")).toBeTruthy();
    expect(
      screen.getByTestId("artifact-canonical-root-badge").textContent,
    ).toContain("~/.local/share/fl/artifacts/");
    expect(
      screen.getByTestId("artifact-session-pattern-badge").textContent,
    ).toContain("[home]/artifacts/[project-name]/[session-id]/");

    // Scattered vendor CLI origins
    expect(
      screen.getByTestId("artifact-vendor-origin-status-antigravity")
        .textContent,
    ).toBe("scattered");
    expect(
      screen.getByTestId("artifact-vendor-origin-status-claude").textContent,
    ).toBe("scattered");
    expect(
      screen.getByTestId("artifact-vendor-origin-status-codex").textContent,
    ).toBe("consolidated");

    // Per-kind & per-project breakdown badges
    for (const kind of [
      "image",
      "markdown",
      "diff",
      "log",
      "bundle",
    ] as const) {
      expect(
        screen.getByTestId(`artifact-kind-breakdown-badge-${kind}`),
      ).toBeTruthy();
    }
    for (const proj of ["FerroLoop", "FerroDeck", "Sonora"]) {
      expect(
        screen.getByTestId(`artifact-project-breakdown-badge-${proj}`),
      ).toBeTruthy();
    }

    // Update retention policy controls
    fireEvent.change(
      screen.getByTestId("artifact-retention-keep-versions-input"),
      { target: { value: "10" } },
    );
    fireEvent.change(
      screen.getByTestId("artifact-retention-archive-days-input"),
      { target: { value: "60" } },
    );

    // Dry-run consolidation preview
    fireEvent.click(screen.getByTestId("artifact-consolidate-dryrun-btn"));
    expect(
      screen.getByTestId("artifact-consolidate-status-banner").textContent,
    ).toContain(
      "Dry-run consolidation preview: 29 scattered vendor files ready to migrate into [home]/artifacts/[project-name]/[session-id]/.",
    );

    // Apply consolidation
    fireEvent.click(screen.getByTestId("artifact-consolidate-apply-btn"));
    expect(
      screen.getByTestId("artifact-consolidate-status-banner").textContent,
    ).toContain(
      "Consolidated all scattered vendor artifacts into ~/.local/share/fl/artifacts/ (keep_latest_versions=10, auto_archive_days=60).",
    );
    expect(
      screen.getByTestId("artifact-vendor-origin-status-antigravity")
        .textContent,
    ).toBe("consolidated");
    expect(
      screen.getByTestId("artifact-vendor-origin-status-claude").textContent,
    ).toBe("consolidated");
  });

  it("displays canonical session layout summary inside ArtifactPreviewPane Clean Up modal", () => {
    const client = new HubApiClient();
    render(<ArtifactPreviewPane apiClient={client} />);

    fireEvent.click(screen.getByTestId("artifact-cleanup-btn"));
    expect(
      screen.getByTestId("artifact-cleanup-canonical-layout-summary")
        .textContent,
    ).toContain("[home]/artifacts/[project-name]/[session-id]/");
  });
});

describe("Wave 12 Item 2 (Record #68): FerroDeck System Console Layout Mode Selector UI", () => {
  it("renders all 4 System Console layout options, defaults to left-panel, and persists selection to localStorage and data-console-layout", () => {
    expect(ReExportedLayoutKey).toBe(CONSOLE_LAYOUT_MODE_STORAGE_KEY);
    expect(ReExportedLayoutOptions).toEqual(SYSTEM_CONSOLE_LAYOUT_OPTIONS);
    expect(SYSTEM_CONSOLE_LAYOUT_OPTIONS.map((o) => o.id)).toEqual([
      "left-panel",
      "modal-consoles",
      "dual-right-header",
      "bottom-dock",
    ]);

    render(
      <SettingsModal
        isOpen={true}
        onClose={() => {}}
        initialTab="accessibility"
      />,
    );

    expect(screen.getByTestId("system-console-layout-card")).toBeTruthy();
    expect(
      screen.getByTestId("console-layout-active-badge").textContent,
    ).toBe("Mode: left-panel");
    expect(
      screen
        .getByTestId("console-layout-option-left-panel")
        .getAttribute("aria-pressed"),
    ).toBe("true");

    // Switch to modal-consoles
    fireEvent.click(screen.getByTestId("console-layout-option-modal-consoles"));
    expect(
      screen.getByTestId("console-layout-active-badge").textContent,
    ).toBe("Mode: modal-consoles");
    expect(
      window.localStorage.getItem(CONSOLE_LAYOUT_MODE_STORAGE_KEY),
    ).toBe("modal-consoles");
    expect(
      document.documentElement.getAttribute("data-console-layout"),
    ).toBe("modal-consoles");
    expect(
      screen.getByTestId("console-layout-preview-summary").textContent,
    ).toContain("Modal Overlay: System Consoles");

    // Switch to bottom-dock
    fireEvent.click(screen.getByTestId("console-layout-option-bottom-dock"));
    expect(
      screen.getByTestId("console-layout-active-badge").textContent,
    ).toBe("Mode: bottom-dock");
    expect(
      window.localStorage.getItem(CONSOLE_LAYOUT_MODE_STORAGE_KEY),
    ).toBe("bottom-dock");
    expect(
      screen.getByTestId("console-layout-status-banner").textContent,
    ).toContain("Applied System Console layout mode 'bottom-dock'");
  });
});

// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  DEFAULT_LOCAL_MODEL_PROVIDERS as ReExportedLocalProviders,
  DEFAULT_VENDOR_CLOUD_SESSIONS as ReExportedCloudSessions,
} from "../components/HubDrawer";
import {
  DEFAULT_LOCAL_MODEL_PROVIDERS,
  DEFAULT_VENDOR_CLOUD_SESSIONS,
  SettingsModal,
} from "../components/SettingsModal";
import { SpinUpAgentModal } from "../components/SpinUpAgentModal";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("Wave 11 Item 1 (Record #71): Manage Vendor Cloud Sessions in FerroDeck UI", () => {
  it("renders discovered vendor cloud sessions across Claude Cloud, OpenAI Codex Cloud, Google Jules / Gemini Cloud, and xAI Grok Cloud with Attach/Mirror, Sync Handoff, Pause/Resume, and Pull Branch/PR actions", () => {
    expect(ReExportedCloudSessions).toEqual(DEFAULT_VENDOR_CLOUD_SESSIONS);
    expect(DEFAULT_VENDOR_CLOUD_SESSIONS.map((s) => s.vendor)).toEqual([
      "claude-cloud",
      "codex-cloud",
      "gemini-jules-cloud",
      "grok-cloud",
    ]);

    render(
      <SettingsModal isOpen={true} onClose={() => {}} initialTab="agents" />,
    );

    expect(screen.getByTestId("vendor-cloud-sessions-card")).toBeTruthy();
    expect(
      screen.getByTestId("vendor-cloud-sessions-count-badge").textContent,
    ).toContain("4 Cloud Sessions (2 Mirrored)");

    // Verify all 4 sessions are rendered
    for (const session of DEFAULT_VENDOR_CLOUD_SESSIONS) {
      expect(
        screen.getByTestId(`cloud-session-row-${session.session_id}`),
      ).toBeTruthy();
      expect(
        screen.getByTestId(`cloud-session-status-${session.session_id}`)
          .textContent,
      ).toBe(session.status);
    }

    // Attach / Mirror unattached OpenAI Codex Cloud session
    expect(
      screen.getByTestId("cloud-session-mirrored-cs-codex-cloud-412")
        .textContent,
    ).toBe("unattached");
    fireEvent.click(
      screen.getByTestId("cloud-session-attach-btn-cs-codex-cloud-412"),
    );
    expect(
      screen.getByTestId("cloud-session-mirrored-cs-codex-cloud-412")
        .textContent,
    ).toBe("mirrored");
    expect(
      screen.getByTestId("vendor-cloud-sessions-count-badge").textContent,
    ).toContain("4 Cloud Sessions (3 Mirrored)");
    expect(
      screen.getByTestId("vendor-cloud-sessions-banner").textContent,
    ).toContain(
      "Attached and mirrored remote cloud session cs-codex-cloud-412 (OpenAI Codex Cloud).",
    );

    // Sync Handoff on Claude Cloud session
    fireEvent.click(
      screen.getByTestId("cloud-session-sync-handoff-btn-cs-claude-web-901"),
    );
    expect(
      screen.getByTestId("vendor-cloud-sessions-banner").textContent,
    ).toContain(
      "Synced FerroState handoff checkpoint for cs-claude-web-901",
    );

    // Pause / Resume toggle on OpenAI Codex Cloud session (initially paused -> running)
    fireEvent.click(
      screen.getByTestId("cloud-session-toggle-pause-btn-cs-codex-cloud-412"),
    );
    expect(
      screen.getByTestId("cloud-session-status-cs-codex-cloud-412").textContent,
    ).toBe("running");
    expect(
      screen.getByTestId("vendor-cloud-sessions-banner").textContent,
    ).toContain(
      "Updated cs-codex-cloud-412 cloud session status to running.",
    );

    // Pull Branch / PR on Google Jules / Gemini Cloud session
    fireEvent.click(
      screen.getByTestId("cloud-session-pull-branch-btn-cs-jules-cloud-207"),
    );
    expect(
      screen.getByTestId("vendor-cloud-sessions-banner").textContent,
    ).toContain(
      "Pulled remote branch Artificial-Humanity/FerroLoop@audit/acp-matrix from Google Jules / Gemini Cloud.",
    );
  });
});

describe("Wave 11 Item 2 (Record #72): Local Model & API-Key Agent Lifecycle UI", () => {
  it("renders Ollama, Lemonade, and OpenAI-Compatible presets with FerroGate Vault references, context-window/output-token overrides, and SpinUpAgentModal integration", () => {
    expect(ReExportedLocalProviders).toEqual(DEFAULT_LOCAL_MODEL_PROVIDERS);
    expect(DEFAULT_LOCAL_MODEL_PROVIDERS.map((p) => p.kind)).toEqual([
      "ollama",
      "lemonade",
      "openai-compatible",
    ]);

    render(
      <SettingsModal isOpen={true} onClose={() => {}} initialTab="agents" />,
    );

    expect(screen.getByTestId("local-model-providers-card")).toBeTruthy();
    expect(
      screen.getByTestId("local-providers-lifecycle-badge").textContent,
    ).toBe("Lifecycle: spin-up · compact · reforge · recast");

    // Load Lemonade (AMD Ryzen AI / ROCm) preset into the editor
    fireEvent.click(
      screen.getByTestId("local-provider-load-btn-lemonade-rocm"),
    );
    expect(
      (screen.getByTestId("local-provider-endpoint-input") as HTMLInputElement)
        .value,
    ).toBe("http://127.0.0.1:8000/v1");
    expect(
      (screen.getByTestId("local-provider-kind-select") as HTMLSelectElement)
        .value,
    ).toBe("lemonade");

    // Update context window and save preset
    fireEvent.change(
      screen.getByTestId("local-provider-context-window-input"),
      {
        target: { value: "262144" },
      },
    );
    fireEvent.click(screen.getByTestId("local-provider-save-btn"));

    expect(
      screen.getByTestId("local-provider-status-banner").textContent,
    ).toContain(
      "Saved lemonade provider (http://127.0.0.1:8000/v1, keyless (loopback), ctx=262144).",
    );
  });

  it("exposes local/API-key provider preset selector in SpinUpAgentModal with endpoint & lifecycle preview", () => {
    render(
      <SpinUpAgentModal
        isOpen={true}
        onClose={() => {}}
        initialProject="FerroLoop"
      />,
    );

    expect(screen.getByTestId("spin-up-local-provider-section")).toBeTruthy();
    const select = screen.getByTestId(
      "spin-up-local-provider-select",
    ) as HTMLSelectElement;
    expect(select.value).toBe("vendor-native");

    fireEvent.change(select, { target: { value: "lemonade-rocm" } });
    expect(select.value).toBe("lemonade-rocm");

    const preview = screen.getByTestId(
      "spin-up-local-provider-endpoint-preview",
    );
    expect(preview.textContent).toContain("http://127.0.0.1:8000/v1 (ROCm)");
  });
});

// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import {
  BUILTIN_ROLE_REGISTRY as ReExportedRoles,
  DEFAULT_OPERATOR_PUSH_STATE as ReExportedPushState,
} from "../components/HubDrawer";
import {
  BUILTIN_ROLE_REGISTRY,
  DEFAULT_OPERATOR_PUSH_STATE,
  SettingsModal,
} from "../components/SettingsModal";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("Wave 10 Item 1 (Record #67): FerroDeck+ Operator Push Notifications UI", () => {
  it("renders VAPID Web Push, APNs, and FCM status badges, per-project/per-severity triggers, quiet hours, and deep-link test actions", () => {
    expect(ReExportedPushState).toEqual(DEFAULT_OPERATOR_PUSH_STATE);
    expect(DEFAULT_OPERATOR_PUSH_STATE.web_push_permission).toBe("granted");
    expect(DEFAULT_OPERATOR_PUSH_STATE.apns_ios_status).toBe("active");
    expect(DEFAULT_OPERATOR_PUSH_STATE.fcm_android_status).toBe("active");

    render(
      <SettingsModal isOpen={true} onClose={() => {}} initialTab="system" />,
    );

    expect(screen.getByTestId("operator-push-notifications-card")).toBeTruthy();
    expect(
      screen.getByTestId("push-vapid-status-badge").textContent,
    ).toContain("VAPID SW: Registered (granted)");
    expect(screen.getByTestId("push-apns-status-badge").textContent).toBe(
      "APNs: active",
    );
    expect(screen.getByTestId("push-fcm-status-badge").textContent).toBe(
      "FCM: active",
    );

    // Toggle <N>? and <N>! triggers and project scope
    const questionCheckbox = screen.getByTestId(
      "push-trigger-question-checkbox",
    ) as HTMLInputElement;
    expect(questionCheckbox.checked).toBe(true);
    fireEvent.click(questionCheckbox);
    expect(questionCheckbox.checked).toBe(false);

    fireEvent.change(screen.getByTestId("push-project-scope-select"), {
      target: { value: "FerroLoop" },
    });
    expect(
      (screen.getByTestId("push-project-scope-select") as HTMLSelectElement)
        .value,
    ).toBe("FerroLoop");

    // Quiet hours inputs
    fireEvent.change(screen.getByTestId("push-quiet-hours-start-input"), {
      target: { value: "23:00" },
    });
    expect(
      (screen.getByTestId("push-quiet-hours-start-input") as HTMLInputElement)
        .value,
    ).toBe("23:00");

    // Subscribe VAPID Web Push & Test Push Deep-Link
    fireEvent.click(screen.getByTestId("push-subscribe-vapid-btn"));
    expect(screen.getByTestId("push-action-banner").textContent).toContain(
      "Registered VAPID Web Push subscription",
    );

    fireEvent.click(screen.getByTestId("push-test-deep-link-btn"));
    expect(screen.getByTestId("push-action-banner").textContent).toContain(
      "Dispatched test push notification deep-link (/triage?focus=perm-req-01)",
    );
  });
});

describe("Wave 10 Item 2 (Record #74): User-Configurable Agent Role Registry UI", () => {
  it("renders all 11 built-in roles and supports editing existing roles and creating custom RoleRecord definitions", async () => {
    expect(ReExportedRoles).toEqual(BUILTIN_ROLE_REGISTRY);
    expect(BUILTIN_ROLE_REGISTRY.map((r) => r.slug)).toEqual([
      "lead",
      "assistant",
      "strategist",
      "ui-developer",
      "lab-manager",
      "qa-tester",
      "security-engineer",
      "devops-release",
      "data-ml-engineer",
      "technical-writer",
      "journalist",
    ]);

    const apiClient = new HubApiClient({
      fetchImpl: vi.fn(async (input, init) => new Response(JSON.stringify(
        String(input).endsWith("/api/v1/roles")
          ? init?.method === "POST" ? JSON.parse(String(init.body)) : BUILTIN_ROLE_REGISTRY
          : {},
      ), { status: 200, headers: { "Content-Type": "application/json" } })),
    });
    render(
      <SettingsModal isOpen={true} onClose={() => {}} initialTab="agents" apiClient={apiClient} />,
    );
    await screen.findByTestId("role-registry-row-ui-developer");

    expect(
      screen.getByTestId("configurable-role-registry-card"),
    ).toBeTruthy();
    expect(
      screen.getByTestId("role-registry-count-badge").textContent,
    ).toContain("11 Roles");

    for (const roleRec of BUILTIN_ROLE_REGISTRY) {
      expect(
        screen.getByTestId(`role-registry-row-${roleRec.slug}`),
      ).toBeTruthy();
    }

    // Load an existing role into the editor
    fireEvent.click(screen.getByTestId("role-registry-edit-btn-ui-developer"));
    expect(
      (screen.getByTestId("role-registry-slug-input") as HTMLInputElement)
        .value,
    ).toBe("ui-developer");

    // Create a custom RoleRecord
    fireEvent.change(screen.getByTestId("role-registry-slug-input"), {
      target: { value: "benchmark-auditor" },
    });
    fireEvent.change(screen.getByTestId("role-registry-label-input"), {
      target: { value: "Benchmark & Telemetry Auditor" },
    });
    fireEvent.change(screen.getByTestId("role-registry-description-input"), {
      target: { value: "Audits benchmark reproducibility and token ledgers." },
    });
    fireEvent.change(screen.getByTestId("role-registry-review-policy-select"), {
      target: { value: "lab-manager-review" },
    });
    fireEvent.click(screen.getByTestId("role-registry-save-btn"));

    await waitFor(() => expect(
      screen.getByTestId("role-registry-count-badge").textContent,
    ).toContain("12 Roles"));
    expect(
      screen.getByTestId("role-registry-row-benchmark-auditor"),
    ).toBeTruthy();
    expect(
      screen.getByTestId("role-registry-status-banner").textContent,
    ).toContain("Saved RoleRecord 'benchmark-auditor'");
  });
});

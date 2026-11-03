import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { FerroGatePane } from "../components/FerroGatePane";
import { SettingsModal } from "../components/SettingsModal";

describe("Records #96, #97, #98 & #99 — FerroGate Access Bundles, Rate/Budget Admission, Failover & Schema Caps", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it("renders session-scoped revocable access bundles (#99), shared rate/budget admission (#97), endpoint failover (#98), and strict output schema caps (#96)", () => {
    render(
      <FerroGatePane
        secrets={[]}
        connectors={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );

    // Record #99: Session-Scoped Revocable Access Bundles
    expect(screen.getByTestId("gate-access-bundles-section")).toBeTruthy();
    expect(screen.getByTestId("gate-access-bundle-bnd-sess-ferris-01")).toBeTruthy();
    expect(
      screen.getByTestId("gate-bundle-warrant-bnd-sess-ferris-01").textContent,
    ).toContain("warrant: wrt-fl-8901");
    expect(
      screen.getByTestId("gate-bundle-status-bnd-sess-ferris-01").textContent,
    ).toBe("ACTIVE");

    // Record #97: Shared Gateway Rate/Budget Admission & Concurrency
    expect(screen.getByTestId("gate-rate-budget-admission-section")).toBeTruthy();
    expect(screen.getByTestId("gate-admission-row-adm-ferroloop-github")).toBeTruthy();
    expect(
      screen.getByTestId("gate-admission-concurrency-adm-ferroloop-github").textContent,
    ).toContain("concurrency: 2/8");
    expect(
      screen.getByTestId("gate-admission-rate-adm-ferroloop-github").textContent,
    ).toContain("rate: 18/60 rpm");
    expect(
      screen.getByTestId("gate-admission-budget-adm-ferroloop-github").textContent,
    ).toContain("budget: $0.42/$5.00");

    // Record #98: Bounded Endpoint Failover Health & Deadline Status
    expect(screen.getByTestId("gate-endpoint-failover-section")).toBeTruthy();
    expect(screen.getByTestId("gate-failover-card-route-llm-gateway")).toBeTruthy();
    expect(
      screen.getByTestId("gate-failover-health-route-llm-gateway").textContent,
    ).toContain("degraded-failover (active: fallback)");
    expect(
      screen.getByTestId("gate-failover-deadline-route-llm-gateway").textContent,
    ).toContain("deadline: 12000ms · retries: 1");

    // Record #96: Strict Output Schema Validation & Byte/Token Cap Status
    expect(screen.getByTestId("gate-schema-cap-section")).toBeTruthy();
    expect(
      screen.getByTestId("gate-schema-policy-github-list_pull_requests"),
    ).toBeTruthy();
    expect(
      screen.getByTestId("gate-schema-mode-badge-github-list_pull_requests")
        .textContent,
    ).toBe("strict-json-schema");
    expect(
      screen.getByTestId("gate-cap-badge-github-list_pull_requests").textContent,
    ).toContain("cap: 256 KiB / 8192 tok (truncate-with-receipt)");
  });

  it("revokes an active session-scoped access bundle (#99) with one click and falls back cleanly on 404", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("Not Found", { status: 404 }),
    );

    render(
      <FerroGatePane
        secrets={[]}
        connectors={[]}
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );

    const revokeBtn = screen.getByTestId(
      "gate-revoke-bundle-btn-bnd-sess-ferris-01",
    ) as HTMLButtonElement;
    expect(revokeBtn.disabled).toBe(false);
    fireEvent.click(revokeBtn);

    await waitFor(() => {
      expect(
        screen.getByTestId("gate-bundle-status-bnd-sess-ferris-01").textContent,
      ).toBe("REVOKED");
    });
    expect(
      (
        screen.getByTestId(
          "gate-revoke-bundle-btn-bnd-sess-ferris-01",
        ) as HTMLButtonElement
      ).disabled,
    ).toBe(true);
    expect(screen.getByTestId("gate-bundle-action-status").textContent).toContain(
      "Revoked session-scoped access bundle bnd-sess-ferris-01",
    );
  });
});

describe("Record #207 — Hub Home Directory & Artifacts Directory Hierarchy in SettingsModal", () => {
  it("renders Hub Home Directory row alongside System Artifacts Directory row", () => {
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        initialTab="system"
        authority={{ tier: "owner", login: "lmcfarlin" }}
      />,
    );

    expect(screen.getByTestId("settings-system-home-dir-row")).toBeTruthy();
    expect(
      screen.getByTestId("settings-system-home-dir-value").textContent,
    ).toContain(".local/share/fl");
    expect(
      screen.getByTestId("settings-system-home-hierarchy-badge").textContent,
    ).toBe("[home]/artifacts/[project-name]");
    expect(screen.getByTestId("settings-system-artifacts-dir-row")).toBeTruthy();
  });
});

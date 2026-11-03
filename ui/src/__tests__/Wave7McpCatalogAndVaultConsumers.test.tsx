// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  DEFAULT_MCP_CATALOG_REPOS as ReExportedGateCatalog,
  DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS as ReExportedGateConsumers,
  FerroGatePane,
} from "../components/FerroGatePane";
import {
  DEFAULT_MCP_CATALOG_REPOS as ReExportedSettingsCatalog,
  DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS as ReExportedSettingsConsumers,
} from "../components/SettingsModal";
import {
  DEFAULT_MCP_CATALOG_REPOS,
  DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
  VaultGatePane,
} from "../components/VaultGatePane";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("Wave 7 & Wave 13 Item 1 (Record #81 / Gate G5): .fl/mcp.toml Workspace Catalog & Vendor Session MCP Deduplication UI", () => {
  it("defaults to honest unverified not-checked state, refuses to fabricate synced on non-2xx responses, and transitions to synced only on 2xx backend responses", async () => {
    expect(ReExportedGateCatalog).toEqual(DEFAULT_MCP_CATALOG_REPOS);
    expect(ReExportedSettingsCatalog).toEqual(DEFAULT_MCP_CATALOG_REPOS);
    expect(DEFAULT_MCP_CATALOG_REPOS.every((r) => r.status === "not-checked")).toBe(
      true,
    );
    expect(
      DEFAULT_MCP_CATALOG_REPOS.every(
        (r) =>
          r.deduplicated_daemons.length === 0 &&
          r.active_vendor_sessions === null &&
          r.spawned_mcp_processes === null,
      ),
    ).toBe(true);

    // 1. Offline / 404 responses must NOT fabricate synced status
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue({
      ok: false,
      status: 404,
    } as Response);

    render(<VaultGatePane />);

    expect(screen.getByTestId("gate-mcp-catalog-g5-section")).toBeTruthy();
    expect(screen.getByTestId("mcp-catalog-dedup-badge").textContent).toContain(
      "Unverified (offline preview — run fl mcp check)",
    );

    expect(
      screen.getByTestId("mcp-catalog-repo-status-FerroLoop").textContent,
    ).toBe("not-checked");
    expect(
      screen.getByTestId("mcp-catalog-repo-status-Sonora").textContent,
    ).toBe("not-checked");
    expect(
      screen.getByTestId("mcp-catalog-repo-dedup-stats-FerroLoop").textContent,
    ).toContain("sessions: unverified · shared procs: unverified");

    fireEvent.click(screen.getByTestId("mcp-catalog-check-btn"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith("/api/v1/mcp/catalog");
      expect(
        screen.getByTestId("mcp-catalog-status-banner").textContent,
      ).toContain(
        "Catalog check failed: backend /api/v1/mcp/catalog unavailable",
      );
      expect(
        screen.getByTestId("mcp-catalog-repo-status-FerroLoop").textContent,
      ).toBe("not-checked");
    });

    fireEvent.click(screen.getByTestId("mcp-catalog-sync-btn"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith(
        "/api/v1/mcp/sync",
        expect.objectContaining({ method: "POST" }),
      );
      expect(
        screen.getByTestId("mcp-catalog-status-banner").textContent,
      ).toBe("Sync failed: backend /api/v1/mcp/sync unavailable");
      expect(
        screen.getByTestId("mcp-catalog-repo-status-Sonora").textContent,
      ).toBe("not-checked");
    });

    // 2. Live 200 OK response transitions rows to verified synced state
    fetchSpy.mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        repos: DEFAULT_MCP_CATALOG_REPOS.map((r) => ({
          ...r,
          status: "synced",
          deduplicated_daemons: ["fl-state", "agent-bus", "research-papers"],
          active_vendor_sessions: 3,
          spawned_mcp_processes: 3,
        })),
      }),
    } as Response);

    fireEvent.click(screen.getByTestId("mcp-catalog-sync-btn"));
    await waitFor(() => {
      expect(
        screen.getByTestId("mcp-catalog-repo-status-Sonora").textContent,
      ).toBe("synced");
      expect(
        screen.getByTestId("mcp-catalog-dedup-badge").textContent,
      ).toContain("Verified · Committed .fl/mcp.toml Catalogs Synced");
      expect(
        screen.getByTestId("mcp-catalog-repo-dedup-stats-FerroLoop").textContent,
      ).toBe("sessions: 3 · shared procs: 3");
    });
  });
});

describe("Wave 7 & Wave 13 Item 2 (Record #86 / Gate G6): FerroGate Vault Consumer Cutover & Signing Broker Attestation UI", () => {
  it("defaults Vault consumer cutover rows to pending-cutover (plaintext=unverified) and transitions to cutover-complete (plaintext=0) only on 2xx verification", async () => {
    expect(ReExportedGateConsumers).toEqual(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
    );
    expect(ReExportedSettingsConsumers).toEqual(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
    );
    expect(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS.every(
        (r) => r.status === "pending-cutover" && r.plaintext_secrets === null,
      ),
    ).toBe(true);

    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue({
      ok: false,
      status: 503,
    } as Response);

    render(<FerroGatePane />);

    expect(
      screen.getByTestId("gate-vault-consumer-cutover-g6-section"),
    ).toBeTruthy();
    expect(
      screen.getByTestId("gate-vault-zero-plaintext-badge").textContent,
    ).toContain(
      "Plaintext Audit: Unverified (pending /api/v1/gate/vault-consumers)",
    );
    expect(
      screen.getByTestId("gate-vault-signing-broker-badge").textContent,
    ).toContain("Signing Broker: Pending Cutover Verification");

    for (const row of DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS) {
      expect(
        screen.getByTestId(`gate-vault-consumer-row-${row.id}`),
      ).toBeTruthy();
      expect(
        screen.getByTestId(`gate-vault-consumer-binding-${row.id}`).textContent,
      ).toContain("vault://");
      expect(
        screen.getByTestId(`gate-vault-consumer-status-${row.id}`).textContent,
      ).toContain("pending-cutover (plaintext=unverified)");
    }

    // Non-2xx response keeps rows pending-cutover
    fireEvent.click(screen.getByTestId("gate-vault-verify-consumers-btn"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith("/api/v1/gate/vault-consumers");
      expect(
        screen.getByTestId("gate-vault-consumer-status-banner").textContent,
      ).toBe(
        "Verification failed: backend /api/v1/gate/vault-consumers unavailable",
      );
      expect(
        screen.getByTestId(
          "gate-vault-consumer-status-consumer-github-signing-broker",
        ).textContent,
      ).toContain("pending-cutover (plaintext=unverified)");
    });

    // 2xx response transitions rows to cutover-complete (plaintext=0)
    fetchSpy.mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        consumers: DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS.map((r) => ({
          ...r,
          status: "cutover-complete",
          plaintext_secrets: 0,
        })),
      }),
    } as Response);

    fireEvent.click(screen.getByTestId("gate-vault-verify-consumers-btn"));
    await waitFor(() => {
      expect(
        screen.getByTestId("gate-vault-zero-plaintext-badge").textContent,
      ).toContain("Zero Plaintext Secrets in Config Files");
      expect(
        screen.getByTestId("gate-vault-signing-broker-badge").textContent,
      ).toContain("GitHub App Signing Broker Active");
      expect(
        screen.getByTestId(
          "gate-vault-consumer-status-consumer-github-signing-broker",
        ).textContent,
      ).toContain("cutover-complete (plaintext=0)");
    });
  });
});

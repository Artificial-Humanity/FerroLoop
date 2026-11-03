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

describe("Wave 7 Item 1 (Record #81 / Gate G5): .fl/mcp.toml Workspace Catalog & Vendor Session MCP Deduplication UI", () => {
  it("renders per-repo .fl/mcp.toml status, deduplicated daemon badge, and executes Check Catalog & Sync Configs", async () => {
    expect(ReExportedGateCatalog).toEqual(DEFAULT_MCP_CATALOG_REPOS);
    expect(ReExportedSettingsCatalog).toEqual(DEFAULT_MCP_CATALOG_REPOS);

    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue({
      ok: false,
      status: 404,
    } as Response);

    render(<VaultGatePane />);

    expect(screen.getByTestId("gate-mcp-catalog-g5-section")).toBeTruthy();
    expect(screen.getByTestId("mcp-catalog-dedup-badge").textContent).toContain(
      "Zero Duplicate research-papers-mcp / agent-bus-mcp Spawns",
    );

    expect(
      screen.getByTestId("mcp-catalog-repo-status-FerroLoop").textContent,
    ).toBe("synced");
    expect(
      screen.getByTestId("mcp-catalog-repo-status-Sonora").textContent,
    ).toBe("drift");

    fireEvent.click(screen.getByTestId("mcp-catalog-check-btn"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith("/api/v1/mcp/catalog");
      expect(
        screen.getByTestId("mcp-catalog-status-banner").textContent,
      ).toContain("fl mcp check");
    });

    fireEvent.click(screen.getByTestId("mcp-catalog-sync-btn"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith(
        "/api/v1/mcp/sync",
        expect.objectContaining({ method: "POST" }),
      );
      expect(
        screen.getByTestId("mcp-catalog-repo-status-Sonora").textContent,
      ).toBe("synced");
    });
  });
});

describe("Wave 7 Item 2 (Record #86 / Gate G6): FerroGate Vault Consumer Cutover & Signing Broker Attestation UI", () => {
  it("renders Vault consumer cutover bindings, zero-plaintext attestation, and GitHub App signing broker status", () => {
    expect(ReExportedGateConsumers).toEqual(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
    );
    expect(ReExportedSettingsConsumers).toEqual(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
    );

    render(<FerroGatePane />);

    expect(
      screen.getByTestId("gate-vault-consumer-cutover-g6-section"),
    ).toBeTruthy();
    expect(
      screen.getByTestId("gate-vault-zero-plaintext-badge").textContent,
    ).toContain("Zero Plaintext Secrets in Config Files");
    expect(
      screen.getByTestId("gate-vault-signing-broker-badge").textContent,
    ).toContain("GitHub App Signing Broker Active");

    for (const row of DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS) {
      expect(
        screen.getByTestId(`gate-vault-consumer-row-${row.id}`),
      ).toBeTruthy();
      expect(
        screen.getByTestId(`gate-vault-consumer-binding-${row.id}`).textContent,
      ).toContain("vault://");
      expect(
        screen.getByTestId(`gate-vault-consumer-status-${row.id}`).textContent,
      ).toContain("cutover-complete (plaintext=0)");
    }
  });
});

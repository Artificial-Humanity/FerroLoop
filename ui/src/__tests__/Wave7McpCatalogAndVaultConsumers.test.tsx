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

describe("Wave 7, Wave 13 & Wave 14 Item 1 (Record #81 / Gate G5): .fl/mcp.toml Workspace Catalog & Compare-And-Swap Sync UI", () => {
  it("defaults to honest unverified not-checked state, enforces CAS expect_digest on POST /api/v1/mcp/sync, rejects 200 ok:false / repo errors, and re-fetches GET /api/v1/mcp/catalog on success", async () => {
    const digestA = "a".repeat(64);
    const digestB = "b".repeat(64);

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
          r.spawned_mcp_processes === null &&
          r.catalog_digest === null,
      ),
    ).toBe(true);

    // 1. Offline / 404 responses must NOT fabricate synced status, and Sync button is disabled without a catalog_digest
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
      (screen.getByTestId("mcp-catalog-sync-btn") as HTMLButtonElement).disabled,
    ).toBe(true);

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

    // 2. GET /api/v1/mcp/catalog returns drift (with catalog_digest, enabled_servers, changes) and error (with error + refusals)
    fetchSpy.mockResolvedValueOnce({
      ok: true,
      status: 200,
      json: async () => ({
        repos: [
          {
            repo: "FerroLoop",
            catalog_path: "FerroLoop/.fl/mcp.toml",
            status: "drift",
            deduplicated_daemons: null,
            active_vendor_sessions: null,
            spawned_mcp_processes: null,
            enabled_servers: ["fl-state", "agent-bus"],
            changes: [
              { vendor: "claude", server: "fl-state", action: "add" },
              { vendor: "codex", server: "agent-bus", action: "update" },
            ],
            catalog_digest: digestA,
          },
          {
            repo: "Sonora",
            catalog_path: "Sonora/.fl/mcp.toml",
            status: "error",
            deduplicated_daemons: null,
            active_vendor_sessions: null,
            spawned_mcp_processes: null,
            enabled_servers: ["sonora-tts"],
            changes: null,
            catalog_digest: null,
            error: "1 entries were refused; nothing would be written",
            refusals: ["claude: unmanaged server entry refused"],
          },
        ],
      }),
    } as Response);

    fireEvent.click(screen.getByTestId("mcp-catalog-check-btn"));
    await waitFor(() => {
      expect(
        screen.getByTestId("mcp-catalog-repo-status-FerroLoop").textContent,
      ).toBe("drift");
      expect(
        screen.getByTestId("mcp-catalog-repo-enabled-FerroLoop").textContent,
      ).toContain("fl-state, agent-bus");
      expect(
        screen.getByTestId("mcp-catalog-repo-changes-FerroLoop").textContent,
      ).toContain("claude:fl-state (add)");
      expect(
        screen.getByTestId("mcp-catalog-repo-status-Sonora").textContent,
      ).toBe("error");
      expect(
        screen.getByTestId("mcp-catalog-repo-error-Sonora").textContent,
      ).toContain("1 entries were refused; nothing would be written");
      expect(
        screen.getByTestId("mcp-catalog-repo-refusals-Sonora").textContent,
      ).toContain("claude: unmanaged server entry refused");
      expect(
        screen.getByTestId("mcp-catalog-dedup-badge").textContent,
      ).toContain("Catalog Error / Refusal Detected");
    });

    // Sync button is now enabled because FerroLoop has a valid catalog_digest
    expect(
      (screen.getByTestId("mcp-catalog-sync-btn") as HTMLButtonElement).disabled,
    ).toBe(false);

    // 3. HTTP 200 with ok: false / stale digest error must be treated as a failure and never flip rows to synced
    fetchSpy.mockResolvedValueOnce({
      ok: true,
      status: 200,
      json: async () => ({
        ok: false,
        repos: [
          {
            repo: "FerroLoop",
            status_before: "drift",
            status_after: "drift",
            written: [],
            error: "catalog changed since you looked; reload and review it again",
          },
        ],
      }),
    } as Response);

    fireEvent.click(screen.getByTestId("mcp-catalog-sync-btn"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith(
        "/api/v1/mcp/sync",
        expect.objectContaining({
          method: "POST",
          body: JSON.stringify({
            repo: "FerroLoop",
            expect_digest: digestA,
          }),
        }),
      );
      expect(
        screen.getByTestId("mcp-catalog-status-banner").textContent,
      ).toBe(
        "Sync failed: catalog changed since you looked; reload and review it again",
      );
      expect(
        screen.getByTestId("mcp-catalog-repo-status-FerroLoop").textContent,
      ).toBe("drift");
    });

    // 4. Successful CAS POST /api/v1/mcp/sync (ok: true) triggers follow-up GET /api/v1/mcp/catalog and transitions to synced
    fetchSpy
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          ok: true,
          repos: [
            {
              repo: "FerroLoop",
              status_before: "drift",
              status_after: "synced",
              written: [".mcp.json", ".codex/config.toml"],
            },
          ],
        }),
      } as Response)
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({
          repos: [
            {
              repo: "FerroLoop",
              catalog_path: "FerroLoop/.fl/mcp.toml",
              status: "synced",
              deduplicated_daemons: ["fl-state", "agent-bus"],
              active_vendor_sessions: 3,
              spawned_mcp_processes: 3,
              enabled_servers: ["fl-state", "agent-bus"],
              changes: [],
              catalog_digest: digestB,
            },
          ],
        }),
      } as Response);

    fireEvent.click(screen.getByTestId("mcp-catalog-sync-btn"));
    await waitFor(() => {
      expect(
        screen.getByTestId("mcp-catalog-repo-status-FerroLoop").textContent,
      ).toBe("synced");
      expect(
        screen.getByTestId("mcp-catalog-dedup-badge").textContent,
      ).toContain("Verified · Committed .fl/mcp.toml Catalogs Synced");
      expect(
        screen.getByTestId("mcp-catalog-repo-dedup-stats-FerroLoop").textContent,
      ).toBe("sessions: 3 · shared procs: 3");
      expect(
        screen.getByTestId("mcp-catalog-repo-digest-FerroLoop").textContent,
      ).toContain(`digest: ${digestB.slice(0, 12)}`);
    });
  });
});

describe("Wave 7, Wave 13 & Wave 15 Item 2 (Record #86 / Gate G6): Observational FerroGate Vault Consumer Status UI (PR #261)", () => {
  it("fetches GET /api/v1/gate/vault-consumers on load and refresh, renders consumers[], custody, last_denied, and verbatim disclaimer, and never fabricates cutover-complete", async () => {
    expect(ReExportedGateConsumers).toEqual(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
    );
    expect(ReExportedSettingsConsumers).toEqual(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS,
    );
    expect(
      DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS.every(
        (r) => r.class === "unknown" && r.plaintext_secrets === null,
      ),
    ).toBe(true);

    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue({
      ok: false,
      status: 503,
    } as Response);

    const { container } = render(<FerroGatePane />);

    expect(
      screen.getByTestId("gate-vault-consumer-cutover-g6-section"),
    ).toBeTruthy();

    // Mount fetch is triggered automatically
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledWith("/api/v1/gate/vault-consumers");
    });

    expect(
      screen.getByTestId("gate-vault-zero-plaintext-badge").textContent,
    ).toContain(
      "Observational Status: Unverified (pending /api/v1/gate/vault-consumers)",
    );
    expect(
      screen.getByTestId("gate-vault-signing-broker-badge").textContent,
    ).toContain("Custody: same-unix-user");
    expect(
      screen.getByTestId("gate-vault-consumer-disclaimer").textContent,
    ).toContain(
      "Observational config and audit read only. Does not verify live credential rotation, custody separation, or absence of plaintext copies outside config.toml.",
    );
    expect(screen.getByTestId("gate-vault-last-denied-empty")).toBeTruthy();

    for (const row of DEFAULT_VAULT_CONSUMER_CUTOVER_ROWS) {
      expect(
        screen.getByTestId(`gate-vault-consumer-row-${row.id}`),
      ).toBeTruthy();
      expect(
        screen.getByTestId(`gate-vault-consumer-class-${row.id}`).textContent,
      ).toBe("class: unknown");
    }

    // Non-2xx response on explicit refresh shows failure banner and keeps observational unknown state
    fireEvent.click(screen.getByTestId("gate-vault-consumer-verify-btn"));
    await waitFor(() => {
      expect(
        screen.getByTestId("gate-vault-consumer-status-banner").textContent,
      ).toBe(
        "Refresh failed: backend /api/v1/gate/vault-consumers unavailable",
      );
      expect(
        screen.getByTestId("gate-vault-consumer-class-github.reviewer")
          .textContent,
      ).toBe("class: unknown");
    });

    // 200 OK with PR #261 observational payload renders consumers[], custody, last_denied, and disclaimer without ever claiming cutover-complete
    const customDisclaimer =
      "Observational config and audit read only. Does not verify live credential rotation, custody separation, or absence of plaintext copies outside config.toml.";
    fetchSpy.mockResolvedValueOnce({
      ok: true,
      status: 200,
      json: async () => ({
        consumers: [
          {
            consumer: "github.reviewer",
            class: "vault-ref",
            vault_entry: "GITHUB_APP_REVIEWER_PEM",
            vault_key_field: "pem",
            evidence:
              "`[github] private_key_ref` is configured to a secret named GITHUB_APP_REVIEWER_PEM",
            plaintext_path_present: false,
          },
          {
            consumer: "hub-owner-token",
            class: "plaintext-file",
            vault_entry: null,
            vault_key_field: null,
            evidence: "`[hub] owner_token_file` names the token file",
            plaintext_path_present: true,
          },
          {
            consumer: "vendor:openai",
            class: "unconfigured",
            vault_entry: null,
            vault_key_field: null,
            evidence: "$OPENAI_API_KEY is not set; no Vault secret is named",
            plaintext_path_present: false,
          },
        ],
        custody: {
          mode: "same-unix-user",
          master_key_source: "key-file",
          key_file_mode: "0600",
        },
        last_denied: {
          ts: "2026-11-03T13:00:00Z",
          consumer: "github.reviewer",
          action: "sign_jwt",
          secret: "GITHUB_APP_REVIEWER_PEM",
          target_host: "evil.example.com",
          outcome: "denied:host_not_allowed",
        },
        disclaimer: customDisclaimer,
      }),
    } as Response);

    fireEvent.click(screen.getByTestId("gate-vault-consumer-verify-btn"));
    await waitFor(() => {
      expect(
        screen.getByTestId("gate-vault-zero-plaintext-badge").textContent,
      ).toBe("Observational Config & Audit Read Only");
      expect(
        screen.getByTestId("gate-vault-consumer-disclaimer").textContent,
      ).toBe(customDisclaimer);
      expect(
        screen.getByTestId("gate-vault-custody-mode").textContent,
      ).toBe("mode: same-unix-user");
      expect(
        screen.getByTestId("gate-vault-custody-key-source").textContent,
      ).toBe("master_key_source: key-file");
      expect(
        screen.getByTestId("gate-vault-custody-key-mode").textContent,
      ).toBe("key_file_mode: 0600");

      // Last denied details
      const deniedDetail = screen.getByTestId("gate-vault-last-denied-detail");
      expect(deniedDetail.textContent).toContain(
        "outcome: denied:host_not_allowed",
      );
      expect(deniedDetail.textContent).toContain("consumer: github.reviewer");
      expect(deniedDetail.textContent).toContain(
        "secret: GITHUB_APP_REVIEWER_PEM",
      );
      expect(deniedDetail.textContent).toContain(
        "target_host: evil.example.com",
      );

      // Consumers classes & vault_entry / vault_key_field
      expect(
        screen.getByTestId("gate-vault-consumer-class-github.reviewer")
          .textContent,
      ).toBe("class: vault-ref");
      expect(
        screen.getByTestId("gate-vault-consumer-entry-github.reviewer")
          .textContent,
      ).toBe("vault_entry: GITHUB_APP_REVIEWER_PEM (field: pem)");
      expect(
        screen.getByTestId("gate-vault-consumer-class-hub-owner-token")
          .textContent,
      ).toBe("class: plaintext-file");
      expect(
        screen.getByTestId("gate-vault-consumer-plaintext-path-hub-owner-token")
          .textContent,
      ).toBe("plaintext_path_present: true");
      expect(
        screen.getByTestId("gate-vault-consumer-class-vendor:openai")
          .textContent,
      ).toBe("class: unconfigured");
    });

    // Strict No-False-Passes invariant: never renders "cutover-complete"
    expect(container.textContent).not.toContain("cutover-complete");
  });
});

import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { SettingsModal } from "../components/SettingsModal";
import type {
  HubAuthority,
  RuntimeMatrix,
  RuntimeUpdateResult,
  SessionQuotaUsage,
} from "../types/api";

const OWNER: HubAuthority = { tier: "owner", login: "lmcfarlin" };
const PEER: HubAuthority = { tier: "peer", agent: "kayla" };

const HOUR = 3_600_000;

const CLAUDE_QUOTA: SessionQuotaUsage = {
  tier: "max",
  primary_window: { used_percent: 40, window_minutes: 300, resets_at_ms: Date.now() + 2 * HOUR + 30_000 },
  secondary_window: { used_percent: 10, window_minutes: 10080, resets_at_ms: Date.now() + 50 * HOUR },
};

const CODEX_QUOTA: SessionQuotaUsage = {
  tier: "pro",
  primary_window: { used_percent: 100, window_minutes: 300, resets_at_ms: Date.now() + HOUR + 30_000 },
  exhausted_reason: "Usage limit reached",
};

function cli(over: Record<string, unknown>) {
  return {
    installed: true,
    path: "/bin/x",
    version: "1.0.0",
    error: null,
    active_routes: 0,
    live_sessions: 0,
    probe_ms: 3,
    update_supported: true,
    update_command: null,
    ...over,
  };
}

function makeMatrix(over: Partial<RuntimeMatrix> = {}): RuntimeMatrix {
  return {
    generated_at_ms: 1,
    vault_error: null,
    vault_providers: [],
    local_engines: [],
    vendor_quotas: { claude: CLAUDE_QUOTA, codex: CODEX_QUOTA },
    cli_runtimes: [
      cli({ name: "claude", version: "2.1.0", path: "/home/u/.local/bin/claude", update_command: "claude update" }),
      cli({ name: "codex", installed: false, path: null, version: null, error: "`codex` not found in PATH", update_command: "codex update" }),
      cli({ name: "grok", version: "1.0.70", update_command: "grok update" }),
      cli({ name: "agy", version: "0.9.0", error: "--version timed out", update_command: "agy update" }),
      cli({ name: "muse", version: "1.4.14", update_supported: false, update_command: null }),
    ] as RuntimeMatrix["cli_runtimes"],
    ...over,
  };
}

function result(over: Partial<RuntimeUpdateResult>): RuntimeUpdateResult {
  return {
    runtime: "claude",
    ok: true,
    version_before: "2.1.0",
    version_after: "2.2.0",
    status: "updated",
    output_tail: "installed 2.2.0",
    ...over,
  };
}

function makeClient(over: Record<string, unknown> = {}) {
  // SettingsModal loads its other tabs' data on open; the owner's endpoints are not under test.
  const unreachable = () => vi.fn().mockRejectedValue(new Error("not under test"));
  const client = {
    getSettings: unreachable(),
    getAgents: unreachable(),
    listOperators: unreachable(),
    getCloudAdapters: unreachable(),
    fetchRuntimeMatrix: vi.fn().mockResolvedValue(makeMatrix()),
    probeRuntimes: vi.fn().mockResolvedValue(makeMatrix()),
    updateRuntime: vi.fn().mockResolvedValue([result({})]),
    ...over,
  };
  return client as unknown as HubApiClient & typeof client;
}

function renderTab(client: HubApiClient, authority: HubAuthority | null = OWNER) {
  render(
    <SettingsModal
      isOpen
      onClose={vi.fn()}
      initialTab="runtimes"
      authority={authority}
      apiClient={client}
    />,
  );
}

describe("Settings: Runtimes & Quotas tab", () => {
  it("is offered to the owner only, and a peer who lands on it asks the hub for nothing", async () => {
    const peerClient = makeClient();
    const { unmount } = render(
      <SettingsModal isOpen onClose={vi.fn()} authority={PEER} apiClient={peerClient} />,
    );
    expect(screen.queryByTestId("settings-tab-runtimes")).toBeNull();
    unmount();

    const forced = makeClient();
    const view = render(
      <SettingsModal isOpen onClose={vi.fn()} initialTab="runtimes" authority={PEER} apiClient={forced} />,
    );
    expect(screen.getByTestId("settings-runtimes-owner-only")).toBeInTheDocument();
    expect(screen.queryByTestId("settings-runtimes-panel")).toBeNull();
    expect(forced.fetchRuntimeMatrix).not.toHaveBeenCalled();
    view.unmount();

    const ownerClient = makeClient();
    render(<SettingsModal isOpen onClose={vi.fn()} authority={OWNER} apiClient={ownerClient} />);
    expect(ownerClient.fetchRuntimeMatrix).not.toHaveBeenCalled();
    const tab = screen.getByTestId("settings-tab-runtimes");
    expect(tab).toHaveTextContent("Runtimes & Quotas");
    fireEvent.click(tab);
    expect(tab).toHaveAttribute("aria-selected", "true");
    expect(screen.getByTestId("settings-runtimes-panel")).toBeInTheDocument();
    expect(ownerClient.fetchRuntimeMatrix).toHaveBeenCalledTimes(1);
    expect(await screen.findByTestId("runtime-row-claude")).toBeInTheDocument();
  });

  it("shows each vendor's tier, 5h and 7d windows with bars and reset countdowns, and the exhausted state", async () => {
    renderTab(makeClient());
    const claude = await screen.findByTestId("vendor-quota-claude");
    expect(within(claude).getByTestId("quota-tier-chip")).toHaveTextContent("MAX");
    const primary = within(claude).getByTestId("quota-primary-window");
    expect(primary).toHaveTextContent("5h: 60% left");
    expect(primary).toHaveTextContent(/renews in 2h/);
    expect(within(primary).getByRole("progressbar")).toHaveAttribute("aria-valuenow", "60");
    const secondary = within(claude).getByTestId("quota-secondary-window");
    expect(secondary).toHaveTextContent("7d: 90% left");
    expect(secondary).toHaveTextContent(/renews in 2d/);
    expect(within(secondary).getByRole("progressbar")).toHaveAttribute("aria-valuenow", "90");
    expect(within(claude).getByTestId("quota-usage-badge")).not.toHaveAttribute("data-exhausted");
    expect(within(claude).queryByTestId("quota-exhausted-alert")).toBeNull();

    const codex = screen.getByTestId("vendor-quota-codex");
    expect(within(codex).getByTestId("quota-usage-badge")).toHaveAttribute("data-exhausted", "true");
    expect(within(codex).getByTestId("quota-exhausted-alert")).toHaveTextContent("Usage limit reached");
    expect(within(codex).getByTestId("quota-primary-window")).toHaveTextContent("5h: 0% left");

    // Vendors the hub has no reading for say so, in a fixed order.
    for (const adapter of ["grok", "antigravity", "muse"]) {
      expect(screen.getByTestId(`vendor-quota-${adapter}`)).toHaveTextContent("No allowance data");
    }
    const order = screen
      .getAllByTestId(/^vendor-quota-/)
      .map((el) => el.getAttribute("data-testid"));
    expect(order).toEqual([
      "vendor-quota-claude",
      "vendor-quota-codex",
      "vendor-quota-grok",
      "vendor-quota-antigravity",
      "vendor-quota-muse",
    ]);
  });

  it("lists one row per CLI with version, path and error, and disables Update CLI where it cannot run", async () => {
    renderTab(makeClient());
    const claude = await screen.findByTestId("runtime-row-claude");
    expect(within(claude).getByTestId("runtime-version-claude")).toHaveTextContent("2.1.0");
    expect(claude).toHaveTextContent("/home/u/.local/bin/claude");
    expect(screen.getByTestId("runtime-update-claude")).toBeEnabled();
    expect(screen.getByTestId("runtime-update-claude")).toHaveAttribute("title", expect.stringContaining("claude update"));

    expect(screen.getByTestId("runtime-version-codex")).toHaveTextContent("not installed");
    expect(screen.getByTestId("runtime-error-codex")).toHaveTextContent("not found");
    expect(screen.getByTestId("runtime-update-codex")).toBeDisabled();
    expect(screen.getByTestId("runtime-update-codex")).toHaveAttribute("title", expect.stringContaining("not installed"));

    expect(screen.getByTestId("runtime-error-agy")).toHaveTextContent("timed out");
    expect(screen.getByTestId("runtime-update-agy")).toBeEnabled();

    expect(screen.getByTestId("runtime-update-muse")).toBeDisabled();
    expect(screen.getByTestId("runtime-update-muse")).toHaveAttribute("title", expect.stringContaining("no update command"));
    // A disabled button never arms a confirmation.
    fireEvent.click(screen.getByTestId("runtime-update-muse"));
    expect(screen.queryByTestId("runtime-update-confirm-muse")).toBeNull();
    // Check stays available for every row.
    expect(screen.getByTestId("runtime-check-muse")).toBeEnabled();
    expect(screen.getByTestId("runtime-check-codex")).toBeEnabled();
  });

  it("Check re-probes only that runtime, shows it busy, and leaves the other rows as they were", async () => {
    let release: (m: RuntimeMatrix) => void = () => undefined;
    const client = makeClient({
      probeRuntimes: vi.fn().mockImplementation(
        () => new Promise<RuntimeMatrix>((resolve) => { release = resolve; }),
      ),
    });
    renderTab(client);
    await screen.findByTestId("runtime-row-grok");

    fireEvent.click(screen.getByTestId("runtime-check-grok"));
    expect(client.probeRuntimes).toHaveBeenCalledWith("grok");
    expect(screen.getByTestId("runtime-check-grok")).toBeDisabled();
    expect(screen.getByTestId("runtime-check-grok")).toHaveTextContent("Checking");
    expect(screen.getByTestId("runtime-check-claude")).toBeDisabled();
    expect(screen.getByTestId("runtime-update-claude")).toBeDisabled();

    const full = makeMatrix();
    const probed = {
      ...full,
      cli_runtimes: [{ ...full.cli_runtimes[2], version: "1.0.71" }],
      vendor_quotas: {},
    };
    await act(async () => release(probed));
    await waitFor(() => expect(screen.getByTestId("runtime-version-grok")).toHaveTextContent("1.0.71"));
    expect(screen.getByTestId("runtime-version-claude")).toHaveTextContent("2.1.0");
    // A partial probe does not drop the quotas already shown.
    expect(screen.getByTestId("vendor-quota-claude")).toHaveTextContent("MAX");
    expect(screen.getByTestId("runtime-check-grok")).toBeEnabled();
    expect(client.fetchRuntimeMatrix).toHaveBeenCalledTimes(1);
  });

  it("reports a failed check and keeps what was on screen", async () => {
    const client = makeClient({ probeRuntimes: vi.fn().mockRejectedValue(new Error("owner authority required")) });
    renderTab(client);
    await screen.findByTestId("runtime-row-grok");
    fireEvent.click(screen.getByTestId("runtime-check-grok"));
    expect(await screen.findByTestId("runtimes-error")).toHaveTextContent("owner authority required");
    expect(screen.getByTestId("runtime-version-grok")).toHaveTextContent("1.0.70");
  });

  it("asks before updating, runs the update once confirmed, and shows the outcome and the new version", async () => {
    let finish: (r: RuntimeUpdateResult[]) => void = () => undefined;
    const refreshed = makeMatrix();
    refreshed.cli_runtimes = refreshed.cli_runtimes.map((row) =>
      row.name === "claude" ? { ...row, version: "2.2.0" } : row,
    );
    const client = makeClient({
      updateRuntime: vi.fn().mockImplementation(
        () => new Promise<RuntimeUpdateResult[]>((resolve) => { finish = resolve; }),
      ),
      fetchRuntimeMatrix: vi.fn().mockResolvedValueOnce(makeMatrix()).mockResolvedValue(refreshed),
    });
    renderTab(client);
    await screen.findByTestId("runtime-row-claude");

    fireEvent.click(screen.getByTestId("runtime-update-claude"));
    const confirm = screen.getByTestId("runtime-update-confirm-claude");
    expect(confirm).toHaveTextContent("claude update");
    expect(client.updateRuntime).not.toHaveBeenCalled();
    // Cancel runs nothing.
    fireEvent.click(screen.getByTestId("runtime-update-cancel-btn-claude"));
    expect(screen.queryByTestId("runtime-update-confirm-claude")).toBeNull();
    expect(client.updateRuntime).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("runtime-update-claude"));
    fireEvent.click(screen.getByTestId("runtime-update-confirm-btn-claude"));
    expect(client.updateRuntime).toHaveBeenCalledTimes(1);
    expect(client.updateRuntime).toHaveBeenCalledWith("claude");
    expect(screen.getByTestId("runtime-update-claude")).toBeDisabled();
    expect(screen.getByTestId("runtime-update-claude")).toHaveTextContent("Updating");
    expect(screen.getByTestId("runtime-update-grok")).toBeDisabled();
    expect(screen.getByTestId("runtime-update-all")).toBeDisabled();
    expect(screen.queryByTestId("runtime-update-confirm-claude")).toBeNull();

    await act(async () => finish([result({})]));
    const outcome = await screen.findByTestId("runtime-update-result-claude");
    expect(outcome).toHaveTextContent("updated");
    expect(outcome).toHaveTextContent("2.1.0 → 2.2.0");
    expect(screen.getByTestId("runtime-update-output-claude")).toHaveTextContent("installed 2.2.0");
    await waitFor(() => expect(screen.getByTestId("runtime-version-claude")).toHaveTextContent("2.2.0"));
    expect(client.fetchRuntimeMatrix).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("runtime-update-claude")).toBeEnabled();
  });

  it("shows a failed update with its output, and a hub refusal as an error", async () => {
    const client = makeClient({
      updateRuntime: vi.fn()
        .mockResolvedValueOnce([result({ runtime: "grok", ok: false, status: "failed", version_after: "1.0.70", version_before: "1.0.70", output_tail: "no network" })])
        .mockRejectedValueOnce(new Error("a CLI update is already running; retry when it ends")),
    });
    renderTab(client);
    await screen.findByTestId("runtime-row-grok");

    fireEvent.click(screen.getByTestId("runtime-update-grok"));
    fireEvent.click(screen.getByTestId("runtime-update-confirm-btn-grok"));
    const failed = await screen.findByTestId("runtime-update-result-grok");
    expect(failed).toHaveTextContent("failed");
    expect(screen.getByTestId("runtime-update-output-grok")).toHaveTextContent("no network");

    fireEvent.click(screen.getByTestId("runtime-update-agy"));
    fireEvent.click(screen.getByTestId("runtime-update-confirm-btn-agy"));
    expect(await screen.findByTestId("runtime-update-error")).toHaveTextContent("already running");
    expect(screen.queryByTestId("runtime-update-result-agy")).toBeNull();
    expect(screen.getByTestId("runtime-update-agy")).toBeEnabled();
  });

  it("Update all confirms first, names only the CLIs it can update, and reports every runtime", async () => {
    const client = makeClient({
      updateRuntime: vi.fn().mockResolvedValue([
        result({}),
        result({ runtime: "codex", ok: false, status: "not_installed", version_before: null, version_after: null, output_tail: "`codex` not found" }),
        result({ runtime: "grok", status: "unchanged", version_before: "1.0.70", version_after: "1.0.70", output_tail: "" }),
        result({ runtime: "agy", ok: false, status: "timeout", version_before: "0.9.0", version_after: "0.9.0", output_tail: "update timed out" }),
        result({ runtime: "muse", ok: false, status: "unsupported", version_before: null, version_after: null, output_tail: "no self-update command" }),
      ]),
    });
    renderTab(client);
    await screen.findByTestId("runtime-row-claude");

    fireEvent.click(screen.getByTestId("runtime-update-all"));
    const confirm = screen.getByTestId("runtime-update-confirm-all");
    expect(confirm).toHaveTextContent("claude, grok, agy");
    expect(confirm).not.toHaveTextContent("codex");
    expect(confirm).not.toHaveTextContent("muse");
    expect(client.updateRuntime).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("runtime-update-confirm-btn-all"));
    expect(client.updateRuntime).toHaveBeenCalledWith("all");
    await screen.findByTestId("runtime-update-result-claude");
    expect(screen.getByTestId("runtime-update-result-claude")).toHaveTextContent("updated");
    expect(screen.getByTestId("runtime-update-result-codex")).toHaveTextContent("not installed");
    expect(screen.getByTestId("runtime-update-result-grok")).toHaveTextContent("unchanged");
    expect(screen.getByTestId("runtime-update-result-agy")).toHaveTextContent("timeout");
    expect(screen.getByTestId("runtime-update-output-agy")).toHaveTextContent("update timed out");
    expect(screen.getByTestId("runtime-update-result-muse")).toHaveTextContent("unsupported");
  });

  it("disables Update all when no installed CLI can update", async () => {
    const m = makeMatrix();
    m.cli_runtimes = m.cli_runtimes.map((row) =>
      row.name === "muse" || row.name === "claude" ? row : { ...row, installed: false },
    ).map((row) => (row.name === "claude" ? { ...row, installed: false } : row));
    renderTab(makeClient({ fetchRuntimeMatrix: vi.fn().mockResolvedValue(m) }));
    await screen.findByTestId("runtime-row-muse");
    expect(screen.getByTestId("runtime-update-all")).toBeDisabled();
  });

  it("shows a load failure and can refresh", async () => {
    const client = makeClient({
      fetchRuntimeMatrix: vi.fn()
        .mockRejectedValueOnce(new Error("owner authority required"))
        .mockResolvedValue(makeMatrix()),
    });
    renderTab(client);
    expect(await screen.findByTestId("runtimes-error")).toHaveTextContent("owner authority required");
    expect(screen.queryByTestId("runtime-row-claude")).toBeNull();
    fireEvent.click(screen.getByTestId("runtimes-refresh-btn"));
    expect(await screen.findByTestId("runtime-row-claude")).toBeInTheDocument();
    expect(screen.queryByTestId("runtimes-error")).toBeNull();
  });
});

describe("HubApiClient runtime update", () => {
  function clientReturning(body: unknown) {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify(body), { status: 200 }));
    const client = new HubApiClient({ fetchImpl: fetchMock as unknown as typeof fetch });
    return { client, fetchMock };
  }

  it("POSTs only the runtime name and always returns an array", async () => {
    const one = clientReturning(result({}));
    const list = await one.client.updateRuntime("claude");
    expect(list).toEqual([result({})]);
    const [url, init] = one.fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/api/v1/runtimes/update");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({ runtime: "claude" });

    const many = clientReturning([result({}), result({ runtime: "codex" })]);
    expect((await many.client.updateRuntime("all")).map((r) => r.runtime)).toEqual(["claude", "codex"]);
  });

  it("surfaces the hub's refusal", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ error: "owner authority required" }), { status: 403 }));
    const client = new HubApiClient({ fetchImpl: fetchMock as unknown as typeof fetch });
    await expect(client.updateRuntime("claude")).rejects.toThrow("owner authority required");
  });
});

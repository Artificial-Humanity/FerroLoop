import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { VaultGatePane } from "../components/VaultGatePane";
import { COCKPIT_TABS } from "../components/ContextHeader";
import type {
  GateConnectorSummary,
  HubAuthority,
  VaultSecretMetadata,
} from "../types/api";

const OWNER: HubAuthority = { tier: "owner", login: "lmcfarlin" };
const PEER: HubAuthority = { tier: "peer", agent: "ferris" };

const SAMPLE_SECRETS: VaultSecretMetadata[] = [
  {
    name: "DISCORD_BOT_TOKEN",
    description: "Discord bridge bot credential",
    allowed_hosts: ["discord.com", "*.discord.com"],
    allowed_methods: ["GET", "POST"],
    allowed_agents: ["ferris", "jarvis"],
    allow_private_network: false,
    created_at: "2026-10-27T04:00:00Z",
    updated_at: "2026-10-27T04:00:00Z",
  },
  {
    name: "LOCAL_WEBHOOK_KEY",
    allowed_hosts: ["127.0.0.1"],
    allowed_methods: ["POST"],
    allowed_agents: ["*"],
    allow_private_network: true,
    created_at: "2026-10-27T04:05:00Z",
    updated_at: "2026-10-27T04:05:00Z",
    lease_ttl_secs: 3600,
    expires_at: "2026-10-27T05:05:00Z",
  },
];

const SAMPLE_CONNECTORS: GateConnectorSummary[] = [
  {
    id: "discord",
    title: "Discord",
    version: "0.1.0",
    description: "Discord messaging connector",
    base_url: "https://discord.com/api/v10",
    allowed_hosts: ["discord.com"],
    enabled: true,
    tools: [
      {
        name: "send_message",
        description: "Send a message to a channel",
        method: "POST",
        path: "/channels/{channel_id}/messages",
      },
      {
        name: "list_channels",
        description: "List guild channels",
        method: "GET",
        path: "/guilds/{guild_id}/channels",
      },
    ],
    bridges: [
      {
        id: "discord-inbound",
        kind: "discord",
        enabled: true,
        default_target: "wire:ferris",
        channel_routes: { "12345": "wire:jarvis" },
      },
      {
        id: "discord-legacy",
        kind: "webhook",
        enabled: false,
        default_target: null,
        channel_routes: {},
      },
    ],
  },
];

describe("VaultGatePane", () => {
  it("is reachable from the cockpit tab bar as Vault & Gate", () => {
    expect(COCKPIT_TABS).toContainEqual({ id: "gate", label: "Vault & Gate" });
  });

  it("renders secret metadata badges, expiry, connectors, tools, and bridges without any secret values", () => {
    render(
      <VaultGatePane
        secrets={SAMPLE_SECRETS}
        connectors={SAMPLE_CONNECTORS}
        authority={OWNER}
      />,
    );

    expect(screen.getByTestId("vault-zk-badge")).toHaveTextContent(/AES-256-GCM/i);
    expect(screen.getByTestId("vault-zk-badge")).toHaveTextContent(/never returned/i);

    const discordSecret = screen.getByTestId("vault-secret-DISCORD_BOT_TOKEN");
    expect(discordSecret).toHaveTextContent("DISCORD_BOT_TOKEN");
    expect(discordSecret).toHaveTextContent("Discord bridge bot credential");
    expect(discordSecret).toHaveTextContent("discord.com");
    expect(discordSecret).toHaveTextContent("*.discord.com");
    expect(discordSecret).toHaveTextContent("GET");
    expect(discordSecret).toHaveTextContent("ferris");
    expect(discordSecret).toHaveTextContent(/Never/);

    const localSecret = screen.getByTestId("vault-secret-LOCAL_WEBHOOK_KEY");
    expect(localSecret).toHaveTextContent("127.0.0.1");
    expect(localSecret).toHaveTextContent("2026-10-27T05:05:00Z");
    expect(localSecret).toHaveTextContent(/Private network/i);

    const connector = screen.getByTestId("gate-connector-discord");
    expect(connector).toHaveTextContent("Discord");
    expect(connector).toHaveTextContent("0.1.0");
    expect(connector).toHaveTextContent("https://discord.com/api/v10");

    const tool = screen.getByTestId("gate-tool-send_message");
    expect(tool).toHaveTextContent("POST");
    expect(tool).toHaveTextContent("/channels/{channel_id}/messages");
    expect(tool).toHaveTextContent("Send a message to a channel");
    expect(screen.getByTestId("gate-tool-list_channels")).toHaveTextContent("GET");

    const bridge = screen.getByTestId("gate-bridge-discord-inbound");
    expect(bridge).toHaveTextContent("discord");
    expect(bridge).toHaveTextContent(/Enabled/);
    expect(bridge).toHaveTextContent("wire:ferris");
    expect(bridge).toHaveTextContent("12345");
    expect(bridge).toHaveTextContent("wire:jarvis");
    expect(screen.getByTestId("gate-bridge-discord-legacy")).toHaveTextContent(/Disabled/);
  });

  it("hides the secret form and delete actions for non-owner authority", () => {
    render(
      <VaultGatePane
        secrets={SAMPLE_SECRETS}
        connectors={SAMPLE_CONNECTORS}
        authority={PEER}
      />,
    );

    expect(screen.getByTestId("vault-owner-note")).toBeInTheDocument();
    expect(screen.queryByTestId("vault-secret-form")).toBeNull();
    expect(screen.queryByTestId("vault-delete-DISCORD_BOT_TOKEN")).toBeNull();
    // Read-only metadata and connectors stay visible for peers.
    expect(screen.getByTestId("vault-secret-DISCORD_BOT_TOKEN")).toBeInTheDocument();
    expect(screen.getByTestId("gate-connector-discord")).toBeInTheDocument();
  });

  it("creates a secret for the owner with parsed policy lists and clears the value field", async () => {
    const onSetSecret = vi.fn(async () => ({ secret: SAMPLE_SECRETS[0] }));

    render(
      <VaultGatePane
        secrets={SAMPLE_SECRETS}
        connectors={SAMPLE_CONNECTORS}
        authority={OWNER}
        onSetSecret={onSetSecret}
      />,
    );

    const nameInput = screen.getByLabelText(/Secret Name/i) as HTMLInputElement;
    const valueInput = screen.getByLabelText(/Secret Value/i) as HTMLInputElement;
    const hostsInput = screen.getByLabelText(/Allowed Hosts/i) as HTMLInputElement;
    const methodsInput = screen.getByLabelText(/Allowed Methods/i) as HTMLInputElement;
    const agentsInput = screen.getByLabelText(/Allowed Agents/i) as HTMLInputElement;
    const privateNetwork = screen.getByLabelText(/Allow Private Network/i) as HTMLInputElement;

    // iPadOS AutoFill suppression on the password field (ChatPane pattern).
    expect(valueInput.type).toBe("password");
    expect(valueInput.autocomplete).toBe("new-password");
    expect(valueInput.dataset["1pIgnore"]).toBe("true");
    expect(valueInput.dataset.lpignore).toBe("true");

    fireEvent.change(nameInput, { target: { value: "new_api_key" } });
    fireEvent.change(valueInput, { target: { value: "s3cr3t-value" } });
    fireEvent.change(hostsInput, {
      target: { value: "api.example.com, *.example.com" },
    });
    fireEvent.change(methodsInput, { target: { value: "get, post" } });
    fireEvent.change(agentsInput, { target: { value: "ferris, felix" } });
    fireEvent.click(privateNetwork);

    fireEvent.click(screen.getByRole("button", { name: /Save Secret/i }));

    await waitFor(() => {
      expect(onSetSecret).toHaveBeenCalledWith({
        name: "NEW_API_KEY",
        value: "s3cr3t-value",
        description: undefined,
        allowed_hosts: ["api.example.com", "*.example.com"],
        allowed_methods: ["get", "post"],
        allowed_agents: ["ferris", "felix"],
        allow_private_network: true,
        lease_ttl_secs: undefined,
      });
    });
    await waitFor(() => {
      expect(valueInput.value).toBe("");
    });
  });

  it("rejects an invalid secret name before calling the API", async () => {
    const onSetSecret = vi.fn(async () => ({ secret: SAMPLE_SECRETS[0] }));

    render(
      <VaultGatePane
        secrets={[]}
        connectors={[]}
        authority={OWNER}
        onSetSecret={onSetSecret}
      />,
    );

    fireEvent.change(screen.getByLabelText(/Secret Name/i), {
      target: { value: "bad-name!" },
    });
    fireEvent.change(screen.getByLabelText(/Secret Value/i), {
      target: { value: "x" },
    });
    fireEvent.change(screen.getByLabelText(/Allowed Hosts/i), {
      target: { value: "api.example.com" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Save Secret/i }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/A-Z, 0-9/i);
    expect(onSetSecret).not.toHaveBeenCalled();
  });

  it("deletes a secret for the owner after inline confirmation", async () => {
    const onDeleteSecret = vi.fn(async () => ({ removed: true, name: "DISCORD_BOT_TOKEN" }));

    render(
      <VaultGatePane
        secrets={SAMPLE_SECRETS}
        connectors={SAMPLE_CONNECTORS}
        authority={OWNER}
        onDeleteSecret={onDeleteSecret}
      />,
    );

    fireEvent.click(screen.getByTestId("vault-delete-DISCORD_BOT_TOKEN"));
    expect(onDeleteSecret).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("vault-confirm-delete-DISCORD_BOT_TOKEN"));

    await waitFor(() => {
      expect(onDeleteSecret).toHaveBeenCalledWith("DISCORD_BOT_TOKEN");
    });
  });

  it("shows empty states when no secrets or connectors exist", () => {
    render(<VaultGatePane secrets={[]} connectors={[]} authority={PEER} />);

    expect(screen.getByTestId("vault-secrets-empty")).toBeInTheDocument();
    expect(screen.getByTestId("gate-connectors-empty")).toBeInTheDocument();
  });
});

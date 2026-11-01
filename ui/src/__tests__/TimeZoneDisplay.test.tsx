import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import {
  ArtifactPreviewPane,
  formatVersionTime,
} from "../components/ArtifactPreviewPane";
import { FerroStatePane } from "../components/FerroStatePane";
import { SettingsModal } from "../components/SettingsModal";
import { TurnCard } from "../components/TurnCard";
import { VaultGatePane } from "../components/VaultGatePane";
import { WireMailPane } from "../components/WireMailPane";
import {
  formatTimestamp,
  getActiveTimeZone,
  normalizeTimeZone,
  setActiveTimeZone,
} from "../lib/datetime";
import { DEFAULT_HUB_SETTINGS, type HubSettings } from "../types/api";

describe("TimeZone option & timestamp formatting across FerroDeck+", () => {
  beforeEach(() => {
    window.localStorage.clear();
    setActiveTimeZone("UTC");
  });

  afterEach(() => {
    window.localStorage.clear();
    setActiveTimeZone("UTC");
  });

  it("normalizes timezones and formats UTC, IANA zones, local, and non-date strings cleanly", () => {
    expect(normalizeTimeZone("utc")).toBe("UTC");
    expect(normalizeTimeZone("local")).toBe("local");
    expect(normalizeTimeZone("America/New_York")).toBe("America/New_York");
    expect(normalizeTimeZone("../etc/passwd")).toBe("UTC");
    expect(normalizeTimeZone("/America/New_York")).toBe("UTC");
    expect(normalizeTimeZone("America/New_York/")).toBe("UTC");

    // Default UTC preserves clean ISO-8601 Z strings untouched
    expect(formatTimestamp("2026-10-27T14:30:00Z", "UTC")).toBe("2026-10-27T14:30:00Z");

    // Default UTC formats epoch ms as YYYY-MM-DD HH:MM:SS UTC
    const epochMs = Date.parse("2026-10-27T14:30:00Z");
    expect(formatTimestamp(epochMs, "UTC")).toBe("2026-10-27 14:30:00 UTC");
    expect(formatVersionTime(epochMs, "UTC")).toBe("2026-10-27 14:30:00 UTC");

    // IANA timezones convert both ISO strings and epoch ms
    // 2026-10-27T14:30:00Z is EDT (UTC-4) -> 10:30:00 and PDT (UTC-7) -> 07:30:00
    expect(formatTimestamp("2026-10-27T14:30:00Z", "America/New_York")).toMatch(
      /^2026-10-27 10:30:00/,
    );
    expect(formatTimestamp("2026-10-27T14:30:00Z", "America/Los_Angeles")).toMatch(
      /^2026-10-27 07:30:00/,
    );
    expect(formatVersionTime(epochMs, "America/New_York")).toMatch(
      /^2026-10-27 10:30:00/,
    );

    // Non-date strings pass through untouched
    expect(formatTimestamp("Never", "America/New_York")).toBe("Never");
    expect(formatTimestamp("now", "America/New_York")).toBe("now");
    expect(formatTimestamp(null, "America/New_York")).toBe("");
  });

  it("saves Time Zone in SettingsModal via PUT /api/v1/settings and updates active timezone", async () => {
    let savedPayload: Partial<HubSettings> | null = null;
    const initialSettings: HubSettings = {
      ...DEFAULT_HUB_SETTINGS,
      timezone: "UTC",
    };

    const fetchImpl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      const json = (body: unknown) =>
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });

      if (url.endsWith("/api/v1/settings") && method === "GET") {
        return json(initialSettings);
      }
      if (url.endsWith("/api/v1/settings") && method === "PUT") {
        savedPayload = JSON.parse(String(init?.body)) as Partial<HubSettings>;
        return json(savedPayload);
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const onSettingsUpdated = vi.fn();

    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        onSettingsUpdated={onSettingsUpdated}
        apiClient={apiClient}
      />,
    );

    const tzSelect = (await screen.findByTestId(
      "settings-timezone-select",
    )) as HTMLSelectElement;
    await waitFor(() => {
      expect(tzSelect.value).toBe("UTC");
    });

    fireEvent.change(tzSelect, { target: { value: "America/New_York" } });
    fireEvent.click(screen.getByTestId("save-settings-btn"));

    expect(await screen.findByTestId("settings-saved-banner")).toHaveTextContent(
      /Settings saved/i,
    );
    expect(savedPayload).toMatchObject({
      timezone: "America/New_York",
    });
    expect(getActiveTimeZone()).toBe("America/New_York");
    expect(onSettingsUpdated).toHaveBeenCalledWith(
      expect.objectContaining({ timezone: "America/New_York" }),
    );
  });

  it("updates displayed timestamps in TurnCard, WireMailPane, FerroStatePane, VaultGatePane, and ArtifactPreviewPane when active timezone changes", async () => {
    const isoUtc = "2026-10-27T14:30:00Z";
    const epochMs = Date.parse(isoUtc);

    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const json = (body: unknown) =>
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      if (url.includes("/api/v1/artifacts/list")) {
        return json({
          artifacts: [
            {
              path: "/tmp/spec.md",
              name: "spec.md",
              kind: "markdown",
              size_bytes: 12,
              Archived: false,
            },
          ],
        });
      }
      if (url.includes("/api/v1/artifacts/preview")) {
        return json({
          path: "/tmp/spec.md",
          name: "spec.md",
          kind: "markdown",
          mime_type: "text/markdown",
          size_bytes: 12,
          content: "# Spec",
        });
      }
      if (url.includes("/api/v1/artifacts/versions")) {
        return json({
          path: "/tmp/spec.md",
          versions: [
            {
              version: 1,
              sha256: "abcdef1234567890",
              size_bytes: 12,
              recorded_at_ms: epochMs,
              is_current: true,
            },
          ],
        });
      }
      return json({});
    });

    const apiClient = new HubApiClient({
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });

    render(
      <div>
        <TurnCard
          turn={{
            index: 1,
            role: "assistant",
            content: "Hello operator",
            tool_calls: [],
            created_at: isoUtc,
          }}
        />
        <WireMailPane
          selectedAgentName="felix"
          messages={[
            {
              message: {
                id: "m-1",
                thread_id: "t-1",
                sender_name: "ferris",
                to: "felix",
                subject: "Timezone check",
                body: "Verify display",
                urgency: "normal",
                kind: "direct",
                durable: true,
                created_at: isoUtc,
              },
              receipt: {
                message_id: "m-1",
                recipient_id: "r-felix",
                recipient_name: "felix",
                status: "acked",
                attempts: 1,
                updated_at: isoUtc,
                acked_at: isoUtc,
              },
            },
          ]}
        />
        <FerroStatePane
          entries={[
            {
              id: "st-1",
              scope: "workspace",
              concern: "memory",
              slug: "tz-note",
              updated_at: isoUtc,
              by: "felix",
              sensitive: false,
              ever_sensitive: false,
              head_revision: "1",
              payload: {
                memory: {
                  category: "note",
                  basis: "measured",
                  lifecycle: "active",
                  summary: "Timezone note",
                  body: "Body",
                  pinned: false,
                },
              },
            },
          ]}
        />
        <VaultGatePane
          secrets={[
            {
              name: "OPENAI_API_KEY",
              description: "Test key",
              allowed_hosts: ["api.openai.com"],
              allowed_methods: ["POST"],
              allowed_agents: ["*"],
              allow_private_network: false,
              created_at: isoUtc,
              updated_at: isoUtc,
              expires_at: isoUtc,
            },
          ]}
        />
        <ArtifactPreviewPane
          apiClient={apiClient}
          activePath="/tmp/spec.md"
        />
      </div>,
    );

    // Open ArtifactPreviewPane version history
    const historyBtn = await screen.findByTestId("artifact-history-toggle");
    fireEvent.click(historyBtn);

    // Initially in UTC
    expect(screen.getAllByText(isoUtc).length).toBeGreaterThanOrEqual(4);
    expect(await screen.findByText("2026-10-27 14:30:00 UTC")).toBeInTheDocument();

    // Switch active timezone to America/New_York (EDT = UTC-4 -> 2026-10-27 10:30:00)
    act(() => {
      setActiveTimeZone("America/New_York");
    });

    await waitFor(() => {
      const matches = screen.getAllByText(/^2026-10-27 10:30:00/);
      expect(matches.length).toBeGreaterThanOrEqual(5);
    });

    // Switch active timezone to America/Los_Angeles (PDT = UTC-7 -> 2026-10-27 07:30:00)
    act(() => {
      setActiveTimeZone("America/Los_Angeles");
    });

    await waitFor(() => {
      const matches = screen.getAllByText(/^2026-10-27 07:30:00/);
      expect(matches.length).toBeGreaterThanOrEqual(5);
    });
  });
});

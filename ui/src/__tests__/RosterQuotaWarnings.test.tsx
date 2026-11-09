import React from "react";
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { RosterSidebar } from "../components/RosterSidebar";
import { AgentSettingsModal } from "../components/AgentSettingsModal";
import {
  evaluateQuotaStatus,
  resolveRosterStatusDot,
  QUOTA_STALE_MS,
} from "../utils/quotaStatus";
import type {
  AgentLivenessItem,
  AgentRecord,
  DiscoveredSessionSummary,
  SessionQuotaUsage,
} from "../types/api";

const FIXED_NOW_MS = 1_760_000_000_000;

function makeAgent(nameKey: string, displayName: string, role = "web-developer"): AgentRecord {
  return {
    id: `urn:fl:agent:${nameKey}`,
    name_key: nameKey,
    display_name: displayName,
    project: "FerroLoop",
    role,
    status: "active",
  };
}

function makeLiveness(
  agent: string,
  liveness: "alive" | "stale" | "dead" = "alive",
  adapter = "claude",
): AgentLivenessItem {
  return {
    agent,
    role: "web-developer",
    project: "FerroLoop",
    adapter,
    has_route: liveness !== "dead",
    verdict: {
      state: liveness,
      status: liveness === "alive" ? "live" : liveness === "stale" ? "unverifiable" : "exited",
    },
  };
}

function makeSession(
  id: string,
  persona: string,
  adapter: string,
  model: string,
  quota_usage: SessionQuotaUsage | null,
  status = "running",
): DiscoveredSessionSummary {
  return {
    id,
    source: "route",
    persona,
    adapter,
    model,
    status,
    updated_at: new Date(FIXED_NOW_MS).toISOString(),
    quota_usage,
  };
}

describe("Issue #352: Roster & AgentSettingsModal vendor usage allowance warnings", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  describe("evaluateQuotaStatus & threshold boundaries", () => {
    it("evaluates >20% remaining (e.g. 20.1% and 50%) as normal", () => {
      const at50 = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        model: "claude-opus-4-6",
        quotaUsage: {
          tier: "max",
          observed_at_ms: FIXED_NOW_MS - 60_000,
          primary_window: {
            used_percent: 50,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 7_200_000,
          },
        },
      });
      expect(at50.status).toBe("normal");
      expect(at50.reasonKind).toBe("normal");
      expect(at50.limitingWindow?.remainingPercent).toBe(50);
      expect(at50.limitingWindow?.windowLabel).toBe("5h");

      const at20Point1 = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 30_000,
          primary_window: {
            used_percent: 79.9,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 3_600_000,
          },
        },
      });
      expect(at20Point1.status).toBe("normal");
      expect(at20Point1.reasonKind).toBe("normal");
    });

    it("evaluates 10-20% remaining (20%, 15%, 10.1%) as yellow warning", () => {
      for (const usedPercent of [80, 85, 89.9]) {
        const res = evaluateQuotaStatus({
          nowMs: FIXED_NOW_MS,
          vendor: "claude",
          quotaUsage: {
            observed_at_ms: FIXED_NOW_MS - 60_000,
            primary_window: {
              used_percent: usedPercent,
              window_minutes: 300,
              resets_at_ms: FIXED_NOW_MS + 4_800_000,
            },
          },
        });
        expect(res.status).toBe("yellow");
        expect(res.reasonKind).toBe("approaching_yellow");
        expect(res.shortReason).toContain("remaining");
        expect(res.shortReason).toContain("5h window");
        expect(res.shortReason).toContain("resets in 1h 20m");
      }
    });

    it("evaluates 1-10% remaining (10%, 5%, 1%) as orange warning", () => {
      for (const usedPercent of [90, 95, 99]) {
        const res = evaluateQuotaStatus({
          nowMs: FIXED_NOW_MS,
          vendor: "codex",
          quotaUsage: {
            tier: "pro",
            observed_at_ms: FIXED_NOW_MS - 60_000,
            primary_window: {
              used_percent: usedPercent,
              window_minutes: 300,
              resets_at_ms: FIXED_NOW_MS + 2_700_000,
            },
          },
        });
        expect(res.status).toBe("orange");
        expect(res.reasonKind).toBe("approaching_orange");
        expect(res.shortReason).toContain("remaining");
        expect(res.shortReason).toContain("resets in 45m");
      }
    });

    it("evaluates <1% remaining (0.5% remaining / 99.5% used) as red below_1_percent without claiming exhaustion/failure", () => {
      const res = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 15_000,
          primary_window: {
            used_percent: 99.5,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        },
      });
      expect(res.status).toBe("red");
      expect(res.reasonKind).toBe("below_1_percent");
      expect(res.shortReason).toContain("<1% remaining");
      expect(res.shortReason).not.toContain("Quota exhausted");
      expect(res.detailText).toContain("Critical (<1% remaining)");
      expect(res.detailText).toContain("99.5% used");
      expect(res.detailText).not.toMatch(/failed/i);
    });

    it("evaluates confirmed exhaustion (100% used or exhausted_reason) as red exhausted with reset time", () => {
      const by100 = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 10_000,
          primary_window: {
            used_percent: 100,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        },
      });
      expect(by100.status).toBe("red");
      expect(by100.reasonKind).toBe("exhausted");
      expect(by100.shortReason).toContain("Quota exhausted");
      expect(by100.shortReason).toContain("resets in 30m");

      const byReason = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "codex",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 10_000,
          exhausted_reason: "usage_limit_reached",
          primary_window: {
            used_percent: 94,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 900_000,
          },
        },
      });
      expect(byReason.status).toBe("red");
      expect(byReason.reasonKind).toBe("exhausted");
      expect(byReason.detailText).toContain("Quota exhausted");
      expect(byReason.detailText).toContain("reason: usage_limit_reached");
      expect(byReason.detailText).toContain("resets in 15m");
    });
  });

  describe("Limiting window selection", () => {
    it("selects secondary_window (7d) when it has lower remaining % than primary_window (5h)", () => {
      const res = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 5_000,
          primary_window: {
            used_percent: 60,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 3_600_000,
          },
          secondary_window: {
            used_percent: 88,
            window_minutes: 10080,
            resets_at_ms: FIXED_NOW_MS + 86_400_000,
          },
        },
      });
      expect(res.status).toBe("yellow");
      expect(res.limitingWindow?.key).toBe("secondary");
      expect(res.limitingWindow?.windowLabel).toBe("7d");
      expect(res.limitingWindow?.remainingPercent).toBe(12);
    });

    it("selects primary_window (5h) when it has lower remaining % than secondary_window (7d)", () => {
      const res = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 5_000,
          primary_window: {
            used_percent: 95,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 3_600_000,
          },
          secondary_window: {
            used_percent: 30,
            window_minutes: 10080,
            resets_at_ms: FIXED_NOW_MS + 86_400_000,
          },
        },
      });
      expect(res.status).toBe("orange");
      expect(res.limitingWindow?.key).toBe("primary");
      expect(res.limitingWindow?.windowLabel).toBe("5h");
      expect(res.limitingWindow?.remainingPercent).toBe(5);
    });
  });

  describe("Past-reset (resets_at_ms <= now) and stale / missing observed_at_ms rules", () => {
    it("clears windows and exhausted_reason whose resets_at_ms <= nowMs and returns unknown (never red/exhausted)", () => {
      const expired = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 120_000,
          exhausted_reason: "5h window exhausted",
          primary_window: {
            used_percent: 100,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS - 10_000,
          },
        },
      });
      expect(expired.status).toBe("unknown");
      expect(expired.reasonKind).toBe("expired");
      expect(expired.limitingWindow).toBeNull();
    });

    it("clears a generic exhausted_reason (e.g. usage_limit_reached) when primary_window expired and secondary_window is <100%, but keeps it when secondary_window is 100%", () => {
      const clearedWithGenericReason = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "codex",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 60_000,
          exhausted_reason: "usage_limit_reached",
          primary_window: {
            used_percent: 100,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS - 5_000,
          },
          secondary_window: {
            used_percent: 40,
            window_minutes: 10080,
            resets_at_ms: FIXED_NOW_MS + 86_400_000,
          },
        },
      });
      expect(clearedWithGenericReason.status).toBe("normal");
      expect(clearedWithGenericReason.reasonKind).toBe("normal");
      expect(clearedWithGenericReason.exhaustedReason).toBeNull();
      expect(clearedWithGenericReason.limitingWindow?.key).toBe("secondary");
      expect(clearedWithGenericReason.limitingWindow?.remainingPercent).toBe(60);

      const keptWhenSecondaryAt100 = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "codex",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 60_000,
          exhausted_reason: "usage_limit_reached",
          primary_window: {
            used_percent: 100,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS - 5_000,
          },
          secondary_window: {
            used_percent: 100,
            window_minutes: 10080,
            resets_at_ms: FIXED_NOW_MS + 86_400_000,
          },
        },
      });
      expect(keptWhenSecondaryAt100.status).toBe("red");
      expect(keptWhenSecondaryAt100.reasonKind).toBe("exhausted");
      expect(keptWhenSecondaryAt100.exhaustedReason).toBe("usage_limit_reached");
    });

    it("reports usedPercent: null and summaryLabel 'exhausted, <age>' on windowless stale exhaustion without fabricating 100%", () => {
      const threeHoursAgo = FIXED_NOW_MS - 3 * QUOTA_STALE_MS;
      const staleWindowless = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: threeHoursAgo,
          exhausted_reason: "rate_limit_exceeded",
        },
      });
      expect(staleWindowless.status).toBe("unknown");
      expect(staleWindowless.reasonKind).toBe("stale");
      expect(staleWindowless.staleReading?.usedPercent).toBeNull();
      expect(staleWindowless.staleReading?.remainingPercent).toBeNull();
      expect(staleWindowless.staleReading?.summaryLabel).toBe("exhausted, 3 h ago");
    });

    it("treats telemetry older than QUOTA_STALE_MS (1h) as unknown on the dot and records staleReading with age", () => {
      const threeHoursAgo = FIXED_NOW_MS - 3 * QUOTA_STALE_MS;
      const stale = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: threeHoursAgo,
          primary_window: {
            used_percent: 92,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 3_600_000,
          },
        },
      });
      expect(stale.status).toBe("unknown");
      expect(stale.reasonKind).toBe("stale");
      expect(stale.staleReading).not.toBeNull();
      expect(stale.staleReading?.summaryLabel).toBe("92% used, 3 h ago");
      expect(stale.shortReason).toContain("92% used, 3 h ago");
      expect(stale.detailText).toContain("Unknown (stale: 92% used, 3 h ago, 5h window)");
    });

    it("treats missing observed_at_ms on a window reading as unknown (stale) and never normal/healthy", () => {
      const missingObserved = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: null,
          primary_window: {
            used_percent: 50,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 3_600_000,
          },
        },
      });
      expect(missingObserved.status).toBe("unknown");
      expect(missingObserved.reasonKind).toBe("stale");
      expect(missingObserved.staleReading?.summaryLabel).toBe("50% used, age unknown");
    });

    it("treats null / unsupported quota_usage as unknown (missing)", () => {
      const missing = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "muse",
        quotaUsage: null,
      });
      expect(missing.status).toBe("unknown");
      expect(missing.reasonKind).toBe("missing");
      expect(missing.detailText).toContain("Unknown");
    });
  });

  describe("RosterSidebar status dot rendering, account/model-pool isolation, and offline + quota separation", () => {
    it("renders yellow, orange, red, and unknown dots on the existing status dot without adding extra badges", () => {
      vi.spyOn(Date, "now").mockReturnValue(FIXED_NOW_MS);

      const agents: AgentRecord[] = [
        makeAgent("felix", "Felix"),
        makeAgent("ferris", "Ferris"),
        makeAgent("jarvis", "JARVIS"),
        makeAgent("athena", "Athena"),
        makeAgent("hermes", "Hermes"),
      ];
      const liveness: AgentLivenessItem[] = [
        makeLiveness("felix", "alive", "claude"),
        makeLiveness("ferris", "alive", "codex"),
        makeLiveness("jarvis", "alive", "claude"),
        makeLiveness("athena", "alive", "claude"),
        makeLiveness("hermes", "dead", "claude"),
      ];
      const sessions: DiscoveredSessionSummary[] = [
        // Felix: 15% remaining -> yellow
        makeSession("sess-felix", "felix", "claude", "claude-opus-4-6", {
          tier: "max",
          observed_at_ms: FIXED_NOW_MS - 30_000,
          primary_window: {
            used_percent: 85,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 4_800_000,
          },
        }),
        // Ferris: 5% remaining -> orange
        makeSession("sess-ferris", "ferris", "codex", "gpt-5.4", {
          tier: "pro",
          observed_at_ms: FIXED_NOW_MS - 30_000,
          primary_window: {
            used_percent: 95,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 2_700_000,
          },
        }),
        // JARVIS: 99.6% used (<1% remaining) -> red (below_1_percent)
        makeSession("sess-jarvis", "jarvis", "claude", "claude-opus-4-6", {
          tier: "max",
          observed_at_ms: FIXED_NOW_MS - 30_000,
          primary_window: {
            used_percent: 99.6,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        }),
        // Athena: 92% used, 3 hours old -> stale -> unknown on dot
        makeSession("sess-athena", "athena", "claude", "claude-sonnet-4-6", {
          tier: "max",
          observed_at_ms: FIXED_NOW_MS - 3 * QUOTA_STALE_MS,
          primary_window: {
            used_percent: 92,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 3_600_000,
          },
        }),
        // Hermes: offline (dead) + 100% exhausted -> red dot with both offline and quota reason
        makeSession(
          "sess-hermes",
          "hermes",
          "claude",
          "claude-opus-4-6",
          {
            tier: "max",
            observed_at_ms: FIXED_NOW_MS - 20_000,
            exhausted_reason: "5h limit reached",
            primary_window: {
              used_percent: 100,
              window_minutes: 300,
              resets_at_ms: FIXED_NOW_MS + 1_800_000,
            },
          },
          "stopped",
        ),
      ];

      render(
        <RosterSidebar
          agents={agents}
          routes={[]}
          sessions={sessions}
          liveness={liveness}
          unreadByAgent={{}}
          selectedAgentName={null}
          onSelectAgent={() => {}}
        />,
      );

      const felixDot = screen.getByTestId("liveness-dot-felix");
      expect(felixDot).toHaveAttribute("data-liveness-state", "alive");
      expect(felixDot).toHaveAttribute("data-quota-state", "yellow");
      expect(felixDot).toHaveAttribute("data-quota-reason", "approaching_yellow");
      expect(felixDot.className).toBe("fd-dot fd-dot-alive fd-dot-quota-yellow");
      expect(felixDot.getAttribute("title")).toContain("Liveness: alive");
      expect(felixDot.getAttribute("title")).toContain(
        "Quota warning: 15% remaining (5h window, 85% used, resets in 1h 20m)",
      );

      const ferrisDot = screen.getByTestId("liveness-dot-ferris");
      expect(ferrisDot).toHaveAttribute("data-liveness-state", "alive");
      expect(ferrisDot).toHaveAttribute("data-quota-state", "orange");
      expect(ferrisDot).toHaveAttribute("data-quota-reason", "approaching_orange");
      expect(ferrisDot.className).toBe("fd-dot fd-dot-alive fd-dot-quota-orange");
      expect(ferrisDot.getAttribute("title")).toContain("Liveness: alive");
      expect(ferrisDot.getAttribute("title")).toContain(
        "Quota warning: 5% remaining (5h window, 95% used, resets in 45m)",
      );

      const jarvisDot = screen.getByTestId("liveness-dot-jarvis");
      expect(jarvisDot).toHaveAttribute("data-liveness-state", "alive");
      expect(jarvisDot).toHaveAttribute("data-quota-state", "red");
      expect(jarvisDot).toHaveAttribute("data-quota-reason", "below_1_percent");
      expect(jarvisDot.className).toBe("fd-dot fd-dot-alive fd-dot-quota-red");
      expect(jarvisDot.getAttribute("title")).toContain(
        "Liveness: alive · Quota critical: <1% remaining (0.4% remaining, 5h window, 99.6% used, resets in 30m)",
      );

      const athenaDot = screen.getByTestId("liveness-dot-athena");
      expect(athenaDot).toHaveAttribute("data-liveness-state", "alive");
      expect(athenaDot).toHaveAttribute("data-quota-state", "unknown");
      expect(athenaDot).toHaveAttribute("data-quota-reason", "stale");
      expect(athenaDot.className).toBe("fd-dot fd-dot-alive");
      expect(athenaDot.getAttribute("title")).toContain(
        "Liveness: alive · Quota: unknown (92% used, 3 h ago, 5h window)",
      );

      const hermesDot = screen.getByTestId("liveness-dot-hermes");
      expect(hermesDot).toHaveAttribute("data-liveness-state", "dead");
      expect(hermesDot).toHaveAttribute("data-quota-state", "red");
      expect(hermesDot).toHaveAttribute("data-quota-reason", "exhausted");
      expect(hermesDot.className).toBe("fd-dot fd-dot-dead fd-dot-quota-red");
      expect(hermesDot.getAttribute("title")).toContain("Liveness: dead (process offline)");
      expect(hermesDot.getAttribute("title")).toContain(
        "Quota exhausted (5h window, 0% remaining, 100% used, resets in 30m, reason: 5h limit reached)",
      );
    });

    it("isolates Antigravity model pools (3p pool vs gemini pool) and separate Codex accounts across agents", () => {
      vi.spyOn(Date, "now").mockReturnValue(FIXED_NOW_MS);

      const agents: AgentRecord[] = [
        makeAgent("agy-claude", "AgyClaude"),
        makeAgent("agy-gemini", "AgyGemini"),
        makeAgent("codex-a", "CodexAccountA"),
        makeAgent("codex-b", "CodexAccountB"),
      ];
      const liveness: AgentLivenessItem[] = [
        makeLiveness("agy-claude", "alive", "antigravity"),
        makeLiveness("agy-gemini", "alive", "antigravity"),
        makeLiveness("codex-a", "alive", "codex"),
        makeLiveness("codex-b", "alive", "codex"),
      ];
      const sessions: DiscoveredSessionSummary[] = [
        // Antigravity agent on Claude model -> 3p pool exhausted
        makeSession("s-agy-claude", "agy-claude", "antigravity", "claude-opus-4-6", {
          tier: "ultra",
          observed_at_ms: FIXED_NOW_MS - 10_000,
          primary_window: {
            used_percent: 100,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        }),
        // Antigravity agent on Gemini model -> gemini pool healthy (25% used / 75% remaining)
        makeSession("s-agy-gemini", "agy-gemini", "antigravity", "gemini-3.1-pro-high", {
          tier: "ultra",
          observed_at_ms: FIXED_NOW_MS - 10_000,
          primary_window: {
            used_percent: 25,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        }),
        // Codex Account A -> 95% used (orange)
        makeSession("s-codex-a", "codex-a", "codex", "gpt-5.4", {
          tier: "plus",
          observed_at_ms: FIXED_NOW_MS - 10_000,
          primary_window: {
            used_percent: 95,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        }),
        // Codex Account B -> 10% used (normal)
        makeSession("s-codex-b", "codex-b", "codex", "gpt-5.4", {
          tier: "pro",
          observed_at_ms: FIXED_NOW_MS - 10_000,
          primary_window: {
            used_percent: 10,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        }),
      ];

      render(
        <RosterSidebar
          agents={agents}
          routes={[]}
          sessions={sessions}
          liveness={liveness}
          unreadByAgent={{}}
          selectedAgentName={null}
          onSelectAgent={() => {}}
        />,
      );

      const agyClaudeDot = screen.getByTestId("liveness-dot-agy-claude");
      expect(agyClaudeDot).toHaveAttribute("data-quota-state", "red");
      expect(agyClaudeDot).toHaveAttribute("data-quota-pool", "antigravity · 3p pool (ultra)");

      const agyGeminiDot = screen.getByTestId("liveness-dot-agy-gemini");
      expect(agyGeminiDot).toHaveAttribute("data-quota-state", "normal");
      expect(agyGeminiDot).toHaveAttribute("data-quota-pool", "antigravity · gemini pool (ultra)");

      const codexADot = screen.getByTestId("liveness-dot-codex-a");
      expect(codexADot).toHaveAttribute("data-quota-state", "orange");
      expect(codexADot).toHaveAttribute("data-quota-pool", "codex (plus)");

      const codexBDot = screen.getByTestId("liveness-dot-codex-b");
      expect(codexBDot).toHaveAttribute("data-quota-state", "normal");
      expect(codexBDot).toHaveAttribute("data-quota-pool", "codex (pro)");
    });
  });

  describe("AgentSettingsModal Status & Allowance detail", () => {
    const baseModalProps = {
      isOpen: true,
      onClose: () => {},
      agentName: "felix",
      agentDisplayName: "Felix",
      agentRole: "web-developer",
      agentProject: "FerroLoop",
      adapter: "antigravity",
      sessionId: "sess-felix",
      selectedModel: "claude-opus-5-5",
      selectedThinkingLevel: "high",
      availableModels: ["gemini-4-argon", "claude-opus-5-5"],
      onChangeModelAndThinking: () => {},
      contextEstimate: {
        usedTokens: 24_000,
        maxTokens: 200_000,
        percent: 12,
        source: "reported" as const,
      },
      executionMode: "auto",
      turnCount: 3,
      canCompact: false,
      compacting: false,
      onCompactContext: () => {},
      canClear: true,
      clearing: false,
      onOpenClearModal: () => {},
      canDecommission: true,
      decommissioning: false,
      onOpenDecommissionModal: () => {},
    };

    it("surfaces offline process liveness and quota state together without conflating them, and never renders forbidden legacy test IDs", () => {
      vi.spyOn(Date, "now").mockReturnValue(FIXED_NOW_MS);

      render(
        <AgentSettingsModal
          {...baseModalProps}
          livenessState="dead"
          quotaUsage={{
            tier: "ultra",
            observed_at_ms: FIXED_NOW_MS - 30_000,
            exhausted_reason: "3p_pool_exhausted",
            primary_window: {
              used_percent: 100,
              window_minutes: 300,
              resets_at_ms: FIXED_NOW_MS + 2_400_000,
            },
          }}
        />,
      );

      const statusDetail = screen.getByTestId("agent-settings-status-detail");
      expect(statusDetail).toHaveAttribute("data-liveness-state", "dead");
      expect(statusDetail).toHaveAttribute("data-quota-state", "red");
      expect(statusDetail).toHaveAttribute("data-quota-reason", "exhausted");
      expect(statusDetail).toHaveAttribute("data-quota-pool", "antigravity · 3p pool (ultra)");

      expect(screen.getByTestId("agent-settings-liveness-detail")).toHaveTextContent(
        "Process: Offline (dead)",
      );
      const allowanceDetail = screen.getByTestId("agent-settings-allowance-detail");
      expect(allowanceDetail).toHaveTextContent(
        "Allowance: Quota exhausted · 5h window, 0% remaining, 100% used, resets in 40m, reason: 3p_pool_exhausted",
      );
      expect(allowanceDetail.style.color).toBe("var(--status-danger)");
      expect(screen.getByTestId("agent-settings-offline-quota-note")).toHaveTextContent(
        "Process is offline; usage allowance is reported separately (quota exhausted).",
      );

      // Compatibility check: legacy removed test IDs must never exist
      expect(screen.queryByTestId("agent-settings-quota-summary")).toBeNull();
      expect(screen.queryByTestId("quota-usage-badge")).toBeNull();
      expect(screen.queryByTestId("quota-tier-chip")).toBeNull();
      expect(screen.queryByTestId("quota-primary-window")).toBeNull();
      expect(screen.queryByTestId("quota-secondary-window")).toBeNull();
      expect(screen.queryByText("Vendor Account Quota")).toBeNull();
    });

    it("surfaces stale reading with its age in AgentSettingsModal and updates pool attribution when model changes", () => {
      vi.spyOn(Date, "now").mockReturnValue(FIXED_NOW_MS);

      const { rerender } = render(
        <AgentSettingsModal
          {...baseModalProps}
          livenessState="alive"
          selectedModel="claude-opus-5-5"
          quotaUsage={{
            tier: "ultra",
            observed_at_ms: FIXED_NOW_MS - 3 * QUOTA_STALE_MS,
            primary_window: {
              used_percent: 92,
              window_minutes: 300,
              resets_at_ms: FIXED_NOW_MS + 3_600_000,
            },
          }}
        />,
      );

      expect(screen.getByTestId("agent-settings-status-detail")).toHaveAttribute(
        "data-quota-state",
        "unknown",
      );
      expect(screen.getByTestId("agent-settings-allowance-detail")).toHaveTextContent(
        "Allowance: Unknown (stale: 92% used, 3 h ago, 5h window)",
      );
      expect(screen.getByTestId("agent-settings-allowance-pool")).toHaveTextContent(
        "Pool: antigravity · 3p pool (ultra)",
      );

      // Switching model to gemini-4-argon updates pool attribution to gemini pool
      const modelSelect = screen.getByTestId("session-model-select");
      fireEvent.change(modelSelect, { target: { value: "gemini-4-argon" } });
      rerender(
        <AgentSettingsModal
          {...baseModalProps}
          livenessState="alive"
          selectedModel="gemini-4-argon"
          quotaUsage={{
            tier: "ultra",
            observed_at_ms: FIXED_NOW_MS - 30_000,
            primary_window: {
              used_percent: 88,
              window_minutes: 300,
              resets_at_ms: FIXED_NOW_MS + 1_800_000,
            },
          }}
        />,
      );

      expect(screen.getByTestId("agent-settings-allowance-pool")).toHaveTextContent(
        "Pool: antigravity · gemini pool (ultra)",
      );
      expect(screen.getByTestId("agent-settings-status-detail")).toHaveAttribute(
        "data-quota-state",
        "yellow",
      );
      expect(screen.getByTestId("agent-settings-allowance-detail")).toHaveTextContent(
        "Allowance: Approaching limit (10–20% remaining) · 12% remaining (5h window, 88% used, resets in 30m)",
      );

      // Switching adapter across vendors without a new quota reading resets quota to unknown for the new vendor
      rerender(
        <AgentSettingsModal
          {...baseModalProps}
          livenessState="alive"
          adapter="codex"
          selectedModel="gpt-5.4"
          quotaUsage={{
            tier: "ultra",
            observed_at_ms: FIXED_NOW_MS - 30_000,
            primary_window: {
              used_percent: 88,
              window_minutes: 300,
              resets_at_ms: FIXED_NOW_MS + 1_800_000,
            },
          }}
        />,
      );
      expect(screen.getByTestId("agent-settings-status-detail")).toHaveAttribute(
        "data-quota-state",
        "unknown",
      );
      expect(screen.getByTestId("agent-settings-allowance-pool")).toHaveTextContent(
        "Pool: codex",
      );
    });
  });

  describe("resolveRosterStatusDot precedence", () => {
    it("emits true liveness base class and only the matching quota class", () => {
      const quotaEval = evaluateQuotaStatus({
        nowMs: FIXED_NOW_MS,
        vendor: "claude",
        quotaUsage: {
          observed_at_ms: FIXED_NOW_MS - 10_000,
          primary_window: {
            used_percent: 94,
            window_minutes: 300,
            resets_at_ms: FIXED_NOW_MS + 1_800_000,
          },
        },
      });
      const resolved = resolveRosterStatusDot("dead", quotaEval);
      expect(resolved.className).toBe("fd-dot fd-dot-dead fd-dot-quota-orange");
      expect(resolved.accessibleLabel).toContain("Liveness: dead (process offline)");
      expect(resolved.accessibleLabel).toContain(
        "Quota warning: 6% remaining (5h window, 94% used, resets in 30m)",
      );
    });
  });
});

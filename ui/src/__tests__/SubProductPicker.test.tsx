import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import {
  SettingsModal,
  computeProductDependencyClosure,
  inferInstallProfilePreset,
  resolveProfileSubsystems,
} from "../components/SettingsModal";

function makeStubApiClient(): HubApiClient {
  const fetchImpl = vi.fn(async () =>
    new Response(JSON.stringify({}), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
  return new HubApiClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
}

describe("Record #84 (Issue #205 Phase B) — Sub-Product Component Picker, Profile Presets & Provenance Inspector", () => {
  it("computes transitive dependency closure and profile subsystem presets accurately", () => {
    // 1. Minimal selection (empty array still resolves required FerroLoop + embedded FerroStore)
    const minimal = computeProductDependencyClosure([], "minimal");
    expect(minimal.resolvedProducts).toEqual(["ferroloop", "ferrostore"]);
    expect(minimal.storeMode).toBe("embedded");
    expect(minimal.plannedUnits).toEqual([]);

    // 2. Selecting FerroDeck automatically pulls in FerroWire and FerroStore
    const deckOnly = computeProductDependencyClosure(["ferrodeck"], "full");
    expect(deckOnly.resolvedProducts).toContain("ferroloop");
    expect(deckOnly.resolvedProducts).toContain("ferrostore");
    expect(deckOnly.resolvedProducts).toContain("ferrodeck");
    expect(deckOnly.resolvedProducts).toContain("ferrowire");
    expect(deckOnly.autoIncludedBy.ferrowire).toContain("ferrodeck");
    expect(deckOnly.plannedUnits).toContain("ferroloop-hub.service");
    expect(deckOnly.plannedUnits).toContain("ferrowire.service");
    expect(deckOnly.plannedMcpCatalog).toContain("fl-wire-mcp");

    // 3. Lab profile enables ferrostore@ per-project socket daemons
    const labClosure = computeProductDependencyClosure(
      ["ferrodeck", "ferrostate", "ferrogate"],
      "lab",
    );
    expect(labClosure.storeMode).toBe("lab-socket-daemon");
    expect(labClosure.plannedUnits).toContain("ferrostore@ferroloop.service");
    expect(labClosure.plannedMcpCatalog).toEqual([
      "fl-wire-mcp",
      "fl-state-mcp",
      "fl-gate-mcp",
    ]);

    // 4. Profile presets resolve and round-trip via inferInstallProfilePreset
    expect(
      inferInstallProfilePreset(resolveProfileSubsystems("minimal"), false),
    ).toBe("minimal");
    expect(
      inferInstallProfilePreset(resolveProfileSubsystems("headless"), false),
    ).toBe("headless");
    expect(
      inferInstallProfilePreset(resolveProfileSubsystems("full"), false),
    ).toBe("full");
    expect(
      inferInstallProfilePreset(resolveProfileSubsystems("lab"), true),
    ).toBe("lab");
  });

  it("renders profile presets, enforces forward/reverse FerroDeck <-> FerroWire auto-closure, and inspects install receipt provenance", () => {
    const apiClient = makeStubApiClient();
    render(
      <SettingsModal
        isOpen={true}
        onClose={vi.fn()}
        initialTab="products"
        authority={{ tier: "owner", login: "lmcfarlin" }}
        apiClient={apiClient}
      />,
    );

    expect(
      screen.getByTestId("products-shared-cli-banner"),
    ).toHaveTextContent(
      "Shared CLI included; select the services and integrations to enable.",
    );

    const deckToggle = screen.getByTestId(
      "toggle-ferrodeck",
    ) as HTMLInputElement;
    const wireToggle = screen.getByTestId(
      "toggle-ferrowire",
    ) as HTMLInputElement;
    const stateToggle = screen.getByTestId(
      "toggle-ferrostate",
    ) as HTMLInputElement;

    // Apply 'minimal' preset -> all optional services unchecked
    fireEvent.click(screen.getByTestId("profile-preset-minimal"));
    expect(screen.getByTestId("active-install-profile-badge")).toHaveTextContent(
      "Profile: minimal",
    );
    expect(deckToggle.checked).toBe(false);
    expect(wireToggle.checked).toBe(false);
    expect(stateToggle.checked).toBe(false);
    expect(
      screen.getByTestId("install-plan-resolved-products"),
    ).toHaveTextContent("ferroloop, ferrostore");

    // Forward auto-closure: enabling FerroDeck while in minimal automatically enables FerroWire
    fireEvent.click(deckToggle);
    expect(deckToggle.checked).toBe(true);
    expect(wireToggle.checked).toBe(true);
    expect(
      screen.getByTestId("autoclose-badge-ferrowire"),
    ).toHaveTextContent("Required by FerroDeck");
    expect(screen.getByTestId("product-autoclose-notice")).toHaveTextContent(
      /Auto-enabled FerroWire/i,
    );

    // Reverse auto-closure: disabling FerroWire while FerroDeck is enabled automatically disables FerroDeck
    fireEvent.click(wireToggle);
    expect(wireToggle.checked).toBe(false);
    expect(deckToggle.checked).toBe(false);
    expect(screen.getByTestId("product-autoclose-notice")).toHaveTextContent(
      /Disabled FerroDeck because its required dependency FerroWire was disabled/i,
    );

    // Apply 'headless' preset -> CLI/daemons enabled, FerroDeck disabled
    fireEvent.click(screen.getByTestId("profile-preset-headless"));
    expect(screen.getByTestId("active-install-profile-badge")).toHaveTextContent(
      "Profile: headless",
    );
    expect(deckToggle.checked).toBe(false);
    expect(wireToggle.checked).toBe(true);
    expect(stateToggle.checked).toBe(true);

    // Apply 'lab' preset -> full + ferrostore@ socket daemon
    fireEvent.click(screen.getByTestId("profile-preset-lab"));
    expect(screen.getByTestId("active-install-profile-badge")).toHaveTextContent(
      "Profile: lab",
    );
    expect(screen.getByTestId("install-plan-store-mode")).toHaveTextContent(
      /ferrostore@ socket daemon/i,
    );
    expect(
      screen.getByTestId("embedded-ferrostore-dependency"),
    ).toHaveTextContent(/ferrostore@ socket daemon/i);

    // Receipt & Provenance Inspector: simulate unmanaged binary and adopt (--adopt)
    expect(screen.getByTestId("receipt-adoption-badge")).toHaveTextContent(
      "Managed Receipt (Schema v1)",
    );
    fireEvent.click(screen.getByTestId("receipt-simulate-unmanaged-btn"));
    expect(screen.getByTestId("receipt-adoption-badge")).toHaveTextContent(
      "Unmanaged Artifact (--adopt required)",
    );
    fireEvent.click(screen.getByTestId("receipt-adopt-btn"));
    expect(screen.getByTestId("receipt-adoption-badge")).toHaveTextContent(
      "Managed Receipt (Schema v1)",
    );
  });
});

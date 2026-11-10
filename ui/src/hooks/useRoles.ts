import { useEffect, useMemo, useRef, useState } from "react";
import { HubApiClient } from "../api/client";
import type { RoleRecord } from "../types/api";

export interface RoleOption {
  value: string;
  label: string;
}

/** Preserve wire identity while removing the origin prefix from fallback presentation. */
export function roleLabel(
  value: string,
  roles: readonly RoleRecord[] = [],
): string {
  const custom = /^custom:/i.test(value);
  const slug = value.replace(/^custom:/i, "");
  if (custom) {
    const structural = ["lead", "assistant", "lab-manager", "lab_manager"].includes(slug.toLowerCase());
    const label = roles.find((role) => role.slug === slug && (!structural || !role.builtin))?.label;
    return label ?? slug.replace(/[-_]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
  }
  const catalogSlug = slug === "lab_manager" ? "lab-manager" : slug;
  const label = roles.find((role) => role.slug === catalogSlug)?.label;
  if (catalogSlug === "assistant" && (!label || label === "Assisting Engineer")) return "Software Engineer";
  return label ?? slug.replace(/[-_]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
}

/** Fixed executable IDs determine structural roles, never catalog labels or flags. */
export function roleOptions(
  legacy: readonly RoleOption[],
  roles: readonly RoleRecord[],
): RoleOption[] {
  const options = new Map(
    legacy.map((option) => [option.value, { value: option.value, label: roleLabel(option.value, roles) }]),
  );
  for (const role of roles) {
    const slug = role.slug.toLowerCase();
    const compatible =
      slug === "lab-manager" ? ["lab-manager", "lab_manager"] : [slug];
    const value =
      compatible.find((id) => options.has(id)) ?? `custom:${role.slug}`;
    options.set(value, { value, label: roleLabel(value, roles) });
  }
  return Array.from(options.values()).sort((a, b) => a.label.localeCompare(b.label));
}

/** Keep an existing unknown selection in alphabetical order while its catalog loads. */
export function selectedRoleOptions(options: RoleOption[], value: string, roles: readonly RoleRecord[]): RoleOption[] {
  if (!value || value === "__create_custom__" || options.some((o) => o.value === value)) return options;
  return [...options, { value, label: roleLabel(value, roles) }].sort((a, b) => a.label.localeCompare(b.label));
}

/** A modal session owns its catalog request and reconciles acknowledged mutations. */
export function useRoles(enabled: boolean, apiClient?: HubApiClient) {
  const client = useMemo(() => apiClient ?? new HubApiClient(), [apiClient]);
  const [roles, setRoles] = useState<RoleRecord[]>([]);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [mutationError, setMutationError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const session = useRef(0);
  const busy = useRef(false);
  // Only retain overlays while this session's initial catalog is pending.
  const pendingCatalog = useRef<Map<string, RoleRecord | null> | null>(null);

  useEffect(() => {
    const currentSession = ++session.current;
    const overlays = new Map<string, RoleRecord | null>();
    pendingCatalog.current = enabled ? overlays : null;
    busy.current = false;
    setSaving(false);
    setStatus(null);
    setLoadError(null);
    setMutationError(null);
    setRoles([]);
    setLoading(enabled);
    if (enabled) {
      void (async () => {
        try {
          const loaded = await client.getRoles();
          if (!Array.isArray(loaded))
            throw new Error("Invalid role catalog response");
          if (session.current === currentSession) {
            const reconciled = new Map(loaded.map((role) => [role.slug, role.slug === "assistant" && role.label === "Assisting Engineer" ? { ...role, label: "Software Engineer" } : role]));
            for (const [slug, record] of overlays) {
              if (record) reconciled.set(slug, record);
              else reconciled.delete(slug);
            }
            setRoles(Array.from(reconciled.values()));
          }
        } catch (err) {
          if (session.current === currentSession) {
            setLoadError(
              err instanceof Error ? err.message : "Unable to load roles",
            );
          }
        } finally {
          if (session.current === currentSession) {
            pendingCatalog.current = null;
            setLoading(false);
          }
        }
      })();
    }
    return () => {
      ++session.current;
    };
  }, [enabled, client]);

  async function mutate(
    operation: () => Promise<{
      slug: string;
      record: RoleRecord | null;
      message: string;
    }>,
  ) {
    if (!enabled || busy.current) return;
    busy.current = true;
    const currentSession = session.current;
    setSaving(true);
    setMutationError(null);
    setStatus(null);
    try {
      const result = await operation();
      if (session.current === currentSession) {
        pendingCatalog.current?.set(result.slug, result.record);
        setRoles((previous) => [
          ...previous.filter((role) => role.slug !== result.slug),
          ...(result.record ? [result.record] : []),
        ]);
        setStatus(result.message);
      }
    } catch (err) {
      if (session.current === currentSession) {
        setMutationError(
          err instanceof Error ? err.message : "Unable to save roles",
        );
      }
    } finally {
      if (session.current === currentSession) {
        busy.current = false;
        setSaving(false);
      }
    }
  }

  const save = (draft: RoleRecord) =>
    mutate(async () => {
      const saved = await client.upsertRole(draft);
      return {
        slug: saved.slug,
        record: saved,
        message: `Saved RoleRecord '${saved.slug}' (${saved.review_policy}) to registry.`,
      };
    });
  const remove = (slug: string) =>
    mutate(async () => {
      await client.deleteRole(slug);
      return {
        slug,
        record: null,
        message: `Removed RoleRecord '${slug}' from registry.`,
      };
    });
  // A saved row does not mean the initial catalog loaded. Keep both failure sources visible.
  const error = [loadError, mutationError].filter(Boolean).join("\n") || null;
  return { roles, loading, saving, error, status, save, remove };
}

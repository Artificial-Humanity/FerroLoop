import { useEffect, useRef } from "react";
import type { KeyboardEvent, MutableRefObject, RefObject } from "react";

// owner/repo: each segment starts alphanumeric, then [\w.-]; the repo may not
// end in ".git" (so "." and ".." can never match either).
const GITHUB_PATTERN = /^[A-Za-z0-9][\w.-]*\/[A-Za-z0-9][\w.-]*$/;

const FOCUSABLE =
  'a[href], button, input, select, textarea, [tabindex]:not([tabindex="-1"])';

/** Error text for a GitHub tracker value, or `undefined` when it is valid (or empty). */
export function validateGithubTracker(value: string): string | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  if (!GITHUB_PATTERN.test(trimmed) || trimmed.endsWith(".git"))
    return "GitHub tracker must look like owner/repo.";
  return undefined;
}

/** Error text for a Directory Project path, or `undefined` when it is valid. */
export function validateProjectPath(value: string): string | undefined {
  const trimmed = value.trim();
  if (!trimmed) return "Directory path is required.";
  // The hub is not known to expand "~", so only absolute paths pass.
  if (!trimmed.startsWith("/"))
    return "Directory path must be absolute (start with /).";
  return undefined;
}

/**
 * Keeps Tab inside `dialog`. Browsers tab only to the checked radio of a
 * group, so unchecked radios are skipped.
 */
export function trapTabKey(
  event: KeyboardEvent<HTMLElement>,
  dialog: HTMLElement | null,
): void {
  if (event.key !== "Tab" || !dialog) return;
  const items = Array.from(dialog.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
    (el) =>
      !(el as HTMLInputElement).disabled &&
      !(el instanceof HTMLInputElement && el.type === "radio" && !el.checked),
  );
  if (items.length === 0) {
    event.preventDefault();
    return;
  }
  const first = items[0];
  const last = items[items.length - 1];
  const active = document.activeElement;
  if (event.shiftKey && (active === first || !dialog.contains(active))) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && (active === last || !dialog.contains(active))) {
    event.preventDefault();
    first.focus();
  }
}

/**
 * Shared dialog behaviour: initial focus on mount, focus return to the opener
 * captured at click time on unmount, and Escape (unless busy) closes.
 */
export function useDialogBehavior({
  getInitialFocus,
  returnFocusTo,
  busyRef,
  onClose,
}: {
  /** Picks the element that takes focus when the dialog opens. */
  getInitialFocus: () => HTMLElement | null | undefined;
  returnFocusTo?: RefObject<HTMLElement | null>;
  busyRef: MutableRefObject<boolean>;
  onClose: () => void;
}): void {
  const returnFocusRef = useRef(returnFocusTo);
  returnFocusRef.current = returnFocusTo;
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    getInitialFocus()?.focus();
    return () => {
      returnFocusRef.current?.current?.focus();
    };
    // Mount and unmount only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape" && !busyRef.current) onCloseRef.current();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [busyRef]);
}

export const LABEL_STYLE = {
  fontSize: "11.5px",
  fontWeight: 600,
  color: "var(--text-secondary)",
} as const;

export const HELP_STYLE = {
  fontSize: "11px",
  color: "var(--text-muted)",
  margin: 0,
} as const;

export const ERROR_TEXT_STYLE = {
  fontSize: "11px",
  color: "var(--diff-del-text)",
  margin: 0,
} as const;

export const FIELD_STYLE = {
  display: "flex",
  flexDirection: "column",
  gap: "4px",
} as const;

export const BACKDROP_STYLE = {
  position: "fixed",
  inset: 0,
  backgroundColor: "rgba(9, 13, 22, 0.72)",
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  zIndex: 1300,
  padding: "16px",
} as const;

export const DIALOG_STYLE = {
  width: "min(520px, 100%)",
  maxHeight: "calc(100dvh - 32px)",
  overflowY: "auto",
  backgroundColor: "var(--bg-surface, #0f172a)",
  border: "1px solid var(--border-accent)",
  borderRadius: "8px",
  padding: "16px",
  display: "flex",
  flexDirection: "column",
  gap: "12px",
  boxShadow: "0 20px 44px rgba(0, 0, 0, 0.55)",
} as const;

export const ALERT_STYLE = {
  borderColor: "var(--diff-del-border)",
  backgroundColor: "var(--diff-del-bg)",
  color: "var(--diff-del-text)",
  fontSize: "11.5px",
  padding: "6px 10px",
} as const;

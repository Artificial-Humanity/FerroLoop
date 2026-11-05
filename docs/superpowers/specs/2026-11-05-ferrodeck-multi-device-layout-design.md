# FerroDeck multi-device layout split (Local Record #122)

## Goal

FerroDeck squeezes one three-pane cockpit (Roster sidebar, chat workspace, right tactical dock)
onto every screen, using slide-over drawers and CSS media queries on phones. This design splits the
shell by device form factor. The shell differs per device. State, API hooks and domain panes
(`ChatPane`, `GitPrPane`, `RecordsBoardPane`, and the rest) stay shared.

## Device classes

`useDeviceLayout()` (`ui/src/hooks/useDeviceLayout.ts`) reads `window.innerWidth` and
`window.innerHeight`, and re-reads on `resize` and `orientationchange`. It returns
`{ layout, orientation, width }`.

| Layout | Width | Shell |
| --- | --- | --- |
| `phone` | `< 768` | Single-pane mobile navigation shell |
| `tablet` | `768 – 1023` | Two-pane master-detail |
| `desktop` | `>= 1024` | Existing cockpit, unchanged |

`resolveDeviceLayout(width)` is the pure function behind the hook. The constants
`PHONE_MAX_VIEWPORT_PX = 768` and `DESKTOP_MIN_VIEWPORT_PX = 1024` are the only place these
numbers live. `TWO_PANEL_MIN_VIEWPORT_PX` becomes an alias of the phone/tablet boundary (768, was
720). The cockpit root carries `data-device-layout`. The existing `data-viewport-panel-mode` stays.

The three-panel threshold `THREE_PANEL_MIN_VIEWPORT_PX` stays at 1080. A desktop between 1024 and
1079 px keeps today's two-panel mutual exclusion (the right dock opens, the roster collapses).
Chat needs at least 360 px next to two 360 px panels, so three docked panes do not fit below 1080.

## Phone shell (`< 768`)

The phone does not render `ContextHeader`, the drawer-style `RosterSidebar`, or the right split
dock. `PhoneShell` renders one active view at a time:

* **Top bar** (sticky): back button when a tool view is open (returns to Chat), view title,
  active agent name and role badge, compact session selector, WebSocket status dot.
* **Active view** (scrolls inside its own container):
  * `agents` – `RosterSidebar` in `embedded` mode (in flow, no backdrop, no close button, no resize
    handle, no console-tab strip).
  * `chat` – `ChatPane`.
  * `board` – the Records & Board pane.
  * `watch` – `FerroWatchPane`.
  * Tool views opened from **More**: `git` (Diffs / Git & PRs), `terminal`, `artifacts`, `events`
    (`TriageQueuePane`), `mail`, `hub`, `gate`, `state`. All come from the existing
    `renderSecondaryPane`.
* **Bottom navigation**: Agents, Chat, Board, Watch, More. More opens a bottom sheet that lists
  the tool views plus Settings, Help, Theme, Nodes, Spin up, Pair and Log out (the actions the
  desktop header holds). A tool view highlights More. Targets are at least 44 px high. The bar pads
  with `env(safe-area-inset-bottom)`.
* **State**: a `phoneView` state in `App` only. Phone navigation never edits the desktop state
  (`splitView`, `leftPanelTab`, `mobileRosterOpen`), so rotating or resizing into tablet or
  desktop restores the last desktop arrangement.
* **Entry points**: selecting an agent opens `chat`. `handleSelectTab`, `handleSelectLeftPanelTab`
  and `handleOpenArtifact` delegate to a single `navigatePhone` mapper when the layout is `phone`
  (`artifacts` chips from chat open the `artifacts` tool view). Wire/Events deep links from the
  panes use the same handlers, so they work unchanged.
* The unread badge from the roster shows on the Agents tab. The Watch tab has no badge.

## Tablet shell (`768 – 1023`)

Two panes: master (Roster, or a console tab) and detail (chat, or the secondary tool pane).

* **Landscape** (`width > height`): master is docked, 320 – 360 px, collapsible with the header
  toggle (today's behavior). `data-tablet-master="docked"`.
* **Portrait**: master is an overlay drawer, at most 360 px wide, with a backdrop. It closes after
  an agent is picked. `data-tablet-master="overlay"`.
* The right dock and the master stay mutually exclusive (today's two-panel rule).
* Touch ergonomics: tab, roster row and bottom-of-sheet targets are at least 44 px under
  `[data-device-layout="tablet"]`.

## Desktop (`>= 1024`)

No behavior change. Existing desktop tests must pass unmodified except where they assert the
720 px boundary.

## Non-goals

No new API endpoints, no native apps, no change to pane contents, no change to persisted panel
widths. No swipe gestures in this record.

## Testing

Vitest. The hook and the pure function get unit tests (boundaries 767/768, 1023/1024, orientation,
resize). `App` gets render tests per device (`desktop`, `tablet` portrait and landscape, `phone`)
that assert the shell, navigation between all five bottom tabs and the More sheet, agent select
into chat, artifact-chip routing, and that desktop state survives a phone round trip. Existing
tests that assert drawer behavior below 768 px are rewritten for the phone shell.

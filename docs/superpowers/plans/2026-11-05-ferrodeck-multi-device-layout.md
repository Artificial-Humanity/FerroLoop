# FerroDeck multi-device layout split – implementation plan (Local Record #122)

Spec: [2026-11-05-ferrodeck-multi-device-layout-design.md](../specs/2026-11-05-ferrodeck-multi-device-layout-design.md).
All work is under `ui/`. Verification for every task: `cd ui && npm run lint && npm test`.
Existing tests that assert drawer behavior below 768 px are rewritten, not deleted, in the task
that changes that behavior.

## Task 1 – `useDeviceLayout` and one set of thresholds

1. Create `ui/src/hooks/useDeviceLayout.ts`: `PHONE_MAX_VIEWPORT_PX = 768`,
   `DESKTOP_MIN_VIEWPORT_PX = 1024`, `type DeviceLayout`, pure `resolveDeviceLayout(width)`,
   `resolveOrientation(width, height)`, and the `useDeviceLayout()` hook returning
   `{ layout, orientation, width }`. Read `window.innerWidth/innerHeight` (tests set them with
   `Object.defineProperty` and fire `resize`). Listen to `resize` and `orientationchange`. SSR-safe
   (default 1200 × 800).
2. In `App.tsx`: make `TWO_PANEL_MIN_VIEWPORT_PX` equal `PHONE_MAX_VIEWPORT_PX`; replace
   `isMobileViewport` with `deviceLayout === "phone"`; add `data-device-layout` to the
   `fd-cockpit` root. Keep `THREE_PANEL_MIN_VIEWPORT_PX = 1080` and `data-viewport-panel-mode`.
3. Replace the literal `720` in `ChatPane.tsx` (composer height and `isMobileComposer`) and the
   `max-width: 719px` / `min-width: 720px` media queries in `ui/src/styles/tokens.css` with 768 and
   767/768. Do not restyle anything else.
4. Tests: `ui/src/__tests__/useDeviceLayout.test.tsx` (boundaries 767/768 and 1023/1024,
   orientation, resize and orientationchange update, cleanup on unmount). Update the Record #109
   test title and any assertion that depends on 720.

## Task 2 – Phone shell

1. `ui/src/components/phone/`: `PhoneShell.tsx` (layout: top bar, view slot, bottom nav, More
   sheet), `PhoneTopBar.tsx`, `PhoneBottomNav.tsx`, `PhoneMoreSheet.tsx`, and
   `phoneNavigation.ts` (`PhoneView`, `PHONE_PRIMARY_TABS`, `PHONE_TOOL_VIEWS`,
   `mapToPhoneView(tab: CockpitTab | LeftPanelTab): PhoneView`). Test ids: `phone-shell`,
   `phone-top-bar`, `phone-back-btn`, `phone-bottom-nav`, `phone-tab-agents|chat|board|watch|more`,
   `phone-more-sheet`, `phone-more-item-<view>`. `aria-current="page"` on the active tab; the
   sheet is `role="dialog"` with focus return and Escape to close.
2. `RosterSidebar.tsx`: add `embedded?: boolean`. When true: no backdrop, no `onClose` button, no
   resize handle, no left-panel tab strip (roster only), in-flow full width. Desktop output is
   byte-identical when the prop is absent.
3. `App.tsx`: `phoneView` state; a `deviceLayoutRef`; `navigatePhone(tab)`. When the layout is
   `phone`: `handleSelectTab`, `handleSelectLeftPanelTab`, `handleOpenArtifact` call
   `navigatePhone` and return before touching desktop state; roster `onSelectAgent` sets
   `phoneView = "chat"`. Render `PhoneShell` instead of `ContextHeader` + `fd-cockpit-body`.
   Modals (`SettingsModal`, `SpinUpAgentModal`, `HelpDrawer`, and the rest) stay mounted outside
   the shell. Do not duplicate pane JSX: reuse `renderSecondaryPane` and the existing `ChatPane`
   element.
4. `tokens.css`: `.fd-phone-shell` (100dvh grid: auto / 1fr / auto), sticky top bar, bottom nav
   with safe-area padding, 44 px targets, More sheet with backdrop. Remove the phone-only
   slide-over rules for `.fd-sidebar` that no longer apply.
5. Tests `PhoneShell.test.tsx`: at 390 × 844 the shell renders and `context-header` /
   `right-split-panel` do not; each bottom tab shows its pane; More sheet lists every tool view
   and each opens its pane with the back button returning to Chat; selecting an agent lands on
   Chat; an artifact chip click lands on the artifacts view; resizing 390 → 1280 → 390 keeps the
   desktop `splitView` and `leftPanelTab` unchanged. Rewrite the existing tests that drove the
   drawer below 768 px (`RosterSidebar.test.tsx` narrow viewport case, Record #109 single-panel
   case, Record #114/#117 safe-area case, `ThreeTierNavigation`/`Wave12` cases that fail).

## Task 3 – Tablet master-detail

1. `App.tsx`: derive `tabletMaster = orientation === "portrait" ? "overlay" : "docked"` when the
   layout is `tablet`; add `data-tablet-master` to the cockpit root. Overlay mode: the roster uses
   the existing `isOpen` / backdrop path, width `min(360px, 90vw)`, closes after an agent select
   and after opening a tool pane. Docked mode: current two-panel behavior, master width clamped
   to 320–360 px.
2. `tokens.css`: tablet rules scoped to `[data-device-layout="tablet"]` (44 px targets for tabs,
   roster rows, header buttons; overlay drawer width). No change for other layouts.
3. Tests `TabletLayout.test.tsx`: 1024 × 768 landscape is `desktop`; 900 × 700 is tablet
   docked; 800 × 1100 is tablet overlay; rotating changes the mode without losing the selected
   agent; overlay closes after agent select; the right dock still collapses the master.

## Task 4 – Desktop regression and docs

1. Desktop tests: at 1280 × 800 and 1440 × 900 the cockpit has `data-device-layout="desktop"`,
   the three panes render, and no phone or tablet element exists. A 1050 × 800 desktop keeps
   two-panel mutual exclusion.
2. Help text: if `HelpDrawer.tsx` has a navigation topic, add one line for the phone bottom bar.
3. Run the full UI suite, `npm run build`, and fix only what this record broke.

## Final review

Whole-branch `code-review` on `origin/main..HEAD` against the spec, then open the pull request
with `fl github pr create` and ask Ferris for review over `agent-msg`.

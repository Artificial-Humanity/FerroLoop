# fl browse — FerroBrowse

`fl browse` gives an agent a small, stable view of a web page, and gates a page on a few UI and
accessibility rules. It opens no store and needs no project or config, so it runs anywhere and
`fl gate` can run `fl browse check` directly. Its only network access is the fetch of the
target page.

## Tiers

| Tier | Id | What it is | Needs |
|------|----|------------|-------|
| 1 | `native-distiller` | Fetches an `http(s)` URL or reads a local file, parses the HTML with a small tolerant parser, and distills it. | Nothing: always ready. |
| 2 | `headless-binary` | Renders the page in a headless browser binary and distills the rendered DOM; can write a screenshot. | An installed binary. |

A target is an `http://` or `https://` URL, a `file://` URL, or a path to an HTML file. Any other
scheme is refused. A URL that carries a user name or password is refused.

Tier 2 looks for a binary at `$FL_BROWSE_ENGINE_BIN` (an executable file; an unusable value is an
error, and the value `none` switches the search off), then for the names listed in
`fl_browse::engine::ENGINE_CANDIDATES` in each `PATH` directory and in a few standard install
directories. The binary must take Chromium's command line: it is run as
`BIN --headless --dump-dom --window-size=W,H --user-data-dir=DIR [--screenshot=PATH] -- TARGET`.
It is started without a shell, with a cleared environment (only `PATH` and a scratch `HOME`),
under a time limit after which it is killed.

## Commands

### `fl browse snapshot <target> [--viewport WxH] [--engine native|auto|cdp] [--screenshot PATH] [--json]`

Prints the title, the viewport meta, an outline of the landmarks, headings and interactive
elements, a table of the interactive elements, and the size metrics. The interactive elements
get handles `@e1`, `@e2`, ... in document order. The table lists, per element, the tag, role,
accessible name, `data-testid`, `href`, `autocomplete` and the key attributes `type`, `name`,
`id`, `value`, `disabled` and `required` (`value` is never shown for a password input). Hidden
content (`hidden`, `aria-hidden="true"`, `type="hidden"`, inline `display:none`, and what is
inside such an element) gets no handle. The output is deterministic: the same document gives the
same bytes.

* `--engine native` (the default) never starts a process.
* `--engine auto` uses a headless binary when one is found, else Tier 1.
* `--engine cdp` uses a headless binary and fails when none is found.
* `--screenshot PATH` needs a headless binary. With none found it fails; it never skips. It is
  refused together with `--engine native`.
* `--viewport` is the window size for an engine (default `1280x800`).

Metrics: `raw_bytes` is the length of the document that was parsed (the fetched HTML, or the
DOM the engine printed). `distilled_bytes` is the length of what the command prints before the
metrics: the title, viewport meta, outline and element table. `token_savings_pct` is
`round((1 - distilled_bytes / raw_bytes) * 1000) / 10`, and `0.0` for an empty document. It is a
byte ratio, a stand-in for a token ratio; no tokenizer is involved. It is negative when the
distilled form is larger than the source.

### `fl browse check <target> [--viewport WxH] [--strict] [--json]`

Audits the page at a viewport (default `320x568`) and prints one finding per row:
`severity`, `code`, `handle` (`-` when the element has none) and a message. Tier 1 only.

| Code | Severity | Flags |
|------|----------|-------|
| `missing-viewport-meta` | error | No `<meta name="viewport">` whose content sets `width=device-width`. |
| `unlabeled-control` | error | A visible `button`, `input` (not `type=hidden`), `textarea` or `select` with no accessible name. |
| `ios-contact-autofill-hazard` | error | A `textarea` with `autocomplete="one-time-code"`, which iOS WebKit treats as a contact field. |
| `fixed-min-width-overflow-hazard` | warning | An inline `min-width` in `px` wider than the viewport. |

The accessible name is the first non-empty of `aria-labelledby`, `aria-label`, then (by element)
the associated `<label>`, the element's text, the `value` of a button-like input, the `alt` of an
image input, `title`, and `placeholder`. A submit or reset button with no `value` is named by the
browser's default.

`missing-viewport-meta`, `unlabeled-control` and `ios-contact-autofill-hazard` are defects on any
device and are errors. `fixed-min-width-overflow-hazard` depends on the viewport and on the
container, so it is a warning.

### `fl browse doctor [--json]`

Reports `native-distiller` (`ready`) and `headless-binary` (`detected` with its path and the
first line of `--version`, `not-installed`, or `misconfigured` when `$FL_BROWSE_ENGINE_BIN`
names something that cannot run). `doctor` runs a detected binary with `--version` and nothing
else.

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success. For `check`: no error finding, and with `--strict` no warning either. |
| 1 | `check`: the gate failed. `doctor`: `$FL_BROWSE_ENGINE_BIN` is misconfigured. |
| 2 | Any other failure: a bad argument or viewport, a target that cannot be fetched or read, a non-HTML or oversize target, a missing engine for `--engine cdp` or `--screenshot`, an engine that failed. |

A target that cannot be read is exit 2, never 0, so a gate cannot pass on a page it did not see.

## Bounds

A fetch has a 15 s timeout, follows at most 5 redirects (only to `http` and `https`), and reads
at most 4 MiB, counted after decompression. It sends no proxy, cookie or authorization header.
The parser reads at most 4 MiB of HTML, keeps the open-element stack at most 256 deep (a deeper
element becomes a leaf) and stops at 200,000 nodes; when one of these bounds is hit the snapshot
says it is partial (`truncated` in JSON). Text is read as UTF-8, invalid bytes replaced.

## Limits

* Tier 1 runs no JavaScript. A page that builds its content in the browser distills to the
  markup the server sent. Use an engine for such a page.
* Neither tier computes layout or pixels for the audit: `check` reads markup and inline styles
  only. A `min-width` from a stylesheet, a `calc()` or a unit other than `px` is not seen.
* The parser is a reduced one, not the full HTML tree-construction algorithm. It handles void
  elements, raw-text elements, comments, character references (the common named ones and
  numeric ones), unclosed and misnested tags, upper-case names, and unquoted and boolean
  attributes; it does not repair a table or a form the way a browser does.
* The accessible name is a close reading of the common cases, not the full accessible-name
  computation, and the audit does not check contrast, focus order or ARIA validity.
* Tier 2 is only as good as the binary: it must accept the Chromium flags above.

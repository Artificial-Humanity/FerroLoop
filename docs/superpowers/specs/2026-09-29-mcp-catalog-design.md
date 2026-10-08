# MCP catalog — design

**Date:** 2026-09-29 (rev 2, 2026-10-07 and 2026-10-08)
**Status:** Approved by the owner 2026-09-29. Sequenced after sub-projects 3 (GitHub ledger)
and 4 (routing and escalation). Rev 2 folds in the owner's decisions 10–11 (2026-10-07) and
12–15 (2026-10-08), and the corrections found while planning against the code and re-checking the facts on 2026-10-07
(`docs/superpowers/plans/2026-10-07-mcp-catalog.md`, "Spec defects"); each change is marked
as an owner decision or `[agent]`.
**Scope:** One committed catalog of the MCP servers a project uses, a read-only client for an
MCP registry, and a writer that turns the catalog into each coding-agent CLI's own MCP
configuration.

---

## Reading this document

Constraints carry a status tag, as in the identity spec:

| tag | meaning |
|---|---|
| **Invariant** | Must hold in any version. Violating it breaks the product's premise. |
| **Release scope** | True of this sub-project's release. A later version can change it. |
| **Open** | Undecided. Named here so it is not decided by accident. |

An untagged statement is descriptive, not binding.

---

## 0. Where this sits

A person who uses several agent CLIs keeps one MCP configuration per vendor, each in its own
file and format: `.mcp.json` for Claude Code, `.codex/config.toml` for Codex,
`.agents/mcp_config.json` for Antigravity *(rev 2, decision 10)*. The copies drift. Organizations with a private MCP registry
can point some tools at it; small teams and solo developers have no registry, and of the CLIs
FerroLoop drives, none accepts a custom registry URL.

This sub-project gives a project **one catalog** and makes fl write every vendor's file from
it. Enabling a server once makes it available to every agent CLI in the project; disabling it
once removes it from all of them.

The facts about the registry API and the vendor files come from research done on 2026-09-28
(official MCP Registry documentation, each vendor's documentation, and this machine's `agy`).
Where a fact is medium confidence, the section says so and names the check that settles it.

### 0.1 Owner decisions this design rests on (2026-09-28)

1. **What "our own registry" means:** a catalog and config writer. fl runs no registry
   service.
2. **Catalog home:** committed in the repository, with switches for each machine in the user's
   config (§2).
3. **Ownership:** fl adds, changes and removes only the entries it wrote. A hand edit to an
   fl-managed entry is refused, never overwritten (§4).
4. **Pinning:** exact versions. Writing the configs never contacts the registry; an upgrade is
   an explicit command, and its change arrives in a commit (§3).
5. **Vendors:** Claude Code, Codex and Antigravity. Gemini CLI is not in this release.
6. **Antigravity:** its only MCP file is machine-wide. A project's server reaches it only when
   a person opts that server in on their own machine, under a name that includes the project
   (§4.1).
7. **Project vendor files are generated and gitignored.** Only the catalog is committed (§4.4).
8. **Approach:** a new crate that writes the files itself. Driving the vendor CLIs
   (`claude mcp add`, `agy mcp add`) was rejected: it needs every CLI installed, depends on
   their flags, and puts secrets in argv — `agy mcp add --env TOKEN=xxx` is the vendor's own
   example.
9. **Sequence:** after the GitHub ledger (3) and routing (4).
10. **Antigravity gets a project file, like the others** (owner, 2026-10-07): fl writes
    `<root>/.agents/mcp_config.json` — generated, gitignored, plain server names, a default
    vendor. A live check with agy 1.2.17 showed agy loads that file in a session. This replaces
    decision 6's machine-wide opt-in.
11. **Antigravity and secrets** (owner, 2026-10-07): agy expands no `${NAME}` (checked live),
    but a stdio server inherits agy's environment. So a secret environment variable is left
    out of an Antigravity entry, and a server with a secret header is refused for Antigravity
    only, naming the reason.
12. **No default registry** (owner, 2026-10-08). `search`, `add --from` and `upgrade` are
    refused until the catalog names one; the refusal prints the official address as an example.
13. **Search by name, through the registry** (owner, 2026-10-08). fl sends the registry's
    `search` and also filters what comes back client-side by name (case-insensitive), so a
    registry that ignores the parameter cannot flood the list. §3.1's "name and description" is
    amended to name.
14. **A vendor file absent as a whole is recreated from the catalog** (owner, 2026-10-08) —
    after `git clean -fdX` or a fresh clone, fl's own generated file is gone; `sync` writes it
    anew and replaces the record's entries for it. A file that exists but lost an fl entry is
    still refused as a hand edit (§4.3).
15. **A machine switch naming a server the catalog no longer has is a warning, not an error**
    (owner, 2026-10-08) — `sync` and `check` print a `warning:` naming the stale switch and
    carry on (§2.2).

### 0.2 Out of scope

* A registry service, local or hosted (§10).
* Gemini CLI, VS Code, Cursor and other vendors.
* Package types other than npm, PyPI and OCI, and bundles that fl would have to download
  (`mcpb`, `cargo`, `nuget`) — refused with a message naming the type (§3.2).
* Starting, stopping or health-checking MCP servers. fl writes configuration only.
* User-scope servers that belong to no project.

---

## 1. Components

### 1.1 `fl-mcp` (new crate)

Depends on `fl-core` only. It never opens a store.

| module | responsibility |
|---|---|
| `catalog` | The `.fl/mcp.toml` model and its parser; the machine switches (§2.2) |
| `registry` | A read-only client for the registry API (§3.1) |
| `vendor` | One writer per vendor — `claude`, `codex`, `antigravity` — behind a `Vendor` trait (§4.2) |
| `sync` | Builds the desired entries, plans every target, applies the plan (§4.3) |
| `fake` | A registry fake on `tiny_http`, behind the `fake` feature, as in `fl-github` |

**Dependencies.** `ureq` with `rustls` (already in the workspace), `toml_edit` for Codex's file,
`serde_json`, and `sha2` (already in the workspace). `[agent]` (rev 2): not `serde_json`'s
`preserve_order`, which the GitHub ledger forbids workspace-wide (it needs a `Value`'s keys
sorted); a JSON vendor file keeps its key order through `indexmap` and `serde_json`'s
`raw_value`. Each must be
compatible with Apache-2.0. No dependency may need a runtime service or a program outside the
binary. *(Invariant — the single-binary rule.)*

### 1.2 `fl-cli`

A new `cmd/mcp.rs`:

```text
fl mcp registry <url>                         set the catalog's registry
fl mcp search <text>                          list matching servers in the registry
fl mcp add <name> --from <registry-name> [--version V] [--package npm|pypi|oci | --remote] [--env NAME=value]… [--with NAME]…
fl mcp add <name> [--env NAME[=value]]… -- <command> [args…]
fl mcp add <name> --url <url> [--header NAME[=ENV[:SCHEME]]]…
fl mcp remove <name>
fl mcp enable <name> | disable <name>         change the team default in the catalog
fl mcp upgrade <name> [--to V] [--env NAME=value]… [--with NAME]…
fl mcp sync [--replace <name>]…
fl mcp check
```

`enable` and `disable` change the committed default. The per-machine switches (§2.2) are edited
in the config file by hand, like the rest of it.

`[agent]` (rev 2): `--with NAME` includes an optional secret variable or argument, which `add`
otherwise leaves out. By hand, `--env NAME=value` is a literal and `--env NAME` alone a secret
reference; every `--header` is a secret reference — `NAME` alone reads `<SERVER>_<NAME>`,
`NAME=ENV` reads `ENV`, `NAME=ENV:SCHEME` sends `<SCHEME> <value>` — and a name where a
variable's name goes must look like one, so a pasted token is refused. `upgrade` takes `--env`
and `--with` for what a new version needs. Each form of `add` refuses another form's flags.

---

## 2. The catalog

### 2.1 `.fl/mcp.toml`

Committed; the only source of truth for which servers a project uses. *(Invariant.)*

```toml
registry = "https://registry.modelcontextprotocol.io"    # optional

[server.github]
from = "io.github.github/github-mcp-server"   # registry name; absent = added by hand
version = "0.13.0"                            # exact pin; required when `from` is present
enabled = true                                # team default
vendors = ["claude", "codex"]                 # optional; default = every project-scoped vendor
transport = "stdio"                           # "stdio" | "http" | "sse"
command = "docker"
args = ["run", "-i", "--rm", "-e", "GITHUB_TOKEN", "ghcr.io/github/github-mcp-server:0.13.0"]
env.GITHUB_TOKEN = { secret = true }

[server.docs]
transport = "http"
url = "https://mcp.example.com/mcp"
headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }
```

* The **launch spec is frozen** into the entry when it is added: `sync` reads nothing else, so
  it needs no network and gives the same output on every machine. *(Invariant.)*
* An `env` or header value is either a literal string (committed, public if the repository is)
  or `{ secret = true }`, which is a **reference**: the value comes from the environment
  variable of the same name (or `env = "…"`) when the agent starts the server. fl never reads,
  stores or writes a secret's value. *(Invariant.)*
* `deny_unknown_fields` everywhere: a typo is refused, not ignored.
* *(Rev 2: no `project` field — its only use was Antigravity's machine-wide name, which
  decision 10 removes.)*
* Server names: `[a-z0-9-]`, 1-32 chars, unique. `[agent]` (rev 2): not `workspace`,
  `computer-use` or `claude-in-chrome`, which Claude Code reserves.
* `[agent]` (rev 2): a secret header requires `env`, the variable that holds its value; a
  header's name is not a variable's name, so there is no default.
* `[agent]` (rev 2): `fl mcp` works on the nearest ancestor of the working directory that holds
  `.fl/mcp.toml`; with none, on the nearest that holds `.git`, where `registry` and `add` create
  the catalog. A directory with neither above it is refused. fl runs no git to find it.
* `vendors` may name only vendors this release knows. *(Rev 2, decision 10: `antigravity` is a
  default vendor like the others.)*

### 2.2 Machine switches

In `$XDG_CONFIG_HOME/fl/config.toml`, a new `[[mcp]]` table, found by the same longest-ancestor
rule as `[[project]]` and independent of it, so that a project needs no store binding to use
the catalog:

```toml
[[mcp]]
root = "/home/you/code/app"
disable = ["github"]        # off on this machine, though the team default is on
enable = ["sentry"]         # on here, though the team default is off
```

* *(Rev 2, decision 15: a switch that names a server not in the catalog is a warning, not an
  error: `sync` and `check` name it and carry on.)*
* The same name in `enable` and `disable` is an error.
* *(Rev 2: no `antigravity` key — decision 10.)*
* The config file's top level has `deny_unknown_fields`, so an older `fl` refuses a config
  that has an `[[mcp]]` table. Upgrading `fl` on that machine is the remedy. *(Release scope.)*

---

## 3. The registry

### 3.1 The client

The registry API is the official MCP Registry's, version `v0.1` (in an API freeze, not yet
GA). Any registry implementing the same OpenAPI document is accepted — that is the documented
contract for a sub-registry.

* `GET /v0.1/servers` (cursor pagination: `limit`, `cursor`, `metadata.nextCursor`),
  `GET /v0.1/servers/{name}/versions`, and `GET /v0.1/servers/{name}/versions/{version}`.
  Names are URL-encoded.
* **No credential is sent.** Reads are unauthenticated. *(Release scope — a private registry
  that needs a token is §10.)*
* The guards of `fl-github`'s client: https only, or http to a loopback address with no
  userinfo; no redirect followed at all *(`[agent]` rev 2, plan ruling 9: not even one on the
  registry's own origin, which the error names)*; the status is classified before the body is
  parsed; a body over 4 MiB is refused; an HTML error page is reported as the registry's
  failure, not as a parse error.
* `fl mcp search` pages through `GET /v0.1/servers` and filters by name *(rev 2, decision 13: not
  by description)*, up to 20 pages, and says so when it stops early. The planning step checks the OpenAPI document for
  a server-side search parameter and uses it if present. `[agent]` (rev 2): it exists —
  `search`, a case-insensitive substring of the name only — and is used with `version=latest`.
  fl also keeps only the servers whose name holds the text (decision 13).
* *(Rev 2, decision 12: there is no default registry. `search`, `add --from` and `upgrade` are
  refused until the catalog names one, and the refusal gives the official address as an
  example.)*
* `[agent]` (rev 2): the registry serves entries outside its own schema (an argument `type` of
  `flag`, seen 2026-10-08, failed a whole search), so fl reads a search leniently — only each
  server's name, description, version and status — and refuses an argument type it does not
  know when it freezes the entry, naming it.

### 3.2 `add --from`

1. Read the version (default: `latest`), and record the version the registry returns — never
   the word `latest`.
2. Refuse a server whose `status` is `deleted`; warn on `deprecated`. `[agent]` (rev 2): the
   registry hides a deleted entry unless asked with `include_deleted=true`, so fl asks that way.
3. Choose one launch route. With more than one on offer and no `--package` or `--remote`, refuse
   and list them.
4. Freeze the launch spec:

   | route | command and args |
   |---|---|
   | npm | `npx -y <identifier>@<version>` |
   | PyPI | `uvx <identifier>==<version>` |
   | OCI | `docker run -i --rm` + the runtime arguments + one `-e NAME` per variable + `<identifier>:<version>` |
   | remote | `transport` from the remote's type; `url`; headers |

   Package arguments with a fixed `value` or a `default` are appended. A required argument with
   neither, or a remote URL with a `{variable}` that has no default, is refused with a
   suggestion to add the server by hand. `[agent]` (rev 2): an OCI identifier already carries
   its tag, so it is used as given (one with no tag or digest is refused); a package whose
   runtime arguments include a positional is refused; a package that serves HTTP locally is
   refused; a secret inside an argument is accepted only as a docker `-e NAME={var}`, which
   becomes `-e NAME` and a reference; a secret header the registry gives no value is recorded
   with an env name fl derives and no scheme. The plan's rulings 14–18 give the rules.
5. Environment variables and headers: `isSecret` → a `{ secret = true }` reference. A required
   non-secret with no default needs `--env NAME=value`, and `add` warns that the value will be
   committed.
6. Refuse `mcpb`, `cargo` and `nuget` packages, naming the type.

### 3.3 `upgrade`

Reads the registry, then shows the difference between the frozen launch spec and the new one
(version, command, arguments, variables) and rewrites only that entry. It does not run `sync`:
the change reaches the vendor files after the person reviews and commits it. A version that is
not newer is refused unless given with `--to`.

---

## 4. Writing the vendor files

### 4.1 Targets

| vendor | file | section | entry name |
|---|---|---|---|
| Claude Code | `<root>/.mcp.json` | `mcpServers` | `<server>` |
| Codex | `<root>/.codex/config.toml` | `[mcp_servers.<server>]` | `<server>` |
| Antigravity | `<root>/.agents/mcp_config.json` (decision 10) | `mcpServers` | `<server>` |

* *(Rev 2: decision 10 replaces the machine-wide file and its per-machine opt-in. The catalog's
  `project` field and the `[[mcp]]` table's `antigravity` key go with it, and so does the §4.3
  row about two projects sharing a name, now removed. `[agent]`: the remote key is `serverUrl`, which agy
  accepts and its web docs name; a JSON vendor file with comments or trailing commas — agy
  accepts JSONC — is refused, never rewritten.)*
* Codex reads a project's `.codex/config.toml` only when the user has marked the project
  trusted. `sync` reads `~/.codex/config.toml` (never writes it) and warns when the project is
  not trusted. `[agent]` (rev 2): Codex matches the trust key exactly — the project root, else
  the git root — and an ancestor's trust does not count.

### 4.2 Per-vendor shapes

| | stdio | secret env | remote | secret header |
|---|---|---|---|---|
| Claude Code | `command`, `args`, `env` | `"NAME": "${NAME}"` | `type` `http`/`sse`, `url`, `headers` | `"Bearer ${NAME}"` |
| Codex | `command`, `args`, `env` | `env_vars = ["NAME"]` | streamable HTTP only: `url` | `bearer_token_env_var` or `env_http_headers` |
| Antigravity | `command`, `args`, `env` | left out: the server inherits it (decision 11) | `serverUrl` | refused for Antigravity (decision 11) |

* A server a vendor cannot express — an SSE remote for Codex, for example — is refused **for
  that vendor**, naming the reason; the other vendors still get it.
* Antigravity's `${NAME}` substitution was checked with agy 1.2.17 on 2026-10-07: it does not
  hold, in `env` or in headers. Decision 11 applies. `[agent]`: SSE is refused for Antigravity
  as for Codex (agy has no SSE); for Codex, a secret header with no scheme maps to
  `env_http_headers` (the variable holds the whole value) and one with `Bearer` to
  `bearer_token_env_var`; any other scheme is refused for Codex.

### 4.3 Ownership and the plan

For each target file fl keeps an **ownership record** under `$XDG_STATE_HOME/fl/mcp/`, keyed by
the SHA-256 of the target's canonical path: for every entry fl wrote, its name, the owning
project root, and the SHA-256 of the entry as written. The record is local to the machine and
never in a repository.

`sync` builds the desired entries from the catalog and this machine's switches, then classifies
every name in every target:

| situation | action |
|---|---|
| desired; absent from the file and the record | add |
| in the record; the file matches its hash | update, or remove if no longer desired |
| in the record; the entry differs from its hash but means what fl would write (laid out anew) | adopt `[agent]` (rev 2) |
| in the record; the whole file is gone | add, or forget if no longer desired *(rev 2, decision 14)* |
| in the record; gone from a file that exists, and no longer desired | forget `[agent]` (rev 2) |
| in the record; the entry differs, or is gone from a file that exists though desired | **refuse** — hand-edited |
| desired; in the file but not the record, and different from what fl would write | **refuse** — someone else's entry |
| in the file, exactly what fl would write, not in the record | adopt (recovers a crash between the write and the record) |
| anything else | untouched |

* fl never changes or removes an entry it did not write. *(Invariant.)*
* `[agent]` (rev 2): a target that is a symbolic link, or whose path below the project root
  passes through a linked directory (`.codex`, `.agents`), is refused, naming the path: fl writes
  only plain files it can see whole. A link could carry the write into a file git tracks, which
  the gitignore guard (§4.4) cannot see, or out of the project. The root itself may be reached
  through a link.
* A refusal names the file, the entry and the difference, and the remedy: restore the entry, or
  `sync --replace <name>`, which shows the difference and overwrites.
* **All or nothing:** every target is planned before any is written; one refusal writes nothing.
* **Writing:** under a lock file in the state directory; the target is read again just before the
  write, and a change since planning is refused (another program may be writing the same file); the
  new content goes to a temporary file in the same directory, which is renamed over the target
  with the original mode; the record is updated after the rename.
* **What is kept:** Codex's TOML is edited with `toml_edit`, so comments and layout survive.
  JSON keeps key order, and `[agent]` (rev 2) every foreign entry byte for byte: only fl's own
  entries and the separators around them are written fresh.

### 4.4 The gitignore guard

`.mcp.json`, `.codex/config.toml` and `.agents/mcp_config.json` (decision 10) must be ignored by
git. If any is tracked, or would not be ignored, `sync` refuses and prints the lines to add. fl does not edit `.gitignore`.

### 4.5 `check`

`fl mcp check` runs the same plan and writes nothing:

| exit | meaning |
|---|---|
| 0 | every target matches the catalog |
| 1 | a `sync` would change something; the changes are listed |
| 2 | a refusal, a divergence, or an error |

It never contacts the network, so it can serve as a gate.

---

## 5. Errors

* Errors exit 2 and print `error: …`, as the rest of `fl`.
* Every refusal names the entry, the file and what to do next.
* A registry that cannot be reached fails `search`, `add --from` and `upgrade` only. `sync` and
  `check` have no network path. *(Invariant.)*

---

## 6. Security

* **Secrets:** references only (§2.1). No secret value in the catalog, a vendor file, a record,
  argv, or an error message. *(Invariant.)*
* **What runs:** a registry entry is code that runs on every teammate's machine. Nothing the
  registry says changes what runs without a commit to the catalog. *(Invariant.)*
* **Public repositories:** the catalog is committed, so a literal value in it is public. `add`
  warns on every literal `env` or header value it records.
* **Machine paths:** the catalog holds none. Roots live in the user's config.

---

## 7. Testing

### 7.1 No network, no real home

* The registry fake serves `/v0.1` fixtures: several packages, a remote, `isSecret` variables
  and headers, a deprecated and a deleted server, an off-origin redirect, an oversized body and
  an HTML 502.
* Every test runs with `HOME` and the `XDG_*` variables pointed at a temporary directory. A
  guard test fails if any resolved target lies outside that directory. *(Invariant for tests.)*

### 7.2 What is tested

* **Writers:** golden files per vendor; a round trip keeps foreign entries, key order and the
  Codex file's comments.
* **Ownership:** one test per row of §4.3; all-or-nothing leaves every file byte-identical; a file
  changed between plan and write is refused; crash recovery adopts an exact match.
* **Secrets:** tests set real-looking values in the environment, run `add` and `sync`, and search
  the catalog, every vendor file and every record for them. None may appear.
* **Guards:** a tracked or unignored vendor file; an untrusted Codex project; a switch naming an
  unknown server (a warning, decision 15); an unknown catalog field; a vendor that
  cannot express a server; the registry client's guards.
* **Mutation checks:** each guard's test is shown to fail with the guard reverted.

### 7.3 Live checks

* An ignored test that reads the real public registry.
* A manual check of Antigravity's `${NAME}` substitution and name separator with `agy`, recorded
  in §4.1-4.2. *(Rev 2: done 2026-10-07 with agy 1.2.17. `${NAME}` is not expanded (decision 11);
  agy loads a project's `.agents/mcp_config.json` (decision 10), so a server keeps its plain name
  and the separator question is moot. The ignored registry test, run 2026-10-08, first failed on
  an entry outside the registry's schema (§3.1), then passed.)*

---

## 8. Documents updated in the same change

* `docs/mcp.md` (new) — the catalog, the switches, the vendors and their limits.
* `docs/README.md` — a link to it.
* `docs/getting-started.md` — the `fl mcp` lines in the help transcript, which its test executes.
* `README.md` — a link line.

---

## 9. Sequencing and dependencies

Sequenced after sub-projects 3 and 4. It shares no code with them; it reuses the config
lookup in `fl-cli` and the client guards of `fl-github` (moved to a shared place if both need
them — decided in the plan).

---

## 10. Open questions

* **A registry service.** A local server implementing the registry API, for tools that accept a
  custom registry URL. Set aside by the owner; only VS Code's enterprise policy accepts one today.
  *(Open.)*
* **Private registries that need a credential.** *(Open.)*
* **Pinning by content digest.** Only `mcpb` packages carry a digest in `server.json`; an OCI
  image could be pinned by digest with a call to its container registry. This release pins by
  version. *(Open.)*
* **Gemini CLI**, whose disabled state lives in a separate user file, and **VS Code** and
  **Cursor**. *(Open.)*
* **Where this sits relative to the agent registry** (FerroWire's second phase). *(Open.)*

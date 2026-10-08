# A project's MCP servers

Each agent CLI keeps its own list of MCP servers, in its own file and format: `.mcp.json` for
Claude Code, `.codex/config.toml` for Codex, `.agents/mcp_config.json` for Antigravity. Kept by
hand, the copies drift. With `fl mcp`, a project has **one MCP catalog**, `.fl/mcp.toml`,
committed with the code, and fl writes every agent CLI's file from it. Turn a server on once and
every agent CLI in the project has it; turn it off once and all of them lose it.

The catalog holds the exact command each server runs, frozen when the server is added. Writing
the agent CLIs' files reads nothing else: no registry and no network, so every machine gets the
same files from the same commit. fl never reads, stores or writes a secret's value; the catalog
names the environment variable that holds it, and the agent CLI reads the variable when it
starts the server.

`fl mcp` opens no store and needs no project in fl's config. `--db` is refused with it, and
`$FL_DB` is ignored.

## The project

`fl mcp` works on the nearest directory, from the working directory up, that holds
`.fl/mcp.toml`; with none, on the nearest that holds `.git`, where the first `fl mcp registry` or
`fl mcp add` creates the catalog. A directory with neither above it is refused. Every vendor file
is written in that directory.

## The catalog

```toml
registry = "https://registry.modelcontextprotocol.io"

[server.notes]
from = "io.example/notes"
version = "1.2.0"
enabled = true
transport = "stdio"
command = "npx"
args = ["-y", "@example/notes-mcp@1.2.0", "./notes"]
env.NOTES_LOG = "info"
env.NOTES_TOKEN = { secret = true }

[server.docs]
enabled = true
vendors = ["claude", "codex"]
transport = "http"
url = "https://docs.example.com/mcp"
headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }
```

The top level has two keys:

| key | meaning |
|---|---|
| `registry` | The registry `fl mcp search`, `fl mcp add --from` and `fl mcp upgrade` read. Optional; there is no default. |
| `server` | One table per server, `[server.<name>]`. |

A server's name is 1 to 32 lowercase letters, digits and `-`, and it is the server's name in
every vendor file. `workspace`, `computer-use` and `claude-in-chrome` are refused: Claude Code
reserves them. A
server's table holds:

| key | meaning |
|---|---|
| `from` | The registry's name for the server, such as `io.example/notes`. Absent for a server added by hand. |
| `version` | The exact registry version the launch was frozen from. Present exactly when `from` is. |
| `enabled` | The team default: `true` or `false`. Default `true`. |
| `vendors` | The agent CLIs that get the server: any of `claude`, `codex` and `antigravity`. Default: all three. |
| `transport` | `stdio` (fl's vendor files start the server), `http` (streamable HTTP) or `sse` (the older HTTP with server-sent events). |
| `command` | The program a `stdio` server runs. Required for `stdio`, refused otherwise. |
| `args` | Its arguments, a list of strings. `stdio` only. |
| `env` | Its environment variables, `env.<NAME> = …`. `stdio` only. |
| `url` | The address of an `http` or `sse` server. Required for those, refused for `stdio`. |
| `headers` | The headers sent to an `http` or `sse` server, `headers.<Name> = …`. |

An unknown key is refused, not ignored, so a typo cannot pass for a setting. fl's edits keep
the catalog's comments and layout.

### Values and secret references

A value in `env` or `headers` is a literal string or a secret reference. A literal is committed
with the catalog, and is public if the repository is: `fl mcp add` warns about every literal it
records. A secret reference names the variable that holds the value, which fl never reads:

| reference | the value comes from |
|---|---|
| `env.API_TOKEN = { secret = true }` | the variable `API_TOKEN` |
| `env.API_TOKEN = { secret = true, env = "OTHER" }` | the variable `OTHER` |
| `headers.X-Api-Key = { secret = true, env = "DOCS_KEY" }` | `DOCS_KEY`, the whole header value |
| `headers.Authorization = { secret = true, env = "DOCS_TOKEN", scheme = "Bearer" }` | `DOCS_TOKEN`, sent as `Bearer <value>` |

In a reference, `secret` is always `true`: `secret = false` is refused, since a literal is
written as a string. `env` names the variable when it is not the key's own name; a secret header
needs `env`, and `scheme` is optional. Set each variable in the environment the agent CLI starts in; fl writes only
its name.

## Each machine's switches

A person turns a server on or off on their own machine in fl's config,
`$XDG_CONFIG_HOME/fl/config.toml` (default `~/.config/fl/config.toml`), never in the repository:

```toml
[[mcp]]
root = "/home/you/code/app"
disable = ["notes"]        # off on this machine, though the team default is on
enable = ["sentry"]        # on here, though the team default is off
```

`root` must be an absolute path. An `[[mcp]]` entry applies to the project at `root` and to every
project below it; the entry with the longest `root` wins, and it is independent of any
`[[project]]` entry. Two entries for one root are refused unless they say the same. A name in both
`enable` and `disable` is refused. A name the catalog does not have, such as a server removed
since, is ignored: `fl mcp sync` and `fl mcp check` print a warning naming it and carry on.
An fl older than this one refuses a config that has an `[[mcp]]` table; upgrade fl on that machine.

## The agent CLIs

| agent CLI | file, in the project | entries under |
|---|---|---|
| Claude Code | `.mcp.json` | `mcpServers` |
| Codex | `.codex/config.toml` | `[mcp_servers.<name>]` |
| Antigravity | `.agents/mcp_config.json` | `mcpServers` |

A server one agent CLI cannot run is left out of that file only, with a message naming the reason;
the others still get it. To say so in the catalog, give the server a `vendors` list without that
agent CLI.

**Claude Code** gets `command`, `args` and `env` for a `stdio` server, and `type`, `url` and
`headers` for an `http` or `sse` one. A secret is written as `"${NAME}"`, or `"Bearer ${NAME}"`
with a scheme, and Claude Code reads the variable when it starts the server. Claude Code passes an
unset `${NAME}` on as the literal text `${NAME}`, so a server whose secret is optional would get
that text instead of nothing: `fl mcp add` leaves an optional secret out unless named with
`--with`.

**Codex** gets `command`, `args`, `env` (literals) and `env_vars` (the names of the secret
variables it forwards) for a `stdio` server. It connects to streamable HTTP only, so an `sse`
server is left out. For an `http` server it gets `url`; a secret `Authorization` header with the
scheme `Bearer` becomes `bearer_token_env_var`, a secret header with no scheme becomes
`env_http_headers` (the variable holds the whole value), and a literal header `http_headers`. A
secret header with any other scheme, and a secret read from a variable of another name (`env =
"OTHER"`), are refused for Codex: it can add neither a prefix nor a new name. Codex reads a
project's `.codex/config.toml` only when its user trusts the project. `fl mcp sync` and `fl mcp
check` read `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`), never write it, and warn
unless it trusts the project's root, the directory holding `.git`: a `[projects."<root>"]` table
with `trust_level = "trusted"`. In a linked worktree, the main checkout's root is looked up when
the worktree's own has no table. A parent directory's trust does not count: Codex matches the
exact path. The warning prints the two lines to add.

**Antigravity** gets `command`, `args` and `env` for a `stdio` server, and `serverUrl` and
`headers` for an `http` one. It expands no `${NAME}`, but a `stdio` server inherits Antigravity's
environment: a secret variable is left out of the entry, and the server reads it from that
environment under its own name, so a secret read from a variable of another name is refused for
Antigravity. A server with a secret header is left out of Antigravity's file, since the header
cannot reach the server. Antigravity has no SSE, so an `sse` server is left out too.

### What fl keeps

fl adds, changes and removes only the entries it wrote. Every other entry in a vendor file stays
as it is, byte for byte, in its place; the keys keep their order. Codex's file keeps its comments
and layout. In a JSON file, the layout between the entries is rewritten with two-space indentation;
nothing but whitespace changes outside fl's own entries. A JSON file that is not strict JSON — comments, trailing commas, a
byte-order mark — is refused, never rewritten, since Antigravity accepts such files and a rewrite
would lose what they hold. Remove them by hand, then run `fl mcp sync` again.

## The commands

```text
fl mcp registry <url>
fl mcp search <text>
fl mcp add <name> --from <registry-name> [--version V] [--package npm|pypi|oci | --remote] [--env NAME=value]… [--with NAME]…
fl mcp add <name> [--env NAME[=value]]… -- <command> [args…]
fl mcp add <name> --url <url> [--header NAME[=ENV[:SCHEME]]]…
fl mcp remove <name>
fl mcp enable <name>
fl mcp disable <name>
fl mcp upgrade <name> [--to V] [--env NAME=value]… [--with NAME]…
fl mcp sync [--replace <name>]…
fl mcp check
```

`fl mcp registry <url>` sets the catalog's `registry`. `fl mcp search <text>` lists the registry's
servers whose name holds the text, with each one's latest version and description, and its status
when it is not `active`, such as `(deprecated)`.

`fl mcp add <name> --from <registry-name>` reads the registry's latest version, or `--version V`,
and freezes its launch into the catalog under `<name>`, recording the version the registry
returns. A server the registry offers several ways to start needs `--package npm`, `--package
pypi`, `--package oci` or `--remote`, and the refusal lists them. A variable the registry marks
secret becomes a reference. A required variable that is not secret and has no default needs
`--env NAME=value`, which is committed. An optional secret variable or argument is left out, with a
note naming the `--with NAME` that includes it. `add` prints the variables to set. The launch it
freezes:

| route | command and arguments |
|---|---|
| npm | `npx -y <package>@<version>` and the package's arguments |
| PyPI | `uvx <package>==<version>` and the package's arguments |
| OCI | `docker run -i --rm`, the image's runtime arguments, `-e NAME` for each variable passed, the image as the registry names it, and the package's arguments |
| remote | its `url`, `transport` from the remote's type, and its headers |

By hand, `fl mcp add <name> -- <command> [args…]` records a `stdio` server: `--env NAME=value` is a
literal, and `--env NAME` alone a secret reference. Either way `NAME` must be a variable's name,
capital letters, digits and `_`, as after `--header NAME=` below, or the refusal does not repeat
it. Under `--from` and `upgrade`, an `--env` with no `=` is refused as well, and repeated only if
it is a variable's name. `fl mcp add <name> --url <url>` records an
`http` server, whose `--header` is always a secret reference: `--header NAME` reads the variable
`<SERVER>_<NAME>` (uppercased, `-` as `_`), `--header NAME=ENV` reads `ENV`, and `--header
NAME=ENV:SCHEME` sends `<SCHEME> <value>`. What follows `=` must be a variable's name — capital
letters, digits and `_` — so a token typed there is refused, and the refusal does not repeat it. A
literal header is added by editing the catalog.

Each form of `add` takes its own flags: `--version`, `--package`, `--remote` and `--with` go with
`--from` only, `--header` with `--url` only, and `--env` with `--from` or a command. Any other is
refused, naming it, rather than ignored.

`add` prints the launch it recorded: the transport and the command with its arguments, or the URL
and the names of its headers. It never prints a value.

`fl mcp remove`, `fl mcp enable` and `fl mcp disable` change the catalog: the server, or its team
default.

`fl mcp upgrade <name>` reads the registry's latest version, or `--to V`, freezes it the way the
server was added (the same route, literals and included secrets), prints the difference field by
field, and rewrites that one entry, keeping its `enabled` and `vendors`. A version that is not
newer is refused unless named with `--to`. A new version may need what the old one did not: give a
required value with `--env NAME=value` and include an optional secret with `--with NAME`, as `add`
takes them. A server added by hand is refused: edit the catalog.

None of these writes a vendor file. Commit the catalog, then run `fl mcp sync`.

### `fl mcp sync`

`sync` reads the catalog and this machine's switches, plans every vendor file, prints the plan,
and writes it: `add`, `update` or `remove` for each entry it changes, `adopt` for an entry already
as fl writes it, `forget` for one a person removed that the catalog no longer wants. An entry of
fl's that a formatter, or Antigravity's own panel, laid out anew is adopted when it means the same
(the same JSON value, or the same TOML table with its keys in any order). A vendor file that is gone
as a whole, after a fresh clone or `git clean -fdX`, is written anew; an entry gone from a file
that still exists was removed by hand, and is refused. A vendor file that is a symbolic link, or
lies under a linked `.codex` or `.agents`, is refused: fl writes only plain files it can see whole,
never through a link into a file git tracks or one outside the project. The project itself may be
reached through a link. It is all or nothing:
every file is planned before any is written, and one refusal writes nothing.
The files are written under a lock, each read again before any is written; a file that changed
since it was planned is refused, and nothing is written. Each file is written to a temporary file
beside it and renamed over it, keeping its mode.

### `fl mcp check`

`check` plans as `sync` does and writes nothing:

| exit | meaning |
|---|---|
| 0 | every vendor file matches the catalog |
| 1 | a `sync` would change something; the plan lists what |
| 2 | a refusal, or an error |

`sync` and `check` never contact a registry or the network, so `check` can serve as a gate: run it
before an agent session, and `sync` when it exits 1. On a fresh clone, where no vendor file exists
yet, it exits 1; an exit of 2 means the catalog, the switches or a vendor file needs a person.

## The `.gitignore` lines

The vendor files are generated on each machine from the catalog, so git must ignore them. The
project's `.gitignore` holds:

```text
/.mcp.json
/.codex/config.toml
/.agents/mcp_config.json
```

`sync` and `check` refuse a vendor file fl writes, or holds an entry in, that git tracks or would
not ignore, and print the lines to add; a tracked one also names `git rm --cached <file>`. fl does
not edit `.gitignore`. A vendor file fl has no entry in needs no line.

## What fl refuses, and why

Every refusal exits 2 and names the file, the entry and what to do next; an error prints
`error: …`.

* **The catalog.** A key fl does not know, a value of the wrong kind, a name it does not accept, a
  `stdio` server with no `command` or with a `url`, an `http` or `sse` server with no `url`, a
  `from` without a `version`. The catalog is the only source of truth, so fl reads none of it
  until all of it is right. A catalog that does not parse is refused with the line, the column and
  the parser's message, never the line itself, which could hold a pasted token; a key or value the
  message would quote is shown as `<value>`. A rule's refusal names the server and the field,
  never the value.
* **The registry's address.** Anything but `https://`, or `http://` to this machine
  (`127.0.0.1`, `localhost` or `[::1]`), and an address with a user name or password: a registry
  entry is code that will run on every teammate's machine.
* **A registry that misbehaves.** A redirect (fl follows none), an HTML page, a body larger than 4
  MiB (counted after fl decodes a compressed answer), an answer that is not the registry API's
  shape (a body that is not valid UTF-8 is one), an error status: each is reported as the
  registry's failure. `sync` and `check` need no registry.
* **A registry entry fl cannot freeze honestly.** A deleted server, naming the publisher's
  message; several launch routes and no `--package` or `--remote`; a package type other than npm,
  PyPI and OCI, such as `mcpb`, `cargo` or `nuget`; a package that serves HTTP itself, which would
  need starting; an OCI image with neither a tag nor a digest, tagged `latest`, or whose name starts with
  `-`, which docker would read as an option; a version that is not exact (an npm package's
  must be a semver version and a PyPI package's an exact PyPI version) or a package name that
  is not a plain npm or PyPI name; a header whose name is not an HTTP header name; any string of
  the entry that holds a control or invisible character, which fl would have to commit;
  runtime arguments with a positional argument; a secret inside an argument, except
  docker's `-e NAME={secret}`, which becomes `-e NAME` and a reference; a required argument or
  variable with no value; an argument of a type fl does not know. Each refusal names what to do,
  often `fl mcp add <name> -- <command>` by hand. An `--env` key or a `--with` name that the
  entry does not use is refused too; when it is not a variable's name, the refusal does not
  repeat it.
* **A secret given as a value.** `--env NAME=value` for a variable the registry marks secret, a
  value after `--header NAME=`, and an `--env` with no `=` that is not a variable's name: a
  secret is never recorded. Set it in the environment instead. The
  refusal does not repeat the value.
* **An upgrade that is not newer**, unless named with `--to`.
* **A vendor file fl cannot edit safely.** A JSON file that is not strict JSON, whose top level or
  `mcpServers` is not an object, or with a key twice; a Codex file that is not valid TOML, or whose
  `mcp_servers` is not a table. Fix it by hand; the message never shows the file's text.
* **An entry someone else changed.** An entry fl wrote that was changed by hand since (not just laid
  out anew), or removed by hand from a file that still exists while the catalog still wants it,
  and an entry fl did not write that differs from what fl would write under the same name. fl
  never changes or removes an entry it did not write. The
  refusal names the fields that differ, never their values, since a hand-edited entry may hold a
  pasted token. Restore the entry, or run `fl mcp sync --replace <name>`, which shows the same
  difference and writes fl's entry over it. An entry fl wrote that was changed by hand while
  the catalog no longer wants it is refused too, and `--replace <name>` removes it.
* **A file that changed while `sync` planned**, by another program or a person. Run `sync` again.
* **A vendor file git would commit.** See the `.gitignore` lines above.
* **A vendor file reached through a link**: the file, or a directory on its way, is a symbolic
  link.
* **No catalog**, for `sync`, `check` and the edits other than `registry` and `add`: fl never reads
  a missing catalog as an empty one, which would remove every entry it wrote. **No registry** in the
  catalog, for `search`, `add --from` and `upgrade`.

A vendor that cannot run a server — an `sse` server for Codex or Antigravity, a secret header for
Antigravity, a secret under another name or a header scheme Codex cannot send — is not a refusal:
the server is left out of that one file, and the message names the reason.

## The registry

Any registry that serves the MCP registry API, version `v0.1`, will do; the official one is
`https://registry.modelcontextprotocol.io`. fl sends no credential: it reads only what anyone can.
`fl mcp search` uses the registry's own search, which matches a substring of the server's name,
not its description, and keeps only the servers whose name holds the text, whatever the registry
sends. It reads up to 20 pages of results, saying so when it stops there, and prints the
registry's text without its control characters.

`add --from` and `upgrade` record the version the registry returns, never the word `latest`. A
server the registry marks deprecated is added with a warning that gives its message; one marked
deleted is refused. Nothing the registry says changes what runs on anyone's machine until a
person commits the catalog.

## Where fl keeps what it wrote

For each vendor file, fl keeps an **ownership record** in `$XDG_STATE_HOME/fl/mcp/` (default
`~/.local/state/fl/mcp/`): a JSON file named by the SHA-256 of the vendor file's canonical path,
listing each entry fl wrote there, the project root it came from, and the SHA-256 of the entry as
written. That is how `sync` tells its own entries from a person's. The records belong to the
machine and are never in a repository. The same directory holds `sync.lock`, which a second `sync`
waits for.

A record fl cannot read is refused, naming it: delete it, and the next `sync` adopts every entry
that is exactly as fl would write it. An entry that differs is then refused as one fl did not
write, and `--replace` takes it back.

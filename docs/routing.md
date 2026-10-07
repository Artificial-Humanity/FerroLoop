# Routing items between the local store and GitHub

A project can keep its records and findings in two places at once: the **local tier** — the
project's local store, for developer-level items such as code, code quality and unit tests —
and the **github tier** — the GitHub repository its config binds, for human-level items such as
design, product and security review. Each new record and finding goes to one of the two by its
**area**, a short name such as `code` or `design`, through the project's **routing map**.

## Declaring areas

`fl routing set --project <project> <area> <tier>` routes an area to a tier, `local` or
`github`. An area name is 1 to 32 lowercase letters, digits and `-`. The project's first `set`
writes the starting set first, then the area it names:

| area | tier | sensitive |
|---|---|---|
| `code` | `local` | |
| `design` | `github` | |
| `product` | `github` | |
| `security` | `github` | yes |
| `tests` | `local` | |

`--sensitive` marks an area whose items are security items: a finding made there is a security
finding, and the map never sends such an item to a repository that is not private. Setting an
area that exists changes its tier for new items only; items already made stay where they are. A
`set` without `--sensitive` keeps the area's sensitivity: changing an area's tier never clears it.
After the first `set`, a `set` that adds an area the map does not declare must say
`--sensitive` or `--not-sensitive`: the area may have been removed, and may have been sensitive.
`fl routing show --project <project>` prints the map, one area per line: its name, its tier, and
`sensitive` or `-`.

The map is authored in the store that authors the project, like its gates, and reaches other
machines in the committed manifest ([sharing-gates.md](sharing-gates.md)): after changing it, run
`fl manifest export` and commit. A store that imported the project refuses `fl routing set`.

A routed project needs a store of its own, because handles are numbered per store: `fl routing
set` is refused while the project's store holds another project, and a routed store takes no
other project afterwards. Give the project its own store with a config entry of its own
`store`.

## Handles change

Once a project has a routing map, `#41` and `owner/repo#41` always name GitHub issue 41, and a
bare `41` names local item 41. The first `fl routing set` says what that changes for the project:

* a project that was local-only: `#3` named local item 3, and now names GitHub issue 3;
* a project that was bound to GitHub: a bare `41` named issue 41, and now names local item 41,
  and local records made before the binding appear in lists again.

The import that first brings a routing map to another machine says the same there.

`fl record` and `fl finding` read a bare number as a local item: one that no local item holds
is refused, asking whether `#41` was meant. A GitHub item prints as `#41` and a local one as
`41`, so every handle fl prints can be typed back. `fl github …` names GitHub items only, so a
bare number there is still an issue.

## What a routed project needs

A project is routed once it has a routing map, whatever the map holds. The `github` tier is
available on a machine whose config binds the project to a repository (the `tracker` binding of
[github-tracker.md](github-tracker.md)). Without one, the machine works on local items: a
`github`-tier item is refused, naming the missing config entry, and fl never puts it in the local
tier instead.

fl opens GitHub only when a command needs a GitHub item — a GitHub-tier create, a GitHub id or
`#41` handle, a list of both tiers — so work on local items needs neither the network nor a
credential. `fl github …` names GitHub items only, and opens GitHub at the start.

A routed project keeps its runs and decisions in the local store: a binding with `ledger =
"github"` is refused while the project is routed, and `fl routing set` refuses to route a project
whose binding names it.

Every machine routes the same way. On a machine that imported the project, a new item needs the
import to be current (`fl manifest import`). On the machine that authors it, once the project has
a manifest, the manifest must carry the current map (`fl manifest export`, then commit).

## Making items

In a routed project every new record and finding has an area. `fl record add --area <area>`
routes a record by its area; `--tier local` or `--tier github` puts it in that tier instead, and
the area is recorded either way. `fl finding raise` takes the same two options. With no `--area`,
a finding takes its record's area, and fl says so: `note: area: code, from its record`. A finding
whose record has no area — one made before the project was routed — needs `--area`. An area the
map does not declare is refused, naming the declared ones; so are `--area` and `--tier` in a
project without a routing map.

A finding and its record may be in different tiers. A finding on GitHub about a local record
shows the record's title and IRI as text; on a repository that is not private, fl warns before it
publishes them — and refuses, saying to use `--tier local`, when the record is in a sensitive
area: nothing about an item in a sensitive area reaches a repository that is not private.
A record whose area the map no longer declares counts as sensitive here, since it may have been
declared sensitive once.

A finding made in a sensitive area is a security finding. When the map would send a security
finding, or any item of a sensitive area, to a repository that is not private, fl refuses and
says to use `--tier local`: it never moves an item to the local tier by itself.

## Lists

In a routed project `fl record list` and `fl finding list` read both tiers and show each item's
tier in a second column (`1\tlocal\ttodo\t…`, `#1\tgithub\ttodo\t…`); `--tier local` or `--tier
github` reads one. When a tier cannot be read — GitHub is down, or this machine binds no
repository — the list is refused rather than shown in part, and the refusal suggests `--tier
local`. `fl finding list --record <id>` lists one record's findings, from both tiers; it works in
any project. The withdrawal counts under a finding list sum both tiers; with `--tier`, they count
that tier, and say so.

## Removing an area

`fl routing remove --project <project> <area>` removes an area from the map. It is refused while
any item in either tier names the area; the refusal gives the count and lists up to ten of the
items. fl finds GitHub items by the block in each issue, not by the area label, so an item that
lost its label is still found. A tier that cannot be read refuses the removal. fl can read only
this machine's local tier: an item in another machine's local store keeps the area as history,
and only new items are refused it. Such an item counts as sensitive while the map does not
declare its area: fl cannot know what the removed area was, so a finding about it never reaches a
repository that is not private. fl never deletes an area's label from GitHub. There is no rename:
add the new area and remove the old one.

`fl routing set --project <project> <area> <tier> --not-sensitive` clears an area's sensitivity.
It is refused, by the same check and with the same list, while any item in either tier names the
area. Setting an area the map does not declare — a removed one again, or a new one — needs
`--sensitive` or `--not-sensitive`, and `--not-sensitive` there is refused by the same check.
That check reads only this machine's local tier: after `--not-sensitive`, another machine's
local items that name the area no longer count as sensitive.

## Limits

* A machine with no binding for the project lists with `--tier local`: a list of both tiers needs
  both.
* A routed project has a store to itself: a store holding another project cannot be routed.
* An older fl refuses a routed project's manifest (format 3) and its store (format 5) as newer
  formats. On a machine whose store is already bound to GitHub and that runs an older fl, the
  project behaves as an ordinary GitHub-bound project, and makes GitHub items with no area.
* A routed project keeps its ledger local: the GitHub ledger is not available to it yet.

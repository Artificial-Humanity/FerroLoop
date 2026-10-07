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
`set` without `--sensitive` keeps the area's sensitivity: changing an area's tier never clears it. `fl routing show --project <project>` prints the map, one area per line:
its name, its tier, and `sensitive` or `-`.

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

# Council Chair catalog role

Lead scope approval: FerroWire `01a1b1a0-91ae-7739-85f7-2692e6412891`, following Owner request `01a1b19b-a4fa-733e-b226-47567afabd04`.

Council Chair is an editable `RoleRecord`, slug `council-chair`, label `Council Chair`, standard `lead-review`. Its runtime identity remains `AgentRole::Custom("council-chair")` / `custom:council-chair`. It inherits strategist guidance and adds preferred advisory contact, request framing and decision criteria, evidence/disagreement synthesis, and bounded adjudication to a concrete recommendation/action or named unresolved decision. It cannot override the Owner or Project Lead and receives no additional permission or review tier.

Use existing catalog data and persisted overrides, not a new enum variant, role pack framework, or shared `.agents` file. Council Chair is an optional starter record, **not** a protected builtin slug: it must be editable/removable, and a tombstone must not resurrect it. Existing built-in conversion is deferred. The template references existing strategist guidance and supplies Chair responsibilities inline. Lead clarified this acceptance detail in `01a1b1a5-3525-76bb-baa9-5c6a4e3a8a5f` (Record #357).

UI choices use authenticated catalog reads and persisted saves, retaining existing executable roles that are not in the catalog. Labels are origin-neutral; internal wire IDs remain compatible. In particular, `custom:lead`, `custom:assistant`, and custom Lab Manager spellings remain nonstructural. Display labels never authorize an operation.

Creation resolves supported catalog roles before side effects and puts current stored guidance into hydration. Recast remains registry-only and tells the user guidance requires explicit Reforge. Explicit current-role hydration must not restore stale registered role/project from cached hydration or hosted metadata. Preserve default/legacy role behavior; resolve opaque legacy hydration compatibility narrowly rather than introducing a general hydration framework. No automatic injection, restart or runtime rollout.

Lead clarification `01a1b1a5-e671-70ba-97f2-df94602c1faa` permits narrow persisted per-agent recast-guidance-required Runtime metadata on actual project/role reassignment. Bind it to stable agent identity and intended current assignment; it selects current registry/catalog guidance at explicit hydration, not automatic dispatch. An unchanged legacy/default agent retains the external `agent-hydrate` behavior. Do not edit that command or its files, infer role from arbitrary prompt text, clear precedence on transport acceptance, or report a fully completed recast if required metadata persistence failed.

Alternatives: a new enum/file would duplicate role data and was rejected by the request; a label-only addition would leave validation and guidance disconnected; automatic recast dispatch would introduce a new delivery contract and is explicitly excluded. The accepted path connects the existing catalog and explicit hydration boundary.

No live agent/session changes, installation, shared instructions, Host work, or held peer production work are included. Felix cleared UI overlap; JARVIS confirmed no shared role file is necessary.

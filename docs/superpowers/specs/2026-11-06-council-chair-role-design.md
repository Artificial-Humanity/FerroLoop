# Council Chair catalog role

Lead scope approval: FerroWire `01a1b1a0-91ae-7739-85f7-2692e6412891`, following Owner request `01a1b19b-a4fa-733e-b226-47567afabd04`.

Council Chair is an editable `RoleRecord`, slug `council-chair`, label `Council Chair`, standard `lead-review`. Its runtime identity remains `AgentRole::Custom("council-chair")` / `custom:council-chair`. It inherits strategist guidance and adds preferred advisory contact, request framing and decision criteria, evidence/disagreement synthesis, and bounded adjudication to a concrete recommendation/action or named unresolved decision. It cannot override the Owner or Project Lead and receives no additional permission or review tier.

Use the existing default role catalog and persisted overrides, not a new enum variant, role pack framework, or shared `.agents` file. A default catalog record is editable; its default/builtin metadata does not confer structural authority. The template references existing strategist guidance and supplies Chair responsibilities inline.

UI choices use authenticated catalog reads and persisted saves, retaining existing executable roles that are not in the catalog. Labels are origin-neutral; internal wire IDs remain compatible. In particular, `custom:lead`, `custom:assistant`, and custom Lab Manager spellings remain nonstructural. Display labels never authorize an operation.

Creation resolves supported catalog roles before side effects and puts current stored guidance into hydration. Recast remains registry-only and tells the user guidance requires explicit Reforge. Explicit current-role hydration must not restore stale registered role/project from cached hydration or hosted metadata. Preserve default/legacy role behavior; resolve opaque legacy hydration compatibility narrowly rather than introducing a general hydration framework. No automatic injection, restart or runtime rollout.

Alternatives: a new enum/file would duplicate role data and was rejected by the request; a label-only addition would leave validation and guidance disconnected; automatic recast dispatch would introduce a new delivery contract and is explicitly excluded. The accepted path connects the existing catalog and explicit hydration boundary.

No live agent/session changes, installation, shared instructions, Host work, or held peer production work are included. Felix cleared UI overlap; JARVIS confirmed no shared role file is necessary.

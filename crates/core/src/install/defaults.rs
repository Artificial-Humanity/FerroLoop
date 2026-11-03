//! The manifest compiled into the binary.
//!
//! No signed release exists yet, so this is a `source_build` manifest: it
//! declares the products, their dependencies, profiles and actions, and lists
//! no artifacts. It is data; the unit tests check it against the architecture
//! model (`fl-cli`'s `install_manifest_matches_architecture` test).

use super::manifest::{
    ActionKind, ActionSpec, Compat, FileKind, ProductEntry, ProductFile, ProfileEntry, ReleaseInfo,
    ReleaseKind, ReleaseManifest,
};
use super::{Arch, MANIFEST_SCHEMA_VERSION, Os, Product, Profile, Target};

/// The FerroWire protocol version this source tree speaks. Data, not a link:
/// `fl-cli`'s `install_manifest_matches_the_code` test compares it with
/// `fl_wire::peer::WIRE_PROTOCOL_VERSION`.
const PROTOCOL_MAJOR: u32 = 2;
/// The oldest and newest store format this tree opens (`fl_store::FORMAT_VERSION`
/// and `FORMAT_WITH_WORKFLOW`; compared in the same test).
const STORE_FORMAT_MIN: u32 = 2;
const STORE_FORMAT_MAX: u32 = 8;

fn ids(ps: &[Product]) -> Vec<String> {
    ps.iter().map(|p| p.as_wire().to_string()).collect()
}

fn unix_targets() -> Vec<Target> {
    let mut t = Vec::new();
    for os in [Os::Linux, Os::Macos] {
        for arch in [Arch::X86_64, Arch::Aarch64] {
            t.push(Target { os, arch });
        }
    }
    t
}

fn file(path: &str, kind: FileKind) -> ProductFile {
    ProductFile {
        path: path.to_string(),
        kind,
    }
}

fn action(kind: ActionKind, subject: &str, detail: &str) -> ActionSpec {
    ActionSpec {
        kind,
        subject: subject.to_string(),
        os: Vec::new(),
        planned_in: Vec::new(),
        by_profile: Vec::new(),
        detail: detail.to_string(),
    }
}

impl ActionSpec {
    fn linux(mut self) -> Self {
        self.os = vec![Os::Linux];
        self
    }
    fn planned_in(mut self, p: &[Profile]) -> Self {
        self.planned_in = p.to_vec();
        self
    }
    fn by_profile(mut self, p: &[Profile]) -> Self {
        self.by_profile = p.to_vec();
        self
    }
}

const LINGER: &str = "boot persistence needs linger: loginctl enable-linger $USER";
const DAEMON: &[Profile] = &[Profile::Headless, Profile::Full, Profile::Lab];
const FULL_UP: &[Profile] = &[Profile::Full, Profile::Lab];

fn entry(p: Product) -> ProductEntry {
    ProductEntry {
        id: p.as_wire().to_string(),
        requires: Vec::new(),
        optional: Vec::new(),
        targets: unix_targets(),
        files: Vec::new(),
        aliases: Vec::new(),
        actions: Vec::new(),
    }
}

/// The built-in manifest.
pub fn default_manifest() -> ReleaseManifest {
    let mut loop_ = entry(Product::Ferroloop);
    loop_.requires = ids(&[Product::Ferrostore]);
    loop_.files = vec![file("bin/fl", FileKind::Binary)];
    loop_.actions = vec![action(
        ActionKind::PathChange,
        "launcher-dir",
        "put the launcher directory on PATH",
    )];

    let mut store = entry(Product::Ferrostore);
    store.files = vec![
        file("bin/ferrostore", FileKind::Binary),
        file("contrib/units/ferrostore.service", FileKind::UnitTemplate),
        file("contrib/units/ferrostore@.service", FileKind::UnitTemplate),
    ];
    store.actions = vec![
        action(ActionKind::RegisterService, "ferrostore.service", "")
            .linux()
            .planned_in(DAEMON)
            .by_profile(FULL_UP),
        action(ActionKind::EnableAtBoot, "ferrostore.service", LINGER)
            .linux()
            .planned_in(DAEMON),
        action(
            ActionKind::RegisterService,
            "ferrostore@.service",
            "per-project store instances",
        )
        .linux()
        .planned_in(&[Profile::Lab])
        .by_profile(&[Profile::Lab]),
        action(
            ActionKind::MigrateState,
            "store",
            "a normal store open can migrate .redb to .fls",
        ),
    ];

    let mut wire = entry(Product::Ferrowire);
    wire.requires = ids(&[Product::Ferrostore]);
    wire.files = vec![file(
        "contrib/units/ferrowire.service",
        FileKind::UnitTemplate,
    )];
    wire.aliases = vec!["agent-msg".to_string(), "agent-bus-mcp".to_string()];
    wire.actions = vec![
        action(ActionKind::RegisterService, "ferrowire.service", "")
            .linux()
            .planned_in(DAEMON)
            .by_profile(FULL_UP),
        action(ActionKind::EnableAtBoot, "ferrowire.service", LINGER)
            .linux()
            .planned_in(DAEMON),
        action(ActionKind::StartListener, "ferrowire.service", "")
            .linux()
            .planned_in(DAEMON),
    ];

    let mut state = entry(Product::Ferrostate);
    state.requires = ids(&[Product::Ferrostore]);
    state.actions = vec![
        action(
            ActionKind::McpCatalog,
            "ferrostate",
            "`fl state mcp` catalog entry",
        )
        .planned_in(DAEMON)
        .by_profile(DAEMON),
    ];

    let mut gate = entry(Product::Ferrogate);
    gate.actions = vec![
        action(
            ActionKind::McpCatalog,
            "ferrogate",
            "`ferrogate` catalog entry",
        )
        .planned_in(DAEMON)
        .by_profile(DAEMON),
        action(
            ActionKind::InitCredentials,
            "vault",
            "initialize the vault; an existing key is never replaced",
        ),
    ];

    let mut track = entry(Product::Ferrotrack);
    track.requires = ids(&[Product::Ferrostore]);

    let mut watch = entry(Product::Ferrowatch);
    watch.requires = ids(&[Product::Ferrostore]);

    let mut deck = entry(Product::Ferrodeck);
    deck.requires = ids(&[Product::Ferrostore, Product::Ferrowire]);
    deck.optional = ids(&[Product::Ferrogate, Product::Ferrowatch, Product::Ferrotrack]);
    deck.files = vec![
        file("ui/dist", FileKind::AssetDir),
        file(
            "contrib/units/ferroloop-hub.service",
            FileKind::UnitTemplate,
        ),
    ];
    deck.actions = vec![
        action(ActionKind::RegisterService, "ferroloop-hub.service", "")
            .linux()
            .planned_in(FULL_UP)
            .by_profile(FULL_UP),
        action(ActionKind::EnableAtBoot, "ferroloop-hub.service", LINGER)
            .linux()
            .planned_in(FULL_UP),
        action(
            ActionKind::StartListener,
            "ferroloop-hub.service",
            "a loopback listener; a public listener is never bound",
        )
        .linux()
        .planned_in(FULL_UP),
    ];

    let mut step = entry(Product::Ferrostep);
    step.requires = ids(&[Product::Ferroloop]);

    let all = [
        Product::Ferroloop,
        Product::Ferrowire,
        Product::Ferrostate,
        Product::Ferrogate,
        Product::Ferrotrack,
        Product::Ferrowatch,
        Product::Ferrodeck,
        Product::Ferrostep,
    ];
    let headless = [
        Product::Ferroloop,
        Product::Ferrowire,
        Product::Ferrostate,
        Product::Ferrogate,
        Product::Ferrowatch,
        Product::Ferrotrack,
    ];

    ReleaseManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        release: ReleaseInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            kind: ReleaseKind::SourceBuild,
            commit: None,
            toolchain: None,
        },
        compat: Compat {
            protocol_major: PROTOCOL_MAJOR,
            store_format_min: STORE_FORMAT_MIN,
            store_format_max: STORE_FORMAT_MAX,
        },
        artifacts: Vec::new(),
        products: vec![loop_, store, wire, state, gate, track, watch, deck, step],
        profiles: vec![
            ProfileEntry {
                id: Profile::Minimal,
                products: ids(&[Product::Ferroloop]),
            },
            ProfileEntry {
                id: Profile::Headless,
                products: ids(&headless),
            },
            ProfileEntry {
                id: Profile::Full,
                products: ids(&all),
            },
            ProfileEntry {
                id: Profile::Lab,
                products: ids(&all),
            },
        ],
    }
}

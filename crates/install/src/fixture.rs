//! Test fixtures: an EXTRACTED artifact tree for the host, and a signed-release manifest that
//! pins it with its tree digest. Used by this crate's tests and, through the `fixtures`
//! feature, by fl-cli's.

use crate::tree::scan_tree;
use fl_core::install::{
    Artifact, HostInfo, Product, ReleaseKind, ReleaseManifest, default_manifest, sha256_hex,
};
use std::fs;
use std::path::Path;

/// The files a release of `variant` holds (relative path, content).
pub fn payload(variant: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![
        ("bin/fl".into(), format!("#!/bin/sh\necho fl {variant}\n")),
        (
            "bin/ferrostore".into(),
            format!("#!/bin/sh\necho ferrostore {variant}\n"),
        ),
        (
            "contrib/units/ferrostore.service".into(),
            format!("[Unit]\nDescription=store {variant}\n"),
        ),
        (
            "contrib/units/ferrostore@.service".into(),
            format!("[Unit]\nDescription=store instance {variant}\n"),
        ),
        (
            "contrib/units/ferrowire.service".into(),
            format!("[Unit]\nDescription=wire {variant}\n"),
        ),
        (
            "contrib/units/ferroloop-hub.service".into(),
            format!("[Unit]\nDescription=hub {variant}\n"),
        ),
        (
            "ui/dist/index.html".into(),
            format!("<html>{variant}</html>\n"),
        ),
        (
            "ui/dist/assets/app.js".into(),
            format!("console.log('{variant}');\n"),
        ),
        // A file no product declares: covered by the digest, never staged.
        ("NOTICE".into(), format!("notice {variant}\n")),
    ];
    v.sort();
    v
}

/// Write the artifact tree `<root>/<name>/` for `variant`.
pub fn write_artifact(root: &Path, name: &str, variant: &str) {
    for (rel, content) in payload(variant) {
        let p = root.join(name).join(&rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
}

/// A signed-release manifest for the host: the default manifest's products and profiles, one
/// artifact `<name>` that provides every product, pinned by the tree digest of what is on disk.
pub fn manifest_for(root: &Path, name: &str, version: &str) -> ReleaseManifest {
    let scanned = scan_tree(&root.join(name), u64::MAX / 2).expect("fixture tree scans");
    let host = HostInfo::from_consts(std::env::consts::OS, std::env::consts::ARCH)
        .expect("a supported host");
    let mut m = default_manifest();
    m.release.version = version.to_string();
    m.release.kind = ReleaseKind::Release;
    m.release.commit = Some(sha256_hex(version.as_bytes())[..40].to_string());
    m.artifacts = vec![Artifact {
        id: format!(
            "tree-{host_os}-{host_arch}",
            host_os = host.os.as_wire(),
            host_arch = host.arch.as_wire()
        ),
        target: host.target(),
        file: name.to_string(),
        sha256: scanned.digest,
        size: scanned.total_bytes,
        provides: Product::ALL.iter().map(|p| p.to_string()).collect(),
    }];
    m
}

/// Write the artifact tree and the manifest for one release in one go.
pub fn release(root: &Path, version: &str, variant: &str) -> ReleaseManifest {
    let name = format!("payload-{version}-{variant}");
    write_artifact(root, &name, variant);
    manifest_for(root, &name, version)
}

/// The release directory name a fixture manifest produces.
pub fn release_name(m: &ReleaseManifest) -> String {
    fl_core::install::release_dir_name(m)
}

//! Stamps the build with the commit, the dirty flag and the build time. The logic is the
//! shared, tested `crates/core/build/stamp.rs`.

#[path = "../core/build/stamp.rs"]
mod stamp;

fn main() {
    // This package and every crate it builds from by path: their sources decide the dirty
    // flag and are what cargo reruns the script for (with `HEAD`). Cargo rebuilds the package
    // and its dependants whenever the script reruns, so the list is exactly the closure, no
    // wider; `crates/core/tests/build_stamp.rs` checks it against the `Cargo.toml` files.
    stamp::emit(&[
        ".",
        "../core",
        "../exec",
        "../gate",
        "../github",
        "../host-proto",
        "../install",
        "../mcp",
        "../store",
        "../watch",
        "../wire",
    ]);
}

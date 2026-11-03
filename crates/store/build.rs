//! Stamps the build with the commit, the dirty flag and the build time. The logic is the
//! shared, tested `crates/core/build/stamp.rs`.

#[path = "../core/build/stamp.rs"]
mod stamp;

fn main() {
    // The crate's own sources move the dirty flag; the git state moves the commit.
    stamp::emit(&["src", "Cargo.toml"]);
}

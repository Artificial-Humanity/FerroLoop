//! Stamps the build with the commit, the dirty flag and the build time. The logic is the
//! shared, tested `crates/core/build/stamp.rs`.

#[path = "../core/build/stamp.rs"]
mod stamp;

fn main() {
    // `fl` links every crate of the workspace, so a change to any of them moves the dirty
    // flag: watch `crates/` and the lock file. (A leaf binary, so the rebuild costs nothing
    // that the change itself would not.)
    stamp::emit(&["..", "../../Cargo.lock"]);
}

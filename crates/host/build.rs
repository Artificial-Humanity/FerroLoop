//! Stamp the standalone daemon without depending on core or vendor libraries.
#[path = "../core/build/stamp.rs"]
mod stamp;

fn main() {
    stamp::emit(&[".", "../host-proto"]);
}

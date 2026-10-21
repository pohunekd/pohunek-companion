//! Exposes the pinned `pohunek-client` version (from `Cargo.lock`) to the
//! setup screen, which names it next to the daemon's when the two disagree.

use std::fs;
use std::path::Path;

fn main() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = fs::read_to_string(&lock).expect("read the workspace Cargo.lock");
    let version = text
        .split("[[package]]")
        .find(|block| block.contains("\nname = \"pohunek-client\"\n"))
        .and_then(|block| {
            block
                .lines()
                .find_map(|line| line.strip_prefix("version = \""))
        })
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("Cargo.lock pins pohunek-client");
    println!("cargo:rustc-env=POHUNEK_CLIENT_VERSION={version}");
}

//! Exposes the pinned `pohunek-client` version and git tag (from `Cargo.lock`)
//! to the setup screen: it names the version next to the daemon's when the
//! two disagree, and the installer downloads exactly that release.

use std::fs;
use std::path::Path;

fn main() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = fs::read_to_string(&lock).expect("read the workspace Cargo.lock");
    let block = text
        .split("[[package]]")
        .find(|block| block.contains("\nname = \"pohunek-client\"\n"))
        .expect("Cargo.lock pins pohunek-client");
    let field = |prefix: &str| {
        block
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .and_then(|rest| rest.strip_suffix('"'))
    };
    let package_version = field("version = \"").expect("pohunek-client has a version");
    let source = field("source = \"").expect("pohunek-client has a git source");
    // A `rev` pin (pre-release builds) has no tag; the package version names the release.
    let tag = match source.split_once("?tag=") {
        Some((_, rest)) => rest
            .split('#')
            .next()
            .expect("split yields at least one part")
            .to_owned(),
        None if source.contains("?rev=") => format!("v{package_version}"),
        None => panic!("pohunek-client is pinned to neither a git tag nor a rev"),
    };
    // The tag becomes a URL path segment and part of file names.
    assert!(
        tag.len() > 1
            && tag.starts_with('v')
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
        "unexpected pohunek-client tag {tag:?}"
    );
    println!("cargo:rustc-env=POHUNEK_CLIENT_VERSION={package_version}");
    println!("cargo:rustc-env=POHUNEK_CLIENT_TAG={tag}");
}
